//! The app's "Tools": convert images, audio and video into other formats, make them smaller for
//! a size limit (Discord, e-mail, WhatsApp) and pack files into a ZIP.
//!
//! * [`probe`] — ffprobe reads a file into a [`MediaInfo`] (kind, display size, duration, …).
//! * [`convert`] — the formats each kind of file can become, and ffmpeg's arguments for them.
//! * [`shrink`] — the plan for a size limit (bitrates, resolution, quality) and the runs that
//!   meet it, checked afterwards.
//! * [`pack`] — ZIP archives (pure Rust).
//!
//! Results go next to the original under a new name ([`output_path`], [`unique_path`]); the
//! original is never touched. Plans and arguments are pure functions, unit tested.

pub mod convert;
pub mod pack;
pub mod probe;
pub mod shrink;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, VideoError};
use crate::ffmpeg::run_ffmpeg;
use crate::progress::Stage;
use crate::tools::Tool;

pub use convert::{Quality, Target};
pub use shrink::{Grade, ShrinkOutcome, ShrinkPlan, Verdict};

/// The encoders of an ffmpeg build (`ffmpeg -encoders`), asked once per ffmpeg. `None` when it
/// can't be asked.
pub(crate) async fn encoders(ffmpeg: &Path) -> Option<std::sync::Arc<str>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    static LISTS: Mutex<Option<HashMap<PathBuf, Arc<str>>>> = Mutex::new(None);
    let cached = LISTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(ffmpeg)
        .cloned();
    if cached.is_some() {
        return cached;
    }
    let out = crate::process::command(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let list: Arc<str> = String::from_utf8_lossy(&out.stdout).into();
    LISTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(ffmpeg.to_path_buf(), list.clone());
    Some(list)
}

/// Whether `ffmpeg -encoders` output lists `name` (lines look like ` A....D libvorbis  ...`).
pub(crate) fn has_encoder(list: &str, name: &str) -> bool {
    list.lines()
        .any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// Whether an ffmpeg with these encoders can make `target`. A system ffmpeg can be built without
/// some (Homebrew's has no libwebp or libvorbis); Prism's own builds have them all. OGG always
/// works: ffmpeg's own Vorbis encoder stands in for libvorbis.
pub fn can_make(target: Target, encoders: &str) -> bool {
    let need: &[&str] = match target {
        Target::Mp3 => &["libmp3lame"],
        Target::Webp => &["libwebp"],
        Target::Mp4 => &["libx264"],
        Target::Webm => &["libvpx-vp9", "libopus"],
        _ => &[],
    };
    need.iter().all(|e| has_encoder(encoders, e))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    /// Anything else (a document, an archive): only packing into a ZIP.
    #[default]
    Other,
}

/// What went wrong, for a sentence in the UI (no codec jargon).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaProblem {
    /// The converter can't read the file (damaged, or not what its name says).
    Unreadable,
    /// A HEIC/HEIF photo the converter can't read (an ffmpeg older than 7.1).
    Heic,
    /// A video without sound can't become an MP3.
    NoAudio,
    /// The size limit is too small for this much video or sound.
    TooLong,
    /// The file is under the size limit already.
    AlreadySmall,
    /// Still over the size limit after a second, smaller try.
    OverTarget,
    NoSpace,
    /// The folder next to the original can't be written.
    Access,
    /// The original is gone.
    Missing,
    /// This ffmpeg can't write the format (an encoder missing from the build).
    Encoder,
    /// ffmpeg isn't available and couldn't be downloaded.
    Converter,
    Failed,
}

