//! ffmpeg post-processing for the accelerated path: mux video+audio (`-c copy`),
//! embed subtitles / thumbnail / metadata / chapters, and audio-only conversions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, VideoError};
use crate::model::{AudioFormat, Chapter, Container};
use crate::plan::StreamRole;
use crate::process::{self, Spawned};
use crate::progress::{FfmpegProgress, Stage, YtProgress};
use crate::tools::Tool;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostInput {
    pub role: StreamRole,
    /// Suggested temp file name (matches the corresponding `StreamSpec::file_name`).
    pub file_name: String,
    /// Filled by the core once the stream is downloaded.
    pub path: Option<PathBuf>,
    /// Subtitle language (`en`, `cs` ...).
    pub language: Option<String>,
    /// Subtitle track name (`English`).
    pub name: Option<String>,
    /// Codec as reported by yt-dlp (`mp4a.40.2`, `opus` ...).
    pub codec: Option<String>,
    pub ext: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioConversion {
    pub format: AudioFormat,
    /// Target bitrate for lossy encodes (`None` = format default / VBR best).
    pub bitrate_kbps: Option<u32>,
    /// Source audio codec, used to decide between stream copy and re-encode.
    pub source_codec: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub title: Option<String>,
    /// Uploader.
    pub artist: Option<String>,
    /// `YYYYMMDD`.
    pub date: Option<String>,
    /// Webpage URL.
    pub comment: Option<String>,
    /// Trimmed description.
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SponsorBlockRequest {
    pub video_id: String,
    pub categories: Vec<String>,
}

/// Everything ffmpeg needs to turn the downloaded streams into the final file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostProcess {
    pub inputs: Vec<PostInput>,
    pub output_path: PathBuf,
    /// Container for video outputs (ignored for audio conversions).
    pub container: Container,
    /// Set for audio-only outputs.
    pub audio_conversion: Option<AudioConversion>,
    pub metadata: Option<Metadata>,
    pub chapters: Vec<Chapter>,
    pub embed_subtitles: bool,
    pub embed_thumbnail: bool,
    pub embed_chapters: bool,
    /// Keep subtitle files next to the output (`<stem>.<lang>.<ext>`).
    pub write_subtitles: bool,
    /// Keep the thumbnail next to the output (`<stem>.jpg`).
    pub write_thumbnail: bool,
    /// Fetch SponsorBlock segments and add them as chapters.
    pub sponsorblock: Option<SponsorBlockRequest>,
    /// Seconds; used for progress.
    pub duration: Option<f64>,
    /// Delete the input files after success.
    pub delete_inputs: bool,
    /// Only a part of the video, cut out of the downloaded pieces.
    #[serde(default)]
    pub cut: Option<Cut>,
}

/// A part of the video cut out of streams that were downloaded only around it: each starts on
/// the keyframe before the part, and ffmpeg starts the output on the exact frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cut {
    /// Seconds into the video where the part starts.
    pub start: f64,
    /// Seconds.
    pub length: f64,
    /// Seconds from the start of the main input (the video, or the audio source of an audio
    /// output) to the part; set by the core once it knows where the pieces start.
    #[serde(default)]
    pub main_offset: f64,
    /// The same for the separate audio input.
    #[serde(default)]
    pub audio_offset: f64,
}

impl PostProcess {
    /// Point every input at `dir/<file_name>`.
    pub fn set_input_dir(&mut self, dir: &Path) {
        for i in &mut self.inputs {
            i.path = Some(dir.join(&i.file_name));
        }
    }

    pub fn input(&self, role: StreamRole) -> Option<&PostInput> {
        self.inputs.iter().find(|i| i.role == role)
    }

    pub fn is_audio_only(&self) -> bool {
        self.audio_conversion.is_some()
    }
}

/// Concrete, already-prepared input files for one ffmpeg run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FfmpegInputs {
    /// Video (or muxed) stream; for audio conversions, the audio source.
    pub main: PathBuf,
    /// Separate audio stream for video outputs.
    pub audio: Option<PathBuf>,
    /// (path, language, title)
    pub subtitles: Vec<(PathBuf, Option<String>, Option<String>)>,
    /// JPEG/PNG cover image.
    pub thumbnail: Option<PathBuf>,
    /// FFMETADATA file (global metadata and/or chapters).
    pub ffmetadata: Option<PathBuf>,
    /// For a part ([`PostProcess::cut`]): the arguments that encode the video again, so it
    /// starts on the exact frame (see [`cut_encoder`]). Empty: the video is copied.
    pub video_encoder: Vec<String>,
}

