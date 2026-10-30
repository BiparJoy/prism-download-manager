//! The segment table: dynamic work-stealing segmentation of one file.
//!
//! The file `[0, total)` is always partitioned into segments `[start, end)`. Each segment has
//! a receive frontier `pos` (bytes `[start, pos)` were received and handed to the writer) and a
//! durable frontier `written` (bytes `[start, written)` were written to the OS by the writer).
//!
//! * An idle worker first takes an unowned unfinished segment; otherwise it **splits** the
//!   owned segment with the largest remainder (if at least `2 * min_split` remains) at its
//!   4 KiB-aligned midpoint — or, if the victim's current bounded request reaches beyond the
//!   midpoint, at the end of that request, so the victim's socket survives. The victim's `end`
//!   shrinks; it notices on its next chunk.
//! * When nothing can be split, an idle worker may **race** a slow tail segment: it downloads
//!   the same bytes from the current frontier. Both participants write identical bytes to the
//!   same offsets, the frontier is the maximum of both, and whoever reaches `end` first
//!   finishes the segment; the other stops at its next chunk.
//!
//! A single mutex protects the table. It is taken once per received network chunk (tens of
//! kilobytes), which is negligible next to the copy and syscall costs of the data itself, and
//! it keeps every invariant trivially consistent (splits, races, flush accounting, snapshots).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::progress::{SegmentStatus, SegmentView};
use crate::resume::SegmentState;
use crate::speed::SpeedMeter;

/// Identifies a worker (connection) of one download.
pub(crate) type WorkerId = u32;

/// Split points are aligned to this many bytes.
pub(crate) const SPLIT_ALIGN: u64 = 4096;

/// End marker of a segment whose size is unknown (no `Content-Length`).
pub(crate) const UNKNOWN_END: u64 = u64::MAX;

#[derive(Debug)]
struct Seg {
    start: u64,
    pos: u64,
    end: u64,
    /// End of the owner's current request (split hint), 0 = none.
    req_end: u64,
    written: u64,
    /// A flushed range beyond `written` (from a race participant), merged once contiguous.
    extra: Option<(u64, u64)>,
    owner: Option<WorkerId>,
    racer: Option<WorkerId>,
    retrying: bool,
    failed: bool,
    received: u64,
    meter: SpeedMeter,
    peak_rate: u64,
    /// Adapter the owner's current request runs over (multi-network downloads).
    network: Option<String>,
}

impl Seg {
    fn new(start: u64, pos: u64, end: u64) -> Self {
        Seg {
            start,
            pos,
            end,
            req_end: 0,
            written: pos,
            extra: None,
            owner: None,
            racer: None,
            retrying: false,
            failed: false,
            received: 0,
            meter: SpeedMeter::default(),
            peak_rate: 0,
            network: None,
        }
    }

    fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }

    fn done(&self) -> bool {
        self.pos >= self.end
    }
}

/// A unit of work handed to a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Assignment {
    /// Segment index (stable for the lifetime of the table).
    pub seg: usize,
    /// Where the worker starts receiving.
    pub cursor: u64,
    /// `true` if this is a duplicate race on a slow tail.
    pub race: bool,
}

/// How an assignment was obtained (for events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquireKind {
    /// An unowned segment (initial or released).
    Unowned,
    /// A new piece `[at, end)` split from another segment.
    Split { at: u64, end: u64 },
    /// A race on `[cursor, end)`.
    Race { end: u64 },
}

/// Result of [`SegmentTable::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Advance {
    /// Bytes of the chunk (from its start) that belong to the segment and must be written.
    pub accept: u64,
    /// New bytes credited to the download (0 when a race partner was ahead).
    pub progressed: u64,
    /// The worker must stop this assignment after writing `accept` bytes.
    pub stop: bool,
}

struct Inner {
    segs: Vec<Seg>,
    total: Option<u64>,
    min_split: u64,
    splittable: bool,
}

/// Thread-safe segment table of one download.
pub(crate) struct SegmentTable {
    inner: Mutex<Inner>,
    downloaded: AtomicU64,
}

fn align_down(v: u64) -> u64 {
    v / SPLIT_ALIGN * SPLIT_ALIGN
}

fn align_up(v: u64) -> u64 {
    v.div_ceil(SPLIT_ALIGN) * SPLIT_ALIGN
}

