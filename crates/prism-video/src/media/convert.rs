//! Converting a file into another format: which formats make sense for it, and ffmpeg's
//! arguments for each (sensible defaults, a stream copy when the streams already fit).

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{
    MediaInfo, MediaKind, MediaProblem, ProgressFn, base_args, discard, finish_output, path_arg,
    run_mapped, s, temp_output,
};
use crate::error::{Result, VideoError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Mp3,
    M4a,
    Wav,
    Flac,
    Ogg,
    Jpg,
    Png,
    Webp,
    Mp4,
    Webm,
    Gif,
}

impl Target {
    pub fn ext(self) -> &'static str {
        match self {
            Target::Mp3 => "mp3",
            Target::M4a => "m4a",
            Target::Wav => "wav",
            Target::Flac => "flac",
            Target::Ogg => "ogg",
            Target::Jpg => "jpg",
            Target::Png => "png",
            Target::Webp => "webp",
            Target::Mp4 => "mp4",
            Target::Webm => "webm",
            Target::Gif => "gif",
        }
    }

    /// The format a file of this extension already is.
    pub fn of_ext(ext: &str) -> Option<Target> {
        Some(match ext {
            "mp3" => Target::Mp3,
            "m4a" | "m4b" => Target::M4a,
            "wav" => Target::Wav,
            "flac" => Target::Flac,
            "ogg" | "oga" => Target::Ogg,
            "jpg" | "jpeg" | "jfif" => Target::Jpg,
            "png" => Target::Png,
            "webp" => Target::Webp,
            "mp4" | "m4v" => Target::Mp4,
            "webm" => Target::Webm,
            "gif" => Target::Gif,
            _ => return None,
        })
    }

    pub fn is_audio(self) -> bool {
        matches!(
            self,
            Target::Mp3 | Target::M4a | Target::Wav | Target::Flac | Target::Ogg
        )
    }

    pub fn is_image(self) -> bool {
        matches!(self, Target::Jpg | Target::Png | Target::Webp)
    }
}

/// GIFs are for short clips: longer videos aren't offered as one.
pub const GIF_MAX_SECONDS: f64 = 60.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Quality {
    #[default]
    Standard,
    High,
}

/// The formats a file can become, most useful first. Its own format is left out, except an
/// MP4 that isn't H.264 (it becomes one that plays everywhere).
pub fn targets_for(info: &MediaInfo) -> Vec<Target> {
    if info.problem.is_some() {
        return Vec::new();
    }
    let own = Target::of_ext(&info.ext);
    let all: &[Target] = match info.kind {
        MediaKind::Image => &[Target::Jpg, Target::Png, Target::Webp],
        MediaKind::Audio => &[
            Target::Mp3,
            Target::M4a,
            Target::Wav,
            Target::Flac,
            Target::Ogg,
        ],
        MediaKind::Video => &[Target::Mp4, Target::Webm, Target::Gif, Target::Mp3],
        MediaKind::Other => &[],
    };
    all.iter()
        .copied()
        .filter(|t| {
            Some(*t) != own || (*t == Target::Mp4 && info.video_codec.as_deref() != Some("h264"))
        })
        .filter(|t| *t != Target::Mp3 || info.kind != MediaKind::Video || info.has_audio)
        .collect()
}

/// The video stream can go into an MP4 / WebM unchanged.
fn copies_into(target: Target, info: &MediaInfo) -> bool {
    let v = info.video_codec.as_deref().unwrap_or("");
    let a = info.audio_codec.as_deref();
    match target {
        Target::Mp4 => {
            v == "h264"
                && info
                    .pix_fmt
                    .as_deref()
                    .is_none_or(|p| p == "yuv420p" || p == "yuvj420p")
                && a.is_none_or(|a| matches!(a, "aac" | "mp3"))
        }
        Target::Webm => {
            matches!(v, "vp8" | "vp9" | "av1") && a.is_none_or(|a| matches!(a, "opus" | "vorbis"))
        }
        _ => false,
    }
}

/// Long side at most `max` px, never bigger, even sizes (works whichever way the picture is
/// turned, also when ffmpeg turns it by its rotation).
pub(crate) fn fit_long_side(max: u32) -> String {
    format!("scale=w='if(gte(iw,ih),min({max},iw),-2)':h='if(gte(iw,ih),-2,min({max},ih))'")
}

