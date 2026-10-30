//! Network integration tests. All `#[ignore]`d; run manually with
//! `cargo test -p prism-video --test live -- --ignored --nocapture --test-threads 1`.
//!
//! Tools are installed once into `<target>/tmp/prism-video-tools` and reused.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use futures_util::StreamExt;
use prism_video::{
    CancellationToken, ChoiceKind, OutputOptions, Plan, Probe, RequestContext, Section, Stage,
    StreamSpec, Tool, ToolSource, UpdateOutcome, VideoInfo, VideoOptions, VideoService,
};

const BBB: &str = "https://www.youtube.com/watch?v=aqz-KE-bpKQ"; // Big Buck Bunny (CC-BY 3.0)

fn shared_tools_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("prism-video-tools")
}

static LOCK: Mutex<()> = Mutex::new(());

async fn ready_service() -> VideoService {
    let svc = VideoService::new(shared_tools_dir(), VideoOptions::default());
    let status = svc
        .ensure_tools(|p| {
            if p.downloaded == 0 {
                eprintln!("{:?} {:?}", p.tool, p.phase)
            }
        })
        .await
        .expect("ensure_tools");
    assert!(status.ready(), "{status:?}");
    svc
}

fn ffprobe_json(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            "-show_chapters",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", path.display());
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test]
#[ignore]
async fn ensure_tools_downloads_ytdlp_and_deno_with_checksums() {
    let _g = LOCK.lock();
    let dir = tempfile::tempdir().unwrap();
    let svc = VideoService::new(dir.path().to_path_buf(), VideoOptions::default());
    let before = svc.tool_status().await;
    eprintln!(
        "before: yt-dlp {:?}, deno {:?}, ffmpeg {:?}",
        before.yt_dlp.source, before.deno.source, before.ffmpeg.source
    );
    let progress_events = std::sync::atomic::AtomicUsize::new(0);
    let status = svc
        .ensure_tools(|p| {
            progress_events.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if p.downloaded == 0 {
                eprintln!("{:?} {:?} total={:?}", p.tool, p.phase, p.total);
            }
        })
        .await
        .unwrap();
    eprintln!("{status:#?}");
    assert!(progress_events.into_inner() > 10);
    if before.yt_dlp.source == ToolSource::Missing {
        assert_eq!(status.yt_dlp.source, ToolSource::Managed);
    }
    if before.deno.source == ToolSource::Missing {
        assert_eq!(status.deno.source, ToolSource::Managed);
        assert!(status.deno.version.as_deref().unwrap().starts_with('2'));
    }
    assert!(status.yt_dlp.version.as_deref().unwrap().starts_with("20"));
    assert!(status.ready());

    // Same SHA-256 as published -> up to date.
    let up = svc.update_ytdlp().await.unwrap();
    eprintln!("update #1: {up:?}");
    assert!(matches!(up, UpdateOutcome::UpToDate { .. }));

    // Damage the managed copy (the unpacked build's executable) -> it no longer starts ->
    // re-downloaded and verified.
    let managed = status.yt_dlp.path.clone().unwrap();
    std::fs::write(&managed, b"not yt-dlp").unwrap();
    let up = svc.update_ytdlp().await.unwrap();
    eprintln!("update #2: {up:?}");
    assert!(matches!(up, UpdateOutcome::Updated { .. }));
    assert!(svc.tool_status().await.yt_dlp.version.is_some());
}

#[tokio::test]
#[ignore]
async fn install_managed_ffmpeg() {
    let _g = LOCK.lock();
    let dir = tempfile::tempdir().unwrap();
    let svc = VideoService::new(dir.path().to_path_buf(), VideoOptions::default());
    let t0 = std::time::Instant::now();
    let last = Mutex::new(0u64);
    let info = svc
        .install_tool(Tool::Ffmpeg, |p| {
            let mut l = last.lock().unwrap();
            if p.downloaded >= *l + 20_000_000 || p.downloaded == 0 {
                *l = p.downloaded;
                eprintln!(
                    "{:?} {:?} {} / {:?}",
                    p.tool, p.phase, p.downloaded, p.total
                );
            }
        })
        .await
        .unwrap();
    eprintln!("ffmpeg installed in {:?}: {info:?}", t0.elapsed());
    assert_eq!(info.source, ToolSource::Managed);
    let status = svc.tool_status().await;
    assert_eq!(status.ffprobe.source, ToolSource::Managed);
    eprintln!(
        "ffmpeg {:?} / ffprobe {:?}",
        status.ffmpeg.version, status.ffprobe.version
    );
    assert!(status.ffmpeg.version.is_some() && status.ffprobe.version.is_some());
}

