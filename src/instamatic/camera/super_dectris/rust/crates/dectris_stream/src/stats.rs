//! Stream-owned `StreamStats` counters (docs/DESIGN.md §2.1 `StreamStats`).
//!
//! This crate owns `received, delivered, stale, flushed, flushed_armed` [G2-I]`, timeouts,
//! reconnects, attribution_mismatch` [G2-D]` and last_recv_monotonic`. The remaining
//! `StreamStats` fields come from elsewhere: `trigger_unblocked` / `triggers_skipped` from
//! `dectris_simplon::Detector::stats()`, `log_errors` from the py crate's `GuardedLogger`.
//!
//! Definitions:
//! * `received` — messages taken off the socket by a receive path (not by a flush);
//! * `delivered` — images handed to a caller (incl. stale images under `stale_series_policy: deliver`);
//! * `stale` — messages classified stale (§4.3 rows 2–4), plus queued armed images skipped by
//!   `single_frame_fresh` and the 50 ms drain;
//! * `flushed` — messages discarded by `flush_zmq_buffer` (its return value, summed);
//! * `flushed_armed` — armed-series images among them [G2-I];
//! * `timeouts` — receive windows that expired;
//! * `reconnects` — successful `reconnect_zmq()`;
//! * `attribution_mismatch` — [G2-D] self-check hits;
//! * `last_recv` — `Instant` of the last message taken off the socket (receive or flush). The py
//!   layer converts it to `time.monotonic()` seconds (`monotonic() - last_recv.elapsed()`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

/// Atomic counters (DESIGN §3.2 `stats`).
#[derive(Debug, Default)]
pub struct StreamStats {
    pub(crate) received: AtomicU64,
    pub(crate) delivered: AtomicU64,
    pub(crate) stale: AtomicU64,
    pub(crate) flushed: AtomicU64,
    pub(crate) flushed_armed: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) reconnects: AtomicU64,
    pub(crate) attribution_mismatch: AtomicU64,
    /// Nanoseconds since `base()` + 1; 0 = never.
    last_recv: AtomicU64,
}

/// A consistent-enough copy of [`StreamStats`] (each field read atomically).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamStatsSnapshot {
    pub received: u64,
    pub delivered: u64,
    pub stale: u64,
    pub flushed: u64,
    pub flushed_armed: u64,
    pub timeouts: u64,
    pub reconnects: u64,
    pub attribution_mismatch: u64,
    pub last_recv: Option<Instant>,
}

impl StreamStats {
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        if n > 0 {
            c.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub(crate) fn mark_recv(&self, at: Instant) {
        let ns = at.saturating_duration_since(base()).as_nanos();
        let v = u64::try_from(ns).unwrap_or(u64::MAX - 1).saturating_add(1);
        self.last_recv.store(v, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StreamStatsSnapshot {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let lr = l(&self.last_recv);
        StreamStatsSnapshot {
            received: l(&self.received),
            delivered: l(&self.delivered),
            stale: l(&self.stale),
            flushed: l(&self.flushed),
            flushed_armed: l(&self.flushed_armed),
            timeouts: l(&self.timeouts),
            reconnects: l(&self.reconnects),
            attribution_mismatch: l(&self.attribution_mismatch),
            last_recv: (lr > 0).then(|| base() + Duration::from_nanos(lr - 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_last_recv() {
        let s = StreamStats::default();
        assert_eq!(s.snapshot(), StreamStatsSnapshot::default());
        StreamStats::add(&s.received, 3);
        StreamStats::add(&s.stale, 0);
        // base() is lazy: initialise it first, or `t` predates it and saturates to base()
        let _ = base();
        let t = Instant::now();
        s.mark_recv(t);
        let snap = s.snapshot();
        assert_eq!(snap.received, 3);
        assert_eq!(snap.stale, 0);
        let lr = snap.last_recv.unwrap();
        assert!(lr <= t + Duration::from_micros(1) && t <= lr + Duration::from_micros(1));
    }
}