/// Short side at most `max` px (720p, …), never bigger, even sizes.
pub(crate) fn fit_short_side(max: u32) -> String {
    format!("scale=w='if(gte(iw,ih),-2,min({max},iw))':h='if(gte(iw,ih),min({max},ih),-2)'")
}

/// Transparent parts become white (JPEG has no transparency): over white is
/// `255 - (255 - c) · alpha`, i.e. negate, premultiply, negate.
pub(crate) const FLATTEN_ON_WHITE: &str = "format=rgba,negate,premultiply=inplace=1,negate";

/// ffmpeg's arguments for converting `info` into `target`, writing `out`. Pure.
pub fn convert_args(
    info: &MediaInfo,
    target: Target,
    quality: Quality,
    out: &Path,
) -> Result<Vec<String>> {
    let high = quality == Quality::High;
    let mut a = base_args();
    a.extend([s("-i"), path_arg(&info.path)]);
    let vf = |a: &mut Vec<String>, filters: Vec<String>| {
        let f: Vec<String> = filters.into_iter().filter(|f| !f.is_empty()).collect();
        if !f.is_empty() {
            a.extend([s("-vf"), f.join(",")]);
        }
    };
    if target.is_audio() {
        if !info.has_audio {
            return Err(VideoError::Media(MediaProblem::NoAudio));
        }
        a.extend([s("-map"), s("0:a:0")]);
        let cover = info.has_cover && matches!(target, Target::Mp3 | Target::M4a | Target::Flac);
        if cover {
            a.extend([
                s("-map"),
                s("0:v:0"),
                s("-c:v"),
                s("copy"),
                s("-disposition:v:0"),
                s("attached_pic"),
            ]);
        } else {
            a.push(s("-vn"));
        }
        a.extend([s("-sn"), s("-dn")]);
        let src = info.audio_codec.as_deref();
        match target {
            Target::Mp3 => {
                if src == Some("mp3") && !high {
                    a.extend([s("-c:a"), s("copy")]);
                } else if high {
                    a.extend([s("-c:a"), s("libmp3lame"), s("-b:a"), s("320k")]);
                } else {
                    // VBR around 190 kbps.
                    a.extend([s("-c:a"), s("libmp3lame"), s("-q:a"), s("2")]);
                }
                a.extend([s("-id3v2_version"), s("3"), s("-f"), s("mp3")]);
            }
            Target::M4a => {
                if src == Some("aac") {
                    a.extend([s("-c:a"), s("copy")]);
                } else {
                    let b = if high { "256k" } else { "192k" };
                    a.extend([s("-c:a"), s("aac"), s("-b:a"), s(b)]);
                }
                a.extend([s("-movflags"), s("+faststart"), s("-f"), s("ipod")]);
            }
            Target::Wav => a.extend([s("-c:a"), s("pcm_s16le"), s("-f"), s("wav")]),
            Target::Flac => a.extend([s("-c:a"), s("flac"), s("-f"), s("flac")]),
            Target::Ogg => {
                let q = if high { "7" } else { "5" };
                a.extend([
                    s("-c:a"),
                    s("libvorbis"),
                    s("-q:a"),
                    s(q),
                    s("-f"),
                    s("ogg"),
                ]);
            }
            _ => unreachable!("audio targets"),
        }
    } else if target.is_image() {
        let mut f = Vec::new();
        match target {
            Target::Jpg => {
                if info.has_alpha {
                    f.push(s(FLATTEN_ON_WHITE));
                }
                f.push(s("format=yuvj420p"));
                vf(&mut a, f);
                let q = if high { "2" } else { "3" };
                a.extend([s("-frames:v"), s("1"), s("-update"), s("1")]);
                a.extend([s("-c:v"), s("mjpeg"), s("-q:v"), s(q), s("-f"), s("image2")]);
            }
            Target::Png => {
                a.extend([s("-frames:v"), s("1"), s("-update"), s("1")]);
                a.extend([s("-c:v"), s("png"), s("-f"), s("image2")]);
            }
            Target::Webp => {
                let q = if high { "92" } else { "85" };
                a.extend([s("-frames:v"), s("1")]);
                a.extend([
                    s("-c:v"),
                    s("libwebp"),
                    s("-quality"),
                    s(q),
                    s("-f"),
                    s("webp"),
                ]);
            }
            _ => unreachable!("image targets"),
        }
        a.extend([s("-map_metadata"), s("-1")]);
    } else {
        match target {
            Target::Mp4 if copies_into(Target::Mp4, info) => {
                a.extend([
                    s("-map"),
                    s("0:v:0"),
                    s("-map"),
                    s("0:a:0?"),
                    s("-c"),
                    s("copy"),
                ]);
                a.extend([s("-movflags"), s("+faststart"), s("-f"), s("mp4")]);
            }
            Target::Webm if copies_into(Target::Webm, info) => {
                a.extend([
                    s("-map"),
                    s("0:v:0"),
                    s("-map"),
                    s("0:a:0?"),
                    s("-c"),
                    s("copy"),
                ]);
                a.extend([s("-f"), s("webm")]);
            }
            Target::Mp4 => {
                a.extend([s("-map"), s("0:v:0"), s("-map"), s("0:a:0?")]);
                vf(&mut a, vec![s("scale=trunc(iw/2)*2:trunc(ih/2)*2")]);
                let (preset, crf) = if high {
                    ("medium", "20")
                } else {
                    ("fast", "23")
                };
                a.extend([
                    s("-c:v"),
                    s("libx264"),
                    s("-preset"),
                    s(preset),
                    s("-crf"),
                    s(crf),
                ]);
                a.extend([s("-pix_fmt"), s("yuv420p")]);
                if info.audio_codec.as_deref() == Some("aac") {
                    a.extend([s("-c:a"), s("copy")]);
                } else {
                    let b = if high { "192k" } else { "160k" };
                    a.extend([s("-c:a"), s("aac"), s("-b:a"), s(b), s("-ac"), s("2")]);
                }
                a.extend([s("-movflags"), s("+faststart"), s("-f"), s("mp4")]);
            }
            Target::Webm => {
                a.extend([s("-map"), s("0:v:0"), s("-map"), s("0:a:0?")]);
                vf(&mut a, vec![s("scale=trunc(iw/2)*2:trunc(ih/2)*2")]);
                let (crf, speed) = if high { ("28", "3") } else { ("32", "5") };
                a.extend([
                    s("-c:v"),
                    s("libvpx-vp9"),
                    s("-b:v"),
                    s("0"),
                    s("-crf"),
                    s(crf),
                ]);
                a.extend([s("-deadline"), s("good"), s("-cpu-used"), s(speed)]);
                a.extend([s("-row-mt"), s("1"), s("-pix_fmt"), s("yuv420p")]);
                if info.audio_codec.as_deref() == Some("opus") {
                    a.extend([s("-c:a"), s("copy")]);
                } else {
                    a.extend([s("-c:a"), s("libopus"), s("-b:a"), s("128k")]);
                }
                a.extend([s("-f"), s("webm")]);
            }
            Target::Gif => {
                if info.duration.is_some_and(|d| d > GIF_MAX_SECONDS) {
                    return Err(VideoError::Media(MediaProblem::TooLong));
                }
                let (fps, size) = if high { (15, 640) } else { (12, 480) };
                let graph = format!(
                    "[0:v:0]fps={fps},{}:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4[out]",
                    fit_long_side(size)
                );
                a.extend([s("-filter_complex"), graph, s("-map"), s("[out]")]);
                a.extend([s("-loop"), s("0"), s("-an"), s("-f"), s("gif")]);
            }
            _ => unreachable!("video targets"),
        }
        a.extend([s("-sn"), s("-dn")]);
    }
    a.push(path_arg(out));
    Ok(a)
}