async fn probe_bbb(svc: &VideoService) -> VideoInfo {
    match svc.probe(BBB, &RequestContext::default()).await.unwrap() {
        Probe::Video(v) => *v,
        Probe::Playlist(_) => panic!("expected video"),
    }
}

#[tokio::test]
#[ignore]
async fn probe_public_cc_video_and_playlist() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let v = probe_bbb(&svc).await;
    eprintln!("{} formats, choices:", v.formats.len());
    for c in &v.choices {
        eprintln!(
            "  {:<14} {:<34} {:<16} accel={}",
            c.id, c.label, c.sub_label, c.accelerable
        );
    }
    assert_eq!(v.id, "aqz-KE-bpKQ");
    assert!(v.choices.iter().any(|c| c.height == Some(2160)));
    assert!(v.choices.iter().any(|c| c.kind == ChoiceKind::Audio));

    // Flat playlist (watch URL with list= and want_playlist).
    let ctx = RequestContext {
        want_playlist: true,
        ..Default::default()
    };
    let p = svc
        .probe("https://www.youtube.com/@BlenderOfficial/videos", &ctx)
        .await
        .unwrap();
    let Probe::Playlist(p) = p else {
        panic!("expected playlist")
    };
    eprintln!("playlist {:?}: {} entries", p.title, p.entries.len());
    assert!(p.entries.len() > 10);
    assert!(
        p.entries[0]
            .url
            .starts_with("https://www.youtube.com/watch?v=")
    );
}

#[tokio::test]
#[ignore]
async fn ytdlp_download_360p() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let v = probe_bbb(&svc).await;
    let c = v
        .choices
        .iter()
        .find(|c| c.height == Some(360) && c.kind == ChoiceKind::Video)
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let opts = OutputOptions {
        output_dir: out.path().to_path_buf(),
        embed_subtitles: true,
        ..Default::default()
    };
    let job = svc.plan_ytdlp_forced(&v, c, &opts, &RequestContext::default());
    eprintln!("yt-dlp {}", svc.ytdlp_command_line(&job, None).join(" "));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<prism_video::YtProgress>();
    let collector = tokio::spawn(async move {
        let mut stages = Vec::new();
        let mut n = 0;
        while let Some(p) = rx.recv().await {
            n += 1;
            if stages.last() != Some(&p.stage) {
                stages.push(p.stage);
            }
        }
        (n, stages)
    });
    let path = svc
        .run_ytdlp(&job, tx, CancellationToken::new())
        .await
        .unwrap();
    let (n, stages) = collector.await.unwrap();
    eprintln!(
        "final: {} ({n} progress events, stages {stages:?})",
        path.display()
    );
    assert!(path.is_file());
    assert!(stages.contains(&Stage::Downloading));
    assert!(stages.contains(&Stage::Merging));
    assert_eq!(stages.last(), Some(&Stage::Finished));
    let j = ffprobe_json(&path);
    let vs = j["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] == 0)
        .unwrap();
    assert_eq!(vs["height"], 360);
    assert!(
        j["format"]["tags"]["title"]
            .as_str()
            .unwrap()
            .contains("Big Buck Bunny")
    );
    assert!(
        j["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["disposition"]["attached_pic"] == 1),
        "thumbnail embedded"
    );
}

#[tokio::test]
#[ignore]
async fn ytdlp_cancel_kills_process() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let v = probe_bbb(&svc).await;
    let c = v.choices.iter().find(|c| c.height == Some(2160)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let opts = OutputOptions {
        output_dir: out.path().to_path_buf(),
        ..Default::default()
    };
    let job = svc.plan_ytdlp_forced(&v, c, &opts, &RequestContext::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<prism_video::YtProgress>();
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            if p.stage == Stage::Downloading && p.downloaded.unwrap_or(0) > 1_000_000 {
                t2.cancel();
            }
        }
    });
    let t0 = std::time::Instant::now();
    let err = svc.run_ytdlp(&job, tx, token).await.unwrap_err();
    eprintln!("cancelled after {:?}: {err}", t0.elapsed());
    assert!(err.is_cancelled());
    // The .part file must no longer be growing (process tree killed).
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let size = |d: &Path| -> u64 {
        std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum()
    };
    let s1 = size(out.path());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(s1, size(out.path()), "download continued after cancel");
}

