//! "What's inside": the files in a ZIP the Download File window is about to download, listed
//! from the end of the archive with range requests, and single files of it downloaded on their
//! own (see [`crate::zipdir`]). The requests carry the capture's own request context: the link,
//! its cookies, referer, user agent, headers and credentials, and the proxy settings.

use super::{Core, jobs};
use crate::events::{CoreEvent, ZipProgress};
use crate::zipdir::{self, Entry, ReadAt, ReadError, Unpacker, ZipError};
use prism_engine::{EngineError, RemoteFile};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// How long reading the list may take.
const LIST_TIME: Duration = Duration::from_secs(30);
/// Unpacked bytes handed to the disk at a time.
const OUT_BUF: usize = 256 << 10;
/// How often a file's download reports progress.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// The in-progress file next to the final one, as for downloads.
const PART: &str = ".prismpart";

/// A file in the archive, for the window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ZipFile {
    /// Its number in the archive (what [`Core::extract_zip_file`] takes).
    pub index: u32,
    /// Path inside the archive, folders separated by `/`.
    pub path: String,
    /// Unpacked size.
    pub size: u64,
    /// Protected with a password.
    pub encrypted: bool,
    /// Prism can download just this file (stored or deflated, not encrypted).
    pub extractable: bool,
}

/// Why the list or a file couldn't be read: a code the window explains (`zip:…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekProblem {
    /// The server doesn't send parts of the file (or the link isn't a plain HTTP `GET`).
    NoRanges,
    NotZip,
    /// A split archive, its list spread over the parts.
    Split,
    /// More files than Prism lists.
    TooBig,
    /// The server wants a sign-in, a browser session or a fresh link.
    Protected,
    /// The file on the server changed between two requests.
    Changed,
    Server,
    Network,
    /// The archive's records or a file's data don't add up.
    Damaged,
    /// The file couldn't be written.
    Disk,
    /// The antivirus found a threat in the file (it isn't kept).
    Threat,
    /// Encrypted, or packed in a way Prism doesn't unpack.
    Unsupported,
    /// Stopped, or the window went away.
    Cancelled,
}

impl PeekProblem {
    pub fn code(self) -> &'static str {
        match self {
            PeekProblem::NoRanges => "zip:noRanges",
            PeekProblem::NotZip => "zip:notZip",
            PeekProblem::Split => "zip:split",
            PeekProblem::TooBig => "zip:tooBig",
            PeekProblem::Protected => "zip:protected",
            PeekProblem::Changed => "zip:changed",
            PeekProblem::Server => "zip:server",
            PeekProblem::Network => "zip:network",
            PeekProblem::Damaged => "zip:damaged",
            PeekProblem::Disk => "zip:disk",
            PeekProblem::Threat => "zip:threat",
            PeekProblem::Unsupported => "zip:unsupported",
            PeekProblem::Cancelled => "zip:cancelled",
        }
    }
}

impl std::fmt::Display for PeekProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for PeekProblem {}

impl From<EngineError> for PeekProblem {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::RangeNotSupported => PeekProblem::NoRanges,
            EngineError::AuthRequired
            | EngineError::BrowserCheck
            | EngineError::WebPage
            | EngineError::LinkExpired
            | EngineError::Http {
                status: 401 | 403 | 407,
            } => PeekProblem::Protected,
            EngineError::ValidatorMismatch => PeekProblem::Changed,
            EngineError::Network { .. } | EngineError::Timeout => PeekProblem::Network,
            EngineError::Io { .. } | EngineError::NoSpace { .. } => PeekProblem::Disk,
            EngineError::Cancelled | EngineError::Paused => PeekProblem::Cancelled,
            _ => PeekProblem::Server,
        }
    }
}

impl From<ZipError> for PeekProblem {
    fn from(e: ZipError) -> Self {
        match e {
            ZipError::NotZip => PeekProblem::NotZip,
            ZipError::Damaged => PeekProblem::Damaged,
            ZipError::Split => PeekProblem::Split,
            ZipError::TooBig => PeekProblem::TooBig,
        }
    }
}

impl From<ReadError<EngineError>> for PeekProblem {
    fn from(e: ReadError<EngineError>) -> Self {
        match e {
            ReadError::Zip(e) => e.into(),
            ReadError::Source(e) => e.into(),
        }
    }
}

fn disk(e: std::io::Error) -> PeekProblem {
    tracing::info!("could not save a file from the archive: {e}");
    PeekProblem::Disk
}

