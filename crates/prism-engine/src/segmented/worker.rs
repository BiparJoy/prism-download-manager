//! A connection worker: repeatedly takes work from the segment table, streams it through the
//! rate limiters into the writer, and handles retries, stalls, races and retirement.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::table::{AcquireKind, Assignment, UNKNOWN_END, WorkerId};
use super::{ChunkStream, FailKind, FetchError, InitialStream, RangeFetcher, SegCtx, WorkerExit};
use crate::backoff::Backoff;
use crate::error::EngineError;
use crate::hostlimits::HostPermit;
use crate::limiter::{acquire_all, any_limited};
use crate::progress::DownloadEvent;

/// Upper bound for a server-suggested `Retry-After`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Target duration of one bounded request at the connection's measured speed.
const REQUEST_SECONDS: f64 = 2.0;

enum Pump {
    /// The assignment is finished (or was shrunk/raced away below the cursor).
    SegmentDone,
    /// The bounded request is finished; the assignment continues with a new request.
    RangeDone,
    /// Re-open the stream (source URL changed).
    Reopen,
    /// The job is stopping.
    Stopped,
}

struct Worker<F: RangeFetcher> {
    ctx: Arc<SegCtx>,
    id: WorkerId,
    fetcher: F,
    permit: HostPermit,
    connected: bool,
    allow_race: bool,
    request_size: u64,
    /// Received bytes since the last failure (resets the retry counter).
    progressed: bool,
    /// The worker has received data at least once.
    ever_streamed: bool,
    /// Consecutive connection failures before the first byte was ever received.
    early_failures: u32,
}

