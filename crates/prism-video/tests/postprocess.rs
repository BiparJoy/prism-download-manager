//! ffmpeg argument building (pure) and real post-processing runs on locally
//! generated test media (requires `ffmpeg`/`ffprobe` on PATH; skipped otherwise).

use std::path::{Path, PathBuf};
use std::process::Command;

use prism_video::ffmpeg::{
    AudioConversion, Cut, FfmpegInputs, Metadata, PostInput, PostProcess, build_ffmpeg_args,
};
use prism_video::tools::which;
use prism_video::{
    AudioFormat, CancellationToken, Chapter, Container, StreamRole, VideoError, VideoOptions,
    VideoService,
};

fn base_job(output: &Path) -> PostProcess {
    PostProcess {
        inputs: vec![],
        output_path: output.to_path_buf(),
        container: Container::Mp4,
        audio_conversion: None,
        metadata: Some(Metadata {
            title: Some("Prism Test: \"Video\"".into()),
            artist: Some("Prism Uploader".into()),
            date: Some("20260901".into()),
            comment: Some("https://example.com/watch?v=1".into()),
            description: Some("Line 1\nLine 2 = ok".into()),
        }),
        chapters: vec![
            Chapter {
                start: 0.0,
                end: 2.0,
                title: "Opening".into(),
            },
            Chapter {
                start: 2.0,
                end: 5.0,
                title: "Rest".into(),
            },
        ],
        embed_subtitles: true,
        embed_thumbnail: true,
        embed_chapters: true,
        write_subtitles: false,
        write_thumbnail: false,
        sponsorblock: None,
        duration: Some(5.0),
        delete_inputs: false,
        cut: None,
    }
}

fn inp(role: StreamRole, path: &Path, lang: Option<&str>, codec: Option<&str>) -> PostInput {
    PostInput {
        role,
        file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path: Some(path.to_path_buf()),
        language: lang.map(str::to_string),
        name: lang.map(|_| "English".to_string()),
        codec: codec.map(str::to_string),
        ext: path.extension().unwrap().to_string_lossy().into_owned(),
    }
}

#[test]
fn args_video_mp4() {
    let job = base_job(Path::new("out/v.mp4"));
    let inputs = FfmpegInputs {
        main: "v.mp4".into(),
        audio: Some("a.m4a".into()),
        subtitles: vec![("s.vtt".into(), Some("en".into()), Some("English".into()))],
        thumbnail: Some("t.jpg".into()),
        ffmetadata: Some("m.txt".into()),
        ..Default::default()
    };
    let a = build_ffmpeg_args(&job, &inputs, Path::new("out/v.prismtmp.mp4")).join(" ");
    assert_eq!(
        a,
        "-hide_banner -nostdin -y -loglevel error -nostats -progress pipe:1 \
-i v.mp4 -i a.m4a -i s.vtt -i t.jpg -f ffmetadata -i m.txt \
-map 0:v:0 -map 1:a:0 -map 2:0 -map 3:0 -c copy -c:s mov_text \
-metadata:s:s:0 language=eng -metadata:s:s:0 title=English -disposition:v:1 attached_pic \
-map_metadata 4 -map_chapters 4 -movflags +faststart -f mp4 out/v.prismtmp.mp4"
    );
}

#[test]
fn args_video_mkv_and_muxed() {
    let mut job = base_job(Path::new("v.mkv"));
    job.container = Container::Mkv;
    job.metadata = None;
    job.embed_chapters = false;
    let inputs = FfmpegInputs {
        main: "v.webm".into(),
        thumbnail: Some("t.png".into()),
        ..Default::default()
    };
    let a = build_ffmpeg_args(&job, &inputs, Path::new("v.tmp.mkv")).join(" ");
    assert!(a.contains("-i v.webm -map 0:v:0 -map 0:a? -c copy"), "{a}");
    assert!(
        a.contains(
            "-attach t.png -metadata:s:t:0 mimetype=image/png -metadata:s:t:0 filename=cover.png"
        ),
        "{a}"
    );
    assert!(a.contains("-map_chapters -1 -f matroska v.tmp.mkv"), "{a}");
    assert!(!a.contains("-map_metadata"));
}