impl MediaProblem {
    /// Classify an error from a tool run. `ext` is the input's extension (lowercase).
    pub fn of(err: &VideoError, ext: &str) -> MediaProblem {
        let heic = matches!(ext, "heic" | "heif");
        match err {
            VideoError::Media(p) => *p,
            VideoError::ToolMissing(_) | VideoError::Http(_) | VideoError::HttpStatus { .. } => {
                MediaProblem::Converter
            }
            VideoError::ChecksumMismatch { .. }
            | VideoError::ChecksumMissing(_)
            | VideoError::Archive(_) => MediaProblem::Converter,
            VideoError::MissingInput(_) => MediaProblem::Missing,
            VideoError::Io(e) => match e.kind() {
                std::io::ErrorKind::NotFound => MediaProblem::Missing,
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
                    MediaProblem::Access
                }
                std::io::ErrorKind::StorageFull => MediaProblem::NoSpace,
                _ => MediaProblem::Failed,
            },
            VideoError::ToolFailed { message, .. } => {
                let m = message.to_ascii_lowercase();
                if m.contains("no space left") || m.contains("not enough space") {
                    MediaProblem::NoSpace
                } else if m.contains("permission denied") || m.contains("access is denied") {
                    MediaProblem::Access
                } else if m.contains("unknown encoder") || m.contains("encoder not found") {
                    MediaProblem::Encoder
                } else if m.contains("no such file") {
                    MediaProblem::Missing
                } else if m.contains("invalid data found")
                    || m.contains("could not find codec parameters")
                    || m.contains("no decoder")
                    || m.contains("decoder not found")
                    || m.contains("moov atom not found")
                    || m.contains("does not contain any stream")
                    || m.contains("error while decoding")
                {
                    if heic {
                        MediaProblem::Heic
                    } else {
                        MediaProblem::Unreadable
                    }
                } else if heic {
                    MediaProblem::Heic
                } else {
                    MediaProblem::Failed
                }
            }
            _ => MediaProblem::Failed,
        }
    }
}

/// One file as the tools see it. Sizes are the displayed ones (a phone's portrait video or
/// photo is taller than wide).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MediaInfo {
    pub path: PathBuf,
    pub name: String,
    /// Bytes on disk.
    pub size: u64,
    pub kind: MediaKind,
    /// Extension, lowercase (`jpg`, `mp4`, …).
    pub ext: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Seconds (audio and video).
    pub duration: Option<f64>,
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// kbps of the video stream (estimated from the file's bitrate when the stream has none).
    pub video_kbps: Option<u32>,
    pub audio_kbps: Option<u32>,
    pub has_audio: bool,
    /// Transparency (PNG, WebP, GIF): a JPEG gets a white background.
    pub has_alpha: bool,
    /// An audio file with cover art.
    pub has_cover: bool,
    pub pix_fmt: Option<String>,
    /// A HEIC/AVIF photo stored as a grid of tiles: ffmpeg joins them in its own filter graph,
    /// so the picture is decoded on its own first ([`decode_tiles`]) before anything else.
    pub tiled: bool,
    /// A folder (only for a ZIP).
    pub folder: bool,
    /// What [`MediaKind`] files of this one can become (its own format left out).
    pub targets: Vec<Target>,
    /// Why the file can't be used (it's still listed, e.g. for a ZIP).
    pub problem: Option<MediaProblem>,
}

impl MediaInfo {
    /// A file the tools don't read (no ffprobe run): only for a ZIP.
    pub fn other(path: &Path, size: u64) -> MediaInfo {
        MediaInfo {
            path: path.to_path_buf(),
            name: file_name(path),
            size,
            kind: MediaKind::Other,
            ext: extension(path),
            ..Default::default()
        }
    }

    /// The short side in pixels (`1080` for 1080p, also for portrait video).
    pub fn short_side(&self) -> Option<u32> {
        Some(self.width?.min(self.height?)).filter(|s| *s > 0)
    }
}

/// Lowercase extension without the dot.
pub fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The file's name (`clip.mp4`).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jfif", "png", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff", "gif",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma", "aif", "aiff", "alac", "ape",
    "wv", "m4b", "amr", "ac3",
];
pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "mts", "m2ts",
    "3gp", "ogv", "vob", "f4v",
];

/// The kind a file's name suggests (before ffprobe looks inside).
pub fn kind_by_name(path: &Path) -> MediaKind {
    let ext = extension(path);
    let e = ext.as_str();
    if IMAGE_EXTS.contains(&e) {
        MediaKind::Image
    } else if AUDIO_EXTS.contains(&e) {
        MediaKind::Audio
    } else if VIDEO_EXTS.contains(&e) {
        MediaKind::Video
    } else {
        MediaKind::Other
    }
}

/// Characters that can't be in a file name on Windows (and `/` anywhere).
fn clean_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| {
            !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control()
        })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_string()
}

