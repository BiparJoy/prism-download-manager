//! Protocol-independent multi-connection machinery: the segment table, the writer thread,
//! connection workers and the supervisor that spawns, ramps and retires them.
//!
//! Protocols (HTTP, FTP) plug in through [`RangeFetcher`]: "give me a byte stream starting at
//! offset X (ideally ending at Y)".

pub(crate) mod supervisor;
pub(crate) mod table;
pub(crate) mod worker;
pub(crate) mod writer;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::error::EngineError;
use crate::hostlimits::{HostLimits, HostPermit};
use crate::job::JobShared;
use crate::limiter::{RateLimiter, any_limited};
use crate::network::{Network, NetworkSet};
use table::{Assignment, SegmentTable, WorkerId};
use writer::Writer;

/// How the worker should react to a failed open/read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailKind {
    /// Retry with backoff.
    Transient,
    /// The server explicitly refused this (additional) connection (`429`, `503`, `403` after
    /// a successful probe, FTP `421`): a connection-limit signal when other connections work.
    Refused,
    /// The connection failed before any response (reset/refused while connecting or sending):
    /// only a limit signal if it happens repeatedly before the worker ever received data;
    /// otherwise a transient error (e.g. a dropped connection).
    MaybeRefused,
    /// Stop the whole download.
    Fatal,
}

/// A failure reported by a fetcher.
#[derive(Debug, Clone)]
pub(crate) struct FetchError {
    pub kind: FailKind,
    pub error: EngineError,
    /// Server-suggested delay (`Retry-After`).
    pub retry_after: Option<Duration>,
    /// No connection could be made (refused, unreachable, timed out, no usable address):
    /// on a pinned network that is about the network, not a server limit.
    pub connect: bool,
}

impl FetchError {
    pub(crate) fn transient(error: EngineError) -> Self {
        Self {
            kind: FailKind::Transient,
            error,
            retry_after: None,
            connect: false,
        }
    }
    pub(crate) fn fatal(error: EngineError) -> Self {
        Self {
            kind: FailKind::Fatal,
            error,
            retry_after: None,
            connect: false,
        }
    }
    pub(crate) fn refused(error: EngineError, retry_after: Option<Duration>) -> Self {
        Self {
            kind: FailKind::Refused,
            error,
            retry_after,
            connect: false,
        }
    }
    pub(crate) fn maybe_refused(error: EngineError) -> Self {
        Self {
            kind: FailKind::MaybeRefused,
            error,
            retry_after: None,
            connect: false,
        }
    }
}

/// A stream of body chunks for one request.
pub(crate) trait ChunkStream: Send + 'static {
    /// Next chunk; `Ok(None)` at the end of the response.
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Bytes>, FetchError>> + Send;
    /// Exclusive end offset the server promised for this response, if known (a server may
    /// answer a range request with a shorter range than asked for).
    fn range_end(&self) -> Option<u64> {
        None
    }
}

/// One connection's way of fetching byte ranges.
pub(crate) trait RangeFetcher: Send + 'static {
    /// The stream type.
    type Stream: ChunkStream;
    /// Opens a stream starting at byte `from`. `to` is the exclusive end the worker wants; when
    /// [`RangeFetcher::bounded`] is false the stream may continue past it (the worker stops
    /// reading at the segment end). `None` = until EOF (unknown size).
    fn open(
        &mut self,
        from: u64,
        to: Option<u64>,
    ) -> impl Future<Output = Result<Self::Stream, FetchError>> + Send;
    /// Whether streams end exactly at the requested `to` (HTTP ranges) so the connection can
    /// be reused for the next range.
    fn bounded(&self) -> bool;
    /// The network this connection is pinned to, if any (multi-network downloads).
    fn network(&self) -> Option<&Arc<Network>> {
        None
    }
}

/// Creates per-worker fetchers.
pub(crate) trait FetcherFactory: Send + Sync + 'static {
    type Fetcher: RangeFetcher;
    fn create(&self, worker: WorkerId) -> Self::Fetcher;
}

/// Shared context of one segmented download.
pub(crate) struct SegCtx {
    pub job: Arc<JobShared>,
    pub table: Arc<SegmentTable>,
    pub writer: Arc<Writer>,
    /// `[download, extra…, global]`.
    pub limiters: Vec<Arc<RateLimiter>>,
    /// Cancelled to stop all workers (pause, cancel, fatal error, completion).
    pub stop: CancellationToken,
    pub host: String,
    pub host_limits: Arc<HostLimits>,
    /// Workers with a working response stream.
    pub connected: AtomicUsize,
    /// Signalled when a worker gets its first working stream (drives the ramp-up).
    pub connected_notify: tokio::sync::Notify,
    /// Running workers.
    pub active_workers: AtomicUsize,
    /// Workers above this number retire after their current assignment.
    pub target: AtomicUsize,
    /// Bumped when the source URL changes: workers re-open their streams.
    pub source_generation: Arc<AtomicU64>,
    pub min_request: u64,
    pub max_request: u64,
    pub read_timeout: Duration,
    pub max_retries: u32,
    /// Whether duplicate racing of slow tails is allowed.
    pub race: bool,
    /// The networks connections may be pinned to (empty for FTP).
    pub networks: Arc<NetworkSet>,
    /// Set once the server rate-limited requests (`429`/`503`): from then on every request
    /// covers the whole remaining segment, minimizing the number of requests.
    pub few_requests: AtomicBool,
}

impl SegCtx {
    /// Whether slow tails may be raced right now: not under any speed limit, a network's
    /// included, where the duplicate bytes would eat the capped budget. The supervisor and
    /// the workers must agree, or idle workers are spawned for races they then refuse.
    pub(crate) fn may_race(&self) -> bool {
        self.race && !any_limited(&self.limiters) && !self.networks.any_capped()
    }

    /// Atomically retires one worker if more are running than targeted.
    pub(crate) fn try_retire(&self) -> bool {
        let mut cur = self.active_workers.load(Ordering::Acquire);
        loop {
            if cur <= self.target.load(Ordering::Acquire).max(1) {
                return false;
            }
            match self.active_workers.compare_exchange(
                cur,
                cur - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }
}

/// The first worker's pre-opened stream (the probe response becomes segment #1).
pub(crate) struct InitialStream<S> {
    pub assignment: Assignment,
    pub stream: S,
    /// Exclusive end of the probe request (`None` = open-ended).
    pub req_end: Option<u64>,
    pub permit: HostPermit,
}

/// Why a worker ended.
#[derive(Debug)]
pub(crate) enum WorkerExit {
    /// No work left for it.
    Idle,
    /// Retired because the target connection count dropped.
    Retired,
    /// The job is stopping.
    Stopped,
    /// The server refused the connection (connection-limit signal).
    Refused { retry_after: Option<Duration> },
    /// Too many consecutive failures without progress.
    GaveUp(EngineError),
    /// A fatal error: the download must stop.
    Fatal(EngineError),
}
