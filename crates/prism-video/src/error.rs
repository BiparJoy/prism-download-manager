use std::path::PathBuf;

use crate::tools::Tool;

/// Errors produced by the video backend.
#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("HTTP status {status} for {url}")]
    HttpStatus { status: u16, url: String },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0} is not available (install it or set a custom path)")]
    ToolMissing(Tool),

    #[error("{tool} failed{}: {message}{}", exit_suffix(*code), hint(message))]
    ToolFailed {
        tool: Tool,
        code: Option<i32>,
        message: String,
    },

    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("checksum for {0} not found in the published checksum file")]
    ChecksumMissing(String),

    #[error("archive error: {0}")]
    Archive(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("the video is DRM protected")]
    Drm,

    #[error("operation cancelled")]
    Cancelled,

    #[error("missing input file for post-processing: {0}")]
    MissingInput(PathBuf),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = VideoError> = std::result::Result<T, E>;

impl VideoError {
    /// True when the failure is the user's cancellation rather than an error.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, VideoError::Cancelled)
    }

    /// yt-dlp stopped because it couldn't read the cookies of the browser set under
    /// "Cookies from browser" (it then fails every video, also those that need no login).
    pub fn is_cookie_read_failure(&self) -> bool {
        matches!(self, VideoError::ToolFailed { tool: Tool::YtDlp, message, .. } if cookie_read_failure(message))
    }

    /// YouTube rejected the browser session the extension passed along (out-of-sync or
    /// rotated account cookies); the same video works without them.
    pub fn is_rejected_youtube_session(&self) -> bool {
        matches!(self, VideoError::ToolFailed { tool: Tool::YtDlp, message, .. } if {
            let m = message.to_ascii_lowercase();
            m.contains("the page needs to be reloaded")
                || m.contains("requested format is not available")
                || m.contains("cookies are no longer valid")
        })
    }

    /// This error came from the retry without browser cookies after `first` (a cookie read
    /// failure): keep both, so the message explains why the cookies were missing.
    pub fn after_cookie_read_failure(self, first: &VideoError) -> VideoError {
        match (self, first) {
            (
                VideoError::ToolFailed {
                    tool,
                    code,
                    message,
                },
                VideoError::ToolFailed { message: why, .. },
            ) => VideoError::ToolFailed {
                tool,
                code,
                message: format!("{message} (tried without browser cookies: {why})"),
            },
            (e, _) => e,
        }
    }
}

/// yt-dlp's messages when it can't load cookies from a browser: Chromium browsers on Windows
/// keep the database locked while running and encrypt it (app-bound encryption), or the
/// browser / profile isn't there.
fn cookie_read_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("cookie database")
        || m.contains("cookies database")
        || m.contains("failed to decrypt with dpapi")
        || m.contains("failed to load cookies")
}

fn hint(message: &str) -> &'static str {
    if cookie_read_failure(message) {
        "

Tip: Prism couldn't read the cookies of the browser chosen under Settings → Video & streams → Cookies from browser. Chrome, Edge and other Chromium browsers on Windows lock and encrypt them: close that browser and try again, choose Firefox, or set it to None. Videos sent from the Prism browser extension bring the page's cookies with them."
    } else {
        login_hint(message)
    }
}

fn exit_suffix(code: Option<i32>) -> String {
    code.map(|c| format!(" (exit code {c})"))
        .unwrap_or_default()
}

/// Sites that need an account: point the user at the cookies setting.
fn login_hint(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("logged-in")
        || m.contains("log in")
        || m.contains("login")
        || m.contains("cookies")
        || m.contains("sign in")
    {
        "

Tip: sign in to the site in your browser, then choose that browser under Settings → Video & streams → Use cookies from browser."
    } else {
        ""
    }
}