/// `dir/name.ext`, or `dir/name (label).ext` (`clip (Discord).mp4`).
pub fn output_path(input: &Path, ext: &str, label: Option<&str>) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let label = label.map(clean_label).filter(|l| !l.is_empty());
    let name = match label {
        Some(l) => format!("{stem} ({l}).{ext}"),
        None => format!("{stem}.{ext}"),
    };
    input.with_file_name(name)
}

/// `path`, or `name (2).ext`, `name (3).ext`, … when `taken` says the name is in use.
pub fn unique_path(path: PathBuf, taken: impl Fn(&Path) -> bool) -> PathBuf {
    if !taken(&path) {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..10_000)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !taken(p))
        .unwrap_or(path)
}

/// Where ffmpeg writes before the result gets its name (`name.prismtmp.ext`, same folder).
pub(crate) fn temp_output(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = extension(out);
    out.with_file_name(format!("{stem}.prismtmp.{ext}"))
}

/// Give the finished temp file its name. A file that appeared under that name meanwhile is
/// never overwritten: the result then gets the next free name.
pub(crate) fn finish_output(tmp: &Path, out: &Path) -> Result<PathBuf> {
    let dest = unique_path(out.to_path_buf(), |p| p.exists());
    std::fs::rename(tmp, &dest)?;
    Ok(dest)
}

/// Progress of one tool run: a fraction when known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    pub fraction: Option<f64>,
    /// A second, smaller try after the first came out over the size limit.
    pub retrying: bool,
}

pub type ProgressFn<'a> = &'a (dyn Fn(MediaProgress) + Send + Sync);

