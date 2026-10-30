//! Multi-network tests. Connections are pinned to adapters by name, so these run where the
//! loopback adapter can be bound to (Linux: `lo`); the test server listens on 127.0.0.1.
#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

use common::*;
use prism_engine::{DownloadEvent, MediaRequest, NetworkSpec, Url, list_interfaces};
use prism_testserver::{HlsFixture, LimitMode, ServerConfig, expected_sha256};

fn net(interface: &str, enabled: bool, speed_limit: u64) -> NetworkSpec {
    NetworkSpec {
        interface: interface.into(),
        enabled,
        speed_limit,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_run_over_the_configured_network() {
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 16 * MIB;
    let e = engine();
    // An adapter that does not exist is kept in the list but gets no connections.
    e.networks()
        .configure(&[net("lo", true, 0), net("prism-missing0", true, 0)]);
    assert!(e.networks().is_active());
    let h = e.start(
        request(
            &srv.url(&format!("/file/n.bin?size={size}&seed=31&throttle=8M")),
            dir.path(),
            4,
        ),
        None,
        vec![],
    );
    let done = wait(&h, 60).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(31, size));
    let stats = e.networks().stats();
    let lo = stats.iter().find(|s| s.interface == "lo").unwrap();
    let missing = stats
        .iter()
        .find(|s| s.interface == "prism-missing0")
        .unwrap();
    assert!(lo.present);
    assert!(!missing.present);
    assert!(lo.received >= size);
    assert_eq!(missing.received, 0);
    assert_eq!(lo.connections, 0, "leases are released when workers end");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_speed_limit_applies_across_connections() {
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 6 * MIB;
    let e = engine();
    e.networks().configure(&[net("lo", true, 2 * MIB)]);
    let started = Instant::now();
    let h = e.start(
        request(
            &srv.url(&format!("/file/l.bin?size={size}&seed=32")),
            dir.path(),
            8,
        ),
        None,
        vec![],
    );
    let done = wait(&h, 60).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(32, size));
    // 6 MiB at 2 MiB/s (with a small burst allowance) takes well over two seconds.
    assert!(
        started.elapsed() >= Duration::from_millis(2300),
        "finished in {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_networks_fall_back_to_the_default_route() {
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let e = engine();
    e.networks().configure(&[net("lo", false, 0)]);
    assert!(!e.networks().is_active());
    let h = e.start(
        request(
            &srv.url(&format!("/file/d.bin?size={size}&seed=33")),
            dir.path(),
            4,
        ),
        None,
        vec![],
    );
    let done = wait(&h, 60).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(33, size));
    assert_eq!(e.networks().get("lo").unwrap().received(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_segments_run_over_the_configured_network() {
    let fixture = HlsFixture::synthetic(1, 12, 64 * 1024, 1);
    let srv = server(ServerConfig {
        hls: Some(fixture.clone()),
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine();
    e.networks()
        .configure(&[net("lo", true, 0), net("prism-missing0", true, 0)]);
    let mut req = MediaRequest::new(
        Url::parse(&srv.url("/hls/master.m3u8")).unwrap(),
        dir.path(),
    );
    req.remux = false;
    req.connections = 4;
    let h = e.start_media(req, None, vec![]);
    let done = wait(&h, 60).await.unwrap();
    assert_eq!(
        sha256_file(&done.path),
        sha256(&fixture.expected_variant_bytes(0))
    );
    let lo = e.networks().get("lo").unwrap();
    assert!(lo.received() >= 12 * 64 * 1024, "segments counted on lo");
    assert_eq!(lo.connections(), 0, "leases are released when workers end");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switched_off_network_is_not_used_when_the_chosen_ones_are_gone() {
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let e = engine();
    // "lo" switched off but connected; the only network switched on is unplugged.
    e.networks()
        .configure(&[net("lo", false, 0), net("prism-missing0", true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/x.bin?size={size}&seed=34")),
            dir.path(),
            4,
        ),
        None,
        vec![],
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(h.snapshot().downloaded, 0, "nothing over the default route");
    // A chosen network appears: the download goes ahead over it.
    e.networks()
        .configure(&[net("lo", true, 0), net("prism-missing0", true, 0)]);
    let done = wait(&h, 60).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(34, size));
    assert!(e.networks().get("lo").unwrap().received() >= size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_network_that_cannot_reach_the_server_is_benched() {
    // Any real adapter cannot reach 127.0.0.1 when the socket is pinned to it.
    let Some(other) = list_interfaces().into_iter().next() else {
        eprintln!("no non-loopback adapter: skipped");
        return;
    };
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 16 * MIB;
    let e = engine();
    e.networks()
        .configure(&[net(&other.name, true, 0), net("lo", true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/b.bin?size={size}&seed=34&throttle=8M")),
            dir.path(),
            6,
        ),
        None,
        vec![],
    );
    let done = wait(&h, 90).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(34, size));
    let dead = e.networks().get(&other.name).unwrap();
    assert_eq!(dead.received(), 0);
    // At least the file (raced tails are received twice).
    assert!(e.networks().get("lo").unwrap().received() >= size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_a_network_off_moves_connections_away() {
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 24 * MIB;
    let e = engine();
    e.networks().configure(&[net("lo", true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/s.bin?size={size}&seed=35&throttle=1M")),
            dir.path(),
            4,
        ),
        None,
        vec![],
    );
    let lo = e.networks().get("lo").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = lo.received();
    assert!(before > 0);
    e.networks().set_enabled("lo", false);
    let done = wait(&h, 90).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(35, size));
    // Connections finish their current request on "lo", then reconnect over the default route.
    assert!(
        lo.received() < size,
        "everything still went over the disabled network"
    );
    assert_eq!(lo.connections(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_connection_on_a_dead_network_moves_to_a_working_one() {
    let Some(other) = list_interfaces().into_iter().next() else {
        eprintln!("no non-loopback adapter: skipped");
        return;
    };
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let e = engine();
    // The dead network comes first, so the only connection starts on it.
    e.networks()
        .configure(&[net(&other.name, true, 0), net("lo", true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/s.bin?size={size}&seed=36")),
            dir.path(),
            1,
        ),
        None,
        vec![],
    );
    let done = wait(&h, 90).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(36, size));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_connection_limit_is_learned_on_a_pinned_network() {
    let srv = server(ServerConfig {
        max_connections: Some(4),
        limit_mode: LimitMode::Reset,
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let size = 24 * MIB;
    let e = engine();
    e.networks().configure(&[net("lo", true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/r.bin?size={size}&seed=37&throttle=4M")),
            dir.path(),
            16,
        ),
        None,
        vec![],
    );
    let events = collect_events(&h);
    let done = wait(&h, 120).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(37, size));
    let detected = events
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, DownloadEvent::ServerLimitDetected { .. }));
    assert!(
        detected,
        "the resets are a server limit, not a dead network"
    );
    assert!(!e.networks().get("lo").unwrap().is_failing());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_network_is_not_taken_for_a_server_limit() {
    let Some(other) = list_interfaces().into_iter().next() else {
        eprintln!("no non-loopback adapter: skipped");
        return;
    };
    let srv = default_server().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 16 * MIB;
    let e = engine();
    e.networks()
        .configure(&[net("lo", true, 0), net(&other.name, true, 0)]);
    let h = e.start(
        request(
            &srv.url(&format!("/file/d.bin?size={size}&seed=38&throttle=8M")),
            dir.path(),
            8,
        ),
        None,
        vec![],
    );
    let events = collect_events(&h);
    let done = wait(&h, 90).await.unwrap();
    assert_eq!(sha256_file(&done.path), expected_sha256(38, size));
    let limit = events.lock().unwrap().iter().find_map(|e| match e {
        DownloadEvent::ServerLimitDetected { connections } => Some(*connections),
        _ => None,
    });
    assert_eq!(limit, None, "the server has no connection limit");
    assert!(e.host_limits().is_empty(), "{:?}", e.host_limits());
    assert_eq!(e.networks().get(&other.name).unwrap().received(), 0);
}
