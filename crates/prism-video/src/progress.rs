//! Parsing of yt-dlp progress output (our `--progress-template` JSON lines plus
//! the classic `[Merger]` / `[ExtractAudio]` / `[download]` lines) and of
//! ffmpeg's `-progress pipe:1` key=value stream.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Prefix of `download:` progress-template lines.
pub const DL_PREFIX: &str = "PRISM ";
/// Prefix of `postprocess:` progress-template lines.
pub const PP_PREFIX: &str = "PRISMPP ";
/// Prefix of the `--print after_move:` line carrying the final path.
pub const FILE_PREFIX: &str = "PRISMFILE ";

pub const DL_TEMPLATE: &str = "download:PRISM %(progress)j";
pub const PP_TEMPLATE: &str = "postprocess:PRISMPP %(progress)j";
/// JSON (`\uXXXX` escapes): on Windows yt-dlp writes its output in the console code page and
/// drops what that can't encode, e.g. the `？` `｜` it puts in file names for `?` `|`.
pub const FILE_PRINT: &str = "after_move:PRISMFILE %(filepath)j";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    #[default]
    Starting,
    Downloading,
    Merging,
    /// Cutting a part of the video out of the downloaded pieces.
    Cutting,
    ExtractingAudio,
    Converting,
    EmbeddingSubtitles,
    EmbeddingThumbnail,
    EmbeddingMetadata,
    SponsorBlock,
    Moving,
    PostProcessing,
    Finished,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YtProgress {
    pub stage: Stage,
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    /// `total` is yt-dlp's estimate (fragmented downloads).
    pub total_is_estimate: bool,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Seconds.
    pub eta: Option<u64>,
    pub fragment_index: Option<u64>,
    pub fragment_count: Option<u64>,
    pub filename: Option<String>,
    /// 0–100 when known.
    pub percent: Option<f64>,
    /// Post-processor name as reported by yt-dlp (`Merger`, `ExtractAudio` ...).
    pub postprocessor: Option<String>,
    /// `started` / `processing` / `finished` (post-processing) or `downloading` / `finished`.
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LineEvent {
    Progress(YtProgress),
    FinalPath(String),
    Error(String),
    Warning(String),
}

fn num(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn uint(v: &Value, key: &str) -> Option<u64> {
    num(v, key)
        .filter(|x| x.is_finite() && *x >= 0.0)
        .map(|x| x as u64)
}

pub fn stage_for_postprocessor(name: &str) -> Stage {
    let n = name.strip_prefix("FFmpeg").unwrap_or(name);
    match n {
        "Merger" => Stage::Merging,
        "ExtractAudio" => Stage::ExtractingAudio,
        "EmbedThumbnail" | "ThumbnailsConvertor" => Stage::EmbeddingThumbnail,
        "EmbedSubtitle" | "SubtitlesConvertor" => Stage::EmbeddingSubtitles,
        "Metadata" => Stage::EmbeddingMetadata,
        "SponsorBlock" | "ModifyChapters" => Stage::SponsorBlock,
        "VideoRemuxer" | "VideoConvertor" | "Remuxer" => Stage::Converting,
        "MoveFiles" | "MoveFilesAfterDownload" => Stage::Moving,
        _ => Stage::PostProcessing,
    }
}

fn parse_download_json(v: &Value) -> YtProgress {
    let downloaded = uint(v, "downloaded_bytes");
    let (total, est) = match uint(v, "total_bytes") {
        Some(t) => (Some(t), false),
        None => (uint(v, "total_bytes_estimate"), true),
    };
    let status = v.get("status").and_then(Value::as_str).map(str::to_string);
    let percent = match (downloaded, total) {
        (Some(d), Some(t)) if t > 0 => Some((d as f64 / t as f64 * 100.0).min(100.0)),
        _ => num(v, "_percent"),
    };
    YtProgress {
        stage: Stage::Downloading,
        downloaded,
        total,
        total_is_estimate: total.is_some() && est,
        speed: num(v, "speed"),
        eta: uint(v, "eta"),
        fragment_index: uint(v, "fragment_index"),
        fragment_count: uint(v, "fragment_count"),
        filename: v
            .get("filename")
            .and_then(Value::as_str)
            .map(str::to_string),
        percent,
        postprocessor: None,
        status,
    }
}

/// Parse one line of yt-dlp stdout/stderr. Returns `None` for uninteresting lines.
pub fn parse_line(line: &str) -> Option<LineEvent> {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = line.strip_prefix(DL_PREFIX) {
        let v: Value = serde_json::from_str(rest).ok()?;
        return Some(LineEvent::Progress(parse_download_json(&v)));
    }
    if let Some(rest) = line.strip_prefix(PP_PREFIX) {
        let v: Value = serde_json::from_str(rest).ok()?;
        let pp = v
            .get("postprocessor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Some(LineEvent::Progress(YtProgress {
            stage: stage_for_postprocessor(&pp),
            postprocessor: Some(pp),
            status: v.get("status").and_then(Value::as_str).map(str::to_string),
            ..Default::default()
        }));
    }
    if let Some(rest) = line.strip_prefix(FILE_PREFIX) {
        let raw = rest.trim();
        let path = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string());
        return Some(LineEvent::FinalPath(path));
    }
    if let Some(rest) = line.strip_prefix("ERROR: ") {
        return Some(LineEvent::Error(rest.to_string()));
    }
    if let Some(rest) = line.strip_prefix("WARNING: ") {
        return Some(LineEvent::Warning(rest.to_string()));
    }
    // Classic "[Tag] message" lines.
    let rest = line.strip_prefix('[')?;
    let (tag, msg) = rest.split_once(']')?;
    let msg = msg.trim();
    if tag == "download" {
        return parse_classic_download(msg).map(LineEvent::Progress);
    }
    let stage = match tag {
        "Merger" => Stage::Merging,
        "ExtractAudio" => Stage::ExtractingAudio,
        "EmbedThumbnail" | "ThumbnailsConvertor" => Stage::EmbeddingThumbnail,
        "EmbedSubtitle" | "SubtitlesConvertor" => Stage::EmbeddingSubtitles,
        "Metadata" => Stage::EmbeddingMetadata,
        "SponsorBlock" | "ModifyChapters" => Stage::SponsorBlock,
        "VideoRemuxer" | "VideoConvertor" => Stage::Converting,
        "MoveFiles" => Stage::Moving,
        t if t.starts_with("Fixup") => Stage::PostProcessing,
        _ => return None,
    };
    let filename = msg
        .split_once("into \"")
        .or_else(|| msg.split_once("Destination: "))
        .map(|(_, f)| f.trim_end_matches('"').to_string());
    Some(LineEvent::Progress(YtProgress {
        stage,
        postprocessor: Some(tag.to_string()),
        filename,
        status: Some("processing".into()),
        ..Default::default()
    }))
}