#[test]
fn args_part_cut() {
    let mut job = base_job(Path::new("p.mp4"));
    job.metadata = None;
    job.embed_chapters = false;
    job.cut = Some(Cut {
        start: 83.0,
        length: 82.0,
        main_offset: 1.25,
        audio_offset: 0.5,
    });
    let inputs = FfmpegInputs {
        main: "v.mp4".into(),
        audio: Some("a.m4a".into()),
        thumbnail: Some("t.jpg".into()),
        video_encoder: vec!["-c:v:0".into(), "libx264".into()],
        ..Default::default()
    };
    let a = build_ffmpeg_args(&job, &inputs, Path::new("p.tmp.mp4")).join(" ");
    // Each stream is read from its own offset for the part's length; the video (not the cover)
    // is encoded again, the audio copied.
    assert!(
        a.contains(
            "-ss 1.250 -t 82.000 -i v.mp4 -ss 0.500 -t 82.000 -i a.m4a -i t.jpg \
             -map 0:v:0 -map 1:a:0 -map 2:0 -c copy -c:v:0 libx264 -disposition:v:1 attached_pic"
        ),
        "{a}"
    );
    // The same streams without a cut: no seeking, nothing encoded.
    job.cut = None;
    let a = build_ffmpeg_args(&job, &inputs, Path::new("p.tmp.mp4")).join(" ");
    assert!(!a.contains("-ss") && !a.contains("libx264"), "{a}");

    // An audio part: the audio source is the main input.
    job.cut = Some(Cut {
        start: 10.0,
        length: 5.5,
        main_offset: 0.75,
        audio_offset: 0.0,
    });
    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Mp3,
        bitrate_kbps: Some(192),
        source_codec: Some("opus".into()),
    });
    let inputs = FfmpegInputs {
        main: "a.webm".into(),
        ..Default::default()
    };
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.mp3")).join(" ");
    assert!(a.contains("-ss 0.750 -t 5.500 -i a.webm -map 0:a:0"), "{a}");
}

#[test]
fn args_audio_conversions() {
    let mut job = base_job(Path::new("a.mp3"));
    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Mp3,
        bitrate_kbps: Some(320),
        source_codec: Some("opus".into()),
    });
    let inputs = FfmpegInputs {
        main: "a.webm".into(),
        thumbnail: Some("t.jpg".into()),
        ffmetadata: Some("m.txt".into()),
        ..Default::default()
    };
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.mp3")).join(" ");
    assert!(a.contains("-i a.webm -i t.jpg -f ffmetadata -i m.txt -map 0:a:0 -map 1:v:0 -c:v copy -disposition:v:0 attached_pic"), "{a}");
    assert!(
        a.contains(
            "-c:a libmp3lame -b:a 320k -id3v2_version 3 -map_metadata 2 -map_chapters 2 -f mp3"
        ),
        "{a}"
    );

    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::M4a,
        bitrate_kbps: None,
        source_codec: Some("mp4a.40.2".into()),
    });
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.m4a")).join(" ");
    assert!(a.contains("-c:a copy -movflags +faststart"), "{a}");
    assert!(a.contains("-f ipod"));

    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::M4a,
        bitrate_kbps: None,
        source_codec: Some("opus".into()),
    });
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.m4a")).join(" ");
    assert!(a.contains("-c:a aac -b:a 256k"), "{a}");

    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Opus,
        bitrate_kbps: None,
        source_codec: Some("opus".into()),
    });
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.opus")).join(" ");
    assert!(a.contains("-c:a copy") && !a.contains("t.jpg"), "{a}");

    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Flac,
        bitrate_kbps: None,
        source_codec: None,
    });
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.flac")).join(" ");
    assert!(a.contains("-c:a flac") && a.contains("-f flac"), "{a}");

    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Wav,
        bitrate_kbps: None,
        source_codec: None,
    });
    let a = build_ffmpeg_args(&job, &inputs, Path::new("a.tmp.wav")).join(" ");
    assert!(
        a.contains("-c:a pcm_s16le -map_metadata 1 -map_chapters -1 -f wav"),
        "{a}"
    );
}

// ----------------------------------------------------------------------------
// Real ffmpeg runs
// ----------------------------------------------------------------------------

fn have_ffmpeg() -> bool {
    let exe = std::env::consts::EXE_SUFFIX;
    which(&format!("ffmpeg{exe}")).is_some() && which(&format!("ffprobe{exe}")).is_some()
}

fn ff(args: &[&str]) {
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

fn probe(path: &Path) -> serde_json::Value {
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
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn tag<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    let tags = v["format"]["tags"].as_object()?;
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
}

struct Media {
    dir: tempfile::TempDir,
    video: PathBuf,
    audio: PathBuf,
    subs: PathBuf,
    thumb: PathBuf,
}

fn make_media() -> Media {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("in.f137.mp4");
    let audio = dir.path().join("in.f140.m4a");
    let subs = dir.path().join("in.en.vtt");
    let thumb = dir.path().join("in.thumb.webp");
    ff(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=25:duration=5",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-an",
        video.to_str().unwrap(),
    ]);
    ff(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=5",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        audio.to_str().unwrap(),
    ]);
    // A real WebP like yt-dlp's thumbnails. Every ffmpeg decodes WebP natively, but encoding
    // needs libwebp (Homebrew's ffmpeg has none), so the image is a checked-in fixture, made with
    // `ffmpeg -f lavfi -i testsrc=size=640x360 -frames:v 1 -c:v libwebp -q:v 75 thumbnail.webp`.
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/thumbnail.webp"),
        &thumb,
    )
    .unwrap();
    std::fs::write(&subs, "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello Prism\n\n00:00:02.000 --> 00:00:04.000\nSecond cue\n").unwrap();
    Media {
        dir,
        video,
        audio,
        subs,
        thumb,
    }
}