/// Seconds needed to transfer `bytes` at `rate` (rate 0 = effectively infinite time).
fn eta_secs(bytes: u64, rate: u64) -> f64 {
    if rate == 0 {
        f64::INFINITY
    } else {
        bytes as f64 / rate as f64
    }
}

impl SegmentTable {
    /// A fresh table: one unowned segment covering the file. `total = None` means unknown size
    /// (single stream until EOF). `splittable = false` disables splitting and racing.
    pub(crate) fn new(total: Option<u64>, min_split: u64, splittable: bool) -> Self {
        let end = total.unwrap_or(UNKNOWN_END);
        Self {
            inner: Mutex::new(Inner {
                segs: vec![Seg::new(0, 0, end)],
                total,
                min_split: min_split.max(SPLIT_ALIGN),
                splittable: splittable && total.is_some(),
            }),
            downloaded: AtomicU64::new(0),
        }
    }

    /// Rebuilds a table from persisted segments (already validated to partition the file).
    pub(crate) fn from_states(states: &[SegmentState], total: u64, min_split: u64) -> Self {
        let mut sorted = states.to_vec();
        sorted.sort_by_key(|s| s.start);
        let segs: Vec<Seg> = sorted
            .iter()
            .map(|s| Seg::new(s.start, s.pos, s.end))
            .collect();
        let downloaded = segs.iter().map(|s| s.pos - s.start).sum();
        Self {
            inner: Mutex::new(Inner {
                segs,
                total: Some(total),
                min_split: min_split.max(SPLIT_ALIGN),
                splittable: true,
            }),
            downloaded: AtomicU64::new(downloaded),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("segment table lock poisoned")
    }

    /// Total size, if known.
    pub(crate) fn total(&self) -> Option<u64> {
        self.lock().total
    }

    /// Bytes received so far (unique bytes; races are not double counted).
    pub(crate) fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// `true` when every byte was received.
    pub(crate) fn all_done(&self) -> bool {
        self.lock().segs.iter().all(Seg::done)
    }

    /// Bytes not yet received (`None` when the size is unknown).
    pub(crate) fn remaining(&self) -> Option<u64> {
        let g = self.lock();
        g.total?;
        Some(g.segs.iter().map(Seg::remaining).sum())
    }

    /// Claims a specific unowned segment for `worker` (used for the first request on resume).
    pub(crate) fn claim(&self, seg: usize, worker: WorkerId) -> Option<Assignment> {
        let mut g = self.lock();
        let s = g.segs.get_mut(seg)?;
        if s.owner.is_some() || s.done() {
            return None;
        }
        s.owner = Some(worker);
        s.failed = false;
        Some(Assignment {
            seg,
            cursor: s.pos,
            race: false,
        })
    }

    /// Index of the first unfinished segment by offset.
    pub(crate) fn first_pending(&self) -> Option<usize> {
        let g = self.lock();
        g.segs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.done())
            .min_by_key(|(_, s)| s.start)
            .map(|(i, _)| i)
    }

    /// `(start, pos, end)` of a segment.
    pub(crate) fn bounds(&self, seg: usize) -> (u64, u64, u64) {
        let g = self.lock();
        let s = &g.segs[seg];
        (s.start, s.pos, s.end)
    }

    /// Whether a new worker would find something to do (unowned work, a splittable segment,
    /// or — with `allow_race` — a slow tail worth racing).
    pub(crate) fn has_work(&self, allow_race: bool) -> bool {
        let g = self.lock();
        if g.segs.iter().any(|s| s.owner.is_none() && !s.done()) {
            return true;
        }
        if !g.splittable {
            return false;
        }
        if g.segs
            .iter()
            .any(|s| s.owner.is_some() && s.racer.is_none() && s.remaining() >= 2 * g.min_split)
        {
            return true;
        }
        allow_race && Self::race_candidate(&g).is_some()
    }

