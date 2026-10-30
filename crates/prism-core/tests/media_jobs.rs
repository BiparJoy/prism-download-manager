//! The Tools' jobs through the core: a ZIP (no ffmpeg needed), and with an ffmpeg on PATH a
//! conversion, a "Make smaller" that has nothing to do, and a job stopped while it waits.

use prism_core::events::CoreEvent;
use prism_core::manager::{Core, CoreOptions};
use prism_core::media::{JobState, MediaJob, MediaRequest};
use prism_video::media::{MediaKind, MediaProblem, Quality, Target};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::broadcast;

async fn core_in(dir: &Path) -> Core {
    Core::start(CoreOptions {
        data_dir: dir.join("data"),
        ipc: false,
        clipboard: false,
    })
    .await
    .unwrap()
}

/// The job's last state once it's finished.
async fn finished(rx: &mut broadcast::Receiver<CoreEvent>, id: &str) -> MediaJob {
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match rx.recv().await {
                Ok(CoreEvent::MediaJob(j)) if j.id == id && j.state.finished() => return j,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event stream closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for the job")
}

fn have_ffmpeg() -> bool {
    let exe = std::env::consts::EXE_SUFFIX;
    prism_video::tools::which(&format!("ffmpeg{exe}")).is_some()
        && prism_video::tools::which(&format!("ffprobe{exe}")).is_some()
}

#[tokio::test]
async fn zip_job_reports_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let core = core_in(dir.path()).await;
    let files: Vec<PathBuf> = ["report.txt", "data.csv"]
        .iter()
        .map(|n| {
            let p = dir.path().join(n);
            std::fs::write(&p, n.repeat(1000)).unwrap();
            p
        })
        .collect();
    // Files Prism doesn't convert are listed for a ZIP without running ffprobe.
    let infos = core.media_inspect(files.clone()).await;
    assert!(
        infos
            .iter()
            .all(|i| i.kind == MediaKind::Other && i.problem.is_none())
    );

    let mut rx = core.subscribe();
    let jobs = core
        .media_start(MediaRequest::Zip {
            paths: files.clone(),
        })
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Queued);
    let done = finished(&mut rx, &jobs[0].id).await;
    assert_eq!(done.state, JobState::Done, "{done:?}");
    let zip = dir.path().join("report.zip");
    assert_eq!(done.output.as_deref(), Some(zip.as_path()));
    assert_eq!(done.after, Some(std::fs::metadata(&zip).unwrap().len()));
    assert!(done.after.unwrap() < done.before);
    assert_eq!(core.media_jobs().len(), 1);
    assert!(core.media_clear().is_empty());
}

#[tokio::test]
async fn convert_shrink_and_stop() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH, skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let core = core_in(dir.path()).await;
    let tone = dir.path().join("tone.wav");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
        ])
        .args(["-i", "sine=frequency=440:duration=3"])
        .arg(&tone)
        .status()
        .unwrap();
    assert!(made.success());
    let info = core.media_inspect(vec![tone.clone()]).await;
    assert_eq!(info[0].kind, MediaKind::Audio);
    let plans = core.media_plan(&info, 10_000_000);
    assert_eq!(plans[0].verdict, prism_video::media::Verdict::AlreadySmall);

    let mut rx = core.subscribe();
    let convert = core
        .media_start(MediaRequest::Convert {
            paths: vec![tone.clone()],
            target: Target::Mp3,
            quality: Quality::Standard,
        })
        .unwrap();
    // Queued behind the conversion, then stopped: it never runs.
    let waiting = core
        .media_start(MediaRequest::Convert {
            paths: vec![tone.clone()],
            target: Target::Flac,
            quality: Quality::Standard,
        })
        .unwrap();
    core.media_cancel(&waiting[0].id);
    let small = core
        .media_start(MediaRequest::Shrink {
            paths: vec![tone.clone()],
            target_bytes: 10_000_000,
            label: "Discord".into(),
        })
        .unwrap();

    let done = finished(&mut rx, &convert[0].id).await;
    assert_eq!(done.state, JobState::Done, "{done:?}");
    assert_eq!(
        done.output.as_deref(),
        Some(dir.path().join("tone.mp3").as_path())
    );
    assert!(done.after.unwrap() > 0);
    let stopped = core
        .media_jobs()
        .into_iter()
        .find(|j| j.id == waiting[0].id)
        .unwrap();
    let stopped = if stopped.state.finished() {
        stopped
    } else {
        finished(&mut rx, &waiting[0].id).await
    };
    assert_eq!(stopped.state, JobState::Cancelled);
    assert!(!dir.path().join("tone.flac").exists());
    let nothing = finished(&mut rx, &small[0].id).await;
    assert_eq!(nothing.state, JobState::Failed);
    assert_eq!(nothing.problem, Some(MediaProblem::AlreadySmall));
}
