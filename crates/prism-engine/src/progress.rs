//! Observation types: progress snapshots, events and the completion record.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::EngineError;

/// Lifecycle state of a download job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    /// Connecting / sending the first request.
    Probing,
    /// Transferring data.
    Downloading,
    /// Flushing, verifying and renaming the file.
    Finalizing,
    /// Muxing/remuxing media with ffmpeg.
    Merging,
    /// Stopped by `pause()`; resumable from the returned state.
    Paused,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Stopped by `cancel()`.
    Cancelled,
}

impl DownloadState {
    /// `true` for states after which the job no longer runs.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Paused | Self::Completed | Self::Failed | Self::Cancelled
        )
    }
}

/// State of one segment in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentStatus {
    /// A connection is transferring this segment.
    Active,
    /// Not yet (or no longer) assigned to a connection.
    Idle,
    /// Completely downloaded.
    Done,
    /// The connection failed and is waiting to retry.
    Retrying,
    /// The connection gave up; another connection may take the segment over.
    Failed,
}

/// One segment of a snapshot. For HTTP/FTP downloads the values are byte offsets
/// (`[start, end)`, `pos` = next byte to receive). For HLS/DASH jobs each entry is one media
/// segment and the unit is "segments": `start = index`, `end = index + 1`, `pos` is `end` when
/// done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentView {
    /// First byte (or media segment index).
    pub start: u64,
    /// Next byte to receive.
    pub pos: u64,
    /// End (exclusive). `u64::MAX` when the total size is unknown.
    pub end: u64,
    /// Current speed of the connection working on it (bytes/s).
    pub speed: u64,
    /// Adapter the connection runs over when downloading over several networks.
    #[serde(default)]
    pub network: Option<String>,
    /// Status.
    pub status: SegmentStatus,
}

/// A point-in-time view of a download, cheap to take (poll it, e.g. 4×/s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    /// Lifecycle state.
    pub state: DownloadState,
    /// Bytes received so far.
    pub downloaded: u64,
    /// Total size if known (for media: estimate once a few segments are done).
    pub total: Option<u64>,
    /// Smoothed (EWMA) download speed in bytes/s.
    pub speed: u64,
    /// Estimated time remaining.
    pub eta: Option<Duration>,
    /// Connections currently transferring data.
    pub active_connections: u32,
    /// Connections the engine is currently aiming for (after server-limit detection).
    pub target_connections: u32,
    /// Whether the download can be resumed after a pause/restart.
    pub resumable: bool,
    /// Final file name, once known.
    pub file_name: Option<String>,
    /// Final path, once known.
    pub path: Option<PathBuf>,
    /// URL actually used after redirects.
    pub final_url: Option<Url>,
    /// Time since the job started.
    pub elapsed: Duration,
    /// Segments, sorted by start.
    pub segments: Vec<SegmentView>,
}

/// Metadata learned from the first response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInfo {
    /// URL after redirects.
    pub final_url: Url,
    /// Suggested file name (Content-Disposition → URL → MIME), sanitized.
    pub file_name: String,
    /// Total size, if known.
    pub size: Option<u64>,
    /// `true` when byte ranges work (multi-connection + resume possible).
    pub resumable: bool,
    /// `Content-Type` essence.
    pub mime: Option<String>,
    /// `ETag`.
    pub etag: Option<String>,
    /// `Last-Modified`.
    pub last_modified: Option<String>,
    /// `Accept-Ranges: bytes` was present.
    pub accept_ranges: bool,
}

/// Successful completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completed {
    /// Final path of the file.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// `Some(true)` when a requested checksum matched; `None` when none was requested.
    pub checksum_ok: Option<bool>,
    /// Wall time of this run (excluding previous sessions).
    pub elapsed: Duration,
    /// Bytes transferred in this run.
    pub transferred: u64,
    /// Additional output files (e.g. a separate audio track when no remux happened).
    pub extra_files: Vec<PathBuf>,
    /// Non-fatal remark (e.g. "ffmpeg not found; kept MPEG-TS").
    pub warning: Option<String>,
}

impl Completed {
    /// Average speed of this run in bytes/s.
    pub fn average_speed(&self) -> u64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            (self.transferred as f64 / secs) as u64
        } else {
            0
        }
    }
}

/// Discrete events. Progress is not broadcast: poll [`crate::DownloadHandle::snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadEvent {
    /// The first response arrived.
    Probed(ProbeInfo),
    /// Lifecycle state changed.
    StateChanged {
        /// New state.
        state: DownloadState,
    },
    /// A segment was split (work stealing) or raced.
    SegmentSplit {
        /// Start of the new piece.
        at: u64,
        /// End of the new piece.
        end: u64,
        /// `true` if this is a duplicate "race" on a slow tail rather than a split.
        race: bool,
    },
    /// A connection hit an error and will retry.
    Retrying {
        /// Reason.
        reason: String,
        /// Consecutive attempt number.
        attempt: u32,
        /// Delay before the retry.
        delay: Duration,
    },
    /// The server refused extra connections; the engine now uses at most this many.
    ServerLimitDetected {
        /// Connections that work.
        connections: u32,
    },
    /// The (signed) URL expired; the engine falls back or fails with `LinkExpired`.
    LinkExpired,
    /// `change_url` succeeded.
    UrlChanged {
        /// The new URL.
        url: Url,
    },
    /// The resume data could not be used and the download restarted from zero.
    Restarted {
        /// Reason.
        reason: String,
    },
    /// Finished successfully.
    Completed(Completed),
    /// Finished with an error.
    Failed {
        /// The error.
        error: EngineError,
    },
}
