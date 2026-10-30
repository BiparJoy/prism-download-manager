//! The Tools' jobs: convert, make smaller and pack into a ZIP (the work itself is in
//! `prism_video::media`).
//!
//! Jobs belong to the core, so they keep running when the Tools window closes, and every open
//! window sees them ([`CoreEvent::MediaJob`]). ffmpeg jobs run one at a time (ffmpeg already
//! uses every core); ZIPs run beside them. ffmpeg is downloaded on first use if there's none.
//! Results go next to the originals under a new name; a stopped or failed job leaves nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use prism_video::media::{self, MediaProgress, shrink};
pub use prism_video::media::{
    MediaInfo, MediaKind, MediaProblem, Quality, ShrinkPlan, Target, Verdict,
};
use prism_video::{CancellationToken, VideoError};
use serde::{Deserialize, Serialize};

use crate::events::CoreEvent;
use crate::manager::Core;

/// Finished jobs kept in the list (older ones drop off).
const KEEP_FINISHED: usize = 50;

/// What the user asked for (one job per file, or one ZIP).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MediaRequest {
    Convert {
        paths: Vec<PathBuf>,
        target: Target,
        #[serde(default)]
        quality: Quality,
    },
    Shrink {
        paths: Vec<PathBuf>,
        target_bytes: u64,
        /// Goes into the result's name: `clip (Discord).mp4`.
        label: String,
    },
    Zip {
        paths: Vec<PathBuf>,
    },
}

/// What one job does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MediaOp {
    Convert { target: Target, quality: Quality },
    Shrink { target_bytes: u64, label: String },
    Zip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    /// Waiting for the job before it.
    Queued,
    /// Getting ffmpeg (first use), reading the file.
    Preparing,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn finished(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed | JobState::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaJob {
    pub id: String,
    pub op: MediaOp,
    pub inputs: Vec<PathBuf>,
    /// The original's name (a ZIP: the archive's).
    pub name: String,
    /// Where the result goes (known once the job starts), then where it went.
    pub output: Option<PathBuf>,
    pub state: JobState,
    /// 0–1 when known.
    pub progress: Option<f64>,
    /// A second, smaller try (it came out over the limit).
    pub retrying: bool,
    pub problem: Option<MediaProblem>,
    /// ffmpeg's own words, for the log and a tooltip.
    pub detail: Option<String>,
    /// Bytes of the original(s).
    pub before: u64,
    /// Bytes of the result.
    pub after: Option<u64>,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Default)]
pub(crate) struct MediaJobs {
    jobs: Mutex<Vec<MediaJob>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    /// Result names taken by running jobs (two jobs never pick the same one).
    reserved: Mutex<HashSet<PathBuf>>,
    /// One ffmpeg job at a time.
    ffmpeg: tokio::sync::Mutex<()>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

impl Core {
    fn media(&self) -> &MediaJobs {
        &self.0.media
    }

