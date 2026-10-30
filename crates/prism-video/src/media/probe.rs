//! Reading a file with ffprobe into a [`MediaInfo`].

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::convert::targets_for;
use super::{MediaInfo, MediaKind, MediaProblem, extension, file_name, kind_by_name};
use crate::error::{Result, VideoError};
use crate::process;
use crate::tools::Tool;

/// ffprobe the file. Files whose names aren't images, audio or video aren't probed (ffprobe
/// would read a `.txt` as a terminal animation): they come back as [`MediaKind::Other`].
pub(crate) async fn inspect(ffprobe: &Path, path: &Path) -> Result<MediaInfo> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.is_dir() {
        // A folder: only for a ZIP.
        let p = path.to_path_buf();
        let size = tokio::task::spawn_blocking(move || super::pack::total_size(&[p]))
            .await
            .unwrap_or(0);
        let mut info = MediaInfo::other(path, size);
        info.folder = true;
        info.ext = String::new();
        return Ok(info);
    }
    let size = meta.len();
    if kind_by_name(path) == MediaKind::Other {
        return Ok(MediaInfo::other(path, size));
    }
    let json = run_ffprobe(ffprobe, path).await;
    let orientation = if matches!(extension(path).as_str(), "jpg" | "jpeg" | "jfif") {
        let p = path.to_path_buf();
        tokio::task::spawn_blocking(move || jpeg_orientation(&p))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    Ok(match json {
        Ok(v) => parse(path, size, &v, orientation),
        Err(e) => {
            let mut info = MediaInfo::other(path, size);
            info.kind = kind_by_name(path);
            info.problem = Some(match MediaProblem::of(&e, &info.ext) {
                MediaProblem::Failed => {
                    if matches!(info.ext.as_str(), "heic" | "heif") {
                        MediaProblem::Heic
                    } else {
                        MediaProblem::Unreadable
                    }
                }
                p => p,
            });
            info
        }
    })
}

async fn run_ffprobe(ffprobe: &Path, path: &Path) -> Result<Value> {
    let file = path.as_os_str().to_owned();
    let base = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ];
    // Stream groups carry a HEIC photo's real size (it's stored as tiles); ffprobe before 7.1
    // doesn't know the option, so it's asked once more without it.
    for groups in [true, false] {
        let mut args: Vec<std::ffi::OsString> = base.iter().map(Into::into).collect();
        if groups {
            args.push("-show_stream_groups".into());
        }
        args.push("-i".into());
        args.push(file.clone());
        let out =
            process::run_capture(Tool::Ffprobe, ffprobe, args, Duration::from_secs(60)).await?;
        if out.status.success() {
            return Ok(serde_json::from_slice(&out.stdout)?);
        }
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        if groups && err.contains("show_stream_groups") {
            continue;
        }
        return Err(VideoError::ToolFailed {
            tool: Tool::Ffprobe,
            code: out.status.code(),
            message: err.trim().to_string(),
        });
    }
    unreachable!("the second try returns")
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

