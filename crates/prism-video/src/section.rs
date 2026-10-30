//! A part of a video ("from 1:23 to 2:45"): clock times, yt-dlp's `--download-sections`
//! value and the file-name suffix that keeps a part apart from the whole video.

use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The shortest part Prism cuts, in seconds.
pub const MIN_LENGTH: f64 = 1.0;

/// A part of a video, in seconds from its start. The times also deserialize from clock
/// strings (`"1:23"`), see [`parse_time`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    #[serde(deserialize_with = "seconds")]
    pub start: f64,
    #[serde(deserialize_with = "seconds")]
    pub end: f64,
}

impl Section {
    pub fn new(start: f64, end: f64) -> Section {
        Section { start, end }
    }

    /// Seconds between the start and the end.
    pub fn length(&self) -> f64 {
        self.end - self.start
    }

    /// This range as a part of a video `duration` seconds long (when known): cut to the video.
    /// `None` when less than [`MIN_LENGTH`] is left or it covers the whole video (to the second),
    /// which downloads the whole file as usual.
    pub fn part_of(self, duration: Option<f64>) -> Option<Section> {
        if !self.start.is_finite() || !self.end.is_finite() {
            return None;
        }
        let start = self.start.max(0.0);
        let mut end = self.end;
        if let Some(d) = duration.filter(|d| d.is_finite() && *d > 0.0) {
            end = end.min(d);
            if start < 1.0 && end > d - 1.0 {
                return None;
            }
        }
        (end - start >= MIN_LENGTH).then_some(Section { start, end })
    }

    /// yt-dlp's `--download-sections` value: `*83-165`.
    pub fn ytdlp_arg(&self) -> String {
        format!("*{}-{}", plain_number(self.start), plain_number(self.end))
    }

    /// What a part adds to the file name: ` (1m23s–2m45s)`. No `:` (not allowed on Windows),
    /// and the full video's name stays free.
    pub fn file_suffix(&self) -> String {
        format!(" ({}–{})", compact_time(self.start), compact_time(self.end))
    }
}

/// `83` → `83`, `83.25` → `83.25` (at most milliseconds, no exponent).
fn plain_number(secs: f64) -> String {
    let s = format!("{:.3}", secs.max(0.0));
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Whole seconds as `5s`, `1m23s`, `1h02m03s`.
pub fn compact_time(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s:02}s"),
        _ => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Seconds from a time typed by a person: `83`, `1:23`, `01:23`, `1:02:03`, with an optional
/// fraction of a second (`1:23.5`). Minutes and seconds after a colon stay below 60.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    if s.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let digits = part.split_once('.').map_or(*part, |(whole, frac)| {
            if last && !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()) {
                whole
            } else {
                "-" // not a number
            }
        });
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: f64 = part.parse().ok()?;
        if i > 0 && v >= 60.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

fn seconds<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_time(&s),
        _ => None,
    };
    v.filter(|x| x.is_finite() && *x >= 0.0)
        .ok_or_else(|| D::Error::custom("expected seconds or a time like 1:23"))
}
