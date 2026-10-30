//! Reading parts of a remote file with range requests (the "What's inside" peek into a ZIP
//! before downloading it reads only the archive's table of contents, and single files from it).
//!
//! The requests go through the download's own fetcher, so they carry the same cookies,
//! referer, user agent, headers, HTTP authentication and proxy as the download would, and
//! follow redirects the same way: credentials don't go to a server a redirect leads to.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header;

use super::fetcher::{HttpFetcher, HttpStream, is_browser_check};
use super::{HttpCtx, SourceRef, Validators, header_string, parse_content_range, retry_after};
use crate::backoff::Backoff;
use crate::engine::EngineInner;
use crate::error::EngineError;
use crate::request::{DownloadRequest, HttpMethod};
use crate::segmented::{ChunkStream, FailKind, FetchError, RangeFetcher};

/// How many times a transfer cut off on the way is picked up again where it stopped (counted
/// afresh whenever data arrives).
const RETRIES: u32 = 3;
/// Longest wait before picking a transfer up again, whatever the server's `Retry-After` says.
const MAX_WAIT: Duration = Duration::from_secs(10);

/// A file on an HTTP(S) server read in parts. The first read learns its size and ETag; every
/// later read must match them (a file that changes in between is an error, not mixed bytes).
pub struct RemoteFile {
    ctx: Arc<HttpCtx>,
    fetcher: HttpFetcher,
}

impl std::fmt::Debug for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFile")
            .field("url", &self.ctx.sources.original().as_str())
            .field("size", &self.size())
            .finish()
    }
}

impl RemoteFile {
    /// See [`crate::Engine::remote_file`].
    pub(crate) fn new(engine: &EngineInner, req: &DownloadRequest) -> Result<Self, EngineError> {
        if !matches!(req.url.scheme(), "http" | "https") || req.method != HttpMethod::Get {
            return Err(EngineError::RangeNotSupported);
        }
        let ctx = super::download::build_ctx(engine, req, req.url.clone(), None)?;
        ctx.set_validators(Validators {
            ranged: true,
            ..Default::default()
        });
        Ok(RemoteFile {
            fetcher: HttpFetcher::new(ctx.clone(), 0),
            ctx,
        })
    }

    /// The file's size, known after the first read.
    pub fn size(&self) -> Option<u64> {
        self.ctx.validators().total
    }

    /// Another reader of the same file on its own connection, held to the size and ETag this
    /// one learned, and going straight to where a redirect led it.
    pub fn new_reader(&self) -> RemoteFile {
        RemoteFile {
            fetcher: HttpFetcher::new(self.ctx.clone(), 0),
            ctx: self.ctx.clone(),
        }
    }

    /// Reads `len` bytes at `from` into memory.
    pub async fn read(&mut self, from: u64, len: u64) -> Result<Vec<u8>, EngineError> {
        let mut out = Vec::with_capacity(len.min(16 << 20) as usize);
        let mut s = self.stream(from, len);
        while let Some(chunk) = s.chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Streams `len` bytes at `from`, asked for with the first [`RemoteStream::chunk`]. A
    /// transfer cut off on the way continues where it stopped.
    pub fn stream(&mut self, from: u64, len: u64) -> RemoteStream<'_> {
        RemoteStream {
            file: self,
            stream: None,
            pos: from,
            end: from.saturating_add(len),
            retries: 0,
            backoff: Backoff::default(),
        }
    }

    /// Asks for `[from, to)`.
    async fn open(&mut self, from: u64, to: u64) -> Result<HttpStream, FetchError> {
        if self.size().is_some() {
            // Checked against the size and ETag; an expired redirect target is left for the
            // original link.
            return self.fetcher.open(from, Some(to)).await;
        }
        // The first request: what the server answers sets the size and ETag the rest must match.
        let url = self.ctx.sources.original();
        let resp = self
            .fetcher
            .send(&url, Some((from, Some(to))), None)
            .await?;
        self.ctx
            .sources
            .on_redirected(SourceRef::Primary, resp.url());
        if is_browser_check(resp.headers()) {
            return Err(FetchError::fatal(EngineError::BrowserCheck));
        }
        let status = resp.status().as_u16();
        match status {
            206 => {
                let range = header_string(resp.headers(), header::CONTENT_RANGE)
                    .and_then(|v| parse_content_range(&v));
                let Some((a, b, Some(total))) = range else {
                    return Err(FetchError::fatal(EngineError::RangeNotSupported));
                };
                if a != from {
                    return Err(FetchError::fatal(EngineError::network(format!(
                        "the server sent bytes from {a} instead of {from}"
                    ))));
                }
                self.ctx.set_validators(Validators {
                    total: Some(total),
                    etag: header_string(resp.headers(), header::ETAG),
                    ranged: true,
                });
                self.ctx.mark_probed();
                Ok(HttpStream::new(resp, Some(b + 1)))
            }
            // The whole file instead of the part asked for.
            200 => Err(FetchError::fatal(EngineError::RangeNotSupported)),
            // Past the end: the file is shorter than expected.
            416 => Err(FetchError::fatal(EngineError::ValidatorMismatch)),
            401 | 407 => Err(FetchError::fatal(EngineError::AuthRequired)),
            408 | 429 | 500..=599 => Err(FetchError {
                kind: FailKind::Transient,
                error: EngineError::Http { status },
                retry_after: retry_after(resp.headers()),
            }),
            s => Err(FetchError::fatal(EngineError::Http { status: s })),
        }
    }
}

/// The bytes of one [`RemoteFile::stream`] call, chunk by chunk.
pub struct RemoteStream<'a> {
    file: &'a mut RemoteFile,
    stream: Option<HttpStream>,
    pos: u64,
    end: u64,
    retries: u32,
    backoff: Backoff,
}

impl RemoteStream<'_> {
    /// The next chunk; `None` once every byte asked for has arrived.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, EngineError> {
        loop {
            if self.pos >= self.end {
                self.stream = None;
                return Ok(None);
            }
            let failure = match self.stream.as_mut() {
                Some(s) => match s.next_chunk().await {
                    Ok(Some(b)) if b.is_empty() => continue,
                    Ok(Some(b)) => {
                        let take = (self.end - self.pos).min(b.len() as u64) as usize;
                        self.pos += take as u64;
                        self.retries = 0;
                        self.backoff.reset();
                        return Ok(Some(b.slice(..take)));
                    }
                    Ok(None) => FetchError::transient(EngineError::network(
                        "the server closed the connection early",
                    )),
                    Err(e) => e,
                },
                None => match self.file.open(self.pos, self.end).await {
                    Ok(s) => {
                        self.stream = Some(s);
                        continue;
                    }
                    Err(e) => e,
                },
            };
            self.stream = None;
            if failure.kind == FailKind::Fatal
                || !failure.error.is_transient()
                || self.retries >= RETRIES
            {
                return Err(failure.error);
            }
            self.retries += 1;
            let wait = failure
                .retry_after
                .unwrap_or_else(|| self.backoff.next_delay())
                .min(MAX_WAIT);
            tracing::debug!(error = %failure.error, ?wait, "range read cut off, continuing");
            tokio::time::sleep(wait).await;
        }
    }
}