fn field(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(num)
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `30000/1001` → 29.97.
fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

fn flag(stream: &Value, key: &str) -> bool {
    stream
        .get("disposition")
        .and_then(|d| d.get(key))
        .and_then(Value::as_i64)
        == Some(1)
}

/// Rotation of a stream (display matrix side data, or the old `rotate` tag), in degrees.
fn rotation(v: &Value) -> i64 {
    let side = v
        .get("side_data_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|d| d.get("rotation").and_then(num));
    let tag = v.get("tags").and_then(|t| t.get("rotate")).and_then(num);
    side.or(tag).map(|r| r.round() as i64).unwrap_or(0)
}

pub fn pix_fmt_has_alpha(p: &str) -> bool {
    p.starts_with("yuva")
        || p.starts_with("gbrap")
        || p.starts_with("ya")
        || p.starts_with("rgba")
        || p.starts_with("bgra")
        || p.starts_with("argb")
        || p.starts_with("abgr")
        || p == "pal8"
}

const STILL_CODECS: &[&str] = &["mjpeg", "png", "bmp", "gif", "webp", "tiff", "jpegxl"];

/// Turn ffprobe's JSON into a [`MediaInfo`]. `exif_orientation` is a JPEG's EXIF orientation
/// (ffmpeg turns the picture; ffprobe doesn't say so).
pub fn parse(path: &Path, size: u64, v: &Value, exif_orientation: Option<u16>) -> MediaInfo {
    let mut info = MediaInfo::other(path, size);
    info.name = file_name(path);
    let ext = info.ext.clone();
    let format = v.get("format").cloned().unwrap_or(Value::Null);
    let format_name = text(&format, "format_name").unwrap_or("");
    let brand = format
        .get("tags")
        .and_then(|t| text(t, "major_brand"))
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let streams: Vec<&Value> = v
        .get("streams")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let audio = streams
        .iter()
        .find(|s| text(s, "codec_type") == Some("audio"))
        .copied();
    let videos: Vec<&Value> = streams
        .iter()
        .filter(|s| text(s, "codec_type") == Some("video") && !flag(s, "attached_pic"))
        .copied()
        .collect();
    // A cover picture in a music file (some files don't mark it as one).
    let cover = streams.iter().any(|s| {
        text(s, "codec_type") == Some("video")
            && (flag(s, "attached_pic")
                || (audio.is_some() && STILL_CODECS.contains(&text(s, "codec_name").unwrap_or(""))))
    });
    // A HEIC/AVIF photo stored as a grid of tiles: the group has the picture's size.
    let grid = v
        .get("stream_groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|g| text(g, "type") == Some("Tile Grid"))
        .find_map(|g| g.get("components").and_then(Value::as_array)?.first());
    let main_video = videos
        .iter()
        .find(|s| flag(s, "default"))
        .or_else(|| videos.first())
        .copied();

    let frames = main_video
        .and_then(|s| field(s, "nb_frames"))
        .unwrap_or(0.0);
    let duration = field(&format, "duration")
        .or_else(|| main_video.and_then(|s| field(s, "duration")))
        .or_else(|| audio.and_then(|s| field(s, "duration")))
        .filter(|d| *d > 0.0);
    let still_format = format_name.ends_with("_pipe")
        || format_name == "image2"
        || matches!(
            brand.as_str(),
            "heic" | "heix" | "mif1" | "msf1" | "avif" | "avis"
        );

    info.kind = if ext == "gif" || format_name == "gif" {
        // A moving GIF is a short video (it can become an MP4); a still one is a picture.
        if frames > 1.0 || duration.is_some_and(|d| d > 0.2) {
            MediaKind::Video
        } else {
            MediaKind::Image
        }
    } else if grid.is_some()
        || (main_video.is_some() && (still_format || super::IMAGE_EXTS.contains(&ext.as_str())))
    {
        MediaKind::Image
    } else if main_video.is_some()
        && !(audio.is_some()
            && STILL_CODECS.contains(&main_video.and_then(|s| text(s, "codec_name")).unwrap_or("")))
    {
        MediaKind::Video
    } else if audio.is_some() {
        MediaKind::Audio
    } else {
        MediaKind::Other
    };

    // Size as shown: a grid's size, turned by its rotation.
    let (w, h, rot) = match (grid, main_video) {
        (Some(g), _) => (field(g, "width"), field(g, "height"), rotation(g)),
        (None, Some(s)) => (field(s, "width"), field(s, "height"), rotation(s)),
        _ => (None, None, 0),
    };
    let turned =
        rot.rem_euclid(180) == 90 || exif_orientation.is_some_and(|o| (5..=8).contains(&o));
    let (w, h) = (w.map(|x| x as u32), h.map(|x| x as u32));
    if info.kind != MediaKind::Audio {
        (info.width, info.height) = if turned { (h, w) } else { (w, h) };
    }

    if matches!(info.kind, MediaKind::Audio | MediaKind::Video) {
        info.duration = duration;
    }
    if info.kind == MediaKind::Video {
        info.fps = main_video
            .and_then(|s| text(s, "avg_frame_rate").and_then(rate))
            .or_else(|| main_video.and_then(|s| text(s, "r_frame_rate").and_then(rate)))
            .filter(|f| *f < 1000.0);
        info.video_codec = main_video
            .and_then(|s| text(s, "codec_name"))
            .map(str::to_string);
    }
    if let Some(a) = audio {
        info.has_audio = true;
        info.audio_codec = text(a, "codec_name").map(str::to_string);
        info.audio_kbps = field(a, "bit_rate").map(|b| (b / 1000.0).round() as u32);
    }
    info.has_cover = info.kind == MediaKind::Audio && cover;
    info.tiled = info.kind == MediaKind::Image && grid.is_some();
    let pix = main_video
        .and_then(|s| text(s, "pix_fmt"))
        .map(str::to_string);
    info.has_alpha = pix.as_deref().is_some_and(pix_fmt_has_alpha);
    info.pix_fmt = pix;
    if info.kind == MediaKind::Video {
        let total = field(&format, "bit_rate").or_else(|| duration.map(|d| size as f64 * 8.0 / d));
        info.video_kbps = main_video
            .and_then(|s| field(s, "bit_rate"))
            .map(|b| b / 1000.0)
            .or_else(|| total.map(|t| t / 1000.0 - info.audio_kbps.unwrap_or(0) as f64))
            .filter(|k| *k > 0.0)
            .map(|k| k.round() as u32);
    }
    if info.kind == MediaKind::Other {
        info.problem = Some(if matches!(ext.as_str(), "heic" | "heif") {
            MediaProblem::Heic
        } else {
            MediaProblem::Unreadable
        });
    } else if info.kind != MediaKind::Audio && (info.width.is_none() || info.height.is_none()) {
        info.problem = Some(MediaProblem::Unreadable);
    }
    info.targets = targets_for(&info);
    info
}

/// The EXIF orientation of a JPEG (1–8), read from its first APP1 segment.
pub fn jpeg_orientation(path: &Path) -> Option<u16> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 128 * 1024];
    let n = f.read(&mut buf).ok()?;
    exif_orientation(&buf[..n])
}

pub fn exif_orientation(jpeg: &[u8]) -> Option<u16> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if marker == 0xDA || len < 2 {
            return None;
        }
        let seg = jpeg.get(i + 4..i + 2 + len)?;
        if marker == 0xE1 && seg.starts_with(b"Exif\0\0") {
            return tiff_orientation(&seg[6..]);
        }
        i += 2 + len;
    }
    None
}