// ---------------------------------------------------------------------------
// FFMETADATA
// ---------------------------------------------------------------------------

fn escape_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '=' | ';' | '#' | '\\' | '\n' => {
                out.push('\\');
                out.push(c);
            }
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Generate an FFMETADATA1 document with global tags and chapters.
pub fn ffmetadata(meta: Option<&Metadata>, chapters: &[Chapter]) -> String {
    let mut s = String::from(";FFMETADATA1\n");
    if let Some(m) = meta {
        let tags = [
            ("title", &m.title),
            ("artist", &m.artist),
            ("date", &m.date),
            ("comment", &m.comment),
            ("description", &m.description),
            ("synopsis", &m.description),
        ];
        for (k, v) in tags {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                s.push_str(&format!("{k}={}\n", escape_meta(v)));
            }
        }
    }
    for c in chapters {
        let start = (c.start * 1000.0).round().max(0.0) as u64;
        let end = (c.end * 1000.0).round().max(0.0) as u64;
        if end <= start {
            continue;
        }
        s.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
        s.push_str(&format!("START={start}\nEND={end}\n"));
        // Always write the title line: without it ffmpeg's MP4 chapter track makes an
        // untitled chapter inherit the next chapter's title.
        s.push_str(&format!("title={}\n", escape_meta(&c.title)));
    }
    s
}

/// ISO 639-1 → ISO 639-2/B (what the MP4 muxer expects); 3-letter codes pass through.
pub fn iso639_2(lang: &str) -> String {
    let base = lang
        .split(['-', '_'])
        .next()
        .unwrap_or(lang)
        .to_ascii_lowercase();
    let code = match base.as_str() {
        "en" => "eng",
        "de" => "ger",
        "fr" => "fre",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "ru" => "rus",
        "ja" => "jpn",
        "ko" => "kor",
        "zh" => "chi",
        "cs" => "cze",
        "sk" => "slo",
        "pl" => "pol",
        "nl" => "dut",
        "sv" => "swe",
        "no" | "nb" | "nn" => "nor",
        "da" => "dan",
        "fi" => "fin",
        "hu" => "hun",
        "tr" => "tur",
        "ar" => "ara",
        "he" | "iw" => "heb",
        "hi" => "hin",
        "uk" => "ukr",
        "el" => "gre",
        "ro" => "rum",
        "bg" => "bul",
        "hr" => "hrv",
        "sr" => "srp",
        "vi" => "vie",
        "th" => "tha",
        "id" => "ind",
        "ms" => "may",
        "fa" => "per",
        "lt" => "lit",
        "lv" => "lav",
        "et" => "est",
        "sl" => "slv",
        "ca" => "cat",
        "eu" => "baq",
        "gl" => "glg",
        "bn" => "ben",
        "ta" => "tam",
        "te" => "tel",
        "ur" => "urd",
        "fil" | "tl" => "fil",
        b if b.len() == 3 && b.chars().all(|c| c.is_ascii_alphabetic()) => return b.to_string(),
        _ => "und",
    };
    code.to_string()
}

fn is_aac(codec: Option<&str>) -> bool {
    codec.is_some_and(|c| c.starts_with("mp4a.40") || c == "aac")
}

fn is_opus(codec: Option<&str>) -> bool {
    codec.is_some_and(|c| c.starts_with("opus"))
}

fn s(v: impl Into<String>) -> String {
    v.into()
}

