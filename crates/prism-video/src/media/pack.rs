//! Packing files and folders into a ZIP (deflate; files that are compressed already are stored
//! as they are, which is as small and much faster).

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, Timelike};
use tokio_util::sync::CancellationToken;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use super::{MediaProgress, ProgressFn, extension, temp_output};
use crate::error::{Result, VideoError};

/// Formats that deflate can't make smaller.
const COMPRESSED: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "mp3", "m4a", "aac", "ogg",
    "opus", "flac", "mp4", "m4v", "mkv", "webm", "mov", "avi", "zip", "7z", "rar", "gz", "xz",
    "zst", "bz2", "tgz", "docx", "xlsx", "pptx", "epub", "odt", "apk", "jar", "dmg", "msi",
];

pub fn stored_as_is(path: &Path) -> bool {
    COMPRESSED.contains(&extension(path).as_str())
}

/// The ZIP's name: the first file's (`photo.jpg` → `photo.zip`), next to it.
pub fn zip_path(inputs: &[PathBuf]) -> Option<PathBuf> {
    let first = inputs.first()?;
    Some(super::output_path(first, "zip", None))
}

/// Top-level names inside the ZIP: the files' and folders' names, a repeated one numbered
/// (`a.txt`, `a (2).txt`).
pub fn entry_names(inputs: &[PathBuf]) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for p in inputs {
        let name = super::file_name(p);
        let unique = super::unique_path(PathBuf::from(&name), |c| {
            used.iter()
                .any(|u| u.eq_ignore_ascii_case(&c.to_string_lossy()))
        });
        used.push(unique.to_string_lossy().into_owned());
    }
    used
}

/// Every file to pack with its name inside the ZIP: a file under its own name, a folder's files
/// under `folder/…` (links aren't followed, empty folders are left out).
pub fn entries(inputs: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for (p, name) in inputs.iter().zip(entry_names(inputs)) {
        if p.is_dir() {
            walk(p, &name, &mut out);
        } else {
            out.push((p.clone(), name));
        }
    }
    out
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(PathBuf, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = rd.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let name = format!("{prefix}/{}", e.file_name().to_string_lossy());
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), &name, out),
            Ok(t) if t.is_file() => out.push((e.path(), name)),
            _ => {}
        }
    }
}