fn tiff_orientation(t: &[u8]) -> Option<u16> {
    let le = match t.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = [*t.get(o)?, *t.get(o + 1)?];
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = [*t.get(o)?, *t.get(o + 1)?, *t.get(o + 2)?, *t.get(o + 3)?];
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count.min(64))
        .find_map(|k| {
            let e = ifd + 2 + k * 12;
            (u16_at(e)? == 0x0112).then(|| u16_at(e + 8)).flatten()
        })
        .filter(|o| (1..=8).contains(o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Target;
    use serde_json::json;

    fn video_json() -> Value {
        json!({
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
                 "pix_fmt": "yuv420p", "avg_frame_rate": "30000/1001", "bit_rate": "4800000",
                 "disposition": {"default": 1, "attached_pic": 0},
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
                {"index": 1, "codec_type": "audio", "codec_name": "aac", "bit_rate": "128000",
                 "disposition": {"default": 1, "attached_pic": 0}}
            ],
            "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "62.5", "bit_rate": "4950000",
                       "tags": {"major_brand": "isom"}}
        })
    }

    #[test]
    fn phone_video() {
        let info = parse(Path::new("/d/clip.mp4"), 38_000_000, &video_json(), None);
        assert_eq!(info.kind, MediaKind::Video);
        // Recorded upright: shown 1080 wide, 1920 tall.
        assert_eq!((info.width, info.height), (Some(1080), Some(1920)));
        assert_eq!(info.short_side(), Some(1080));
        assert!((info.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(info.video_kbps, Some(4800));
        assert_eq!(info.audio_kbps, Some(128));
        assert!(info.has_audio);
        assert_eq!(info.duration, Some(62.5));
        assert_eq!(info.targets, vec![Target::Webm, Target::Gif, Target::Mp3]);
        assert_eq!(info.problem, None);
    }

    #[test]
    fn music_with_cover_art() {
        let v = json!({
            "streams": [
                {"codec_type": "audio", "codec_name": "mp3", "bit_rate": "320000"},
                {"codec_type": "video", "codec_name": "mjpeg", "width": 600, "height": 600,
                 "disposition": {"attached_pic": 1}}
            ],
            "format": {"format_name": "mp3", "duration": "241.3"}
        });
        let info = parse(Path::new("song.mp3"), 9_600_000, &v, None);
        assert_eq!(info.kind, MediaKind::Audio);
        assert!(info.has_cover);
        assert_eq!(info.width, None);
        assert_eq!(
            info.targets,
            vec![Target::M4a, Target::Wav, Target::Flac, Target::Ogg]
        );
    }

    #[test]
    fn heic_photo_from_tiles() {
        let v = json!({
            "stream_groups": [{"type": "Tile Grid", "components": [{"width": 4032, "height": 3024,
                "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}]}],
            "streams": [{"codec_type": "video", "codec_name": "hevc", "width": 512, "height": 512,
                         "pix_fmt": "yuvj420p", "nb_frames": "1"}],
            "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "tags": {"major_brand": "heic"}}
        });
        let info = parse(Path::new("IMG_4332.HEIC"), 3_447_000, &v, None);
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!((info.width, info.height), (Some(3024), Some(4032)));
        assert!(info.tiled);
        assert_eq!(info.duration, None);
        assert_eq!(info.targets, vec![Target::Jpg, Target::Png, Target::Webp]);
    }

    #[test]
    fn gifs_moving_and_still() {
        let gif = |frames: &str, dur: &str| {
            json!({
                "streams": [{"codec_type": "video", "codec_name": "gif", "width": 320, "height": 240,
                             "pix_fmt": "bgra", "nb_frames": frames, "avg_frame_rate": "10/1"}],
                "format": {"format_name": "gif", "duration": dur}
            })
        };
        let moving = parse(Path::new("a.gif"), 100_000, &gif("20", "2.0"), None);
        assert_eq!(moving.kind, MediaKind::Video);
        assert_eq!(moving.targets, vec![Target::Mp4, Target::Webm]);
        let still = parse(Path::new("b.gif"), 10_000, &gif("1", "0.04"), None);
        assert_eq!(still.kind, MediaKind::Image);
        assert!(still.has_alpha);
    }

    #[test]
    fn transparent_png_and_turned_jpeg() {
        let png = json!({
            "streams": [{"codec_type": "video", "codec_name": "png", "width": 800, "height": 600, "pix_fmt": "rgba"}],
            "format": {"format_name": "png_pipe"}
        });
        let info = parse(Path::new("logo.png"), 50_000, &png, None);
        assert_eq!(info.kind, MediaKind::Image);
        assert!(info.has_alpha);
        let jpg = json!({
            "streams": [{"codec_type": "video", "codec_name": "mjpeg", "width": 4000, "height": 3000, "pix_fmt": "yuvj420p"}],
            "format": {"format_name": "image2"}
        });
        let info = parse(Path::new("p.jpg"), 4_000_000, &jpg, Some(6));
        assert_eq!((info.width, info.height), (Some(3000), Some(4000)));
        assert!(!info.has_alpha);
    }

    #[test]
    fn unreadable_files() {
        let info = parse(
            Path::new("x.heic"),
            10,
            &json!({"streams": [], "format": {}}),
            None,
        );
        assert_eq!(info.kind, MediaKind::Other);
        assert_eq!(info.problem, Some(MediaProblem::Heic));
        assert!(info.targets.is_empty());
    }

    #[test]
    fn reads_exif_orientation() {
        let tiff = |le: bool, o: u16| {
            let mut t: Vec<u8> = Vec::new();
            let w16 = |t: &mut Vec<u8>, v: u16| {
                t.extend(if le { v.to_le_bytes() } else { v.to_be_bytes() })
            };
            let w32 = |t: &mut Vec<u8>, v: u32| {
                t.extend(if le { v.to_le_bytes() } else { v.to_be_bytes() })
            };
            t.extend(if le { b"II" } else { b"MM" });
            w16(&mut t, 42);
            w32(&mut t, 8);
            w16(&mut t, 1);
            w16(&mut t, 0x0112);
            w16(&mut t, 3);
            w32(&mut t, 1);
            w16(&mut t, o);
            w16(&mut t, 0);
            w32(&mut t, 0);
            t
        };
        for le in [true, false] {
            let mut payload = b"Exif\0\0".to_vec();
            payload.extend(tiff(le, 6));
            let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xE1];
            jpeg.extend(((payload.len() + 2) as u16).to_be_bytes());
            jpeg.extend(&payload);
            jpeg.extend([0xFF, 0xDA, 0, 2]);
            assert_eq!(exif_orientation(&jpeg), Some(6));
        }
        assert_eq!(exif_orientation(b"\xFF\xD8\xFF\xDA\x00\x02"), None);
        assert_eq!(exif_orientation(b"PNG"), None);
    }
}