/// `-ss <offset> -t <length>` before an input a part is cut out of (nothing for a whole video).
fn cut_input(cut: Option<&Cut>, offset: impl Fn(&Cut) -> f64) -> Vec<String> {
    cut.map(|c| {
        vec![
            s("-ss"),
            format!("{:.3}", offset(c).max(0.0)),
            s("-t"),
            format!("{:.3}", c.length),
        ]
    })
    .unwrap_or_default()
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// Build the ffmpeg command line for `job`. Pure function (unit tested).
pub fn build_ffmpeg_args(job: &PostProcess, inp: &FfmpegInputs, out_tmp: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        s("-hide_banner"),
        s("-nostdin"),
        s("-y"),
        s("-loglevel"),
        s("error"),
        s("-nostats"),
        s("-progress"),
        s("pipe:1"),
    ];
    let mut idx = 0usize;
    let mut add_input = |a: &mut Vec<String>, p: &Path, pre: &[&str]| {
        a.extend(pre.iter().map(|x| x.to_string()));
        a.push(s("-i"));
        a.push(p.to_string_lossy().into_owned());
        idx += 1;
        idx - 1
    };

    let cut = job.cut.as_ref();
    let main_cut = cut_input(cut, |c| c.main_offset);
    let audio_cut = cut_input(cut, |c| c.audio_offset);

    if let Some(conv) = &job.audio_conversion {
        // ---------------- audio-only ----------------
        let src = add_input(&mut a, &inp.main, &strs(&main_cut));
        let thumb_ok = matches!(
            conv.format,
            AudioFormat::Mp3 | AudioFormat::M4a | AudioFormat::Flac
        );
        let thumb = if job.embed_thumbnail && thumb_ok {
            inp.thumbnail.as_ref().map(|t| add_input(&mut a, t, &[]))
        } else {
            None
        };
        let meta = inp
            .ffmetadata
            .as_ref()
            .map(|m| add_input(&mut a, m, &["-f", "ffmetadata"]));
        a.extend([s("-map"), format!("{src}:a:0")]);
        if let Some(t) = thumb {
            a.extend([
                s("-map"),
                format!("{t}:v:0"),
                s("-c:v"),
                s("copy"),
                s("-disposition:v:0"),
                s("attached_pic"),
            ]);
            if conv.format == AudioFormat::Mp3 {
                a.extend([
                    s("-metadata:s:v"),
                    s("title=Album cover"),
                    s("-metadata:s:v"),
                    s("comment=Cover (front)"),
                ]);
            }
        }
        let src_codec = conv.source_codec.as_deref();
        match conv.format {
            AudioFormat::Mp3 => {
                a.extend([s("-c:a"), s("libmp3lame")]);
                match conv.bitrate_kbps {
                    Some(b) => a.extend([s("-b:a"), format!("{b}k")]),
                    None => a.extend([s("-q:a"), s("0")]),
                }
                a.extend([s("-id3v2_version"), s("3")]);
            }
            AudioFormat::M4a => {
                if is_aac(src_codec) && conv.bitrate_kbps.is_none() {
                    a.extend([s("-c:a"), s("copy")]);
                } else {
                    a.extend([
                        s("-c:a"),
                        s("aac"),
                        s("-b:a"),
                        format!("{}k", conv.bitrate_kbps.unwrap_or(256)),
                    ]);
                }
                a.extend([s("-movflags"), s("+faststart")]);
            }
            AudioFormat::Opus => {
                if is_opus(src_codec) && conv.bitrate_kbps.is_none() {
                    a.extend([s("-c:a"), s("copy")]);
                } else {
                    a.extend([
                        s("-c:a"),
                        s("libopus"),
                        s("-b:a"),
                        format!("{}k", conv.bitrate_kbps.unwrap_or(160)),
                    ]);
                }
            }
            AudioFormat::Flac => a.extend([s("-c:a"), s("flac")]),
            AudioFormat::Wav => a.extend([s("-c:a"), s("pcm_s16le")]),
        }
        push_meta_maps(&mut a, job, meta, conv.format != AudioFormat::Wav);
        let fmt = match conv.format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::M4a => "ipod",
            AudioFormat::Opus => "opus",
            AudioFormat::Flac => "flac",
            AudioFormat::Wav => "wav",
        };
        a.extend([s("-f"), s(fmt), out_tmp.to_string_lossy().into_owned()]);
        return a;
    }

    // ---------------- video ----------------
    let container = job.container;
    let v = add_input(&mut a, &inp.main, &strs(&main_cut));
    let au = inp
        .audio
        .as_ref()
        .map(|p| add_input(&mut a, p, &strs(&audio_cut)));
    let subs: Vec<(usize, Option<String>, Option<String>)> = if job.embed_subtitles {
        inp.subtitles
            .iter()
            .map(|(p, lang, title)| (add_input(&mut a, p, &[]), lang.clone(), title.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let thumb_input = if job.embed_thumbnail && container == Container::Mp4 {
        inp.thumbnail.as_ref().map(|t| add_input(&mut a, t, &[]))
    } else {
        None
    };
    let meta = inp
        .ffmetadata
        .as_ref()
        .map(|m| add_input(&mut a, m, &["-f", "ffmetadata"]));

    a.extend([s("-map"), format!("{v}:v:0")]);
    match au {
        Some(i) => a.extend([s("-map"), format!("{i}:a:0")]),
        None => a.extend([s("-map"), format!("{v}:a?")]),
    }
    for (i, _, _) in &subs {
        a.extend([s("-map"), format!("{i}:0")]);
    }
    if let Some(t) = thumb_input {
        a.extend([s("-map"), format!("{t}:0")]);
    }
    a.extend([s("-c"), s("copy")]);
    if cut.is_some() {
        // The part starts on the exact frame: its video is encoded again (audio frames last a
        // few milliseconds, so copying them is exact already).
        a.extend(inp.video_encoder.iter().cloned());
    }
    if !subs.is_empty() {
        let codec = match container {
            Container::Mp4 => "mov_text",
            Container::Mkv => "copy",
            Container::Webm => "webvtt",
        };
        a.extend([s("-c:s"), s(codec)]);
        for (k, (_, lang, title)) in subs.iter().enumerate() {
            if let Some(l) = lang {
                a.extend([
                    format!("-metadata:s:s:{k}"),
                    format!("language={}", iso639_2(l)),
                ]);
            }
            if let Some(t) = title {
                a.extend([format!("-metadata:s:s:{k}"), format!("title={t}")]);
            }
        }
    }
    if thumb_input.is_some() {
        a.extend([s("-disposition:v:1"), s("attached_pic")]);
    }
    if job.embed_thumbnail
        && container == Container::Mkv
        && let Some(t) = &inp.thumbnail
    {
        let png = t.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
        let (mime, name) = if png {
            ("image/png", "cover.png")
        } else {
            ("image/jpeg", "cover.jpg")
        };
        a.extend([
            s("-attach"),
            t.to_string_lossy().into_owned(),
            s("-metadata:s:t:0"),
            format!("mimetype={mime}"),
            s("-metadata:s:t:0"),
            format!("filename={name}"),
        ]);
    }
    push_meta_maps(&mut a, job, meta, true);
    if container == Container::Mp4 {
        a.extend([s("-movflags"), s("+faststart")]);
    }
    let fmt = match container {
        Container::Mp4 => "mp4",
        Container::Mkv => "matroska",
        Container::Webm => "webm",
    };
    a.extend([s("-f"), s(fmt), out_tmp.to_string_lossy().into_owned()]);
    a
}

fn push_meta_maps(
    a: &mut Vec<String>,
    job: &PostProcess,
    meta: Option<usize>,
    chapters_supported: bool,
) {
    let want_chapters = job.embed_chapters && chapters_supported && !job.chapters.is_empty();
    match meta {
        Some(m) if job.metadata.is_some() => a.extend([s("-map_metadata"), m.to_string()]),
        _ => {}
    }
    match meta {
        Some(m) if want_chapters => a.extend([s("-map_chapters"), m.to_string()]),
        _ => a.extend([s("-map_chapters"), s("-1")]),
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn path_of(input: &PostInput) -> Result<PathBuf> {
    let p = input
        .path
        .clone()
        .ok_or_else(|| VideoError::MissingInput(PathBuf::from(&input.file_name)))?;
    if !p.is_file() {
        return Err(VideoError::MissingInput(p));
    }
    Ok(p)
}

fn with_suffix(output: &Path, suffix: &str) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    output.with_file_name(format!("{stem}{suffix}"))
}

/// Run ffmpeg with `args`, reporting `-progress` updates, killing it on cancel.
pub(crate) async fn run_ffmpeg(
    ffmpeg: &Path,
    args: &[String],
    duration: Option<f64>,
    stage: Stage,
    progress: Option<&UnboundedSender<YtProgress>>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut cmd = process::command(ffmpeg);
    cmd.args(args);
    let mut sp = Spawned::spawn(&mut cmd)?;
    let stdout = sp.child.stdout.take().expect("piped");
    let stderr = sp.child.stderr.take().expect("piped");
    let err_task = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut tail: Vec<String> = Vec::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if !l.trim().is_empty() {
                tail.push(l);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        }
        tail.join("\n")
    });
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut state = FfmpegProgress::default();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                sp.kill_tree().await;
                return Err(VideoError::Cancelled);
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if state.feed(&l)
                            && let Some(tx) = progress {
                                let _ = tx.send(YtProgress {
                                    stage,
                                    percent: state.percent(duration),
                                    status: Some(if state.ended { "finished".into() } else { "processing".into() }),
                                    postprocessor: Some("ffmpeg".into()),
                                    ..Default::default()
                                });
                            }
                    }
                    _ => break,
                }
            }
        }
    }
    let status = tokio::select! {
            biased;
        _ = cancel.cancelled() => {
            sp.kill_tree().await;
            return Err(VideoError::Cancelled);
        }
        st = sp.child.wait() => st?,
    };
    let stderr_tail = err_task.await.unwrap_or_default();
    if !status.success() {
        return Err(VideoError::ToolFailed {
            tool: Tool::Ffmpeg,
            code: status.code(),
            message: stderr_tail,
        });
    }
    Ok(())
}

