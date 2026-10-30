//! # prism-testserver
//!
//! A configurable local HTTP/1.1 server used by the Prism engine tests and benchmarks.
//!
//! File bodies are generated on the fly from `(seed, offset)` (see [`content`]), so arbitrarily
//! large files cost no memory and every range can be verified with [`expected_bytes`] /
//! [`expected_sha256`].
//!
//! ## Routes
//!
//! | Route | Behaviour |
//! |---|---|
//! | `/file/<name>?opts` (also `/`) | generated file; options in [`FileOptions`] (`size`, `seed`, `ranges`, `throttle`, `slow_conn`/`slow_rate`, `drop_after`, `drop_prob`, `ttfb_ms`, `etag`, `weak_etag`, `lm`, `cd`, `type`, `chunked`, `auth`) |
//! | `/expiring/<key>?ttl_ms=N&opts` | like `/file`, but answers `410 Gone` once `ttl_ms` elapsed since the first request to that path (signed-URL simulation) |
//! | `/redirect/<hops>?code=302&to=<target>` | redirect chain of `hops` hops ending at `to`; `{n}` in `to` is replaced by a counter, producing a fresh URL each time |
//! | `/blob/<name>?opts` | bytes added with [`TestServer::put_blob`] (`ranges`, `auth`) |
//! | `/status/<code>` | answers with that status code |
//! | `/challenge` | a Cloudflare bot check: `403`, `cf-mitigated: challenge`, an HTML page |
//! | `/hls/...` | [`HlsFixture`] |
//! | `/dash/...` | [`DashFixture`] |
//!
//! Server-wide behaviour (connection limits, credentials, defaults) lives in [`ServerConfig`].
//! [`TestServer::stats`] exposes per-connection request counts (keyed by peer address) and a
//! request log, so tests can assert connection reuse, headers and URLs.
//!
//! [`FtpServer`] is a minimal FTP server (passive `REST` + `RETR`) serving generated files.

pub mod content;
mod ftp;
pub mod media;
pub mod options;
mod server;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub use content::{expected_bytes, expected_sha256, sha256_hex, versioned_seed};
pub use ftp::FtpServer;
pub use media::{DashFixture, DashRepresentation, HlsFixture, HlsVariant};
pub use options::{AuthMode, FileOptions, LimitMode, ServerConfig, parse_size};
pub use server::{ConnectionStats, RequestLog, ServerStats};

/// A running test server. Dropping it shuts the server down.
pub struct TestServer {
    addr: SocketAddr,
    state: Arc<server::State>,
    shutdown: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

impl TestServer {
    /// Binds and starts serving in the background on the current Tokio runtime.
    pub async fn start(config: ServerConfig) -> io::Result<TestServer> {
        let listener = TcpListener::bind((config.bind, config.port)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(server::State::new(config));
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(server::run(listener, state.clone(), shutdown.clone()));
        Ok(TestServer {
            addr,
            state,
            shutdown,
            task: Some(task),
        })
    }

    /// Socket address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://ip:port` (no trailing slash).
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Absolute URL for a path (with optional query), e.g. `url("/file/a.bin?size=1M")`.
    pub fn url(&self, path_and_query: &str) -> String {
        format!("{}{}", self.base_url(), path_and_query)
    }

    /// Current statistics.
    pub fn stats(&self) -> ServerStats {
        self.state.stats()
    }

    /// Clears request logs and counters (open connections are kept with zeroed counters).
    pub fn reset_stats(&self) {
        self.state.reset_stats()
    }

    /// Changes the content version of `/file/<name>` (new bytes, ETag and Last-Modified).
    pub fn set_version(&self, name: &str, version: u64) {
        self.state.set_version(name, version)
    }

    /// Serves `data` at `/blob/<name>` (replacing what was there: a new ETag).
    pub fn put_blob(&self, name: &str, data: impl Into<Bytes>, content_type: &str) {
        self.state.put_blob(name, data.into(), content_type)
    }

    /// Changes the concurrent connection limit at runtime.
    pub fn set_max_connections(&self, max: Option<usize>) {
        self.state.set_max_connections(max)
    }

    /// The configuration the server was started with.
    pub fn config(&self) -> &ServerConfig {
        &self.state.config
    }

    /// Stops accepting connections and closes all open connections.
    pub async fn shutdown(mut self) {
        self.shutdown.notify_one();
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}