/// `[download]  45.3% of ~ 10.00MiB at  1.00MiB/s ETA 00:05 (frag 3/20)`
fn parse_classic_download(msg: &str) -> Option<YtProgress> {
    if let Some(dest) = msg.strip_prefix("Destination: ") {
        return Some(YtProgress {
            stage: Stage::Downloading,
            filename: Some(dest.to_string()),
            status: Some("downloading".into()),
            ..Default::default()
        });
    }
    let pct_tok = msg.split_whitespace().next()?;
    let percent: f64 = pct_tok.strip_suffix('%')?.parse().ok()?;
    let mut p = YtProgress {
        stage: Stage::Downloading,
        percent: Some(percent),
        status: Some("downloading".into()),
        ..Default::default()
    };
    let toks: Vec<&str> = msg.split_whitespace().collect();
    let mut i = 1;
    while i < toks.len() {
        match toks[i] {
            "of" => {
                let mut j = i + 1;
                if toks.get(j) == Some(&"~") {
                    p.total_is_estimate = true;
                    j += 1;
                }
                let tok = toks.get(j).copied().unwrap_or("");
                let tok = match tok.strip_prefix('~') {
                    Some(t) => {
                        p.total_is_estimate = true;
                        t
                    }
                    None => tok,
                };
                p.total = parse_size(tok);
                i = j;
            }
            "at" => {
                p.speed = toks
                    .get(i + 1)
                    .and_then(|t| parse_size(t.trim_end_matches("/s")))
                    .map(|b| b as f64);
                i += 1;
            }
            "ETA" => {
                p.eta = toks.get(i + 1).and_then(|t| parse_clock(t));
                i += 1;
            }
            "(frag" => {
                if let Some((a, b)) = toks
                    .get(i + 1)
                    .map(|t| t.trim_end_matches(')'))
                    .and_then(|t| t.split_once('/'))
                {
                    p.fragment_index = a.parse().ok();
                    p.fragment_count = b.parse().ok();
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    if let (Some(t), Some(pc)) = (p.total, p.percent) {
        p.downloaded = Some((t as f64 * pc / 100.0) as u64);
    }
    Some(p)
}

/// `10.00MiB` → bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let idx = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (n, unit) = s.split_at(idx);
    let n: f64 = n.parse().ok()?;
    let mul = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0f64.powi(4),
        "KB" | "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return None,
    };
    Some((n * mul) as u64)
}

/// `01:02:03` / `02:03` → seconds.
pub fn parse_clock(s: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in s.split(':') {
        total = total * 60 + part.parse::<u64>().ok()?;
    }
    Some(total)
}

/// Incremental parser for ffmpeg `-progress pipe:1` output.
#[derive(Debug, Default, Clone)]
pub struct FfmpegProgress {
    /// Seconds of output written so far.
    pub out_time: f64,
    /// Bytes of output written so far.
    pub total_size: Option<u64>,
    pub speed: Option<f64>,
    pub ended: bool,
}

impl FfmpegProgress {
    /// Feed one line; returns `true` when a progress block completed (`progress=...`).
    pub fn feed(&mut self, line: &str) -> bool {
        let Some((k, v)) = line.trim().split_once('=') else {
            return false;
        };
        let v = v.trim();
        match k {
            // Both are microseconds (out_time_ms is a historical misnomer).
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = v.parse::<i64>()
                    && us >= 0
                {
                    self.out_time = us as f64 / 1_000_000.0;
                }
            }
            "out_time" => {
                if let Some(t) = parse_hms(v) {
                    self.out_time = t;
                }
            }
            "total_size" => {
                if let Ok(n) = v.parse::<u64>() {
                    self.total_size = Some(n);
                }
            }
            "speed" => self.speed = v.trim_end_matches('x').parse().ok(),
            "progress" => {
                if v == "end" {
                    self.ended = true;
                }
                return true;
            }
            _ => {}
        }
        false
    }

    pub fn percent(&self, duration: Option<f64>) -> Option<f64> {
        if self.ended {
            return Some(100.0);
        }
        let d = duration.filter(|d| *d > 0.0)?;
        Some((self.out_time / d * 100.0).clamp(0.0, 100.0))
    }
}

/// Progress of a part of a video. yt-dlp has ffmpeg fetch and cut the part and reports nothing
/// until it is done, so ffmpeg's `-progress` blocks (see [`crate::plan::PART_PROGRESS_ARGS`])
/// are read instead: the share of the part's time written is the progress. The final size is
/// unknown until then, so no total is made up: `total` stays empty and `percent` carries it.
#[derive(Debug)]
pub struct PartProgress {
    length: f64,
    ff: FfmpegProgress,
    /// Highest percent reported, so the bar never goes back.
    percent: f64,
    /// Recent `(time, bytes written)` samples for the speed.
    samples: VecDeque<(Instant, u64)>,
}

/// How far back the speed looks.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

impl PartProgress {
    /// Progress of a part `length` seconds long.
    pub fn new(length: f64) -> Self {
        PartProgress {
            length: length.max(0.001),
            ff: FfmpegProgress::default(),
            percent: 0.0,
            samples: VecDeque::new(),
        }
    }

    /// One `key=value` line of ffmpeg's `-progress` output (`out_time_us=…`, `progress=end`).
    /// yt-dlp's own lines all start differently (`[download]`, `PRISM {…}`, `ERROR: …`).
    pub fn is_progress_line(line: &str) -> bool {
        line.trim().split_once('=').is_some_and(|(k, _)| {
            !k.is_empty()
                && k.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
    }

    /// Feed one line; a finished block becomes a progress report.
    pub fn feed(&mut self, line: &str) -> Option<YtProgress> {
        self.feed_at(line, Instant::now())
    }

    pub fn feed_at(&mut self, line: &str, now: Instant) -> Option<YtProgress> {
        self.ff.feed(line).then(|| self.report(now))
    }

    fn report(&mut self, now: Instant) -> YtProgress {
        let done = self.ff.out_time.clamp(0.0, self.length);
        let percent = match self.ff.ended {
            true => 100.0,
            false => (done / self.length * 100.0).min(100.0),
        };
        self.percent = self.percent.max(percent);
        if let Some(bytes) = self.ff.total_size {
            self.samples.push_back((now, bytes));
            while self.samples.len() > 2
                && self
                    .samples
                    .get(1)
                    .is_some_and(|(t, _)| now.duration_since(*t) >= SPEED_WINDOW)
            {
                self.samples.pop_front();
            }
        }
        let speed = match (self.samples.front(), self.samples.back()) {
            (Some((t0, b0)), Some((t1, b1)))
                if t1.duration_since(*t0) >= Duration::from_millis(500) =>
            {
                Some(b1.saturating_sub(*b0) as f64 / t1.duration_since(*t0).as_secs_f64())
            }
            _ => None,
        };
        // ffmpeg's speed is media seconds per second: the time left follows from it directly.
        let eta = self
            .ff
            .speed
            .filter(|x| *x > 0.01 && !self.ff.ended)
            .map(|x| ((self.length - done).max(0.0) / x).ceil() as u64);
        YtProgress {
            stage: Stage::Downloading,
            downloaded: self.ff.total_size,
            speed,
            eta,
            percent: Some(self.percent),
            status: Some("downloading".into()),
            ..Default::default()
        }
    }
}

fn parse_hms(s: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(total)
}
