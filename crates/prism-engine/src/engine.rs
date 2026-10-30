//! The engine: shared configuration, limiters and host table; entry points for jobs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use tokio::sync::Semaphore;

use crate::config::EngineConfig;
use crate::error::EngineError;
use crate::hostlimits::{HostLimit, HostLimits};
use crate::http::remote::RemoteFile;
use crate::job::{DownloadHandle, JobShared};
use crate::limiter::RateLimiter;
use crate::progress::ProbeInfo;
use crate::request::{DownloadRequest, MediaRequest};
use crate::resume::{ResumeKind, ResumeState};
use crate::{ftp, http, media};

/// Engine internals shared by all jobs.
pub(crate) struct EngineInner {
    config: RwLock<Arc<EngineConfig>>,
    /// Serializes `update_config` (budget resizing).
    update: Mutex<()>,
    pub global: Arc<RateLimiter>,
    pub host_limits: Arc<HostLimits>,
    /// Engine-wide budget for received-but-unwritten bytes.
    pub budget: Arc<Semaphore>,
    /// Permits the budget semaphore currently represents.
    budget_total: AtomicUsize,
    tls: OnceLock<Result<rustls::ClientConfig, String>>,
}

impl EngineInner {
    /// Snapshot of the current configuration (jobs take one when they start).
    pub(crate) fn config(&self) -> Arc<EngineConfig> {
        self.config.read().expect("config lock").clone()
    }

    /// Current size of the write-memory budget.
    pub(crate) fn budget_total(&self) -> usize {
        self.budget_total.load(Ordering::Acquire)
    }

    /// The shared rustls configuration (built once, on first use).
    pub(crate) fn tls(&self) -> Result<rustls::ClientConfig, EngineError> {
        self.tls
            .get_or_init(http::client::base_tls_config)
            .clone()
            .map_err(|e| EngineError::Network {
                message: format!("TLS setup failed: {e}"),
            })
    }

    /// Effective connection count for a request value (0 = default).
    pub(crate) fn connections(&self, requested: u8) -> usize {
        let cfg = self.config();
        let n = if requested == 0 {
            cfg.default_connections
        } else {
            requested
        };
        usize::from(n.clamp(1, cfg.max_connections_cap))
    }

    /// `(min, max)` bounded request sizes, honouring a per-request cap.
    pub(crate) fn request_sizes(&self, cap: Option<u64>) -> (u64, u64) {
        let cfg = self.config();
        let max = cap.map_or(cfg.max_request_size, |c| {
            c.clamp(64 * 1024, cfg.max_request_size)
        });
        (cfg.min_request_size.min(max), max)
    }
}

