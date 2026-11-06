//! `trigger()` and the workers (DESIGN §3.4; DESIGN §12 #6): mode gate [R23], suppression after
//! cancel/disarm [R4], epoch book [R3] (ints once per series, inte serialised), the worker
//! ceiling, client-side unblocking (Q18, [G1-B] at this layer), timeouts, late outcomes.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{detector, detector_with, fast_budgets, Action, FakeSimplon, TriggerHold};
use dectris_common::DriverError;
use dectris_simplon::{
    Detector, Settings, Signals, TriggerCaller, TriggerRequest, TriggerResult, WorkerPool,
};

fn public() -> TriggerRequest {
    TriggerRequest::public(None, None)
}

fn trigger_puts(fake: &FakeSimplon) -> usize {
    fake.count("PUT detector/command/trigger")
}

fn wait_live_zero(d: &Detector) {
    let t0 = Instant::now();
    while d.pool().live() > 0 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(d.pool().live(), 0, "workers did not finish");
}

#[test]
fn r23_external_modes_raise_without_http() {
    for mode in ["exts", "exte"] {
        let fake = FakeSimplon::start();
        let d = detector(&fake);
        d.set_trigger_mode_local(mode);
        for req in [public(), TriggerRequest::internal(None)] {
            let e = d.trigger(req).unwrap_err();
            match e {
                DriverError::State { state, needed } => {
                    assert_eq!(state, mode);
                    assert_eq!(needed, format!("software trigger not allowed in {mode}"));
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(fake.keys().is_empty(), "{mode}: no HTTP");
    }
}

#[test]
fn r4_trigger_after_cancel_is_suppressed_until_next_arm() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(1, 0.1, 1, None).unwrap();
    d.arm().unwrap();
    d.cancel();
    fake.clear_log();
    let t0 = Instant::now();
    assert_eq!(
        d.trigger(TriggerRequest::public(None, Some(0.1))).unwrap(),
        TriggerResult::Suppressed
    );
    assert_eq!(
        d.trigger(TriggerRequest::internal(None)).unwrap(),
        TriggerResult::Suppressed
    );
    assert!(t0.elapsed() < Duration::from_millis(50));
    assert_eq!(trigger_puts(&fake), 0);
    assert_eq!(d.stats().triggers_skipped, 2);
    // disarm suppresses too
    d.arm().unwrap();
    d.disarm();
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Suppressed);
    // the next arm re-enables triggers
    d.arm().unwrap();
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Completed);
    assert_eq!(trigger_puts(&fake), 1);
}

#[test]
fn q18_cancel_unblocks_a_blocked_trigger() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = Arc::new(detector(&fake));
    d.set_config(99_999, 0.01, 1, None).unwrap();
    d.arm().unwrap();
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || {
        let r = d2.trigger(public());
        (r, Instant::now())
    });
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 1));
    let t_cancel = Instant::now();
    d.cancel();
    let (r, t_ret) = h.join().unwrap();
    assert!(t_ret.duration_since(t_cancel) < Duration::from_millis(500));
    let r = r.unwrap();
    assert!(
        matches!(r, TriggerResult::Released | TriggerResult::Completed),
        "{r:?}"
    );
    // the next arm + trigger works, and the late outcome is drained
    fake.with(|s| s.trigger_hold = TriggerHold::Immediate);
    wait_live_zero(&d);
    d.arm().unwrap();
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Completed);
    assert_eq!(d.orphan_count(), 0);
}

#[test]
fn released_by_disarm_end_and_shutdown() {
    for how in ["disarm", "end", "abort", "shutdown"] {
        let fake = FakeSimplon::start();
        fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
        let d = Arc::new(detector(&fake));
        d.set_config(10, 0.01, 1, None).unwrap();
        d.arm().unwrap();
        let d2 = Arc::clone(&d);
        let h = std::thread::spawn(move || d2.trigger(public()));
        assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 1));
        let t0 = Instant::now();
        match how {
            "disarm" => d.disarm(),
            "abort" => d.abort(),
            "end" => d.signals().set_end_seen(),
            _ => d.request_stop(),
        }
        let r = h.join().unwrap().unwrap();
        assert!(t0.elapsed() < Duration::from_millis(500), "{how}");
        if how == "end" || how == "shutdown" {
            assert_eq!(r, TriggerResult::Released, "{how}");
            assert_eq!(d.stats().trigger_unblocked, 1, "{how}");
        }
        fake.with(|s| s.release_gen += 1);
    }
}

