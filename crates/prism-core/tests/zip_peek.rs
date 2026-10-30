//! "What's inside": listing a ZIP from its end and downloading single files of it, over range
//! requests to the local test server, with the capture's cookies, referer, user agent and
//! credentials.

use prism_core::events::{CaptureInfo, CaptureSource, CoreEvent};
use prism_core::manager::{Core, CoreOptions, NewItem, PeekProblem, ZipFile};
use prism_testserver::{RequestLog, ServerConfig, TestServer};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

async fn core_in(dir: &Path) -> Core {
    let core = Core::start(CoreOptions {
        data_dir: dir.join("data"),
        ipc: false,
        clipboard: false,
    })
    .await
    .unwrap();
    core.update_settings(&serde_json::json!({
        "downloads": {"defaultDir": dir.join("dl"), "useCategoryFolders": false},
        "antivirus": {"enabled": false},
    }))
    .unwrap();
    core
}

/// Noise that doesn't compress.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

/// A 2.5 MB archive: photos stored as they are, text deflated, a folder entry.
fn archive() -> (Vec<u8>, Vec<u8>) {
    let manual: Vec<u8> = (0..60_000)
        .flat_map(|i| format!("Line {i}: how to use the thing.\n").into_bytes())
        .collect();
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.start_file("readme.txt", deflated).unwrap();
    w.write_all(b"Read me first.\n").unwrap();
    w.add_directory("photos/", stored).unwrap();
    w.start_file("photos/IMG_0001.jpg", stored).unwrap();
    w.write_all(&noise(1_200_000, 1)).unwrap();
    w.start_file("photos/IMG_0002.jpg", stored).unwrap();
    w.write_all(&noise(1_000_000, 2)).unwrap();
    w.start_file("docs/manual.txt", deflated).unwrap();
    w.write_all(&manual).unwrap();
    (w.finish().unwrap().into_inner(), manual)
}

/// Captures `url` as the browser would hand it over, and waits until the link was tried.
async fn capture(core: &Core, n: NewItem) -> CaptureInfo {
    let mut rx = core.subscribe();
    let info = core.capture(n, CaptureSource::Browser);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(CoreEvent::CaptureUpdated(c)) = rx.recv().await
                && c.id == info.id
            {
                return c;
            }
        }
    })
    .await
    .expect("the link was tried")
}

/// Bytes a logged request asked for.
fn asked(r: &RequestLog) -> u64 {
    let range = r.range.as_deref().expect("every request asks for a range");
    let (a, b) = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .unwrap();
    b.parse::<u64>().unwrap() - a.parse::<u64>().unwrap() + 1
}