/// A listed archive: its entries, and the server file the list came from (its size, ETag and
/// redirect target hold for every file downloaded from it).
pub(crate) struct Peek {
    file: RemoteFile,
    entries: Vec<Entry>,
}

/// A capture's "What's inside". Dropped with the capture (the window was confirmed or
/// cancelled), it stops the files still downloading.
#[derive(Default)]
pub(crate) struct PeekSlot {
    listed: Option<Arc<Peek>>,
    stop: CancellationToken,
    /// Files downloading now, by index (with a number telling a restarted one apart).
    running: HashMap<u32, (u64, CancellationToken)>,
    runs: u64,
}

impl PeekSlot {
    /// The list is read again next time (the credentials changed).
    pub(crate) fn forget_listing(&mut self) {
        self.listed = None;
    }
}

impl Drop for PeekSlot {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Reads byte ranges through the download's own request context.
struct Ranges<'a>(&'a mut RemoteFile);

impl ReadAt for Ranges<'_> {
    type Error = EngineError;
    async fn read_at(&mut self, from: u64, len: u64) -> Result<Vec<u8>, EngineError> {
        self.0.read(from, len).await
    }
}

impl Core {
    /// The files in the ZIP a Download File window is about to download (folders are in their
    /// paths). Only the end of the archive is read: a few kilobytes, a few hundred for archives
    /// of thousands of files, never the whole archive. The list is kept for the window.
    pub async fn zip_contents(&self, capture_id: &str) -> Result<Vec<ZipFile>, PeekProblem> {
        let peek = match self.listed(capture_id)? {
            Some(peek) => peek,
            None => {
                let peek = tokio::time::timeout(LIST_TIME, self.read_contents(capture_id))
                    .await
                    .map_err(|_| PeekProblem::Network)??;
                let mut st = self.0.state.lock();
                let p = st
                    .captures
                    .get_mut(capture_id)
                    .ok_or(PeekProblem::Cancelled)?;
                p.peek.listed.get_or_insert(Arc::new(peek)).clone()
            }
        };
        Ok(peek
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir())
            .map(|(i, e)| ZipFile {
                index: i as u32,
                path: e.name.clone(),
                size: e.size,
                encrypted: e.encrypted(),
                extractable: e.can_unpack(),
            })
            .collect())
    }

    fn listed(&self, capture_id: &str) -> Result<Option<Arc<Peek>>, PeekProblem> {
        let st = self.0.state.lock();
        let p = st.captures.get(capture_id).ok_or(PeekProblem::Cancelled)?;
        Ok(p.peek.listed.clone())
    }

    async fn read_contents(&self, capture_id: &str) -> Result<Peek, PeekProblem> {
        let (item, size, settings) = {
            let st = self.0.state.lock();
            let p = st.captures.get(capture_id).ok_or(PeekProblem::Cancelled)?;
            (p.item.clone(), p.info.size, st.settings.clone())
        };
        let req = jobs::capture_request(&item, &settings).ok_or(PeekProblem::NoRanges)?;
        let mut file = self.0.engine.remote_file(&req)?;
        // The size the window learned when it tried the link; the server's answer tells if the
        // file changed since.
        let size = match size {
            Some(size) => size,
            None => {
                file.read(0, 1).await?;
                file.size().ok_or(PeekProblem::NoRanges)?
            }
        };
        let mut entries = zipdir::read_entries(&mut Ranges(&mut file), size).await;
        if let Some(actual) = file.size().filter(|&s| s != size) {
            entries = zipdir::read_entries(&mut Ranges(&mut file), actual).await;
        }
        let entries = entries?;
        tracing::info!(
            "listed a ZIP before downloading it: {} entries",
            entries.len()
        );
        Ok(Peek { file, entries })
    }

    /// Downloads one file of the archive (`index` from [`Core::zip_contents`]) into `dir`, under
    /// its own name or a free numbered one, and returns where it was saved. Only that file's
    /// bytes are fetched, and unpacked on their way to the disk. Progress comes as
    /// [`CoreEvent::ZipProgress`]; [`Core::cancel_zip_file`] stops it.
    pub async fn extract_zip_file(
        &self,
        capture_id: &str,
        index: u32,
        dir: &Path,
    ) -> Result<PathBuf, PeekProblem> {
        if self.listed(capture_id)?.is_none() {
            self.zip_contents(capture_id).await?;
        }
        let (peek, run, stop) = {
            let mut st = self.0.state.lock();
            let p = st
                .captures
                .get_mut(capture_id)
                .ok_or(PeekProblem::Cancelled)?;
            let peek = p.peek.listed.clone().ok_or(PeekProblem::Cancelled)?;
            let stop = p.peek.stop.child_token();
            p.peek.runs += 1;
            let run = p.peek.runs;
            if let Some((_, earlier)) = p.peek.running.insert(index, (run, stop.clone())) {
                earlier.cancel();
            }
            (peek, run, stop)
        };
        let entry = peek
            .entries
            .get(index as usize)
            .filter(|e| !e.is_dir() && e.can_unpack())
            .ok_or(PeekProblem::Unsupported);
        let result = match entry {
            Ok(entry) => {
                self.save_entry(&peek, entry, capture_id, index, dir, &stop)
                    .await
            }
            Err(e) => Err(e),
        };
        let mut st = self.0.state.lock();
        if let Some(p) = st.captures.get_mut(capture_id)
            && p.peek.running.get(&index).is_some_and(|(r, _)| *r == run)
        {
            p.peek.running.remove(&index);
        }
        result
    }

    /// Stops downloading a file of the archive.
    pub fn cancel_zip_file(&self, capture_id: &str, index: u32) {
        let mut st = self.0.state.lock();
        if let Some(p) = st.captures.get_mut(capture_id)
            && let Some((_, stop)) = p.peek.running.remove(&index)
        {
            stop.cancel();
        }
    }

    async fn save_entry(
        &self,
        peek: &Peek,
        entry: &Entry,
        capture_id: &str,
        index: u32,
        dir: &Path,
        stop: &CancellationToken,
    ) -> Result<PathBuf, PeekProblem> {
        let name = match super::sanitize_name(entry.file_name()) {
            n if n.is_empty() => "file".to_string(),
            n => n,
        };
        tokio::fs::create_dir_all(dir).await.map_err(disk)?;
        let target = prism_engine::filename::unique_path(dir, &name, PART, false)
            .ok_or(PeekProblem::Disk)?;
        let part = prism_engine::filename::part_path_for(&target, PART);
        let mut out = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
            .map_err(disk)?;
        let written = tokio::select! {
            r = self.write_entry(peek, entry, &mut out, capture_id, index) => r,
            _ = stop.cancelled() => Err(PeekProblem::Cancelled),
        };
        drop(out);
        if let Err(e) = written {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        // A file of that name may have turned up in the meantime.
        let target = if tokio::fs::try_exists(&target).await.unwrap_or(true) {
            prism_engine::filename::unique_path(dir, &name, PART, false).ok_or(PeekProblem::Disk)?
        } else {
            target
        };
        if let Err(e) = tokio::fs::rename(&part, &target).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(disk(e));
        }
        let av = self.settings().antivirus;
        if let crate::antivirus::ScanResult::Infected { detail } =
            crate::antivirus::scan(&av, &target).await
        {
            tracing::warn!("antivirus: threat in a file from an archive: {detail}");
            let _ = tokio::fs::remove_file(&target).await;
            return Err(PeekProblem::Threat);
        }
        Ok(target)
    }

    /// Fetches the entry's local header (where its data starts), then its data, unpacking it
    /// into `out` and checking its size and CRC-32.
    async fn write_entry(
        &self,
        peek: &Peek,
        entry: &Entry,
        out: &mut tokio::fs::File,
        capture_id: &str,
        index: u32,
    ) -> Result<(), PeekProblem> {
        let mut unpacker = Unpacker::new(entry);
        let mut w = tokio::io::BufWriter::new(out);
        if entry.compressed > 0 {
            let mut file = peek.file.new_reader();
            let header = file.read(entry.header, zipdir::LOCAL_LEN).await?;
            let start = zipdir::data_offset(&header, entry)?;
            let mut data = file.stream(start, entry.compressed);
            let mut buf = vec![0u8; OUT_BUF];
            let mut reported = Instant::now();
            while let Some(chunk) = data.chunk().await? {
                let mut rest = &chunk[..];
                loop {
                    let (used, n) = unpacker.step(rest, &mut buf)?;
                    rest = &rest[used..];
                    w.write_all(&buf[..n]).await.map_err(disk)?;
                    if rest.is_empty() && n < buf.len() {
                        break;
                    }
                }
                if reported.elapsed() >= PROGRESS_EVERY {
                    reported = Instant::now();
                    self.emit(CoreEvent::ZipProgress(ZipProgress {
                        capture_id: capture_id.to_string(),
                        index,
                        done: unpacker.written(),
                        total: entry.size,
                    }));
                }
            }
        }
        unpacker.finish()?;
        w.flush().await.map_err(disk)
    }
}