/// Convert into `out` (through a temp file next to it); returns where the result went.
pub(crate) async fn convert(
    ffmpeg: &Path,
    info: &MediaInfo,
    target: Target,
    quality: Quality,
    out: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<std::path::PathBuf> {
    let tmp = temp_output(out);
    let mut args = convert_args(info, target, quality, &tmp)?;
    if target == Target::Ogg
        && super::encoders(ffmpeg)
            .await
            .is_some_and(|list| !super::has_encoder(&list, "libvorbis"))
    {
        use_builtin_vorbis(&mut args);
    }
    let duration = if target.is_image() {
        None
    } else {
        info.duration
    };
    let res = run_mapped(ffmpeg, &args, duration, (0.0, 1.0), false, progress, cancel).await;
    match res {
        Ok(()) if tmp.is_file() => finish_output(&tmp, out),
        Ok(()) => Err(super::ffmpeg_failed("ffmpeg wrote no file")),
        Err(e) => {
            discard(&tmp).await;
            Err(e)
        }
    }
}

/// An ffmpeg without libvorbis (Homebrew's, since 2026) still has its own Vorbis encoder: marked
/// experimental and stereo only, but the result is the usual Ogg Vorbis.
fn use_builtin_vorbis(args: &mut Vec<String>) {
    if let Some(i) = args.iter().position(|a| a == "libvorbis") {
        args[i] = s("vorbis");
        args.splice(i + 1..i + 1, [s("-strict"), s("-2"), s("-ac"), s("2")]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ogg_without_libvorbis_uses_ffmpegs_own_vorbis() {
        let mut a = vec![
            s("-c:a"),
            s("libvorbis"),
            s("-q:a"),
            s("5"),
            s("-f"),
            s("ogg"),
        ];
        use_builtin_vorbis(&mut a);
        assert_eq!(
            a,
            [
                "-c:a", "vorbis", "-strict", "-2", "-ac", "2", "-q:a", "5", "-f", "ogg"
            ]
        );
    }

    fn video(codec: &str, audio: Option<&str>) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("/d/in.mkv"),
            ext: "mkv".into(),
            kind: MediaKind::Video,
            width: Some(1920),
            height: Some(1080),
            duration: Some(30.0),
            video_codec: Some(codec.into()),
            audio_codec: audio.map(str::to_string),
            has_audio: audio.is_some(),
            pix_fmt: Some("yuv420p".into()),
            ..Default::default()
        }
    }

    fn has(a: &[String], pair: &[&str]) -> bool {
        a.windows(pair.len())
            .any(|w| w.iter().zip(pair).all(|(x, y)| x == y))
    }

    #[test]
    fn targets_leave_out_the_own_format() {
        let mut mp4 = video("h264", Some("aac"));
        mp4.ext = "mp4".into();
        assert_eq!(
            targets_for(&mp4),
            vec![Target::Webm, Target::Gif, Target::Mp3]
        );
        // An HEVC phone video can become an MP4 that plays everywhere.
        mp4.video_codec = Some("hevc".into());
        assert_eq!(
            targets_for(&mp4),
            vec![Target::Mp4, Target::Webm, Target::Gif, Target::Mp3]
        );
        // No sound, no MP3.
        let silent = video("vp9", None);
        assert_eq!(
            targets_for(&silent),
            vec![Target::Mp4, Target::Webm, Target::Gif]
        );
        let mut broken = video("h264", None);
        broken.problem = Some(MediaProblem::Unreadable);
        assert!(targets_for(&broken).is_empty());
    }

    #[test]
    fn mp4_copies_when_it_can() {
        let out = Path::new("/d/out.mp4");
        let a = convert_args(
            &video("h264", Some("aac")),
            Target::Mp4,
            Quality::Standard,
            out,
        )
        .unwrap();
        assert!(has(&a, &["-c", "copy"]));
        assert!(!a.contains(&s("libx264")));
        assert_eq!(a.last().unwrap(), "/d/out.mp4");
        let a = convert_args(
            &video("hevc", Some("opus")),
            Target::Mp4,
            Quality::Standard,
            out,
        )
        .unwrap();
        assert!(has(
            &a,
            &["-c:v", "libx264", "-preset", "fast", "-crf", "23"]
        ));
        assert!(has(&a, &["-c:a", "aac", "-b:a", "160k"]));
        assert!(has(&a, &["-pix_fmt", "yuv420p"]));
        assert!(has(&a, &["-movflags", "+faststart", "-f", "mp4"]));
        let a = convert_args(&video("hevc", Some("aac")), Target::Mp4, Quality::High, out).unwrap();
        assert!(has(&a, &["-preset", "medium", "-crf", "20"]));
        assert!(has(&a, &["-c:a", "copy"]));
    }

    #[test]
    fn webm_and_gif() {
        let out = Path::new("/d/out.webm");
        let a = convert_args(
            &video("h264", Some("aac")),
            Target::Webm,
            Quality::Standard,
            out,
        )
        .unwrap();
        assert!(has(&a, &["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "32"]));
        assert!(has(&a, &["-c:a", "libopus"]));
        let a = convert_args(
            &video("vp9", Some("opus")),
            Target::Webm,
            Quality::Standard,
            out,
        )
        .unwrap();
        assert!(has(&a, &["-c", "copy"]));
        let a = convert_args(
            &video("h264", None),
            Target::Gif,
            Quality::Standard,
            Path::new("o.gif"),
        )
        .unwrap();
        let graph = a.iter().find(|x| x.contains("palettegen")).unwrap();
        assert!(graph.starts_with("[0:v:0]fps=12,scale=w='if(gte(iw,ih),min(480,iw),-2)':h='if(gte(iw,ih),-2,min(480,ih))':flags=lanczos,split"), "{graph}");
        assert!(has(&a, &["-loop", "0", "-an", "-f", "gif"]));
        let mut long = video("h264", None);
        long.duration = Some(600.0);
        assert!(matches!(
            convert_args(&long, Target::Gif, Quality::Standard, Path::new("o.gif")),
            Err(VideoError::Media(MediaProblem::TooLong))
        ));
    }

    #[test]
    fn audio_targets() {
        let out = Path::new("/d/out.mp3");
        let a = convert_args(
            &video("h264", Some("aac")),
            Target::Mp3,
            Quality::Standard,
            out,
        )
        .unwrap();
        assert!(has(&a, &["-map", "0:a:0", "-vn"]));
        assert!(has(&a, &["-c:a", "libmp3lame", "-q:a", "2"]));
        let a = convert_args(&video("h264", Some("aac")), Target::Mp3, Quality::High, out).unwrap();
        assert!(has(&a, &["-b:a", "320k"]));
        assert!(matches!(
            convert_args(&video("h264", None), Target::Mp3, Quality::Standard, out),
            Err(VideoError::Media(MediaProblem::NoAudio))
        ));
        let song = MediaInfo {
            path: PathBuf::from("/d/song.flac"),
            ext: "flac".into(),
            kind: MediaKind::Audio,
            has_audio: true,
            has_cover: true,
            audio_codec: Some("flac".into()),
            ..Default::default()
        };
        let a = convert_args(&song, Target::Mp3, Quality::Standard, out).unwrap();
        // The cover picture comes along.
        assert!(has(
            &a,
            &[
                "-map",
                "0:v:0",
                "-c:v",
                "copy",
                "-disposition:v:0",
                "attached_pic"
            ]
        ));
        let a = convert_args(&song, Target::Ogg, Quality::Standard, Path::new("o.ogg")).unwrap();
        assert!(has(&a, &["-vn"]));
        assert!(has(&a, &["-c:a", "libvorbis", "-q:a", "5", "-f", "ogg"]));
        let a = convert_args(&song, Target::Wav, Quality::Standard, Path::new("o.wav")).unwrap();
        assert!(has(&a, &["-c:a", "pcm_s16le", "-f", "wav"]));
        let a = convert_args(&song, Target::M4a, Quality::Standard, Path::new("o.m4a")).unwrap();
        assert!(has(&a, &["-c:a", "aac", "-b:a", "192k"]));
        assert!(has(&a, &["-f", "ipod"]));
    }

    #[test]
    fn image_targets() {
        let png = MediaInfo {
            path: PathBuf::from("/d/logo.png"),
            ext: "png".into(),
            kind: MediaKind::Image,
            width: Some(800),
            height: Some(600),
            has_alpha: true,
            ..Default::default()
        };
        let a = convert_args(&png, Target::Jpg, Quality::Standard, Path::new("o.jpg")).unwrap();
        assert!(has(
            &a,
            &["-vf", &format!("{FLATTEN_ON_WHITE},format=yuvj420p")]
        ));
        assert!(has(&a, &["-c:v", "mjpeg", "-q:v", "3", "-f", "image2"]));
        let a = convert_args(&png, Target::Webp, Quality::Standard, Path::new("o.webp")).unwrap();
        assert!(has(
            &a,
            &["-c:v", "libwebp", "-quality", "85", "-f", "webp"]
        ));
        let a = convert_args(&png, Target::Png, Quality::Standard, Path::new("o.png")).unwrap();
        assert!(has(&a, &["-c:v", "png"]));
    }
}