fn service() -> VideoService {
    let tools = tempfile::tempdir().unwrap();
    VideoService::new(tools.keep(), VideoOptions::default())
}

#[tokio::test]
async fn local_mux_with_subs_thumbnail_metadata_chapters() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH, skipping");
        return;
    }
    let m = make_media();
    let svc = service();
    for container in [Container::Mp4, Container::Mkv] {
        let out = m
            .dir
            .path()
            .join(format!("out/Final ⧸ Video.{}", container.ext()));
        let mut job = base_job(&out);
        job.container = container;
        job.write_subtitles = true;
        job.write_thumbnail = true;
        job.inputs = vec![
            inp(StreamRole::Video, &m.video, None, Some("avc1.64001e")),
            inp(StreamRole::Audio, &m.audio, None, Some("mp4a.40.2")),
            inp(StreamRole::Subtitle, &m.subs, Some("en"), None),
            inp(StreamRole::Thumbnail, &m.thumb, None, None),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let path = svc
            .postprocess(&job, Some(tx), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(path, out);
        let mut last = None;
        while let Ok(p) = rx.try_recv() {
            last = Some(p);
        }
        assert_eq!(last.unwrap().percent, Some(100.0));

        let v = probe(&out);
        let streams = v["streams"].as_array().unwrap();
        let codecs: Vec<&str> = streams
            .iter()
            .map(|s| s["codec_name"].as_str().unwrap_or("?"))
            .collect();
        eprintln!("{container:?}: {codecs:?}");
        assert!(codecs.contains(&"h264") && codecs.contains(&"aac"));
        match container {
            Container::Mp4 => {
                assert!(codecs.contains(&"mov_text"));
                let pic = streams
                    .iter()
                    .find(|s| s["codec_name"] == "mjpeg")
                    .expect("cover art stream");
                assert_eq!(pic["disposition"]["attached_pic"], 1);
                let sub = streams
                    .iter()
                    .find(|s| s["codec_name"] == "mov_text")
                    .unwrap();
                assert_eq!(sub["tags"]["language"], "eng");
            }
            _ => {
                assert!(codecs.contains(&"webvtt"));
                // Matroska image attachments are exposed by ffprobe as attached-pic video streams.
                let att = streams
                    .iter()
                    .find(|s| s["codec_name"] == "mjpeg")
                    .expect("attachment");
                assert_eq!(att["tags"]["mimetype"], "image/jpeg");
                assert_eq!(att["tags"]["filename"], "cover.jpg");
            }
        }
        assert_eq!(tag(&v, "title"), Some("Prism Test: \"Video\""));
        assert_eq!(tag(&v, "artist"), Some("Prism Uploader"));
        assert_eq!(tag(&v, "date"), Some("20260901"));
        assert_eq!(tag(&v, "comment"), Some("https://example.com/watch?v=1"));
        let chapters = v["chapters"].as_array().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0]["tags"]["title"], "Opening");
        // Side files.
        assert!(m.dir.path().join("out/Final ⧸ Video.en.vtt").is_file());
        assert!(m.dir.path().join("out/Final ⧸ Video.jpg").is_file());
        // Inputs kept (delete_inputs = false), temp files gone.
        assert!(m.video.is_file());
        let leftovers: Vec<_> = std::fs::read_dir(m.dir.path().join("out"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("prism"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[tokio::test]
async fn local_audio_conversions() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH, skipping");
        return;
    }
    let m = make_media();
    let svc = service();
    let cases = [
        (AudioFormat::Mp3, Some(320), "mp3", true),
        (AudioFormat::M4a, None, "aac", true),
        (AudioFormat::Opus, None, "opus", false),
        (AudioFormat::Flac, None, "flac", true),
        (AudioFormat::Wav, None, "pcm_s16le", false),
    ];
    for (fmt, kbps, codec, cover) in cases {
        let out = m.dir.path().join(format!("audio/song.{}", fmt.ext()));
        let mut job = base_job(&out);
        job.audio_conversion = Some(AudioConversion {
            format: fmt,
            bitrate_kbps: kbps,
            source_codec: Some("mp4a.40.2".into()),
        });
        job.inputs = vec![
            inp(StreamRole::Audio, &m.audio, None, Some("mp4a.40.2")),
            inp(StreamRole::Thumbnail, &m.thumb, None, None),
        ];
        svc.postprocess(&job, None, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{fmt:?}: {e}"));
        let v = probe(&out);
        let streams = v["streams"].as_array().unwrap();
        let a = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
        assert_eq!(a["codec_name"], codec, "{fmt:?}");
        let has_pic = streams
            .iter()
            .any(|s| s["disposition"]["attached_pic"] == 1);
        assert_eq!(has_pic, cover, "{fmt:?} cover");
        if fmt == AudioFormat::Mp3 {
            let br: u64 = v["format"]["bit_rate"].as_str().unwrap().parse().unwrap();
            assert!(br > 250_000, "mp3 bitrate {br}");
            assert_eq!(tag(&v, "title"), Some("Prism Test: \"Video\""));
            assert_eq!(v["chapters"].as_array().unwrap().len(), 2);
        }
        match fmt {
            AudioFormat::Wav => {}
            // Ogg/Opus keeps Vorbis comments on the stream, not the container.
            AudioFormat::Opus => assert_eq!(
                a["tags"]["artist"]
                    .as_str()
                    .or(a["tags"]["ARTIST"].as_str()),
                Some("Prism Uploader")
            ),
            _ => assert_eq!(tag(&v, "artist"), Some("Prism Uploader"), "{fmt:?}"),
        }
    }
}

#[tokio::test]
async fn local_postprocess_cancel_and_delete_inputs() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH, skipping");
        return;
    }
    let m = make_media();
    let svc = service();
    // Cancel: an already-cancelled token kills ffmpeg right away and leaves no output.
    let out = m.dir.path().join("cancel/x.mp3");
    let mut job = base_job(&out);
    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::Flac,
        bitrate_kbps: None,
        source_codec: None,
    });
    job.inputs = vec![inp(StreamRole::Audio, &m.audio, None, None)];
    let token = CancellationToken::new();
    token.cancel();
    let err = svc.postprocess(&job, None, token).await.unwrap_err();
    assert!(matches!(err, VideoError::Cancelled), "{err}");
    assert!(!out.exists());

    // Missing input.
    let mut bad = job.clone();
    bad.inputs[0].path = Some(m.dir.path().join("nope.m4a"));
    assert!(matches!(
        svc.postprocess(&bad, None, CancellationToken::new()).await,
        Err(VideoError::MissingInput(_))
    ));

    // delete_inputs removes the temp streams after success.
    let copy = m.dir.path().join("copy.m4a");
    std::fs::copy(&m.audio, &copy).unwrap();
    let mut job = base_job(&m.dir.path().join("del/x.m4a"));
    job.audio_conversion = Some(AudioConversion {
        format: AudioFormat::M4a,
        bitrate_kbps: None,
        source_codec: Some("mp4a.40.2".into()),
    });
    job.inputs = vec![inp(StreamRole::Audio, &copy, None, Some("mp4a.40.2"))];
    job.delete_inputs = true;
    svc.postprocess(&job, None, CancellationToken::new())
        .await
        .unwrap();
    assert!(!copy.exists());
}