    /// ffmpeg + ffprobe, downloaded now if there are none (progress as `ToolsProgress`).
    async fn ensure_ffmpeg(&self) -> Result<()> {
        if self.0.video.ffmpeg_ready() {
            return Ok(());
        }
        let c = self.clone();
        self.0
            .video
            .ensure_ffmpeg(move |p| {
                c.emit(CoreEvent::ToolsProgress {
                    tool: format!("{:?}", p.tool).to_lowercase(),
                    downloaded: p.downloaded,
                    total: p.total,
                })
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        self.sync_ffmpeg_path().await;
        Ok(())
    }

    /// Read files for the Tools window. ffmpeg is downloaded first if there's none; a file
    /// that can't be read comes back with its problem.
    pub async fn media_inspect(&self, paths: Vec<PathBuf>) -> Vec<MediaInfo> {
        let needs_probe = paths
            .iter()
            .any(|p| media::kind_by_name(p) != MediaKind::Other);
        let ready = !needs_probe || self.ensure_ffmpeg().await.is_ok();
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let info = if media::kind_by_name(&p) == MediaKind::Other {
                // Not a picture, music or a video (a ZIP takes any file): nothing to probe, so
                // no ffmpeg is needed either.
                MediaInfo::other(&p, file_size(&p))
            } else if !ready {
                let mut i = MediaInfo::other(&p, file_size(&p));
                i.kind = media::kind_by_name(&p);
                i.problem = Some(MediaProblem::Converter);
                i
            } else {
                match self.0.video.media_inspect(&p).await {
                    Ok(i) => i,
                    Err(e) => {
                        let mut i = MediaInfo::other(&p, file_size(&p));
                        i.kind = media::kind_by_name(&p);
                        i.problem = Some(MediaProblem::of(&e, &i.ext));
                        i
                    }
                }
            };
            out.push(info);
        }
        out
    }

    /// The "Make smaller" estimate for each file (no work done).
    pub fn media_plan(&self, files: &[MediaInfo], target_bytes: u64) -> Vec<ShrinkPlan> {
        files
            .iter()
            .map(|f| shrink::plan(f, target_bytes))
            .collect()
    }

    pub fn media_jobs(&self) -> Vec<MediaJob> {
        self.media().jobs.lock().clone()
    }

    /// Start jobs for a request; they run in the background.
    pub fn media_start(&self, req: MediaRequest) -> Result<Vec<MediaJob>> {
        let _rt = crate::rt::enter();
        let (paths, ops): (Vec<Vec<PathBuf>>, MediaOp) = match req {
            MediaRequest::Convert {
                paths,
                target,
                quality,
            } => (
                paths.into_iter().map(|p| vec![p]).collect(),
                MediaOp::Convert { target, quality },
            ),
            MediaRequest::Shrink {
                paths,
                target_bytes,
                label,
            } => {
                if target_bytes < 10_000 {
                    return Err(anyhow!("the size limit is too small"));
                }
                (
                    paths.into_iter().map(|p| vec![p]).collect(),
                    MediaOp::Shrink {
                        target_bytes,
                        label,
                    },
                )
            }
            MediaRequest::Zip { paths } => (vec![paths], MediaOp::Zip),
        };
        if paths.iter().all(|p| p.is_empty()) {
            return Err(anyhow!("no files"));
        }
        let mut started = Vec::new();
        for inputs in paths.into_iter().filter(|p| !p.is_empty()) {
            let name = match ops {
                MediaOp::Zip => media::pack::zip_path(&inputs)
                    .map(|p| media::file_name(&p))
                    .unwrap_or_default(),
                _ => inputs[0]
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            let job = MediaJob {
                id: uuid::Uuid::new_v4().to_string(),
                op: ops.clone(),
                before: media::pack::total_size(&inputs),
                inputs,
                name,
                output: None,
                state: JobState::Queued,
                progress: None,
                retrying: false,
                problem: None,
                detail: None,
                after: None,
                created_at: now_ms(),
                started_at: None,
                finished_at: None,
            };
            let cancel = CancellationToken::new();
            self.media()
                .cancels
                .lock()
                .insert(job.id.clone(), cancel.clone());
            {
                let mut jobs = self.media().jobs.lock();
                jobs.push(job.clone());
                prune(&mut jobs);
            }
            self.emit(CoreEvent::MediaJob(job.clone()));
            let core = self.clone();
            let id = job.id.clone();
            tokio::spawn(async move { core.run_media_job(id, cancel).await });
            started.push(job);
        }
        Ok(started)
    }

    /// Stop a job (a queued one doesn't start).
    pub fn media_cancel(&self, id: &str) {
        if let Some(c) = self.media().cancels.lock().get(id) {
            c.cancel();
        }
    }

    /// Remove finished jobs from the list.
    pub fn media_clear(&self) -> Vec<MediaJob> {
        let mut jobs = self.media().jobs.lock();
        jobs.retain(|j| !j.state.finished());
        jobs.clone()
    }

    fn update_job(&self, id: &str, f: impl FnOnce(&mut MediaJob)) -> Option<MediaJob> {
        let job = {
            let mut jobs = self.media().jobs.lock();
            let j = jobs.iter_mut().find(|j| j.id == id)?;
            f(j);
            j.clone()
        };
        self.emit(CoreEvent::MediaJob(job.clone()));
        Some(job)
    }

    async fn run_media_job(self, id: String, cancel: CancellationToken) {
        let Some(job) = self.media_jobs().into_iter().find(|j| j.id == id) else {
            return;
        };
        let res = if job.op == MediaOp::Zip {
            self.run_zip(&job, &cancel).await
        } else {
            // One ffmpeg job at a time; a job stopped while it waits doesn't start.
            let turn = tokio::select! {
                g = self.media().ffmpeg.lock() => Some(g),
                _ = cancel.cancelled() => None,
            };
            match turn {
                Some(_turn) => self.run_ffmpeg_job(&job, &cancel).await,
                None => Err(VideoError::Cancelled),
            }
        };
        self.media().cancels.lock().remove(&id);
        let ext = job
            .inputs
            .first()
            .map(|p| media::extension(p))
            .unwrap_or_default();
        let finished = self.update_job(&id, |j| {
            j.finished_at = Some(now_ms());
            j.retrying = false;
            match res {
                Ok((path, size)) => {
                    j.state = JobState::Done;
                    j.progress = Some(1.0);
                    j.output = Some(path);
                    j.after = Some(size);
                }
                Err(VideoError::Cancelled) => {
                    j.state = JobState::Cancelled;
                    j.progress = None;
                }
                Err(e) => {
                    j.state = JobState::Failed;
                    j.progress = None;
                    j.problem = Some(MediaProblem::of(&e, &ext));
                    j.detail = Some(e.to_string());
                }
            }
        });
        if let Some(j) = finished {
            if let Some(out) = &j.output {
                self.media().reserved.lock().remove(out);
            }
            match j.state {
                JobState::Failed => tracing::warn!(
                    "{} failed ({:?}): {}",
                    j.name,
                    j.problem,
                    j.detail.as_deref().unwrap_or("")
                ),
                JobState::Done => tracing::info!(
                    "{} → {} ({} → {} bytes)",
                    j.name,
                    j.output.as_deref().unwrap_or(Path::new("")).display(),
                    j.before,
                    j.after.unwrap_or(0)
                ),
                _ => {}
            }
        }
    }

    /// Reserve a free name for the result (on disk and among running jobs).
    fn reserve_output(&self, wanted: PathBuf) -> PathBuf {
        let mut reserved = self.media().reserved.lock();
        let path = media::unique_path(wanted, |p| p.exists() || reserved.contains(p));
        reserved.insert(path.clone());
        path
    }

    async fn run_ffmpeg_job(
        &self,
        job: &MediaJob,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, u64), VideoError> {
        let input = job.inputs[0].clone();
        self.update_job(&job.id, |j| {
            j.state = JobState::Preparing;
            j.started_at = Some(now_ms());
        });
        if !self.0.video.ffmpeg_ready() {
            tokio::select! {
                r = self.ensure_ffmpeg() => r.map_err(|e| {
                    tracing::warn!("ffmpeg isn't available: {e:#}");
                    VideoError::Media(MediaProblem::Converter)
                })?,
                _ = cancel.cancelled() => return Err(VideoError::Cancelled),
            }
        }
        let info = self.0.video.media_inspect(&input).await?;
        if let Some(p) = info.problem {
            return Err(VideoError::Media(p));
        }
        let wanted = match &job.op {
            MediaOp::Convert { target, .. } => {
                if !convertible(&info, *target) {
                    return Err(VideoError::Media(if info.kind == MediaKind::Video {
                        MediaProblem::NoAudio
                    } else {
                        MediaProblem::Unreadable
                    }));
                }
                media::output_path(&input, target.ext(), None)
            }
            MediaOp::Shrink {
                target_bytes,
                label,
            } => {
                let plan = shrink::plan(&info, *target_bytes);
                if let Some(p) = shrink::verdict_problem(&plan) {
                    return Err(VideoError::Media(p));
                }
                media::output_path(&input, &plan.ext, Some(label))
            }
            MediaOp::Zip => unreachable!("ZIPs don't run ffmpeg"),
        };
        let out = self.reserve_output(wanted);
        self.update_job(&job.id, |j| {
            j.state = JobState::Running;
            j.output = Some(out.clone());
            j.progress = Some(0.0);
        });
        let progress = self.progress_reporter(&job.id);
        let res = match &job.op {
            MediaOp::Convert { target, quality } => self
                .0
                .video
                .media_convert(&info, *target, *quality, &out, &progress, cancel)
                .await
                .map(|p| {
                    let size = file_size(&p);
                    (p, size)
                }),
            MediaOp::Shrink { target_bytes, .. } => self
                .0
                .video
                .media_shrink(&info, *target_bytes, &out, &progress, cancel)
                .await
                .map(|o| (o.path, o.size)),
            MediaOp::Zip => unreachable!(),
        };
        if res.is_err() {
            self.media().reserved.lock().remove(&out);
        }
        res
    }

    async fn run_zip(
        &self,
        job: &MediaJob,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, u64), VideoError> {
        let wanted =
            media::pack::zip_path(&job.inputs).ok_or(VideoError::Media(MediaProblem::Missing))?;
        let out = self.reserve_output(wanted);
        self.update_job(&job.id, |j| {
            j.state = JobState::Running;
            j.started_at = Some(now_ms());
            j.output = Some(out.clone());
            j.name = media::file_name(&out);
            j.progress = Some(0.0);
        });
        let progress = self.progress_reporter(&job.id);
        let inputs = job.inputs.clone();
        let cancel = cancel.clone();
        let target = out.clone();
        let res = tokio::task::spawn_blocking(move || {
            media::pack::pack_zip(&inputs, &target, &progress, &cancel)
        })
        .await
        .map_err(|e| VideoError::Other(e.to_string()))?;
        if res.is_err() {
            self.media().reserved.lock().remove(&out);
        }
        res
    }

    /// Progress for a job, sent at most ~5 times a second.
    fn progress_reporter(&self, id: &str) -> impl Fn(MediaProgress) + Send + Sync + 'static {
        let core = self.clone();
        let id = id.to_string();
        let last = Arc::new(Mutex::new((Instant::now() - Duration::from_secs(1), false)));
        move |p: MediaProgress| {
            let mut l = last.lock();
            if l.0.elapsed() < Duration::from_millis(200) && l.1 == p.retrying {
                return;
            }
            *l = (Instant::now(), p.retrying);
            drop(l);
            core.update_job(&id, |j| {
                if j.state == JobState::Running {
                    j.progress = p.fraction;
                    j.retrying = p.retrying;
                }
            });
        }
    }
}

/// Any audio/video/picture target fits a file of that kind (a request from the menu may ask
/// for a file's own format, e.g. an MP4 that's already H.264: it's made again).
fn convertible(info: &MediaInfo, target: Target) -> bool {
    match info.kind {
        MediaKind::Image => target.is_image(),
        MediaKind::Audio => target.is_audio(),
        MediaKind::Video => !target.is_image() && (!target.is_audio() || info.has_audio),
        MediaKind::Other => false,
    }
}

/// Keep running jobs and the newest finished ones.
fn prune(jobs: &mut Vec<MediaJob>) {
    let finished = jobs.iter().filter(|j| j.state.finished()).count();
    if finished <= KEEP_FINISHED {
        return;
    }
    let mut drop = finished - KEEP_FINISHED;
    jobs.retain(|j| {
        if drop > 0 && j.state.finished() {
            drop -= 1;
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_from_the_ui() {
        let r: MediaRequest = serde_json::from_str(
            r#"{"op":"shrink","paths":["C:\\a.mp4"],"targetBytes":20000000,"label":"Discord"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            MediaRequest::Shrink {
                paths: vec![PathBuf::from("C:\\a.mp4")],
                target_bytes: 20_000_000,
                label: "Discord".into()
            }
        );
        let r: MediaRequest =
            serde_json::from_str(r#"{"op":"convert","paths":["/a.flac"],"target":"mp3"}"#).unwrap();
        assert!(matches!(
            r,
            MediaRequest::Convert {
                target: Target::Mp3,
                quality: Quality::Standard,
                ..
            }
        ));
        let op = serde_json::to_value(MediaOp::Shrink {
            target_bytes: 5,
            label: "x".into(),
        })
        .unwrap();
        assert_eq!(
            op,
            serde_json::json!({"type": "shrink", "targetBytes": 5, "label": "x"})
        );
    }

    #[test]
    fn old_finished_jobs_drop_off() {
        let job = |i: usize, state| MediaJob {
            id: i.to_string(),
            op: MediaOp::Zip,
            inputs: vec![],
            name: String::new(),
            output: None,
            state,
            progress: None,
            retrying: false,
            problem: None,
            detail: None,
            before: 0,
            after: None,
            created_at: 0,
            started_at: None,
            finished_at: None,
        };
        let mut jobs: Vec<MediaJob> = (0..KEEP_FINISHED + 5)
            .map(|i| job(i, JobState::Done))
            .collect();
        jobs.insert(2, job(999, JobState::Running));
        prune(&mut jobs);
        assert_eq!(jobs.len(), KEEP_FINISHED + 1);
        assert!(jobs.iter().any(|j| j.id == "999"));
        assert_eq!(jobs.last().unwrap().id, (KEEP_FINISHED + 4).to_string());
    }
}