/// Convert a thumbnail to JPEG when it is not already JPEG/PNG (e.g. WebP).
pub(crate) async fn prepare_thumbnail(
    ffmpeg: &Path,
    src: &Path,
    cancel: &CancellationToken,
) -> Result<(PathBuf, bool)> {
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
        return Ok((src.to_path_buf(), false));
    }
    let out = with_suffix(src, ".cover.jpg");
    let args = vec![
        s("-hide_banner"),
        s("-nostdin"),
        s("-y"),
        s("-loglevel"),
        s("error"),
        s("-i"),
        src.to_string_lossy().into_owned(),
        s("-frames:v"),
        s("1"),
        s("-update"),
        s("1"),
        s("-q:v"),
        s("2"),
        out.to_string_lossy().into_owned(),
    ];
    run_ffmpeg(ffmpeg, &args, None, Stage::EmbeddingThumbnail, None, cancel).await?;
    Ok((out, true))
}

/// How a part's video is encoded again so that it starts on the exact frame: with x264 (VP9 for
/// WebM), set for speed at a quality close to the source. With neither in this ffmpeg, nothing:
/// the video is copied and starts on the keyframe just before the part.
pub(crate) async fn cut_encoder(ffmpeg: &Path, container: Container) -> Vec<String> {
    let list = crate::media::encoders(ffmpeg).await;
    let has = |name: &str| {
        list.as_deref()
            .is_some_and(|l| crate::media::has_encoder(l, name))
    };
    let args: &[&str] = match container {
        Container::Webm if has("libvpx-vp9") => &[
            "-c:v:0",
            "libvpx-vp9",
            "-deadline:v:0",
            "realtime",
            "-cpu-used:v:0",
            "8",
            "-row-mt:v:0",
            "1",
            "-crf:v:0",
            "32",
            "-b:v:0",
            "0",
        ],
        Container::Mp4 | Container::Mkv if has("libx264") => &[
            "-c:v:0",
            "libx264",
            "-preset:v:0",
            "veryfast",
            "-crf:v:0",
            "20",
        ],
        _ => &[],
    };
    args.iter().map(|x| x.to_string()).collect()
}

