//! A part of a video: typed times, yt-dlp's `--download-sections` value, the file-name suffix
//! and ffmpeg's progress while it cuts the part.

use std::time::{Duration, Instant};

use prism_video::Section;
use prism_video::progress::{PartProgress, Stage};
use prism_video::section::{compact_time, parse_time};

#[test]
fn typed_times() {
    assert_eq!(parse_time("83"), Some(83.0));
    assert_eq!(parse_time("1:23"), Some(83.0));
    assert_eq!(parse_time("01:23"), Some(83.0));
    assert_eq!(parse_time(" 1:02:03 "), Some(3723.0));
    assert_eq!(parse_time("0:00"), Some(0.0));
    assert_eq!(parse_time("90:00"), Some(5400.0));
    assert_eq!(parse_time("1:23.5"), Some(83.5));
    for bad in [
        "", " ", "1:", ":23", "1:75", "1:2:3:4", "-5", "1,5", "1.2:00", "1:23.", "abc", "1m23s",
        "1:2 3",
    ] {
        assert_eq!(parse_time(bad), None, "{bad:?}");
    }
}

#[test]
fn ytdlp_value_and_file_suffix() {
    let s = Section::new(83.0, 165.0);
    assert_eq!(s.ytdlp_arg(), "*83-165");
    assert_eq!(s.file_suffix(), " (1m23s–2m45s)");
    assert_eq!(s.length(), 82.0);
    assert_eq!(Section::new(0.0, 10.25).ytdlp_arg(), "*0-10.25");
    assert_eq!(Section::new(5.0, 3723.4).file_suffix(), " (5s–1h02m03s)");
    // Nothing a file system refuses (Windows: \ / : * ? " < > |).
    let suffix = Section::new(3599.0, 7322.0).file_suffix();
    assert!(
        !suffix.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']),
        "{suffix}"
    );
    assert_eq!(compact_time(59.6), "1m00s");
    assert_eq!(compact_time(0.0), "0s");
}

#[test]
fn a_part_within_the_video() {
    let d = Some(635.0);
    assert_eq!(
        Section::new(83.0, 165.0).part_of(d),
        Some(Section::new(83.0, 165.0))
    );
    assert_eq!(
        Section::new(600.0, 900.0).part_of(d),
        Some(Section::new(600.0, 635.0))
    );
    assert_eq!(
        Section::new(-3.0, 10.0).part_of(d),
        Some(Section::new(0.0, 10.0))
    );
    // The whole video is no part; less than a second neither.
    assert_eq!(Section::new(0.0, 635.0).part_of(d), None);
    assert_eq!(Section::new(0.4, 634.5).part_of(d), None);
    assert_eq!(Section::new(10.0, 10.5).part_of(d), None);
    assert_eq!(Section::new(640.0, 700.0).part_of(d), None);
    assert_eq!(Section::new(20.0, 10.0).part_of(d), None);
    assert_eq!(Section::new(f64::NAN, 10.0).part_of(d), None);
    // Unknown length: kept as asked.
    assert_eq!(
        Section::new(0.0, 5000.0).part_of(None),
        Some(Section::new(0.0, 5000.0))
    );
}

#[test]
fn times_deserialize_from_numbers_and_clock_strings() {
    let s: Section = serde_json::from_str(r#"{"start": 83, "end": "2:45"}"#).unwrap();
    assert_eq!(s, Section::new(83.0, 165.0));
    assert_eq!(
        serde_json::to_value(s).unwrap(),
        serde_json::json!({"start": 83.0, "end": 165.0})
    );
    assert!(serde_json::from_str::<Section>(r#"{"start": "soon", "end": 5}"#).is_err());
    assert!(serde_json::from_str::<Section>(r#"{"start": -1, "end": 5}"#).is_err());
}

/// One `-progress` block as ffmpeg 9 writes it (yt-dlp passes `-progress pipe:1 -nostats`).
fn block(out_us: i64, size: u64, speed: &str, end: bool) -> Vec<String> {
    let mut v: Vec<String> = [
        "frame=42",
        "fps=11.61",
        "stream_0_0_q=29.0",
        "bitrate=   0.2kbits/s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push(format!("total_size={size}"));
    v.push(format!("out_time_us={out_us}"));
    v.push(format!("out_time_ms={out_us}"));
    v.push("dup_frames=0".into());
    v.push(format!("speed={speed}"));
    v.push(format!("progress={}", if end { "end" } else { "continue" }));
    v
}

#[test]
fn ffmpeg_progress_of_a_part() {
    // ffmpeg's lines, and only those, are taken for progress.
    for line in [
        "out_time_us=1173333",
        "progress=continue",
        "stream_0_0_q=29.0",
        "bitrate=   0.3kbits/s",
    ] {
        assert!(PartProgress::is_progress_line(line), "{line}");
    }
    for line in [
        r#"PRISM {"status": "finished"}"#,
        "[download] Destination: a.mp4",
        "WARNING: Unknown codec iamf.001.001.Opus",
        "ERROR: unable to download video data: HTTP Error 403: Forbidden",
        "",
    ] {
        assert!(!PartProgress::is_progress_line(line), "{line}");
    }

    let t0 = Instant::now();
    let mut p = PartProgress::new(40.0);
    let mut feed = |lines: Vec<String>, at: u64| {
        let mut out = None;
        for l in lines {
            if let Some(r) = p.feed_at(&l, t0 + Duration::from_millis(at)) {
                out = Some(r);
            }
        }
        out.expect("a block reports")
    };
    let r = feed(block(10_000_000, 1_000_000, "2.0x", false), 0);
    assert_eq!(r.stage, Stage::Downloading);
    assert_eq!(r.percent, Some(25.0));
    // The size of a cut part is unknown until it's done: none is made up.
    assert_eq!(r.total, None);
    assert_eq!(r.downloaded, Some(1_000_000));
    // 30 s of the part left at 2× real time.
    assert_eq!(r.eta, Some(15));
    assert_eq!(r.speed, None, "one sample is no speed");

    let r = feed(block(20_000_000, 3_000_000, "2.0x", false), 1000);
    assert_eq!(r.percent, Some(50.0));
    assert_eq!(r.speed, Some(2_000_000.0));
    assert_eq!(r.eta, Some(10));

    // ffmpeg's clock briefly behind, then past the part's end: the bar never goes back or over.
    let r = feed(block(19_000_000, 3_100_000, "N/A", false), 1500);
    assert_eq!(r.percent, Some(50.0));
    assert_eq!(r.eta, None);
    let r = feed(block(41_000_000, 6_000_000, "2.1x", false), 2000);
    assert_eq!(r.percent, Some(100.0));
    assert_eq!(r.eta, Some(0));
    let r = feed(block(40_000_000, 6_100_000, "2.1x", true), 2500);
    assert_eq!(r.percent, Some(100.0));
    assert_eq!(r.downloaded, Some(6_100_000));
}