#[test]
fn r3_ints_internal_triggers_once_per_series() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = detector(&fake);
    d.set_config(99_999, 0.01, 1, None).unwrap();
    d.arm().unwrap();
    let mut max_live = 0;
    for _ in 0..200 {
        let r = d.trigger(TriggerRequest::internal(None)).unwrap();
        assert!(matches!(r, TriggerResult::Spawned | TriggerResult::Skipped));
        max_live = max_live.max(d.pool().live());
    }
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 1));
    assert_eq!(trigger_puts(&fake), 1);
    assert!(max_live <= 1);
    // trigger_blocking: false makes the public trigger internal-like
    d.update_settings(|s| s.trigger_blocking = false);
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Skipped);
    // a new epoch issues again
    d.disarm();
    wait_live_zero(&d);
    d.arm().unwrap();
    assert_eq!(
        d.trigger(TriggerRequest::internal(None)).unwrap(),
        TriggerResult::Spawned
    );
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 1));
    assert_eq!(trigger_puts(&fake), 2);
    d.disarm();
}

#[test]
fn ints_internal_with_nimages_one_triggers_up_to_ntrigger() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(1, 0.01, 3, None).unwrap();
    d.arm().unwrap();
    let mut spawned = 0;
    for _ in 0..6 {
        if d.trigger(TriggerRequest::internal(None)).unwrap() == TriggerResult::Spawned {
            spawned += 1;
        }
    }
    assert_eq!(spawned, 3);
    wait_live_zero(&d);
    assert_eq!(trigger_puts(&fake), 3);
}

#[test]
fn ints_public_blocking_skips_after_ntrigger() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(5, 0.01, 1, None).unwrap();
    d.arm().unwrap();
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Completed);
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Skipped);
    assert_eq!(trigger_puts(&fake), 1);
    // unknown configuration (no set_config in this process): never skipped by count
    let fake = FakeSimplon::start();
    fake.with(|s| s.detector.insert("ntrigger".into(), serde_json::json!(5)));
    let d = detector(&fake);
    d.arm().unwrap();
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Completed);
    assert_eq!(d.trigger(public()).unwrap(), TriggerResult::Completed);
}

#[test]
fn inte_triggers_are_serialised_with_count_time_body() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::For(Duration::from_millis(150)));
    let d = Arc::new(detector(&fake));
    d.set_trigger_mode("inte").unwrap();
    d.set_config(1, 0.1, 10, None).unwrap();
    d.arm().unwrap();
    fake.clear_log();
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let d = Arc::clone(&d);
            std::thread::spawn(move || d.trigger(TriggerRequest::public(None, Some(0.1))))
        })
        .collect();
    // internal (MovieIter-style) triggers serialise too
    let d3 = Arc::clone(&d);
    let h3 = std::thread::spawn(move || d3.trigger(TriggerRequest::internal(Some(0.1))));
    for h in hs {
        assert_eq!(h.join().unwrap().unwrap(), TriggerResult::Completed);
    }
    assert_eq!(h3.join().unwrap().unwrap(), TriggerResult::Spawned);
    wait_live_zero(&d);
    assert_eq!(
        fake.with(|s| s.max_triggers_in_flight),
        1,
        "never 2 in flight"
    );
    let bodies: Vec<_> = fake
        .log()
        .into_iter()
        .filter(|l| l.key() == "PUT detector/command/trigger")
        .map(|l| l.body)
        .collect();
    assert_eq!(bodies.len(), 3);
    for b in bodies {
        assert_eq!(b, Some(serde_json::json!({"value": 0.1})));
    }
}

#[test]
fn inte_waiter_released_by_cancel_sends_nothing_more() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = Arc::new(detector(&fake));
    d.set_trigger_mode("inte").unwrap();
    d.set_config(1, 0.1, 10, None).unwrap();
    d.arm().unwrap();
    let d1 = Arc::clone(&d);
    let h1 = std::thread::spawn(move || d1.trigger(TriggerRequest::public(None, Some(0.1))));
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 1));
    let d2 = Arc::clone(&d);
    let h2 = std::thread::spawn(move || d2.trigger(TriggerRequest::internal(Some(0.1))));
    std::thread::sleep(Duration::from_millis(50));
    d.cancel();
    let r1 = h1.join().unwrap().unwrap();
    let r2 = h2.join().unwrap().unwrap();
    assert!(
        matches!(r1, TriggerResult::Released | TriggerResult::Completed),
        "{r1:?}"
    );
    assert!(
        matches!(r2, TriggerResult::Released | TriggerResult::Suppressed),
        "{r2:?}"
    );
    wait_live_zero(&d);
    assert_eq!(trigger_puts(&fake), 1, "no trigger PUT after cancel");
}

#[test]
fn worker_ceiling_raises_busy() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = Arc::new(
        Detector::builder(fake.endpoint())
            .budgets(fast_budgets())
            .signals(Signals::new())
            .pool(WorkerPool::new(2))
            .build()
            .unwrap(),
    );
    d.set_config(1, 0.01, 99, None).unwrap();
    d.arm().unwrap();
    let hs: Vec<_> = (0..2)
        .map(|_| {
            let d = Arc::clone(&d);
            std::thread::spawn(move || d.trigger(public()))
        })
        .collect();
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.triggers_in_flight == 2));
    let e = d.trigger(public()).unwrap_err();
    assert!(
        matches!(e, DriverError::Busy(ref m) if m == "too many outstanding trigger requests"),
        "{e:?}"
    );
    // the failed reservation was rolled back: the book still allows the next trigger later
    d.disarm();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    wait_live_zero(&d);
    assert_eq!(trigger_puts(&fake), 2);
}

