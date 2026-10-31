//! #43: a yt-dlp from a Linux distribution's packages, older than `--js-runtimes`, stopped every
//! download with "no such option: --js-runtimes". Prism checks the version of a yt-dlp that isn't
//! its own, leaves the option out for an old one, and installs its own yt-dlp in its place (see
//! `VideoService::needs_tools`).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use prism_video::{VideoOptions, VideoService, YtDlpJob};

/// A stand-in yt-dlp of `version` that fails every real run.
fn script(dir: &Path, name: &str, version: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo {version}; exit 0; fi\n\
             echo 'yt-dlp: error: no such option: --js-runtimes' >&2; exit 2\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn job(dir: &Path) -> YtDlpJob {
    YtDlpJob {
        url: "https://example.com/v/1".into(),
        args: vec![],
        output_dir: dir.to_path_buf(),
        context: Default::default(),
        reason: "test".into(),
        section: None,
    }
}

fn has_js_runtimes(svc: &VideoService, dir: &Path) -> bool {
    svc.ytdlp_command_line(&job(dir), None)
        .iter()
        .any(|a| a == "--js-runtimes")
}

#[tokio::test]
async fn an_old_ytdlp_gets_no_js_runtime_option() {
    let dir = tempfile::tempdir().unwrap();
    let deno = script(dir.path(), "deno", "2.5.6");
    let svc = |ytdlp: PathBuf| {
        VideoService::new(
            dir.path().join("tools"),
            VideoOptions {
                ytdlp_path: Some(ytdlp),
                deno_path: Some(deno.clone()),
                ..Default::default()
            },
        )
    };

    let old = svc(script(dir.path(), "yt-dlp-old", "2024.04.09"));
    // Before its version is known, nothing changes.
    assert!(has_js_runtimes(&old, dir.path()));
    // A probe asks for it first (and then fails: the script is no real yt-dlp)…
    let _ = old
        .probe("https://example.com/v/1", &Default::default())
        .await;
    // …and the old yt-dlp no longer gets the option it doesn't know.
    assert!(!has_js_runtimes(&old, dir.path()));

    let current = svc(script(dir.path(), "yt-dlp-new", "2026.10.29"));
    let _ = current
        .probe("https://example.com/v/1", &Default::default())
        .await;
    assert!(has_js_runtimes(&current, dir.path()));
}

#[tokio::test]
async fn an_old_system_ytdlp_gets_prisms_own_installed() {
    // The only yt-dlp is an old one on PATH (a distribution's package); Prism has none yet.
    let dir = tempfile::tempdir().unwrap();
    let old_bin = dir.path().join("old");
    let new_bin = dir.path().join("new");
    std::fs::create_dir_all(&old_bin).unwrap();
    std::fs::create_dir_all(&new_bin).unwrap();
    script(&old_bin, "yt-dlp", "2025.09.26");
    script(&new_bin, "yt-dlp", "2025.11.12");
    let deno = script(dir.path(), "deno", "2.5.6");
    let svc = VideoService::new(
        dir.path().join("tools"),
        VideoOptions {
            deno_path: Some(deno),
            ..Default::default()
        },
    );
    // SAFETY: the only test in this binary that reads PATH.
    unsafe { std::env::set_var("PATH", &old_bin) };
    assert!(svc.needs_tools().await, "an old yt-dlp is replaced");
    unsafe { std::env::set_var("PATH", &new_bin) };
    assert!(!svc.needs_tools().await, "a current one is used as it is");
}
