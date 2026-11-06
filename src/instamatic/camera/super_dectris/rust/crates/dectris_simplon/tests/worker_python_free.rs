//! Workers are Python-free (DESIGN §3.1 [R1]): every log record emitted on a worker thread — including
//! ureq's own `debug!` records — carries `NO_PYTHON`. One test per process: the capture logger is
//! global and other tests' threads would otherwise mix in.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::time::{Duration, Instant};

use common::{detector, FakeSimplon, TriggerHold};
use dectris_simplon::TriggerRequest;

#[test]
fn worker_threads_never_log_with_python_allowed() {
    assert!(dectris_common::testing::install());
    let me = std::thread::current().id();
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::For(Duration::from_millis(20)));
    let d = detector(&fake);
    d.set_config(1, 0.01, 10, None).unwrap();
    d.arm().unwrap();
    for _ in 0..10 {
        d.trigger(TriggerRequest::internal(None)).unwrap();
        d.trigger(TriggerRequest::public(None, None)).unwrap();
    }
    d.initialize().unwrap();
    let t0 = Instant::now();
    while d.pool().live() > 0 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let recs = dectris_common::testing::take_matching("");
    let ureq_main = recs
        .iter()
        .filter(|r| r.target.starts_with("ureq") && r.thread == me)
        .count();
    let foreign: Vec<_> = recs.iter().filter(|r| r.thread != me).collect();
    assert!(
        ureq_main > 0,
        "ureq emits debug records (the hazard DESIGN §2.9 [G1-A] guards against)"
    );
    assert!(!foreign.is_empty(), "workers ran and ureq logged on them");
    for r in &foreign {
        // fake-server threads never log through `log`; every foreign record is a worker's
        assert!(
            r.no_python,
            "record on a non-test thread without NO_PYTHON: {} {}",
            r.target, r.message
        );
    }
}