/// "Only a part": 10 seconds of the video (cut on exact frames) and of its MP3, with ffmpeg's
/// progress going steadily up to 100 % and never past it.
#[tokio::test]
#[ignore]
async fn ytdlp_cuts_a_10_second_part() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let v = probe_bbb(&svc).await;
    let video = v
        .choices
        .iter()
        .find(|c| c.height == Some(360) && c.kind == ChoiceKind::Video)
        .unwrap();
    let mp3 = v.choices.iter().find(|c| c.id == "a-mp3").unwrap();
    let out = tempfile::tempdir().unwrap();
    let part = Section::new(60.0, 70.0);
    for (c, ext) in [(video, "mp4"), (mp3, "mp3")] {
        let opts = OutputOptions {
            output_dir: out.path().to_path_buf(),
            file_template: format!("bbb{}.%(ext)s", part.file_suffix()),
            section: Some(part),
            ..Default::default()
        };
        let Plan::YtDlp(job) = svc.plan(&v, c, &opts, &RequestContext::default()) else {
            panic!("a part goes through yt-dlp")
        };
        eprintln!("yt-dlp {}", svc.ytdlp_command_line(&job, None).join(" "));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<prism_video::YtProgress>();
        let collector = tokio::spawn(async move {
            let mut shares = Vec::new();
            while let Some(p) = rx.recv().await {
                if p.stage == Stage::Downloading && p.total.is_none() {
                    shares.push(p.percent.unwrap_or(-1.0));
                }
            }
            shares
        });
        let t0 = std::time::Instant::now();
        let path = svc
            .run_ytdlp(&job, tx, CancellationToken::new())
            .await
            .unwrap();
        let shares = collector.await.unwrap();
        let j = ffprobe_json(&path);
        let duration: f64 = j["format"]["duration"].as_str().unwrap().parse().unwrap();
        eprintln!(
            "{} ({duration} s) in {:?}; progress {shares:?}",
            path.display(),
            t0.elapsed()
        );
        assert!(
            path.to_string_lossy()
                .ends_with(&format!("bbb (1m00s–1m10s).{ext}"))
        );
        assert!(!shares.is_empty(), "no progress while cutting");
        assert!(shares.windows(2).all(|w| w[0] <= w[1]), "{shares:?}");
        assert!(
            shares.iter().all(|s| (0.0..=100.0).contains(s)),
            "{shares:?}"
        );
        assert!((duration - 10.0).abs() < 0.25, "{ext}: {duration} s");
        // The whole video's chapters would point past the part's end.
        assert!(j["chapters"].as_array().is_none_or(|c| c.is_empty()));
    }
}

