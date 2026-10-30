//! The Tools on real files: tiny test media made with ffmpeg's own sources (a 3 s video, a
//! tone, pictures), converted and made smaller with the ffmpeg on PATH or in Prism's tools
//! folder. Skipped when there's no ffmpeg (nothing is downloaded).

use std::path::{Path, PathBuf};
use std::process::Command;

use prism_video::media::{MediaKind, MediaProblem, Quality, Target, convert, pack, shrink};
use prism_video::tools::which;
use prism_video::{CancellationToken, VideoError, VideoOptions, VideoService};

/// Prism's own tools folder (where the app keeps the ffmpeg it downloaded).
fn prism_tools_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Prism")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/Prism")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share/prism")
    };
    Some(base.join("tools"))
}

/// The folder with ffmpeg + ffprobe, if any (PATH first).
fn ffmpeg_dir() -> Option<PathBuf> {
    let exe = std::env::consts::EXE_SUFFIX;
    let (ff, fp) = (format!("ffmpeg{exe}"), format!("ffprobe{exe}"));
    if let (Some(a), Some(_)) = (which(&ff), which(&fp)) {
        return a.parent().map(Path::to_path_buf);
    }
    let dir = prism_tools_dir()?;
    (dir.join(&ff).is_file() && dir.join(&fp).is_file()).then_some(dir)
}

fn setup() -> Option<(VideoService, PathBuf, tempfile::TempDir)> {
    let Some(dir) = ffmpeg_dir() else {
        eprintln!("no ffmpeg on PATH or in Prism's tools folder, skipping");
        return None;
    };
    let tools = tempfile::tempdir().unwrap();
    let opts = VideoOptions {
        ffmpeg_path: Some(dir.clone()),
        ..Default::default()
    };
    let svc = VideoService::new(tools.keep(), opts);
    let work = tempfile::tempdir().unwrap();
    Some((svc, dir, work))
}