#[tokio::test]
async fn local_part_is_cut_on_the_exact_frame() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH, skipping");
        return;
    }
    let m = make_media();
    let svc = service();
    let out = m.dir.path().join("part/p.mp4");
    let mut job = base_job(&out);
    job.embed_subtitles = false;
    job.embed_thumbnail = false;
    job.embed_chapters = false;
    job.chapters.clear();
    job.inputs = vec![
        inp(StreamRole::Video, &m.video, None, Some("avc1.64001e")),
        inp(StreamRole::Audio, &m.audio, None, Some("mp4a.40.2")),
    ];
    // The test video has one keyframe, at 0 s; the part starts 1.3 s in and lasts 2 s.
    job.cut = Some(Cut {
        start: 1.3,
        length: 2.0,
        main_offset: 1.3,
        audio_offset: 1.3,
    });
    job.duration = Some(2.0);
    svc.postprocess(&job, None, CancellationToken::new())
        .await
        .unwrap();
    let p = probe(&out);
    let d: f64 = p["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((d - 2.0).abs() < 0.08, "duration {d}");
    // 2 s at 25 fps, starting on the exact frame (a copy would start at the keyframe, 0 s).
    let frames = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&out)
        .output()
        .unwrap();
    let n: u32 = String::from_utf8_lossy(&frames.stdout)
        .trim()
        .parse()
        .unwrap();
    assert_eq!(n, 50);
    let codecs: Vec<&str> = p["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["codec_name"].as_str())
        .collect();
    assert_eq!(codecs, ["h264", "aac"]);
}