/// The download engine. Cheap to clone (all clones share limiters and the host table).
///
/// Jobs run on the Tokio runtime that calls [`Engine::start`] / [`Engine::start_media`];
/// those methods must be called from within a runtime.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("config", &self.inner.config())
            .finish()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl Engine {
    /// Creates an engine. Configuration values are clamped into sane ranges.
    pub fn new(config: EngineConfig) -> Engine {
        let config = config.normalized();
        Engine {
            inner: Arc::new(EngineInner {
                global: Arc::new(RateLimiter::unlimited()),
                host_limits: Arc::new(HostLimits::new(
                    config.max_connections_per_host,
                    config.host_limit_ttl,
                )),
                budget: Arc::new(Semaphore::new(config.memory_budget)),
                budget_total: AtomicUsize::new(config.memory_budget),
                tls: OnceLock::new(),
                config: RwLock::new(Arc::new(config)),
                update: Mutex::new(()),
            }),
        }
    }

    /// The current (normalized) configuration. Returns a shared snapshot; later
    /// [`Engine::update_config`] calls do not change an already returned value.
    pub fn config(&self) -> Arc<EngineConfig> {
        self.inner.config()
    }

    /// Atomically replaces the configuration. Jobs started afterwards use the new values
    /// (user agent, connections, timeouts, retries, proxy, sizes, ffmpeg path, …); running
    /// jobs keep the values they started with. The per-host cap and limit TTL apply
    /// immediately to new connections. The write-memory budget grows immediately and shrinks
    /// as far as currently unused memory allows.
    pub fn update_config(&self, config: EngineConfig) {
        let config = config.normalized();
        let _guard = self.inner.update.lock().expect("update lock");
        self.inner
            .host_limits
            .set_policy(config.max_connections_per_host, config.host_limit_ttl);
        let old = self.inner.budget_total();
        let new = config.memory_budget;
        let total = if new >= old {
            self.inner.budget.add_permits(new - old);
            new
        } else {
            old - self.inner.budget.forget_permits(old - new)
        };
        self.inner.budget_total.store(total, Ordering::Release);
        *self.inner.config.write().expect("config lock") = Arc::new(config);
    }

    /// The global speed limiter shared by every download (0 = unlimited; adjust live with
    /// [`RateLimiter::set_rate`]).
    pub fn global_limiter(&self) -> Arc<RateLimiter> {
        self.inner.global.clone()
    }

    /// Learned per-host connection limits (persist them to start below a server's limit next
    /// time; import with [`Engine::set_host_limit`]).
    pub fn host_limits(&self) -> Vec<HostLimit> {
        self.inner.host_limits.learned()
    }

    /// Sets (or with `None` forgets) a learned connection limit for `host` (`host[:port]`).
    pub fn set_host_limit(&self, host: &str, connections: Option<u32>) {
        self.inner
            .host_limits
            .set(host, connections.map(|c| c as usize));
    }

    /// Sends the first request of `req` (HTTP: `GET Range: bytes=0-…`; FTP: `SIZE`) and
    /// returns what the server reports, without creating any file.
    pub async fn probe(&self, req: &DownloadRequest) -> Result<ProbeInfo, EngineError> {
        match req.url.scheme() {
            "http" | "https" => {
                let ctx = http::download::build_ctx(&self.inner, req, req.url.clone(), None)?;
                let mut f = http::fetcher::HttpFetcher::new(ctx, 0);
                let (min_request, _) = self.inner.request_sizes(req.max_request_size);
                let probe = f
                    .probe(min_request, req.method == crate::request::HttpMethod::Get)
                    .await
                    .map_err(|e| e.error)?;
                let mut info = probe.info;
                if let Some(name) = req
                    .destination
                    .file_name
                    .as_deref()
                    .filter(|n| !n.trim().is_empty())
                {
                    info.file_name = crate::filename::sanitize(name);
                }
                Ok(info)
            }
            "ftp" | "ftps" => ftp::probe(&self.inner, req).await,
            s => Err(EngineError::InvalidUrl {
                message: format!("unsupported scheme {s}"),
            }),
        }
    }

    /// Opens `req` for reading parts of it with range requests, with the download's own
    /// cookies, headers, authentication and proxy (see [`RemoteFile`]). Nothing is sent yet.
    /// Only `GET` links over HTTP(S) can be read in parts.
    pub fn remote_file(&self, req: &DownloadRequest) -> Result<RemoteFile, EngineError> {
        RemoteFile::new(&self.inner, req)
    }

    /// Starts (or with `resume`, continues) an HTTP(S)/FTP(S) download. `extra_limiters` are
    /// applied between the download's own limiter and the global one (e.g. a queue limit).
    pub fn start(
        &self,
        req: DownloadRequest,
        resume: Option<ResumeState>,
        extra_limiters: Vec<Arc<RateLimiter>>,
    ) -> DownloadHandle {
        let scheme = req.url.scheme().to_string();
        let kind = if scheme.starts_with("ftp") {
            ResumeKind::Ftp
        } else {
            ResumeKind::Http
        };
        let (job, cmd_rx) = JobShared::new(req.url.clone(), kind, req.speed_limit);
        let handle = DownloadHandle {
            shared: job.clone(),
        };
        let engine = self.inner.clone();
        tokio::spawn(async move {
            let (result, state) = match scheme.as_str() {
                "http" | "https" => {
                    http::download::run(engine, job.clone(), req, resume, extra_limiters, cmd_rx)
                        .await
                }
                "ftp" | "ftps" => {
                    ftp::run(engine, job.clone(), req, resume, extra_limiters, cmd_rx).await
                }
                s => (
                    Err(EngineError::InvalidUrl {
                        message: format!("unsupported scheme {s}"),
                    }),
                    None,
                ),
            };
            job.finish(result, state);
        });
        handle
    }

    /// Starts (or resumes) an HLS/DASH download. Snapshot segments are media segments.
    pub fn start_media(
        &self,
        req: MediaRequest,
        resume: Option<ResumeState>,
        extra_limiters: Vec<Arc<RateLimiter>>,
    ) -> DownloadHandle {
        let (job, cmd_rx) = JobShared::new(req.url.clone(), ResumeKind::Media, req.speed_limit);
        let handle = DownloadHandle {
            shared: job.clone(),
        };
        let engine = self.inner.clone();
        tokio::spawn(async move {
            let (result, state) = if matches!(req.url.scheme(), "http" | "https") {
                media::run(engine, job.clone(), req, resume, extra_limiters, cmd_rx).await
            } else {
                (
                    Err(EngineError::InvalidUrl {
                        message: format!("unsupported scheme {}", req.url.scheme()),
                    }),
                    None,
                )
            };
            job.finish(result, state);
        });
        handle
    }
}