fn ffmpeg(dir: &Path, args: &[&str]) {
    let exe = dir.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
    let out = Command::new(exe)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `codec_name` of the first stream of a kind, and the format name.
fn probe(dir: &Path, file: &Path, kind: &str) -> (String, String) {
    let exe = dir.join(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
    let out = Command::new(exe)
        .args(["-v", "error", "-select_streams", &format!("{kind}:0")])
        .args([
            "-show_entries",
            "stream=codec_name:format=format_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(file)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines().map(str::to_string);
    (
        lines.next().unwrap_or_default(),
        lines.next().unwrap_or_default(),
    )
}

fn no_temp_files(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.contains("prismtmp")), "{names:?}");
}

fn make_video(dir: &Path, out: &Path, secs: &str, size: &str, bitrate: &str) {
    ffmpeg(
        dir,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate=30,noise=alls=30:allf=t"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            secs,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-b:v",
            bitrate,
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-shortest",
            &out.to_string_lossy(),
        ],
    );
}

#[tokio::test]
async fn converts_video_audio_and_pictures() {
    let Some((svc, dir, work)) = setup() else {
        return;
    };
    let w = work.path();
    let clip = w.join("clip.mkv");
    make_video(&dir, &clip, "3", "320x240", "600k");
    let tone = w.join("tone.wav");
    ffmpeg(
        &dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:duration=2",
            &tone.to_string_lossy(),
        ],
    );
    let pic = w.join("pic.png");
    ffmpeg(
        &dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360",
            "-frames:v",
            "1",
            &pic.to_string_lossy(),
        ],
    );
    let logo = w.join("logo.png");
    ffmpeg(
        &dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red@0.5:s=200x100,format=rgba",
            "-frames:v",
            "1",
            &logo.to_string_lossy(),
        ],
    );
    let before = std::fs::read(&clip).unwrap();

    let info = svc.media_inspect(&clip).await.unwrap();
    assert_eq!(info.kind, MediaKind::Video);
    assert_eq!((info.width, info.height), (Some(320), Some(240)));
    assert!(
        (info.duration.unwrap() - 3.0).abs() < 0.2,
        "{:?}",
        info.duration
    );
    assert!(info.has_audio);
    assert_eq!(
        info.targets,
        vec![Target::Mp4, Target::Webm, Target::Gif, Target::Mp3]
    );

    let cancel = CancellationToken::new();
    let last = std::sync::Mutex::new(None);
    let report = |p: prism_video::media::MediaProgress| *last.lock().unwrap() = p.fraction;
    for (target, kind, codec, format) in [
        (Target::Mp4, "v", "h264", "mov,mp4,m4a,3gp,3g2,mj2"),
        (Target::Webm, "v", "vp9", "matroska,webm"),
        (Target::Gif, "v", "gif", "gif"),
        (Target::Mp3, "a", "mp3", "mp3"),
    ] {
        let out = prism_video::media::output_path(&clip, target.ext(), None);
        let made = svc
            .media_convert(&info, target, Quality::Standard, &out, &report, &cancel)
            .await
            .unwrap();
        assert_eq!(made, out);
        assert_eq!(
            probe(&dir, &made, kind),
            (codec.to_string(), format.to_string()),
            "{target:?}"
        );
    }
    assert!(last.lock().unwrap().is_some_and(|f| f > 0.9));
    // The original is untouched.
    assert_eq!(std::fs::read(&clip).unwrap(), before);

    let tone_info = svc.media_inspect(&tone).await.unwrap();
    assert_eq!(tone_info.kind, MediaKind::Audio);
    for (target, codec) in [
        (Target::Mp3, "mp3"),
        (Target::M4a, "aac"),
        (Target::Flac, "flac"),
        (Target::Ogg, "vorbis"),
    ] {
        let out = prism_video::media::output_path(&tone, target.ext(), None);
        let made = svc
            .media_convert(
                &tone_info,
                target,
                Quality::Standard,
                &out,
                &|_| {},
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(probe(&dir, &made, "a").0, codec, "{target:?}");
    }

    for (src, target, codec) in [
        (&pic, Target::Jpg, "mjpeg"),
        (&pic, Target::Webp, "webp"),
        (&logo, Target::Jpg, "mjpeg"),
    ] {
        let pinfo = svc.media_inspect(src).await.unwrap();
        assert_eq!(pinfo.kind, MediaKind::Image);
        if !pinfo.targets.contains(&target) {
            // This ffmpeg can't make it (Homebrew's has no libwebp), so it isn't offered.
            continue;
        }
        let out = prism_video::media::output_path(src, target.ext(), None);
        let made = svc
            .media_convert(&pinfo, target, Quality::Standard, &out, &|_| {}, &cancel)
            .await
            .unwrap();
        assert_eq!(probe(&dir, &made, "v").0, codec);
    }
    // Converting again doesn't overwrite: the next free name.
    let pinfo = svc.media_inspect(&pic).await.unwrap();
    let again = svc
        .media_convert(
            &pinfo,
            Target::Jpg,
            Quality::High,
            &w.join("pic.jpg"),
            &|_| {},
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(again, w.join("pic (2).jpg"));

    // A text file isn't read by ffprobe at all.
    let notes = w.join("notes.txt");
    std::fs::write(&notes, "hello").unwrap();
    let n = svc.media_inspect(&notes).await.unwrap();
    assert_eq!((n.kind, n.problem), (MediaKind::Other, None));
    // A file that only looks like a video.
    let fake = w.join("fake.mp4");
    std::fs::write(&fake, "not a video").unwrap();
    let f = svc.media_inspect(&fake).await.unwrap();
    assert_eq!(f.problem, Some(MediaProblem::Unreadable));
    assert!(f.targets.is_empty());
    no_temp_files(w);
}

#[tokio::test]
async fn makes_files_smaller_than_the_limit() {
    let Some((svc, dir, work)) = setup() else {
        return;
    };
    let w = work.path();
    let cancel = CancellationToken::new();

    // 4 s of noisy 720p at 8 Mbps (~4 MB) into 1 MB.
    let clip = w.join("noisy.mp4");
    make_video(&dir, &clip, "4", "1280x720", "8M");
    let info = svc.media_inspect(&clip).await.unwrap();
    let target = 1_000_000;
    assert!(info.size > target);
    let plan = shrink::plan(&info, target);
    assert_eq!(plan.verdict, shrink::Verdict::Ok);
    let out = prism_video::media::output_path(&clip, &plan.ext, Some("smaller"));
    let done = svc
        .media_shrink(&info, target, &out, &|_| {}, &cancel)
        .await
        .unwrap();
    assert_eq!(done.path, w.join("noisy (smaller).mp4"));
    assert!(done.size <= target, "{} > {target}", done.size);
    assert_eq!(done.size, std::fs::metadata(&done.path).unwrap().len());
    assert_eq!(probe(&dir, &done.path, "v").0, "h264");
    assert_eq!(probe(&dir, &done.path, "a").0, "aac");

    // 20 s of sound (WAV, 1.9 MB) into 200 kB.
    let tone = w.join("tone.wav");
    ffmpeg(
        &dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=500:duration=20",
            "-ar",
            "48000",
            &tone.to_string_lossy(),
        ],
    );
    let info = svc.media_inspect(&tone).await.unwrap();
    let plan = shrink::plan(&info, 200_000);
    assert_eq!(plan.ext, "mp3");
    let out = prism_video::media::output_path(&tone, &plan.ext, Some("E-mail"));
    let done = svc
        .media_shrink(&info, 200_000, &out, &|_| {}, &cancel)
        .await
        .unwrap();
    assert!(done.size <= 200_000, "{}", done.size);
    assert_eq!(probe(&dir, &done.path, "a").0, "mp3");

    // A noisy 3000×2000 PNG (~18 MB) into 400 kB.
    let png = w.join("scan.png");
    ffmpeg(
        &dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=3000x2000,noise=alls=60:allf=u",
            "-frames:v",
            "1",
            &png.to_string_lossy(),
        ],
    );
    let info = svc.media_inspect(&png).await.unwrap();
    let done = svc
        .media_shrink(
            &info,
            400_000,
            &prism_video::media::output_path(&png, "jpg", Some("smaller")),
            &|_| {},
            &cancel,
        )
        .await
        .unwrap();
    assert!(done.size <= 400_000, "{}", done.size);
    assert_eq!(probe(&dir, &done.path, "v").0, "mjpeg");

    // Already under the limit: nothing is made.
    let small = svc
        .media_shrink(&info, 100_000_000, &w.join("x.jpg"), &|_| {}, &cancel)
        .await;
    assert!(matches!(
        small,
        Err(VideoError::Media(MediaProblem::AlreadySmall))
    ));
    no_temp_files(w);
}