    /// Finds work for an idle `worker` (see the module docs for the policy).
    pub(crate) fn acquire(
        &self,
        worker: WorkerId,
        allow_race: bool,
    ) -> Option<(Assignment, AcquireKind)> {
        let mut g = self.lock();
        // 1. Unowned unfinished segment with the largest remainder.
        if let Some((i, _)) = g
            .segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owner.is_none() && !s.done())
            .max_by_key(|(_, s)| s.remaining())
        {
            let s = &mut g.segs[i];
            s.owner = Some(worker);
            s.failed = false;
            s.req_end = 0;
            return Some((
                Assignment {
                    seg: i,
                    cursor: s.pos,
                    race: false,
                },
                AcquireKind::Unowned,
            ));
        }
        if !g.splittable {
            return None;
        }
        // 2. Split the largest owned remainder.
        let min_split = g.min_split;
        if let Some((i, _)) = g
            .segs
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.owner.is_some() && s.racer.is_none() && s.remaining() >= 2 * min_split
            })
            .max_by_key(|(_, s)| s.remaining())
        {
            let (pos, end, req_end) = {
                let s = &g.segs[i];
                (s.pos, s.end, s.req_end)
            };
            let mid = align_down(pos + (end - pos) / 2);
            let hint = align_up(req_end);
            // Prefer splitting behind the victim's in-flight request (keeps its socket alive).
            let at = if hint > mid && hint < end && end - hint >= min_split {
                hint
            } else {
                mid
            };
            if at > pos && at < end {
                g.segs[i].end = at;
                let mut piece = Seg::new(at, at, end);
                piece.owner = Some(worker);
                g.segs.push(piece);
                let idx = g.segs.len() - 1;
                return Some((
                    Assignment {
                        seg: idx,
                        cursor: at,
                        race: false,
                    },
                    AcquireKind::Split { at, end },
                ));
            }
        }
        // 3. Race a slow tail.
        if allow_race && let Some(i) = Self::race_candidate(&g) {
            let s = &mut g.segs[i];
            s.racer = Some(worker);
            let end = s.end;
            return Some((
                Assignment {
                    seg: i,
                    cursor: s.pos,
                    race: true,
                },
                AcquireKind::Race { end },
            ));
        }
        None
    }

    /// A segment whose estimated completion is far behind what a typical connection of this
    /// download would need: more than 3 s left and at least 4× slower than typical.
    fn race_candidate(g: &Inner) -> Option<usize> {
        let mut peaks: Vec<u64> = g
            .segs
            .iter()
            .map(|s| s.peak_rate)
            .filter(|r| *r > 0)
            .collect();
        if peaks.is_empty() {
            return None;
        }
        peaks.sort_unstable();
        let typical = peaks[peaks.len() / 2];
        g.segs
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.owner.is_some() && s.racer.is_none() && !s.done() && s.end != UNKNOWN_END
            })
            .filter(|(_, s)| {
                let own = eta_secs(s.remaining(), s.meter.rate());
                own > 3.0 && own > 4.0 * eta_secs(s.remaining(), typical)
            })
            .max_by(|(_, a), (_, b)| {
                eta_secs(a.remaining(), a.meter.rate())
                    .total_cmp(&eta_secs(b.remaining(), b.meter.rate()))
            })
            .map(|(i, _)| i)
    }

    /// Records the end of the worker's current (bounded) request — the split hint.
    pub(crate) fn set_request_end(&self, seg: usize, worker: WorkerId, req_end: u64) {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        if s.owner == Some(worker) {
            s.req_end = req_end;
        }
    }

    /// Records the network the owner's current request runs over.
    pub(crate) fn set_network(&self, seg: usize, worker: WorkerId, network: Option<String>) {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        if s.owner == Some(worker) {
            s.network = network;
        }
    }

    /// Current end of a segment (may have shrunk because of a split).
    pub(crate) fn end_of(&self, seg: usize) -> u64 {
        self.lock().segs[seg].end
    }

    /// Accounts `n` received bytes that start at `cursor` for `worker`.
    pub(crate) fn advance(&self, seg: usize, worker: WorkerId, cursor: u64, n: u64) -> Advance {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        let participant = s.owner == Some(worker) || s.racer == Some(worker);
        if !participant || s.done() || cursor >= s.end {
            return Advance {
                accept: 0,
                progressed: 0,
                stop: true,
            };
        }
        let accept = n.min(s.end - cursor);
        let new = cursor + accept;
        let progressed = new.saturating_sub(s.pos);
        if progressed > 0 {
            s.pos = new;
            s.received += progressed;
        }
        let done = s.done();
        if done {
            s.owner = None;
            s.racer = None;
            s.retrying = false;
        }
        let stop = done || new >= s.end;
        drop(g);
        if progressed > 0 {
            self.downloaded.fetch_add(progressed, Ordering::Relaxed);
        }
        Advance {
            accept,
            progressed,
            stop,
        }
    }

    /// Marks the end of an unknown-size stream: the segment ends at its frontier.
    pub(crate) fn complete_unknown(&self, seg: usize) -> u64 {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        s.end = s.pos;
        s.owner = None;
        let size = s.pos;
        g.total = Some(size);
        size
    }

    /// Resets a single-stream (non-resumable) download to zero before restarting it. The
    /// caller must have flushed the writer first.
    pub(crate) fn reset_single(&self) {
        let mut g = self.lock();
        for s in g.segs.iter_mut() {
            s.pos = s.start;
            s.written = s.start;
            s.extra = None;
        }
        drop(g);
        self.downloaded.store(0, Ordering::Relaxed);
    }

    /// Gives an assignment back (error, pause, retirement). A racer is promoted to owner.
    pub(crate) fn release(&self, seg: usize, worker: WorkerId, failed: bool) {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        if s.owner == Some(worker) {
            s.owner = s.racer.take();
            s.req_end = 0;
            s.retrying = false;
            s.failed = failed && s.owner.is_none() && !s.done();
        } else if s.racer == Some(worker) {
            s.racer = None;
        }
    }

    /// Flags a segment as waiting for a retry (for snapshots).
    pub(crate) fn set_retrying(&self, seg: usize, retrying: bool) {
        self.lock().segs[seg].retrying = retrying;
    }

    /// Called by the writer after `[offset, offset + len)` of `seg` was written to the OS.
    pub(crate) fn on_flushed(&self, seg: usize, offset: u64, len: u64) {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        let end = offset + len;
        if offset <= s.written {
            s.written = s.written.max(end);
        } else {
            // Not contiguous yet (race partner ahead): remember it, merging overlaps.
            s.extra = match s.extra {
                Some((a, b)) if offset <= b && end >= a => Some((a.min(offset), b.max(end))),
                Some((a, b)) if a < offset => Some((a, b)), // keep the lower one
                _ => Some((offset, end)),
            };
        }
        if let Some((a, b)) = s.extra
            && a <= s.written
        {
            s.written = s.written.max(b);
            s.extra = None;
        }
        // Never claim beyond the (possibly shrunk) segment end.
        s.written = s.written.min(s.end.max(s.start));
    }

    /// Durable segment states (`pos` = written frontier), sorted by start.
    pub(crate) fn durable_states(&self) -> Vec<SegmentState> {
        let g = self.lock();
        let mut v: Vec<SegmentState> = g
            .segs
            .iter()
            .map(|s| SegmentState {
                start: s.start,
                pos: s.written.min(s.end).max(s.start),
                end: s.end,
            })
            .collect();
        v.sort_by_key(|s| s.start);
        v
    }

    /// Updates speed meters; call periodically.
    pub(crate) fn tick(&self, now: Instant) {
        let mut g = self.lock();
        for s in g.segs.iter_mut() {
            let r = s.meter.update(s.received, now);
            s.peak_rate = s.peak_rate.max(r);
        }
    }

    /// Snapshot views, sorted by start.
    pub(crate) fn views(&self) -> Vec<SegmentView> {
        let g = self.lock();
        let mut v: Vec<SegmentView> = g
            .segs
            .iter()
            .map(|s| SegmentView {
                start: s.start,
                pos: s.pos,
                end: s.end,
                speed: if s.done() { 0 } else { s.meter.rate() },
                network: s.owner.and(s.network.clone()),
                status: if s.done() {
                    SegmentStatus::Done
                } else if s.retrying {
                    SegmentStatus::Retrying
                } else if s.owner.is_some() {
                    SegmentStatus::Active
                } else if s.failed {
                    SegmentStatus::Failed
                } else {
                    SegmentStatus::Idle
                },
            })
            .collect();
        v.sort_by_key(|s| s.start);
        v
    }

    #[cfg(test)]
    pub(crate) fn set_rate_for_test(&self, seg: usize, rate: u64, peak: u64) {
        let mut g = self.lock();
        let s = &mut g.segs[seg];
        // Feed the meter twice so `rate()` reflects the given value.
        let t0 = Instant::now();
        s.meter.reset(0, t0);
        s.meter.update(rate, t0 + std::time::Duration::from_secs(1));
        s.peak_rate = peak;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn check_partition(t: &SegmentTable, total: u64) {
        let states = t.durable_states();
        let mut expect = 0;
        for s in &states {
            assert_eq!(s.start, expect, "gap/overlap in {states:?}");
            assert!(s.start <= s.pos && s.pos <= s.end);
            expect = s.end;
        }
        assert_eq!(expect, total);
    }

    #[test]
    fn splitting_invariants() {
        let total = 100 * MIB + 12345;
        let t = SegmentTable::new(Some(total), MIB, true);
        let (a0, k0) = t.acquire(0, false).unwrap();
        assert_eq!(k0, AcquireKind::Unowned);
        assert_eq!(a0.cursor, 0);
        // 31 more workers steal.
        for w in 1..32 {
            let (a, k) = t.acquire(w, false).unwrap();
            match k {
                AcquireKind::Split { at, end } => {
                    assert_eq!(at % SPLIT_ALIGN, 0, "aligned split");
                    assert!(end - at >= MIB, "min split respected");
                    assert_eq!(a.cursor, at);
                }
                other => panic!("expected split, got {other:?}"),
            }
            check_partition(&t, total);
        }
        // Every piece is still >= min split.
        for s in t.durable_states() {
            assert!(s.end - s.start >= MIB);
        }
    }

    #[test]
    fn no_split_below_twice_min() {
        let t = SegmentTable::new(Some(2 * MIB - 1), MIB, true);
        t.acquire(0, false).unwrap();
        assert!(t.acquire(1, false).is_none());
        assert!(!t.has_work(false));
        let t = SegmentTable::new(Some(2 * MIB), MIB, true);
        t.acquire(0, false).unwrap();
        assert!(t.has_work(false));
        let (_, k) = t.acquire(1, false).unwrap();
        assert_eq!(
            k,
            AcquireKind::Split {
                at: MIB,
                end: 2 * MIB
            }
        );
    }

    #[test]
    fn split_respects_in_flight_request() {
        let t = SegmentTable::new(Some(64 * MIB), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        // Victim requested the first 40 MiB: split behind it.
        t.set_request_end(a.seg, 0, 40 * MIB);
        let (_, k) = t.acquire(1, false).unwrap();
        assert_eq!(
            k,
            AcquireKind::Split {
                at: 40 * MIB,
                end: 64 * MIB
            }
        );
        // Request reaching (almost) the end: fall back to the midpoint.
        let t = SegmentTable::new(Some(64 * MIB), MIB, true);
        t.acquire(0, false).unwrap();
        t.set_request_end(0, 0, 64 * MIB - 4096);
        let (_, k) = t.acquire(1, false).unwrap();
        assert_eq!(
            k,
            AcquireKind::Split {
                at: 32 * MIB,
                end: 64 * MIB
            }
        );
    }

    #[test]
    fn advance_truncates_at_shrunk_end_and_counts_progress() {
        let t = SegmentTable::new(Some(8 * MIB), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        let adv = t.advance(a.seg, 0, 0, 1000);
        assert_eq!(
            adv,
            Advance {
                accept: 1000,
                progressed: 1000,
                stop: false
            }
        );
        let (b, k) = t.acquire(1, false).unwrap();
        let AcquireKind::Split { at, .. } = k else {
            panic!()
        };
        // Victim receives a chunk crossing the new end.
        let adv = t.advance(a.seg, 0, at - 10, 100);
        assert_eq!(adv.accept, 10);
        assert!(adv.stop);
        // Thief completes its piece.
        let adv = t.advance(b.seg, 1, at, 8 * MIB - at);
        assert!(adv.stop);
        assert_eq!(t.downloaded(), 1000 + (at - 1000) + (8 * MIB - at));
        assert!(t.all_done());
        check_partition(&t, 8 * MIB);
    }

    #[test]
    fn unowned_segments_are_taken_first_and_release_works() {
        let t = SegmentTable::new(Some(16 * MIB), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        t.advance(a.seg, 0, 0, MIB);
        t.release(a.seg, 0, true);
        assert_eq!(t.views()[0].status, SegmentStatus::Failed);
        let (b, k) = t.acquire(7, false).unwrap();
        assert_eq!(k, AcquireKind::Unowned);
        assert_eq!(b.cursor, MIB);
        // Non-owner cannot advance.
        assert!(t.advance(b.seg, 0, MIB, 10).stop);
    }

    #[test]
    fn race_on_slow_tail() {
        let t = SegmentTable::new(Some(3 * MIB / 2), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        t.advance(a.seg, 0, 0, 100 * 1024);
        // Not splittable (< 2 MiB). Fast typical peak, slow current rate.
        t.set_rate_for_test(a.seg, 10 * 1024, 10 * MIB);
        assert!(t.acquire(1, false).is_none());
        let (r, k) = t.acquire(1, true).unwrap();
        assert!(r.race);
        assert!(matches!(k, AcquireKind::Race { .. }));
        assert_eq!(r.cursor, 100 * 1024);
        // Racer runs ahead and finishes; owner's next chunk is told to stop.
        let rest = 3 * MIB / 2 - r.cursor;
        let adv = t.advance(r.seg, 1, r.cursor, rest);
        assert_eq!(adv.accept, rest);
        assert!(adv.stop);
        assert!(t.all_done());
        let adv = t.advance(a.seg, 0, 100 * 1024, 4096);
        assert_eq!(
            adv,
            Advance {
                accept: 0,
                progressed: 0,
                stop: true
            }
        );
        assert_eq!(t.downloaded(), 3 * MIB / 2);
    }

    #[test]
    fn race_participant_behind_does_not_double_count() {
        let t = SegmentTable::new(Some(MIB), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        t.advance(a.seg, 0, 0, 200_000);
        t.set_rate_for_test(a.seg, 1000, 10 * MIB);
        let (r, _) = t.acquire(1, true).unwrap();
        // Owner continues ahead of the racer.
        t.advance(a.seg, 0, 200_000, 100_000);
        let adv = t.advance(r.seg, 1, r.cursor, 50_000);
        assert_eq!(adv.accept, 50_000);
        assert_eq!(adv.progressed, 0);
        assert_eq!(t.downloaded(), 300_000);
        // Owner fails: racer is promoted and keeps going.
        t.release(a.seg, 0, true);
        let adv = t.advance(r.seg, 1, 250_000, 100_000);
        assert_eq!(adv.progressed, 50_000);
        assert_eq!(t.downloaded(), 350_000);
    }

    #[test]
    fn flush_accounting_and_durable_states() {
        let t = SegmentTable::new(Some(4 * MIB), MIB, true);
        let (a, _) = t.acquire(0, false).unwrap();
        t.advance(a.seg, 0, 0, 300);
        t.on_flushed(a.seg, 0, 100);
        assert_eq!(t.durable_states()[0].pos, 100);
        // Out-of-order flush beyond the frontier is held back...
        t.on_flushed(a.seg, 200, 100);
        assert_eq!(t.durable_states()[0].pos, 100);
        // ...and merged once the gap is filled.
        t.on_flushed(a.seg, 100, 100);
        assert_eq!(t.durable_states()[0].pos, 300);
        assert_eq!(t.durable_states()[0].end, 4 * MIB);
    }

    #[test]
    fn unknown_size_single_stream() {
        let t = SegmentTable::new(None, MIB, true);
        assert!(t.remaining().is_none());
        let (a, _) = t.acquire(0, true).unwrap();
        assert!(
            t.acquire(1, true).is_none(),
            "unknown size is never split or raced"
        );
        let adv = t.advance(a.seg, 0, 0, 5000);
        assert!(!adv.stop);
        assert_eq!(t.complete_unknown(a.seg), 5000);
        assert_eq!(t.total(), Some(5000));
        assert!(t.all_done());
        t.reset_single();
        assert_eq!(t.downloaded(), 0);
    }

    #[test]
    fn resume_from_states() {
        let states = vec![
            SegmentState {
                start: 0,
                pos: MIB,
                end: MIB,
            },
            SegmentState {
                start: MIB,
                pos: MIB + 10,
                end: 3 * MIB,
            },
        ];
        let t = SegmentTable::from_states(&states, 3 * MIB, MIB);
        assert_eq!(t.downloaded(), MIB + 10);
        assert_eq!(t.first_pending(), Some(1));
        let a = t.claim(1, 0).unwrap();
        assert_eq!(a.cursor, MIB + 10);
        assert!(t.claim(1, 2).is_none());
        assert_eq!(t.durable_states(), states);
    }
}