fn file<'a>(files: &'a [ZipFile], path: &str) -> &'a ZipFile {
    files.iter().find(|f| f.path == path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lists_a_zip_from_its_end_and_downloads_single_files() {
    let server = TestServer::start(ServerConfig::default()).await.unwrap();
    let (zip, manual) = archive();
    server.put_blob("archive.zip", zip.clone(), "application/zip");
    let tmp = tempfile::tempdir().unwrap();
    let core = core_in(tmp.path()).await;

    // Through a redirect, with the browser's cookies, referer and user agent.
    let info = capture(
        &core,
        NewItem {
            url: server.url("/redirect/1?to=/blob/archive.zip"),
            cookies: Some("session=abc".into()),
            referer: Some("https://example.com/downloads".into()),
            user_agent: Some("PrismTest/1.0".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(info.resumable, Some(true));
    assert_eq!(info.size, Some(zip.len() as u64));
    server.reset_stats();

    let files = core.zip_contents(&info.id).await.unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "readme.txt",
            "photos/IMG_0001.jpg",
            "photos/IMG_0002.jpg",
            "docs/manual.txt"
        ],
        "folders aren't listed on their own"
    );
    assert_eq!(file(&files, "docs/manual.txt").size, manual.len() as u64);
    assert!(files.iter().all(|f| f.extractable && !f.encrypted));

    let log = server.stats().requests;
    // The redirect once, then the archive's last 64 KiB (they hold this small directory).
    assert_eq!(log.len(), 2, "{log:#?}");
    assert!(log[0].uri.starts_with("/redirect/"));
    let read: u64 = log[1..].iter().map(asked).sum();
    assert!(read < 70_000, "read {read} bytes of {}", zip.len());
    for r in &log {
        assert_eq!(r.cookie.as_deref(), Some("session=abc"));
        assert_eq!(r.referer.as_deref(), Some("https://example.com/downloads"));
        assert_eq!(r.user_agent.as_deref(), Some("PrismTest/1.0"));
    }

    // One file: its header, then its data, straight from where the redirect led.
    server.reset_stats();
    let out = tmp.path().join("out");
    let manual_file = file(&files, "docs/manual.txt");
    let saved = core
        .extract_zip_file(&info.id, manual_file.index, &out)
        .await
        .unwrap();
    assert_eq!(saved, out.join("manual.txt"));
    assert_eq!(std::fs::read(&saved).unwrap(), manual);
    let log = server.stats().requests;
    assert_eq!(log.len(), 2, "{log:#?}");
    assert!(log.iter().all(|r| r.uri == "/blob/archive.zip"));
    assert_eq!(asked(&log[0]), 30);
    assert!(log.iter().map(asked).sum::<u64>() < manual.len() as u64 / 4);
    assert!(
        log.iter()
            .all(|r| r.cookie.as_deref() == Some("session=abc"))
    );

    // A file of that name already there: a numbered name, the other file left alone.
    std::fs::write(out.join("readme.txt"), b"mine").unwrap();
    let readme = file(&files, "readme.txt");
    let saved = core
        .extract_zip_file(&info.id, readme.index, &out)
        .await
        .unwrap();
    assert_eq!(saved, out.join("readme (1).txt"));
    assert_eq!(std::fs::read(&saved).unwrap(), b"Read me first.\n");
    assert_eq!(std::fs::read(out.join("readme.txt")).unwrap(), b"mine");

    // A stored file, read again from the cached list.
    let photo = file(&files, "photos/IMG_0002.jpg");
    let saved = core
        .extract_zip_file(&info.id, photo.index, &out)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&saved).unwrap(), noise(1_000_000, 2));

    // The archive changed on the server since it was listed: no mixed bytes, nothing left.
    let mut changed = zip.clone();
    let at = changed.len() / 2;
    changed[at] ^= 0xff;
    server.put_blob("archive.zip", changed, "application/zip");
    let photo = file(&files, "photos/IMG_0001.jpg");
    assert_eq!(
        core.extract_zip_file(&info.id, photo.index, &out).await,
        Err(PeekProblem::Changed)
    );
    assert!(!out.join("IMG_0001.jpg").exists());
    assert!(!out.join("IMG_0001.jpg.prismpart").exists());

    // The window went away.
    core.cancel_capture(&info.id);
    assert_eq!(
        core.extract_zip_file(&info.id, readme.index, &out).await,
        Err(PeekProblem::Cancelled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_and_why_a_list_can_t_be_read() {
    let server = TestServer::start(ServerConfig::default()).await.unwrap();
    let (zip, _) = archive();
    server.put_blob("archive.zip", zip, "application/zip");
    let tmp = tempfile::tempdir().unwrap();
    let core = core_in(tmp.path()).await;
    let link = |path: &str| NewItem {
        url: server.url(path),
        ..Default::default()
    };

    // HTTP authentication: with the user's credentials it works, without them it's explained.
    let info = capture(
        &core,
        NewItem {
            username: Some("user".into()),
            password: Some("pass".into()),
            ..link("/blob/archive.zip?auth=digest")
        },
    )
    .await;
    assert_eq!(core.zip_contents(&info.id).await.unwrap().len(), 4);
    let info = capture(&core, link("/blob/archive.zip?auth=basic")).await;
    assert_eq!(
        core.zip_contents(&info.id).await,
        Err(PeekProblem::Protected)
    );

    // A server that sends only the whole file.
    let info = capture(&core, link("/blob/archive.zip?ranges=0")).await;
    assert_eq!(info.resumable, Some(false));
    assert_eq!(
        core.zip_contents(&info.id).await,
        Err(PeekProblem::NoRanges)
    );

    // Something else under a .zip name.
    let info = capture(&core, link("/file/not-really.zip?size=300000")).await;
    assert_eq!(core.zip_contents(&info.id).await, Err(PeekProblem::NotZip));

    // Gone from the server.
    let info = capture(&core, link("/blob/archive.zip")).await;
    server.put_blob("archive.zip", Vec::new(), "application/zip");
    assert!(core.zip_contents(&info.id).await.is_err());
    assert_eq!(
        core.zip_contents("no-such-capture").await,
        Err(PeekProblem::Cancelled)
    );
}