/// Runs a worker to completion. The caller has already incremented `ctx.active_workers`.
pub(crate) async fn run_worker<F: RangeFetcher>(
    ctx: Arc<SegCtx>,
    id: WorkerId,
    fetcher: F,
    initial: Option<InitialStream<F::Stream>>,
    allow_race: bool,
) -> WorkerExit {
    let (permit, initial) = match initial {
        Some(i) => (i.permit, Some((i.assignment, i.stream, i.req_end))),
        None => {
            let p = tokio::select! {
                p = ctx.host_limits.acquire(&ctx.host) => p,
                _ = ctx.stop.cancelled() => {
                    ctx.active_workers.fetch_sub(1, Ordering::AcqRel);
                    return WorkerExit::Stopped;
                }
            };
            (p, None)
        }
    };
    let mut w = Worker {
        request_size: ctx.min_request,
        ctx: ctx.clone(),
        id,
        fetcher,
        permit,
        connected: false,
        allow_race,
        progressed: false,
        ever_streamed: false,
        early_failures: 0,
    };
    let exit = w.run(initial).await;
    w.set_connected(false);
    if !matches!(exit, WorkerExit::Retired) {
        ctx.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
    exit
}

impl<F: RangeFetcher> Worker<F> {
    fn set_connected(&mut self, connected: bool) {
        if self.connected == connected {
            return;
        }
        self.connected = connected;
        self.permit.set_connected(connected);
        if connected {
            self.ctx.connected.fetch_add(1, Ordering::AcqRel);
            self.ctx.connected_notify.notify_one();
        } else {
            self.ctx.connected.fetch_sub(1, Ordering::AcqRel);
        }
    }

    async fn run(
        &mut self,
        mut initial: Option<(Assignment, F::Stream, Option<u64>)>,
    ) -> WorkerExit {
        let ctx = self.ctx.clone();
        let mut current: Option<Assignment> = initial.as_ref().map(|(a, _, _)| *a);
        let mut cursor = current.map_or(0, |a| a.cursor);
        let mut backoff = Backoff::default();
        let mut failures = 0u32;

        loop {
            if ctx.stop.is_cancelled() {
                self.release(current, false);
                return WorkerExit::Stopped;
            }
            let a = match current {
                Some(a) => a,
                None => {
                    if ctx.try_retire() {
                        return WorkerExit::Retired;
                    }
                    // Its network was switched off: a replacement connection goes elsewhere.
                    if self.fetcher.network().is_some_and(|n| !n.is_enabled()) {
                        return WorkerExit::Idle;
                    }
                    match ctx
                        .table
                        .acquire(self.id, self.allow_race && !any_limited(&ctx.limiters))
                    {
                        Some((a, kind)) => {
                            match kind {
                                AcquireKind::Unowned => {}
                                AcquireKind::Split { at, end } => {
                                    ctx.job.emit(DownloadEvent::SegmentSplit {
                                        at,
                                        end,
                                        race: false,
                                    })
                                }
                                AcquireKind::Race { end } => {
                                    ctx.job.emit(DownloadEvent::SegmentSplit {
                                        at: a.cursor,
                                        end,
                                        race: true,
                                    })
                                }
                            }
                            current = Some(a);
                            cursor = a.cursor;
                            a
                        }
                        None => return WorkerExit::Idle,
                    }
                }
            };

            // Open a stream (or take over the probe's).
            let (stream, req_end) = match initial.take() {
                Some((_, s, re)) => (s, re),
                None => {
                    let end = ctx.table.end_of(a.seg);
                    if cursor >= end {
                        current = None;
                        continue;
                    }
                    let req_end = if end == UNKNOWN_END {
                        None
                    } else if self.fetcher.bounded()
                        && !a.race
                        && !ctx.few_requests.load(Ordering::Relaxed)
                    {
                        Some(end.min(cursor.saturating_add(self.request_size)))
                    } else {
                        Some(end)
                    };
                    if !a.race {
                        ctx.table
                            .set_request_end(a.seg, self.id, req_end.unwrap_or(0));
                    }
                    let opened = tokio::select! {
                        r = self.fetcher.open(cursor, req_end) => r,
                        _ = ctx.stop.cancelled() => {
                            self.release(current, false);
                            return WorkerExit::Stopped;
                        }
                    };
                    match opened {
                        Ok(s) => (s, req_end),
                        Err(e) => {
                            match self
                                .on_failure(e, a, &mut cursor, &mut failures, &mut backoff)
                                .await
                            {
                                Some(exit) => return exit,
                                None => continue,
                            }
                        }
                    }
                }
            };
            self.set_connected(true);
            if !a.race {
                let net = self.fetcher.network().map(|n| n.interface().to_string());
                ctx.table.set_network(a.seg, self.id, net);
            }
            // The server may have promised less than we asked for.
            let req_end = match (req_end, stream.range_end()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, _) => a,
            };
            let generation = ctx.source_generation.load(Ordering::Acquire);
            match self.pump(stream, a, &mut cursor, req_end, generation).await {
                Ok(Pump::SegmentDone) => current = None,
                Ok(Pump::RangeDone) => {
                    // Between requests is a good moment to retire (the segment goes back to
                    // the pool and is taken over as a whole by another worker).
                    if !a.race && ctx.try_retire() {
                        self.release(current, false);
                        return WorkerExit::Retired;
                    }
                    if self.fetcher.network().is_some_and(|n| !n.is_enabled()) {
                        self.release(current, false);
                        return WorkerExit::Idle;
                    }
                }
                Ok(Pump::Reopen) => {}
                Ok(Pump::Stopped) => {
                    self.release(current, false);
                    return WorkerExit::Stopped;
                }
                Err(e) => {
                    if let Some(exit) = self
                        .on_failure(e, a, &mut cursor, &mut failures, &mut backoff)
                        .await
                    {
                        return exit;
                    }
                }
            }
        }
    }

    fn release(&self, current: Option<Assignment>, failed: bool) {
        if let Some(a) = current {
            self.ctx.table.release(a.seg, self.id, failed);
        }
    }

    /// Handles a failed open/read. Returns `Some(exit)` when the worker must end.
    async fn on_failure(
        &mut self,
        e: FetchError,
        a: Assignment,
        cursor: &mut u64,
        failures: &mut u32,
        backoff: &mut Backoff,
    ) -> Option<WorkerExit> {
        let ctx = self.ctx.clone();
        if matches!(e.error, EngineError::Http { status: 429 | 503 }) {
            ctx.few_requests.store(true, Ordering::Relaxed);
        }
        match e.kind {
            FailKind::Fatal => {
                self.release(Some(a), false);
                return Some(WorkerExit::Fatal(e.error));
            }
            // On a pinned network, failing to connect says more about that network than about
            // a server connection limit: stop this connection once the network is benched so a
            // replacement goes over another one.
            FailKind::MaybeRefused if self.fetcher.network().is_some() => {
                if self.fetcher.network().is_some_and(|n| n.is_failing()) {
                    self.set_connected(false);
                    self.release(Some(a), false);
                    return Some(WorkerExit::GaveUp(e.error));
                }
            }
            FailKind::Refused | FailKind::MaybeRefused => {
                let others = ctx
                    .connected
                    .load(Ordering::Acquire)
                    .saturating_sub(usize::from(self.connected));
                // A reset/refusal is only a limit signal for a connection that never worked and
                // failed twice in a row; dropped connections are ordinary transient errors.
                let limit_signal = match e.kind {
                    FailKind::Refused => true,
                    _ => {
                        self.early_failures += 1;
                        !self.ever_streamed && self.early_failures >= 2
                    }
                };
                if limit_signal && others > 0 {
                    self.set_connected(false);
                    self.release(Some(a), false);
                    return Some(WorkerExit::Refused {
                        retry_after: e.retry_after,
                    });
                }
                // The only connection was refused: a plain failure (fatal unless transient).
                if !e.error.is_transient() {
                    self.release(Some(a), false);
                    return Some(WorkerExit::Fatal(e.error));
                }
            }
            _ => {}
        }
        self.set_connected(false);
        // After progress, the first retry is immediate (a dropped connection is simply
        // re-opened); only consecutive failures without progress back off.
        let immediate = self.progressed;
        if self.progressed {
            *failures = 0;
            backoff.reset();
            self.progressed = false;
        }
        *failures += 1;
        if *failures > ctx.max_retries {
            self.release(Some(a), true);
            return Some(WorkerExit::GaveUp(e.error));
        }
        // A non-resumable stream can only restart from zero.
        if !ctx.table_resumable() && *cursor > 0 {
            if let Err(err) = ctx.writer.flush(false).await {
                return Some(WorkerExit::Fatal(err));
            }
            ctx.table.reset_single();
            *cursor = 0;
            ctx.job.emit(DownloadEvent::Restarted {
                reason: format!("{} (server does not support resuming)", e.error),
            });
        }
        let delay = match e.retry_after {
            Some(d) => d.min(MAX_RETRY_AFTER),
            None if immediate => Duration::ZERO,
            None => backoff.next_delay(),
        };
        ctx.job.emit(DownloadEvent::Retrying {
            reason: e.error.to_string(),
            attempt: *failures,
            delay,
        });
        tracing::debug!(worker = self.id, attempt = *failures, ?delay, error = %e.error, "retrying");
        ctx.table.set_retrying(a.seg, true);
        let stopped = tokio::select! {
            _ = tokio::time::sleep(delay) => false,
            _ = ctx.stop.cancelled() => true,
        };
        ctx.table.set_retrying(a.seg, false);
        if stopped {
            self.release(Some(a), false);
            return Some(WorkerExit::Stopped);
        }
        None
    }

    async fn pump(
        &mut self,
        mut stream: F::Stream,
        a: Assignment,
        cursor: &mut u64,
        req_end: Option<u64>,
        generation: u64,
    ) -> Result<Pump, FetchError> {
        let ctx = self.ctx.clone();
        let started = Instant::now();
        let start_cursor = *cursor;
        loop {
            let next = tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => return Ok(Pump::Stopped),
                r = tokio::time::timeout(ctx.read_timeout, stream.next_chunk()) => r,
            };
            let chunk = match next {
                Err(_) => return Err(FetchError::transient(EngineError::Timeout)),
                Ok(Err(e)) => return Err(e),
                Ok(Ok(Some(c))) => c,
                Ok(Ok(None)) => {
                    return match req_end {
                        None => {
                            ctx.table.complete_unknown(a.seg);
                            Ok(Pump::SegmentDone)
                        }
                        Some(re) if *cursor >= re => {
                            self.tune_request_size(*cursor - start_cursor, started.elapsed());
                            if *cursor >= ctx.table.end_of(a.seg) {
                                Ok(Pump::SegmentDone)
                            } else {
                                Ok(Pump::RangeDone)
                            }
                        }
                        Some(_) => Err(FetchError::transient(EngineError::network(
                            "connection closed before the requested range was complete",
                        ))),
                    };
                }
            };
            if chunk.is_empty() {
                continue;
            }
            if ctx.source_generation.load(Ordering::Acquire) != generation {
                return Ok(Pump::Reopen);
            }
            let len = chunk.len() as u64;
            tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => return Ok(Pump::Stopped),
                _ = acquire_all(&ctx.limiters, len) => {}
            }
            if let Some(n) = self.fetcher.network() {
                tokio::select! {
                    biased;
                    _ = ctx.stop.cancelled() => return Ok(Pump::Stopped),
                    _ = n.limiter().acquire(len) => {}
                }
                n.add_received(len);
            }
            let permit = tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => return Ok(Pump::Stopped),
                p = ctx.writer.reserve(chunk.len()) => p.map_err(FetchError::fatal)?,
            };
            // No await between accounting and submitting: the table never claims bytes that
            // were not handed to the writer.
            let adv = ctx.table.advance(a.seg, self.id, *cursor, len);
            if adv.accept > 0 {
                ctx.writer
                    .submit(
                        self.id,
                        a.seg,
                        *cursor,
                        chunk.slice(..adv.accept as usize),
                        permit,
                    )
                    .map_err(FetchError::fatal)?;
                *cursor += adv.accept;
            }
            if adv.progressed > 0 {
                self.progressed = true;
                self.ever_streamed = true;
                self.early_failures = 0;
            }
            if adv.stop {
                if self.fetcher.bounded() && req_end.is_some_and(|re| *cursor >= re) {
                    // Read the end of the response so the connection goes back to the pool.
                    let _ =
                        tokio::time::timeout(Duration::from_millis(500), stream.next_chunk()).await;
                    self.tune_request_size(*cursor - start_cursor, started.elapsed());
                }
                return Ok(Pump::SegmentDone);
            }
        }
    }

    /// Sizes the next bounded request to ~2 s of transfer at the measured speed.
    fn tune_request_size(&mut self, bytes: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 || bytes == 0 {
            return;
        }
        let target = (bytes as f64 / secs * REQUEST_SECONDS) as u64;
        self.request_size = target.clamp(self.ctx.min_request, self.ctx.max_request);
    }
}

impl SegCtx {
    fn table_resumable(&self) -> bool {
        self.job.meta().resumable
    }
}
