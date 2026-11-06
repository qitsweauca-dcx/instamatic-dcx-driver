//! StreamStats #[pyclass] snapshot (docs/DESIGN.md §2.1 [G2-I][G2-D]).
//!
//! `cam.stats` composes three sources: the stream counters (`dectris_stream::StreamStats`), the
//! SIMPLON trigger counters (`Detector::stats()`) and `log_errors` from the `GuardedLogger`.

use std::sync::atomic::Ordering;

use dectris_core::driver::Driver;
use pyo3::prelude::*;

use crate::logging::LOG_ERRORS;

#[pyclass(
    frozen,
    get_all,
    skip_from_py_object,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "StreamStats"
)]
#[derive(Debug, Default, Clone)]
pub struct StreamStats {
    pub received: u64,
    pub delivered: u64,
    pub stale: u64,
    pub flushed: u64,
    pub flushed_armed: u64,
    pub timeouts: u64,
    pub reconnects: u64,
    pub trigger_unblocked: u64,
    pub triggers_skipped: u64,
    pub attribution_mismatch: u64,
    pub log_errors: u64,
    /// `time.monotonic()` seconds of the last message taken off the socket; None = never.
    pub last_recv_monotonic: Option<f64>,
}

#[pymethods]
impl StreamStats {
    fn __repr__(&self) -> String {
        format!(
            "StreamStats(received={}, delivered={}, stale={}, flushed={}, flushed_armed={}, \
             timeouts={}, reconnects={}, trigger_unblocked={}, triggers_skipped={}, \
             attribution_mismatch={}, log_errors={})",
            self.received,
            self.delivered,
            self.stale,
            self.flushed,
            self.flushed_armed,
            self.timeouts,
            self.reconnects,
            self.trigger_unblocked,
            self.triggers_skipped,
            self.attribution_mismatch,
            self.log_errors
        )
    }
}

/// Snapshot (`drv = None`: not connected, every counter but `log_errors` is 0).
pub fn snapshot(py: Python<'_>, drv: Option<&Driver>) -> PyResult<StreamStats> {
    let log_errors = LOG_ERRORS.load(Ordering::Relaxed);
    let Some(drv) = drv else {
        return Ok(StreamStats {
            log_errors,
            ..StreamStats::default()
        });
    };
    let (s, t, age) = py.detach(|| {
        let s = drv.stream().stats();
        let age = s.last_recv.map(|at| at.elapsed().as_secs_f64());
        (s, drv.detector().stats(), age)
    });
    let last_recv_monotonic = match age {
        Some(age) => {
            let now: f64 = py.import("time")?.call_method0("monotonic")?.extract()?;
            Some(now - age)
        }
        None => None,
    };
    Ok(StreamStats {
        received: s.received,
        delivered: s.delivered,
        stale: s.stale,
        flushed: s.flushed,
        flushed_armed: s.flushed_armed,
        timeouts: s.timeouts,
        reconnects: s.reconnects,
        trigger_unblocked: t.trigger_unblocked,
        triggers_skipped: t.triggers_skipped,
        attribution_mismatch: s.attribution_mismatch,
        log_errors,
        last_recv_monotonic,
    })
}