/// Full post-processing run. `client` is used for SponsorBlock.
pub(crate) async fn postprocess(
    ffmpeg: &Path,
    client: &reqwest::Client,
    job: &PostProcess,
    progress: Option<&UnboundedSender<YtProgress>>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let audio_only = job.is_audio_only();
    let main = if audio_only {
        job.input(StreamRole::Audio)
            .or_else(|| job.input(StreamRole::Muxed))
            .or_else(|| job.input(StreamRole::Video))
    } else {
        job.input(StreamRole::Video)
            .or_else(|| job.input(StreamRole::Muxed))
    }
    .ok_or_else(|| VideoError::Other("post-process job has no main input".into()))?;
    let main_path = path_of(main)?;
    let audio_path = if audio_only || main.role == StreamRole::Muxed {
        None
    } else {
        job.input(StreamRole::Audio).map(path_of).transpose()?
    };

    let mut temps: Vec<PathBuf> = Vec::new();

    // Subtitles (optional: a missing subtitle file is skipped, not fatal).
    let subtitles: Vec<(PathBuf, Option<String>, Option<String>)> = job
        .inputs
        .iter()
        .filter(|i| i.role == StreamRole::Subtitle)
        .filter_map(|i| {
            i.path
                .clone()
                .filter(|p| p.is_file())
                .map(|p| (p, i.language.clone(), i.name.clone()))
        })
        .collect();

    // Thumbnail (optional).
    let mut thumbnail = None;
    if (job.embed_thumbnail || job.write_thumbnail)
        && let Some(t) = job
            .input(StreamRole::Thumbnail)
            .and_then(|i| i.path.clone())
            .filter(|p| p.is_file())
    {
        match prepare_thumbnail(ffmpeg, &t, cancel).await {
            Ok((p, converted)) => {
                if converted {
                    temps.push(p.clone());
                }
                thumbnail = Some(p);
            }
            Err(VideoError::Cancelled) => return Err(VideoError::Cancelled),
            Err(e) => tracing::warn!("thumbnail conversion failed, skipping: {e}"),
        }
    }

    // Chapters (+ SponsorBlock).
    let mut chapters = if job.embed_chapters {
        job.chapters.clone()
    } else {
        Vec::new()
    };
    if let Some(sb) = &job.sponsorblock {
        match crate::sponsorblock::fetch_segments(client, &sb.video_id, &sb.categories).await {
            Ok(segs) => {
                chapters = crate::sponsorblock::merge_into_chapters(&chapters, &segs, job.duration)
            }
            Err(e) => tracing::warn!("SponsorBlock lookup failed, continuing without: {e}"),
        }
    }
    let mut job_eff = job.clone();
    job_eff.chapters = chapters;
    if job.sponsorblock.is_some() {
        job_eff.embed_chapters = true;
    }

    let out_dir = job
        .output_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    if !out_dir.as_os_str().is_empty() {
        tokio::fs::create_dir_all(&out_dir).await?;
    }
    let ext = job
        .output_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out_tmp = with_suffix(&job.output_path, &format!(".prismtmp.{ext}"));

    let ffmeta =
        if job_eff.metadata.is_some() || (job_eff.embed_chapters && !job_eff.chapters.is_empty()) {
            let p = with_suffix(&job.output_path, ".prismmeta.txt");
            tokio::fs::write(&p, ffmetadata(job_eff.metadata.as_ref(), &job_eff.chapters)).await?;
            temps.push(p.clone());
            Some(p)
        } else {
            None
        };

    let inputs = FfmpegInputs {
        main: main_path,
        audio: audio_path,
        subtitles: subtitles.clone(),
        thumbnail: thumbnail.clone(),
        ffmetadata: ffmeta,
        video_encoder: match &job.cut {
            Some(_) if !audio_only => cut_encoder(ffmpeg, job.container).await,
            _ => Vec::new(),
        },
    };
    let args = build_ffmpeg_args(&job_eff, &inputs, &out_tmp);
    let stage = if job.cut.is_some() {
        Stage::Cutting
    } else if audio_only {
        Stage::ExtractingAudio
    } else {
        Stage::Merging
    };
    let res = run_ffmpeg(ffmpeg, &args, job.duration, stage, progress, cancel).await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&out_tmp).await;
        for t in &temps {
            let _ = tokio::fs::remove_file(t).await;
        }
        return Err(e);
    }
    crate::tools::atomic_replace(&out_tmp, &job.output_path)?;

    // Side files.
    if job.write_subtitles {
        for (p, lang, _) in &subtitles {
            let sext = p
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| "vtt".into());
            let dest = with_suffix(
                &job.output_path,
                &format!(".{}.{sext}", lang.as_deref().unwrap_or("und")),
            );
            let _ = tokio::fs::copy(p, &dest).await;
        }
    }
    if job.write_thumbnail
        && let Some(t) = &thumbnail
    {
        let text = t
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| "jpg".into());
        let _ = tokio::fs::copy(t, with_suffix(&job.output_path, &format!(".{text}"))).await;
    }

    for t in &temps {
        let _ = tokio::fs::remove_file(t).await;
    }
    if job.delete_inputs {
        for i in &job.inputs {
            if let Some(p) = &i.path
                && p != &job.output_path
            {
                let _ = tokio::fs::remove_file(p).await;
            }
        }
    }
    if let Some(tx) = progress {
        let _ = tx.send(YtProgress {
            stage: Stage::Finished,
            percent: Some(100.0),
            filename: Some(job.output_path.to_string_lossy().into_owned()),
            ..Default::default()
        });
    }
    Ok(job.output_path.clone())
}