/// Bytes of the files and folders (what a ZIP of them starts from).
pub fn total_size(inputs: &[PathBuf]) -> u64 {
    entries(inputs)
        .iter()
        .map(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn modified(path: &Path) -> Option<zip::DateTime> {
    let t: chrono::DateTime<Local> = std::fs::metadata(path).ok()?.modified().ok()?.into();
    zip::DateTime::from_date_and_time(
        t.year().clamp(1980, 2107) as u16,
        t.month() as u8,
        t.day() as u8,
        t.hour() as u8,
        t.minute() as u8,
        t.second() as u8,
    )
    .ok()
}

/// Pack `inputs` into `out` (through a temp file). Blocking; returns the ZIP's size.
pub fn pack_zip(
    inputs: &[PathBuf],
    out: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<(PathBuf, u64)> {
    let tmp = temp_output(out);
    match write_zip(inputs, &tmp, progress, cancel) {
        Ok(()) => {
            let path = super::finish_output(&tmp, out)?;
            let size = std::fs::metadata(&path)?.len();
            Ok((path, size))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn write_zip(
    inputs: &[PathBuf],
    tmp: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    let entries = entries(inputs);
    let total: u64 = entries
        .iter()
        .map(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum::<u64>()
        .max(1);
    let mut zip = zip::ZipWriter::new(BufWriter::new(File::create(tmp)?));
    let mut done = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    let archive = |e: zip::result::ZipError| VideoError::Archive(e.to_string());
    for (path, name) in &entries {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let method = if stored_as_is(path) {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        let mut opts = SimpleFileOptions::default()
            .compression_method(method)
            .large_file(len >= u32::MAX as u64);
        if let Some(t) = modified(path) {
            opts = opts.last_modified_time(t);
        }
        zip.start_file(name.as_str(), opts).map_err(archive)?;
        loop {
            if cancel.is_cancelled() {
                return Err(VideoError::Cancelled);
            }
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
            done += n as u64;
            progress(MediaProgress {
                fraction: Some(done as f64 / total as f64),
                retrying: false,
            });
        }
    }
    zip.finish().map_err(archive)?.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn names_inside_and_outside() {
        let inputs = vec![
            PathBuf::from("/a/photo.jpg"),
            PathBuf::from("/b/notes.txt"),
            PathBuf::from("/c/Photo.jpg"),
        ];
        assert_eq!(
            entry_names(&inputs),
            vec!["photo.jpg", "notes.txt", "Photo (2).jpg"]
        );
        assert_eq!(zip_path(&inputs), Some(PathBuf::from("/a/photo.zip")));
        assert!(stored_as_is(Path::new("x.MP4")));
        assert!(!stored_as_is(Path::new("x.txt")));
    }

    #[test]
    fn packs_files_and_cleans_up_when_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("readme.txt");
        let b = dir.path().join("sub");
        std::fs::create_dir(&b).unwrap();
        let b = b.join("readme.txt");
        let c = dir.path().join("clip.mp4");
        std::fs::write(&a, "hello ".repeat(10_000)).unwrap();
        std::fs::write(&b, "second").unwrap();
        std::fs::write(&c, [7u8; 5000]).unwrap();
        let inputs = vec![a.clone(), b.clone(), c.clone()];
        let out = zip_path(&inputs).unwrap();
        let last = std::sync::Mutex::new(0.0);
        let (path, size) = pack_zip(
            &inputs,
            &out,
            &|p| *last.lock().unwrap() = p.fraction.unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(path, dir.path().join("readme.zip"));
        assert_eq!(size, std::fs::metadata(&path).unwrap().len());
        assert!((*last.lock().unwrap() - 1.0).abs() < 1e-9);
        let bytes = std::fs::read(&path).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(z.len(), 3);
        let mut text = String::new();
        z.by_name("readme (2).txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "second");
        let first = z.by_name("readme.txt").unwrap();
        assert_eq!(first.compression(), CompressionMethod::Deflated);
        assert!(first.compressed_size() < 1000);
        drop(first);
        assert_eq!(
            z.by_name("clip.mp4").unwrap().compression(),
            CompressionMethod::Stored
        );
        // A second ZIP gets the next free name; a stopped one leaves nothing behind.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = pack_zip(&inputs, &out, &|_| {}, &cancel).unwrap_err();
        assert!(err.is_cancelled());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.contains("prismtmp")), "{names:?}");
        let (again, _) = pack_zip(&inputs, &out, &|_| {}, &CancellationToken::new()).unwrap();
        assert_eq!(again, dir.path().join("readme (2).zip"));
    }

    #[test]
    fn packs_folders_with_their_insides() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("Photos");
        std::fs::create_dir_all(photos.join("2026/May")).unwrap();
        std::fs::create_dir_all(photos.join("empty")).unwrap();
        std::fs::write(photos.join("a.jpg"), [1u8; 100]).unwrap();
        std::fs::write(photos.join("2026/May/b.jpg"), [2u8; 50]).unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "x").unwrap();
        let inputs = vec![photos.clone(), notes];
        let names: Vec<String> = entries(&inputs).into_iter().map(|(_, n)| n).collect();
        assert_eq!(
            names,
            vec!["Photos/2026/May/b.jpg", "Photos/a.jpg", "notes.txt"]
        );
        assert_eq!(total_size(&inputs), 151);
        let out = zip_path(&inputs).unwrap();
        assert_eq!(out, dir.path().join("Photos.zip"));
        let (path, _) = pack_zip(&inputs, &out, &|_| {}, &CancellationToken::new()).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(std::fs::read(path).unwrap())).unwrap();
        assert_eq!(z.len(), 3);
        assert_eq!(z.by_name("Photos/2026/May/b.jpg").unwrap().size(), 50);
    }
}