/// The common start of every ffmpeg command line (machine-readable progress on stdout).
pub(crate) fn base_args() -> Vec<String> {
    [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub(crate) fn s(v: impl Into<String>) -> String {
    v.into()
}

pub(crate) fn path_arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Run ffmpeg; its progress (`0..1` of `duration`) is mapped into `span` of the whole job.
pub(crate) async fn run_mapped(
    ffmpeg: &Path,
    args: &[String],
    duration: Option<f64>,
    span: (f64, f64),
    retrying: bool,
    progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = unbounded_channel();
    let run = run_ffmpeg(ffmpeg, args, duration, Stage::Converting, Some(&tx), cancel);
    tokio::pin!(run);
    let report = |pct: Option<f64>| {
        let fraction = match (pct, duration) {
            (Some(p), Some(_)) => Some(span.0 + (span.1 - span.0) * (p / 100.0).clamp(0.0, 1.0)),
            _ => None,
        };
        progress(MediaProgress { fraction, retrying });
    };
    loop {
        tokio::select! {
            r = &mut run => return r,
            Some(p) = rx.recv() => report(p.percent),
        }
    }
}

/// A tiled photo decoded into a temp PNG (removed when dropped), and its info pointing at it;
/// `None` for everything else. Size and extension stay the original's, so plans don't change.
pub(crate) async fn decode_tiles(
    ffmpeg: &Path,
    info: &MediaInfo,
    cancel: &CancellationToken,
) -> Result<Option<(MediaInfo, tempfile::TempPath)>> {
    if !info.tiled || info.kind != MediaKind::Image {
        return Ok(None);
    }
    let tmp = tempfile::Builder::new()
        .prefix("prism-photo-")
        .suffix(".png")
        .tempfile()?
        .into_temp_path();
    let mut a = base_args();
    a.extend([
        s("-i"),
        path_arg(&info.path),
        s("-frames:v"),
        s("1"),
        s("-update"),
        s("1"),
    ]);
    a.extend([
        s("-c:v"),
        s("png"),
        s("-compression_level"),
        s("1"),
        s("-f"),
        s("image2"),
    ]);
    a.push(path_arg(&tmp));
    run_mapped(ffmpeg, &a, None, (0.0, 0.0), false, &|_| {}, cancel).await?;
    let flat = MediaInfo {
        path: tmp.to_path_buf(),
        tiled: false,
        ..info.clone()
    };
    Ok(Some((flat, tmp)))
}

/// Remove a temp output (and ignore it being gone already).
pub(crate) async fn discard(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

/// The error a run produced, unless the user stopped it.
pub(crate) fn ffmpeg_failed(message: &str) -> VideoError {
    VideoError::ToolFailed {
        tool: Tool::Ffmpeg,
        code: None,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_encoders_ffmpeg_has() {
        // Like Homebrew's ffmpeg 9: no libwebp, no libvorbis.
        let brew = "Encoders:\n V....D libx264              libx264 H.264\n V....D libvpx-vp9           libvpx VP9\n A....D libopus              libopus Opus\n A....D libmp3lame           libmp3lame MP3\n A..X.D vorbis               Vorbis\n V....D mjpeg                MJPEG\n";
        assert!(has_encoder(brew, "libopus"));
        assert!(!has_encoder(brew, "libvorbis"));
        assert!(!has_encoder(brew, "libvpx"), "whole names only");
        assert!(!can_make(Target::Webp, brew));
        assert!(can_make(Target::Ogg, brew), "ffmpeg's own Vorbis");
        for t in [
            Target::Mp3,
            Target::Mp4,
            Target::Webm,
            Target::Jpg,
            Target::Png,
            Target::Gif,
            Target::Wav,
            Target::Flac,
            Target::M4a,
        ] {
            assert!(can_make(t, brew), "{t:?}");
        }
        assert!(
            !can_make(Target::Webm, " V....D libvpx-vp9  VP9\n"),
            "WebM needs Opus too"
        );
    }

    #[test]
    fn names_go_next_to_the_original() {
        let p = Path::new("/d/Holiday clip.mov");
        assert_eq!(
            output_path(p, "mp4", None),
            Path::new("/d/Holiday clip.mp4")
        );
        assert_eq!(
            output_path(p, "mp4", Some("Discord")),
            Path::new("/d/Holiday clip (Discord).mp4")
        );
        // Characters a file name can't have are dropped from the label.
        assert_eq!(
            output_path(p, "mp4", Some("E-mail: <25 MB>")),
            Path::new("/d/Holiday clip (E-mail 25 MB).mp4")
        );
        assert_eq!(
            output_path(p, "jpg", Some("  ")),
            Path::new("/d/Holiday clip.jpg")
        );
    }

    #[test]
    fn names_are_unique() {
        let taken = [PathBuf::from("/d/a.mp3"), PathBuf::from("/d/a (2).mp3")];
        let is_taken = |p: &Path| taken.iter().any(|t| t == p);
        assert_eq!(
            unique_path(PathBuf::from("/d/a.mp3"), is_taken),
            Path::new("/d/a (3).mp3")
        );
        assert_eq!(
            unique_path(PathBuf::from("/d/b.mp3"), is_taken),
            Path::new("/d/b.mp3")
        );
        assert_eq!(
            temp_output(Path::new("/d/a (3).mp3")),
            Path::new("/d/a (3).prismtmp.mp3")
        );
    }

    #[test]
    fn kinds_by_name() {
        assert_eq!(kind_by_name(Path::new("x.HEIC")), MediaKind::Image);
        assert_eq!(kind_by_name(Path::new("x.flac")), MediaKind::Audio);
        assert_eq!(kind_by_name(Path::new("x.MKV")), MediaKind::Video);
        assert_eq!(kind_by_name(Path::new("x.pdf")), MediaKind::Other);
        assert_eq!(kind_by_name(Path::new("noext")), MediaKind::Other);
    }

    #[test]
    fn problems_in_plain_words() {
        let failed = |m: &str| VideoError::ToolFailed {
            tool: Tool::Ffmpeg,
            code: Some(1),
            message: m.into(),
        };
        let invalid = failed("x.heic: Invalid data found when processing input");
        assert_eq!(MediaProblem::of(&invalid, "heic"), MediaProblem::Heic);
        assert_eq!(MediaProblem::of(&invalid, "mp4"), MediaProblem::Unreadable);
        assert_eq!(
            MediaProblem::of(&failed("Unknown encoder 'libwebp'"), "png"),
            MediaProblem::Encoder
        );
        assert_eq!(
            MediaProblem::of(
                &failed("Error writing trailer: No space left on device"),
                "mp4"
            ),
            MediaProblem::NoSpace
        );
        assert_eq!(
            MediaProblem::of(&VideoError::ToolMissing(Tool::Ffmpeg), "mp4"),
            MediaProblem::Converter
        );
        assert_eq!(
            MediaProblem::of(&VideoError::Media(MediaProblem::TooLong), "mp4"),
            MediaProblem::TooLong
        );
        let gone = VideoError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(MediaProblem::of(&gone, "mp4"), MediaProblem::Missing);
    }
}