#[tokio::test]
async fn stopping_leaves_nothing_behind() {
    let Some((svc, dir, work)) = setup() else {
        return;
    };
    let w = work.path();
    let clip = w.join("long.mp4");
    make_video(&dir, &clip, "20", "1280x720", "4M");
    let info = svc.media_inspect(&clip).await.unwrap();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        stop.cancel();
    });
    let out = prism_video::media::output_path(&clip, "webm", None);
    let r = svc
        .media_convert(&info, Target::Webm, Quality::High, &out, &|_| {}, &cancel)
        .await;
    assert!(matches!(r, Err(VideoError::Cancelled)), "{r:?}");
    assert!(!out.exists());
    no_temp_files(w);
}

#[test]
fn zips_next_to_the_first_file() {
    let work = tempfile::tempdir().unwrap();
    let a = work.path().join("a.txt");
    std::fs::write(&a, "a").unwrap();
    let out = pack::zip_path(std::slice::from_ref(&a)).unwrap();
    let (path, size) = pack::pack_zip(&[a], &out, &|_| {}, &CancellationToken::new()).unwrap();
    assert_eq!(path, work.path().join("a.zip"));
    assert!(size > 0);
    // The argument builders are reachable from outside the crate.
    let _ = convert::targets_for(&Default::default());
}

/// A real iPhone HEIC photo (stored as tiles, turned by its rotation): set `PRISM_TEST_HEIC`
/// to its path and run with `--ignored`. It's copied into a temp folder first.
#[tokio::test]
#[ignore]
async fn heic_photo_from_a_phone() {
    let Some(src) = std::env::var_os("PRISM_TEST_HEIC").map(PathBuf::from) else {
        return;
    };
    let Some((svc, dir, work)) = setup() else {
        return;
    };
    let photo = work.path().join("photo.heic");
    std::fs::copy(&src, &photo).unwrap();
    let info = svc.media_inspect(&photo).await.unwrap();
    assert_eq!(info.kind, MediaKind::Image, "{info:?}");
    assert_eq!(info.problem, None);
    let (w, h) = (info.width.unwrap(), info.height.unwrap());
    let cancel = CancellationToken::new();
    let jpg = svc
        .media_convert(
            &info,
            Target::Jpg,
            Quality::Standard,
            &work.path().join("photo.jpg"),
            &|_| {},
            &cancel,
        )
        .await
        .unwrap();
    let out = svc.media_inspect(&jpg).await.unwrap();
    // The JPEG is the whole picture, the right way up.
    assert_eq!((out.width, out.height), (Some(w), Some(h)));
    assert_eq!(probe(&dir, &jpg, "v").0, "mjpeg");
    let small = svc
        .media_shrink(
            &info,
            500_000,
            &work.path().join("photo (smaller).jpg"),
            &|_| {},
            &cancel,
        )
        .await
        .unwrap();
    assert!(small.size <= 500_000);
}