/// Download one stream the way the engine would: sequential ranged GETs of at most `chunk_size`.
async fn fetch(client: &reqwest::Client, s: &StreamSpec, dest: &Path) {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::File::create(dest).await.unwrap();
    let chunk = s.chunk_size.unwrap_or(u64::MAX / 4);
    let mut pos = 0u64;
    loop {
        let mut req = client.get(&s.url);
        for (k, v) in &s.headers {
            req = req.header(k, v);
        }
        let end = pos + chunk - 1;
        let resp = req
            .header("Range", format!("bytes={pos}-{end}"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        assert!(status.is_success(), "{} -> {status}", s.file_name);
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok());
        let mut body = resp.bytes_stream();
        while let Some(b) = body.next().await {
            let b = b.unwrap();
            pos += b.len() as u64;
            f.write_all(&b).await.unwrap();
        }
        match total {
            Some(t) if pos < t => continue,
            _ => break,
        }
    }
    f.flush().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn accelerated_path_end_to_end_360p() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let v = probe_bbb(&svc).await;
    let c = v
        .choices
        .iter()
        .find(|c| c.height == Some(360) && c.kind == ChoiceKind::Video)
        .unwrap()
        .clone();
    let out = tempfile::tempdir().unwrap();
    let mut opts = OutputOptions {
        output_dir: out.path().join("final"),
        ..Default::default()
    };
    opts.sponsorblock.mode = prism_video::SponsorBlockMode::Mark;
    opts.sponsorblock.categories = vec!["sponsor".into(), "outro".into()];
    let Plan::Accelerated(mut plan) = svc.plan(&v, &c, &opts, &RequestContext::default()) else {
        panic!("expected accelerated plan for {c:?}")
    };
    let client = reqwest::Client::new();
    let t0 = std::time::Instant::now();
    for s in &plan.streams {
        fetch(&client, s, &out.path().join(&s.file_name)).await;
        eprintln!(
            "  {:?} {} ({} bytes)",
            s.role,
            s.file_name,
            std::fs::metadata(out.path().join(&s.file_name))
                .unwrap()
                .len()
        );
    }
    eprintln!("streams downloaded in {:?}", t0.elapsed());
    plan.post.set_input_dir(out.path());
    let path = svc
        .postprocess(&plan.post, None, CancellationToken::new())
        .await
        .unwrap();
    let j = ffprobe_json(&path);
    let streams = j["streams"].as_array().unwrap();
    let codecs: Vec<&str> = streams
        .iter()
        .map(|s| s["codec_name"].as_str().unwrap_or("?"))
        .collect();
    eprintln!("{} -> {codecs:?}", path.display());
    assert!(codecs.contains(&"h264") && codecs.contains(&"aac"));
    assert!(
        streams
            .iter()
            .any(|s| s["disposition"]["attached_pic"] == 1)
    );
    assert_eq!(j["format"]["tags"]["artist"], "Blender");
    // SponsorBlock "outro" segment of BBB (community data) became a chapter.
    let chapters: Vec<String> = j["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["tags"]["title"].as_str().unwrap_or("").to_string())
        .collect();
    eprintln!("chapters: {chapters:?}");
    assert_eq!(chapters, ["", "[SponsorBlock]: Endcards/Credits", ""]);
    // Inputs deleted (delete_temp_inputs defaults to true).
    assert!(!out.path().join(&plan.streams[0].file_name).exists());

    // Audio-only MP3 via the accelerated path.
    let mp3 = v.choices.iter().find(|c| c.id == "a-mp3").unwrap();
    let Plan::Accelerated(mut plan) = svc.plan(&v, mp3, &opts, &RequestContext::default()) else {
        panic!()
    };
    for s in &plan.streams {
        fetch(&client, s, &out.path().join(&s.file_name)).await;
    }
    plan.post.set_input_dir(out.path());
    let path = svc
        .postprocess(&plan.post, None, CancellationToken::new())
        .await
        .unwrap();
    let j = ffprobe_json(&path);
    eprintln!("{} -> {}", path.display(), j["format"]["bit_rate"]);
    assert_eq!(j["streams"][0]["codec_name"], "mp3");
}

#[tokio::test]
#[ignore]
async fn sponsorblock_api_reachable() {
    let client = reqwest::Client::new();
    // A long-running tech channel video; segments may or may not exist — the call must succeed.
    let segs = prism_video::sponsorblock::fetch_segments(
        &client,
        "aqz-KE-bpKQ",
        &["sponsor".into(), "intro".into(), "outro".into()],
    )
    .await
    .unwrap();
    eprintln!("BBB segments: {segs:?}");
    let segs = prism_video::sponsorblock::fetch_segments(
        &client,
        "dQw4w9WgXcQ",
        &[
            "sponsor".into(),
            "intro".into(),
            "outro".into(),
            "music_offtopic".into(),
        ],
    )
    .await
    .unwrap();
    eprintln!("dQw4w9WgXcQ segments: {segs:?}");
}

/// LinkedIn feed post as the browser extension sends it: the post's permalink plus the
/// signed-in session's cookies (Prism leaves those out for LinkedIn posts, see `probe.rs`).
#[tokio::test]
#[ignore]
async fn linkedin_post_permalink_with_session_cookies() {
    let _g = LOCK.lock();
    let svc = ready_service().await;
    let ctx = RequestContext {
        cookies: Some("li_at=AQEDAR_not_a_real_session; JSESSIONID=\"ajax:123\"".into()),
        user_agent: Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36".into()),
        ..Default::default()
    };
    let probe = svc
        .probe(
            "https://www.linkedin.com/feed/update/urn:li:activity:7151241570371948544/",
            &ctx,
        )
        .await
        .expect("probe");
    let Probe::Video(info) = probe else {
        panic!("expected a video")
    };
    eprintln!("{}: {} formats", info.title, info.formats.len());
    assert!(info.title.contains("MathWorks"), "{}", info.title);
    let choices = svc.choices(&info, prism_video::Container::Mp4);
    let video: Vec<_> = choices
        .iter()
        .filter(|c| c.kind != ChoiceKind::Audio)
        .collect();
    assert!(!video.is_empty(), "{choices:?}");
    eprintln!(
        "choices: {:?}",
        choices.iter().map(|c| c.label.clone()).collect::<Vec<_>>()
    );
    // Down to the file, with the same cookies (the download reads the post page again).
    let out = tempfile::tempdir().unwrap();
    let opts = OutputOptions {
        output_dir: out.path().to_path_buf(),
        ..Default::default()
    };
    let job = svc.plan_ytdlp_forced(&info, video[0], &opts, &ctx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<prism_video::YtProgress>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let path = svc
        .run_ytdlp(&job, tx, CancellationToken::new())
        .await
        .expect("download");
    let listing: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    eprintln!("exists={} dir={listing:?}", path.is_file());
    let j = ffprobe_json(&path);
    let kinds: Vec<_> = j["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["codec_type"].as_str().unwrap().to_string())
        .collect();
    eprintln!(
        "{} ({} bytes): {kinds:?}",
        path.display(),
        std::fs::metadata(&path).unwrap().len()
    );
    assert!(
        kinds.contains(&"video".to_string()) && kinds.contains(&"audio".to_string()),
        "{kinds:?}"
    );
}

/// Managed yt-dlp is the unpacked build: installed fresh, migrated from a single-file install by
/// the automatic update, up to date afterwards, and quick to start.
#[tokio::test]
#[ignore]
async fn unpacked_ytdlp_install_migration_update_and_start_time() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    // An old single-file install (what 1.0.5 and earlier left behind).
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(tools.join(Tool::YtDlp.exe_name()), b"old single-file build").unwrap();
    let svc = VideoService::new(tools.clone(), VideoOptions::default());
    let out = svc.update_ytdlp().await.expect("update");
    eprintln!("update from the single file: {out:?}");
    assert!(matches!(out, UpdateOutcome::Updated { .. }), "{out:?}");
    let st = svc.tool_status().await;
    let yt = st.get(Tool::YtDlp);
    let exe = yt.path.clone().expect("yt-dlp path");
    eprintln!("yt-dlp {:?} at {}", yt.version, exe.display());
    assert_eq!(yt.source, ToolSource::Managed);
    assert!(
        exe.parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("yt-dlp-"),
        "{}",
        exe.display()
    );
    assert!(
        !tools.join(Tool::YtDlp.exe_name()).exists(),
        "single file removed"
    );
    let again = svc.update_ytdlp().await.expect("update again");
    assert!(matches!(again, UpdateOutcome::UpToDate { .. }), "{again:?}");
    // Start time of the unpacked build.
    let mut times = Vec::new();
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let o = Command::new(&exe).arg("--version").output().unwrap();
        assert!(o.status.success());
        times.push(t.elapsed().as_millis());
    }
    eprintln!("unpacked yt-dlp --version: {times:?} ms");
    // And it works: a real probe.
    let svc2 = VideoService::new(tools.clone(), VideoOptions::default());
    let t = std::time::Instant::now();
    let probe = svc2
        .probe(
            "https://www.linkedin.com/feed/update/urn:li:activity:7151241570371948544/",
            &RequestContext::default(),
        )
        .await
        .expect("probe");
    eprintln!("probe {} ms", t.elapsed().as_millis());
    assert!(matches!(probe, Probe::Video(_)));
}