#[test]
fn trigger_timeout_and_http_error_propagate() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = detector(&fake);
    d.set_config(1, 0.01, 5, None).unwrap();
    d.arm().unwrap();
    let t0 = Instant::now();
    let e = d
        .trigger(TriggerRequest::public(
            Some(Duration::from_millis(200)),
            None,
        ))
        .unwrap_err();
    assert!(matches!(e, DriverError::Timeout(_)), "{e:?}");
    assert!(t0.elapsed() < Duration::from_millis(1500));
    fake.with(|s| {
        s.trigger_hold = TriggerHold::Immediate;
        s.release_gen += 1;
    });
    fake.rule(
        Some("PUT"),
        "command/trigger",
        Action::Status(500, "bad".into()),
        Some(1),
    );
    let e = d.trigger(public()).unwrap_err();
    assert!(matches!(e, DriverError::Http { status: 500, .. }), "{e:?}");
}

#[test]
fn trigger_budget_defaults_from_last_applied() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    // fast budgets: margin 2 s, min 2 s, max 10 s
    d.set_config(3, 0.5, 1, Some(1.0)).unwrap();
    let b = d.trigger_budget(dectris_simplon::ModeKind::Ints, None);
    assert_eq!(b, Duration::from_secs(5));
    let b = d.trigger_budget(dectris_simplon::ModeKind::Inte, None);
    assert_eq!(b, Duration::from_secs(3));
}

#[test]
fn trigger_during_initialize_is_a_state_error_without_http() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(400));
    let d = Arc::new(detector(&fake));
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || d2.initialize());
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.state == "initialize"));
    let n = fake.log().len();
    assert!(matches!(
        d.trigger(public()),
        Err(DriverError::State { .. })
    ));
    assert_eq!(fake.log().len(), n);
    h.join().unwrap().unwrap();
}

#[test]
fn internal_blocking_caller_waits_for_completion() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::For(Duration::from_millis(100)));
    let d = detector_with(
        &fake,
        Settings {
            trigger_blocking: false,
            ..Settings::default()
        },
    );
    d.set_config(1, 0.01, 1, None).unwrap();
    d.arm().unwrap();
    let t0 = Instant::now();
    let r = d
        .trigger(TriggerRequest {
            caller: TriggerCaller::InternalBlocking,
            timeout: None,
            count_time: None,
        })
        .unwrap();
    assert_eq!(r, TriggerResult::Completed);
    assert!(t0.elapsed() >= Duration::from_millis(90));
}

/// [G1-B] at the trigger layer: blocking ints trigger raced against cancel with 0-100 µs jitter;
/// every trigger returns in < 0.5 s.
#[test]
fn g1b_trigger_cancel_race() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    let d = Arc::new(detector(&fake));
    d.set_config(99_999, 0.01, 1, None).unwrap();
    let iterations = 1000;
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    let mut worst = Duration::ZERO;
    for i in 0..iterations {
        d.arm().unwrap();
        let d2 = Arc::clone(&d);
        let started = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&started);
        let h = std::thread::spawn(move || {
            s2.store(true, Ordering::SeqCst);
            let r = d2.trigger(public());
            (r, Instant::now())
        });
        while !started.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let jitter = Duration::from_micros(seed % 101);
        let t_spin = Instant::now();
        while t_spin.elapsed() < jitter {
            std::hint::spin_loop();
        }
        let t_cancel = Instant::now();
        d.cancel();
        let (r, t_ret) = h.join().unwrap();
        let r = r.unwrap_or_else(|e| panic!("iteration {i}: {e:?}"));
        assert!(
            matches!(
                r,
                TriggerResult::Released | TriggerResult::Completed | TriggerResult::Suppressed
            ),
            "{r:?}"
        );
        let el = t_ret.saturating_duration_since(t_cancel);
        worst = worst.max(el);
        assert!(el < Duration::from_millis(500), "iteration {i}: {el:?}");
    }
    d.disarm();
    wait_live_zero(&d);
    eprintln!("g1b race: {iterations} iterations, worst release {worst:?}");
}

#[test]
fn late_outcomes_are_drained() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(1, 0.01, 1, None).unwrap();
    d.arm().unwrap();
    assert_eq!(
        d.trigger(TriggerRequest::internal(None)).unwrap(),
        TriggerResult::Spawned
    );
    wait_live_zero(&d);
    assert_eq!(d.orphan_count(), 1);
    d.drain_late_outcomes();
    assert_eq!(d.orphan_count(), 0);
}
