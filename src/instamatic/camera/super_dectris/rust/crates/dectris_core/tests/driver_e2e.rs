//! End-to-end driver tests: the fake SIMPLON server (`dectris_simplon::testing`) and a fake
//! Stream V2 PUSH publisher against one [`Driver`] (DESIGN §8, Phase 3→4 integration).
//!
//! Messages a test publishes before the driver reads are simply queued on the PULL side; the
//! driver classifies them only when a receive path runs, i.e. after `arm()` returned.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use dectris_codec::encode::{
    end_message, image_message, start_message, ImageSpec, Payload, StartSpec,
};
use dectris_common::log::Deferred;
use dectris_common::DriverError;
use dectris_core::driver::{
    Driver, DriverConfig, FrameMode, FrameRecv, FrameStep, MessageEvent, Movie, StopReason,
};
use dectris_simplon::testing::{fast_budgets, Action, FakeSimplon, TriggerHold};
use dectris_simplon::{TriggerRequest, TriggerResult, WorkerPool};
use dectris_stream::{ConnectOutcome, PushServer, StreamTuning};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

fn tuning() -> StreamTuning {
    StreamTuning {
        window_floor: Duration::from_millis(300),
        window_cap: Duration::from_secs(60),
        slice: Duration::from_millis(50),
        drain_after_single: Duration::from_millis(50),
        flush_max_messages: 10_000,
        flush_max_time: Duration::from_secs(2),
        flush_quiet: Duration::from_millis(50),
    }
}

struct Fx {
    fake: FakeSimplon,
    push: PushServer,
    drv: Arc<Driver>,
}

fn fixture() -> Fx {
    let fake = FakeSimplon::start();
    let push = PushServer::bind_ephemeral().unwrap();
    let mut cfg = DriverConfig::new(fake.endpoint(), push.endpoint(), 1000);
    cfg.budgets = fast_budgets();
    cfg.tuning = tuning();
    cfg.pool = Some(WorkerPool::new(8));
    cfg.user_agent = "instamatic-dcx7/test".into();
    let drv = Driver::new(cfg).unwrap();
    let mut d = Deferred::new();
    let r = drv.connect(&mut d);
    d.discard();
    assert!(r.startup.reachable);
    assert!(
        matches!(r.zmq.outcome, ConnectOutcome::Connected),
        "{:?}",
        r.zmq.outcome
    );
    fake.clear_log();
    Fx { fake, push, drv }
}

fn uid(sid: u64) -> String {
    format!("01HSERIES{sid:016}")
}

fn start(sid: u64) -> Vec<u8> {
    let mut s = StartSpec::new(sid, (4, 6), "uint16");
    s.series_unique_id = uid(sid);
    s.count_time = 0.25;
    s.frame_time = 0.1;
    start_message(&s).unwrap()
}

fn image(sid: u64, image_id: u64) -> Vec<u8> {
    let data: Vec<u8> = (0..24u16)
        .flat_map(|i| (i + image_id as u16 * 7).to_le_bytes())
        .collect();
    let mut spec = ImageSpec::new(sid, image_id, (4, 6), 69, &data, Payload::BSLZ4);
    spec.series_unique_id = uid(sid);
    image_message(&spec).unwrap()
}

fn end(sid: u64) -> Vec<u8> {
    end_message(sid, &uid(sid))
}

fn send(push: &PushServer, msgs: &[Vec<u8>]) {
    for m in msgs {
        push.send(m).unwrap();
    }
}

fn series(sid: u64, n: u64) -> Vec<Vec<u8>> {
    let mut v = vec![start(sid)];
    v.extend((1..=n).map(|i| image(sid, i)));
    v.push(end(sid));
    v
}

fn records(d: Deferred) -> Vec<String> {
    d.into_records().into_iter().map(|r| r.message).collect()
}

fn is_timeout(e: &DriverError) -> bool {
    matches!(e, DriverError::Timeout(_))
}

// ---------------------------------------------------------------------------------------------
// construction / teardown
// ---------------------------------------------------------------------------------------------

#[test]
fn halves_share_one_signals_and_there_is_no_arc_cycle() {
    let fx = fixture();
    assert!(Arc::ptr_eq(fx.drv.signals(), fx.drv.detector().signals()));
    let weak_det: Weak<_> = Arc::downgrade(fx.drv.detector());
    let weak_stream: Weak<_> = Arc::downgrade(fx.drv.stream());
    let Fx { drv, .. } = fx;
    drop(drv);
    assert!(weak_det.upgrade().is_none(), "Detector leaked (Arc cycle)");
    assert!(weak_stream.upgrade().is_none(), "Stream leaked (Arc cycle)");
}

#[test]
fn release_connection_is_idempotent_and_frees_the_pull_guard() {
    let fx = fixture();
    let mut d = Deferred::new();
    let r1 = fx.drv.release_connection(&mut d);
    assert!(r1.released && r1.disarmed);
    assert!(r1.zmq.as_ref().unwrap().was_connected);
    let r2 = fx.drv.release_connection(&mut d);
    assert!(!r2.released && r2.zmq.is_none());
    d.discard();
    assert!(!fx.drv.stream().is_connected());
    // the endpoint's guard is free again: a second driver connects
    let mut cfg = DriverConfig::new(fx.fake.endpoint(), fx.push.endpoint(), 4);
    cfg.budgets = fast_budgets();
    cfg.tuning = tuning();
    let other = Driver::new(cfg).unwrap();
    let mut d = Deferred::new();
    let r = other.connect(&mut d);
    d.discard();
    assert!(matches!(r.zmq.outcome, ConnectOutcome::Connected));
}

#[test]
fn offline_release_does_no_http() {
    // nothing listens on this port: the startup probe fails, dcu_reachable = false
    let push = PushServer::bind_ephemeral().unwrap();
    let mut cfg = DriverConfig::new("http://127.0.0.1:9", push.endpoint(), 4);
    cfg.budgets = fast_budgets();
    cfg.tuning = tuning();
    let drv = Driver::new(cfg).unwrap();
    let mut d = Deferred::new();
    let r = drv.connect(&mut d);
    assert!(!r.startup.reachable && !drv.dcu_reachable());
    let t = Instant::now();
    let rel = drv.release_connection(&mut d);
    d.discard();
    assert!(rel.released && !rel.disarmed);
    assert!(t.elapsed() < Duration::from_millis(200));
}

// ---------------------------------------------------------------------------------------------
// _acquire_image
// ---------------------------------------------------------------------------------------------

#[test]
fn acquire_image_ints_roundtrip() {
    let fx = fixture();
    send(&fx.push, &series(1, 1));
    let mut d = Deferred::new();
    let x = fx.drv.acquire_image(0.1, &mut d).unwrap();
    let recs = records(d);
    assert_eq!(x.meta().series_id, Some(1));
    assert_eq!(x.meta().image_id, Some(1));
    assert_eq!(x.count_time(), Some(0.25));
    assert_eq!(fx.fake.count("PUT detector/command/arm"), 1);
    assert_eq!(fx.fake.count("PUT detector/command/trigger"), 1);
    assert_eq!(
        fx.fake.count("PUT detector/command/disarm"),
        0,
        "no disarm on success"
    );
    assert_eq!(
        fx.fake.detector_value("nimages"),
        Some(serde_json::json!(1))
    );
    // the 50 ms drain consumed the armed end: end_seen, owner cleared, nothing left queued
    assert!(fx.drv.signals().end_seen());
    assert!(fx.drv.detector().series_owner().is_none());
    assert!(
        recs.iter().any(|m| m.starts_with("received 1 frames")),
        "{recs:?}"
    );
    let mut d = Deferred::new();
    assert_eq!(fx.drv.flush_zmq_buffer(&mut d), 0);
    d.discard();
}

#[test]
fn acquire_image_twice_ignores_the_previous_series() {
    let fx = fixture();
    send(&fx.push, &series(1, 1));
    let mut d = Deferred::new();
    fx.drv.acquire_image(0.1, &mut d).unwrap();
    // a leftover of series 1 (e.g. a late duplicate) is stale for series 2
    send(&fx.push, &[image(1, 1)]);
    send(&fx.push, &series(2, 1));
    let x = fx.drv.acquire_image(0.1, &mut d).unwrap();
    d.discard();
    assert_eq!(x.meta().series_id, Some(2));
    assert!(fx.drv.stream().stats().stale >= 1);
}

#[test]
fn acquire_image_timeout_uses_the_series_tail_then_raises() {
    let fx = fixture();
    send(&fx.push, &[start(1)]);
    let t = Instant::now();
    let mut d = Deferred::new();
    let e = fx.drv.acquire_image(0.1, &mut d).unwrap_err();
    let recs = records(d);
    assert!(is_timeout(&e), "{e:?}");
    let ms = t.elapsed().as_millis();
    // window = 2·0.1 s + 1 s = 1.2 s (from the attributed start), twice (series tail)
    assert!((2300..6000).contains(&ms), "{ms} ms");
    assert!(recs.iter().any(|m| m.contains("series tail")), "{recs:?}");
    assert!(fx.drv.detector().get_detector_state() == "idle");
}

// ---------------------------------------------------------------------------------------------
// R7 series ownership
// ---------------------------------------------------------------------------------------------

#[test]
fn r7_busy_without_http_while_another_thread_owns_the_series() {
    let fx = fixture();
    let det = fx.drv.detector().clone();
    thread::Builder::new()
        .name("cred-thread".into())
        .spawn(move || det.arm().unwrap())
        .unwrap()
        .join()
        .unwrap();
    let before = fx.fake.log().len();
    let mut d = Deferred::new();
    let e = fx.drv.acquire_image(0.1, &mut d).unwrap_err();
    match &e {
        DriverError::Busy(m) => assert!(m.contains("cred-thread"), "{m}"),
        other => panic!("expected Busy, got {other:?}"),
    }
    assert_eq!(fx.fake.log().len(), before, "R7 refusal must not do HTTP");
    let movie = Movie::new(fx.drv.clone(), 3, 0.1);
    assert!(matches!(movie.next(&mut d), Err(DriverError::Busy(_))));
    assert!(matches!(
        fx.drv.arm_for_preview(),
        Err(DriverError::Busy(_))
    ));
    assert_eq!(fx.fake.log().len(), before);
    // the owner's disarm clears ownership
    fx.drv.detector().disarm();
    assert!(fx.drv.check_ownership().is_ok());
    d.discard();
}

#[test]
fn r7_armed_end_clears_ownership() {
    let fx = fixture();
    let det = fx.drv.detector().clone();
    thread::spawn(move || det.arm().unwrap()).join().unwrap();
    assert!(fx.drv.check_ownership().is_err());
    send(&fx.push, &[start(1), end(1)]);
    let mut d = Deferred::new();
    assert!(matches!(
        fx.drv.message_receiver(2000, &mut d).unwrap(),
        MessageEvent::Start(_)
    ));
    assert!(matches!(
        fx.drv.message_receiver(2000, &mut d).unwrap(),
        MessageEvent::End
    ));
    d.discard();
    assert!(fx.drv.check_ownership().is_ok());
}

#[test]
fn r7_acquire_image_releases_ownership_even_if_the_end_is_late() {
    let fx = fixture();
    // only start + image: the end arrives after the 50 ms drain
    send(&fx.push, &[start(1), image(1, 1)]);
    let drv = fx.drv.clone();
    thread::Builder::new()
        .name("executor".into())
        .spawn(move || {
            let mut d = Deferred::new();
            drv.acquire_image(0.1, &mut d).unwrap();
            d.discard();
        })
        .unwrap()
        .join()
        .unwrap();
    send(&fx.push, &[end(1)]);
    assert!(
        fx.drv.check_ownership().is_ok(),
        "a finished single image must not own the detector"
    );
    // the late end is stale for the next series
    send(&fx.push, &series(2, 1));
    let mut d = Deferred::new();
    let x = fx.drv.acquire_image(0.1, &mut d).unwrap();
    d.discard();
    assert_eq!(x.meta().series_id, Some(2));
}

#[test]
fn release_after_begin_shutdown_still_disarms_a_running_series() {
    let fx = fixture();
    fx.fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    fx.drv.detector().ensure_config(10, 0.1, 1, None).unwrap();
    fx.drv.detector().arm().unwrap();
    fx.drv
        .detector()
        .trigger(TriggerRequest::internal(None))
        .unwrap();
    assert!(fx
        .fake
        .wait_for(Duration::from_secs(2), |st| st.state == "acquire"));
    // DESIGN §3.7: _begin_shutdown (threading._register_atexit) runs before the atexit release
    fx.drv.begin_shutdown();
    let mut d = Deferred::new();
    let r = fx.drv.release_connection(&mut d);
    d.discard();
    assert!(r.released && r.disarmed);
    assert_eq!(fx.fake.count("PUT detector/command/disarm"), 1);
    assert_eq!(fx.fake.state(), "idle");
}

#[test]
fn movie_of_zero_frames_does_nothing() {
    let fx = fixture();
    let movie = Movie::new(fx.drv.clone(), 0, 0.1);
    let mut d = Deferred::new();
    assert!(matches!(
        movie.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::NFrames)
    ));
    d.discard();
    assert!(fx.fake.log().is_empty());
}

// ---------------------------------------------------------------------------------------------
// MovieIter core
// ---------------------------------------------------------------------------------------------

#[test]
fn movie_ints_delivers_n_frames_then_stops_and_disarms() {
    let fx = fixture();
    send(&fx.push, &series(1, 3));
    let movie = Movie::new(fx.drv.clone(), 3, 0.1);
    let mut d = Deferred::new();
    for i in 1..=3 {
        match movie.next(&mut d).unwrap() {
            FrameStep::Frame(x) => assert_eq!(x.meta().image_id, Some(i)),
            other => panic!("{other:?}"),
        }
    }
    assert!(matches!(
        movie.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::NFrames)
    ));
    assert!(matches!(
        movie.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::NFrames)
    ));
    d.discard();
    assert_eq!(
        fx.fake.detector_value("nimages"),
        Some(serde_json::json!(3))
    );
    assert_eq!(fx.fake.count("PUT detector/command/arm"), 1);
    // the internal trigger is non-blocking (its PUT may land after the pre-queued frames)
    assert!(fx.fake.wait_for(Duration::from_secs(2), |st| st
        .log
        .iter()
        .any(|l| l.key() == "PUT detector/command/trigger")));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fx.fake.count("PUT detector/command/trigger"),
        1,
        "one trigger per ints series"
    );
    assert_eq!(fx.drv.stream().registry().live_count(), 0);
}

#[test]
fn movie_short_series_raises_timeout_never_a_silent_stop() {
    let fx = fixture();
    send(&fx.push, &series(1, 1));
    let movie = Movie::new(fx.drv.clone(), 3, 0.1);
    let mut d = Deferred::new();
    assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Frame(_)));
    let e = movie.next(&mut d).unwrap_err();
    d.discard();
    match &e {
        DriverError::Timeout(m) => {
            assert!(m.contains("movie ended (end) after 1 of 3 frames"), "{m}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.drv.stream().registry().live_count(), 0);
}

#[test]
fn movie_inte_maps_to_ntrigger_and_triggers_each_frame() {
    let Fx { fake, push, drv } = fixture();
    drv.detector()
        .update_settings(|s| s.trigger_mode = "inte".into());
    thread::scope(|sc| {
        // like the DCU, frame k is published only after trigger k completed
        let f = &fake;
        let dcu = sc.spawn(move || {
            assert!(f.wait_for(Duration::from_secs(3), |st| st.triggers_done >= 1));
            send(&push, &[start(1), image(1, 1)]);
            assert!(f.wait_for(Duration::from_secs(3), |st| st.triggers_done >= 2));
            send(&push, &[image(1, 2), end(1)]);
            push
        });
        let movie = Movie::new(drv.clone(), 2, 0.2);
        let mut d = Deferred::new();
        assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Frame(_)));
        assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Frame(_)));
        assert!(matches!(
            movie.next(&mut d).unwrap(),
            FrameStep::Stopped(StopReason::NFrames)
        ));
        d.discard();
        dcu.join().unwrap();
    });
    assert_eq!(fake.detector_value("nimages"), Some(serde_json::json!(1)));
    assert_eq!(fake.detector_value("ntrigger"), Some(serde_json::json!(2)));
    let bodies: Vec<_> = fake
        .log()
        .into_iter()
        .filter(|l| l.key() == "PUT detector/command/trigger")
        .map(|l| l.body)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies
            .iter()
            .all(|b| b == &Some(serde_json::json!({"value": 0.2}))),
        "{bodies:?}"
    );
}

#[test]
fn movie_close_before_first_next_does_no_http() {
    let fx = fixture();
    let movie = Movie::new(fx.drv.clone(), 3, 0.1);
    movie.close();
    let mut d = Deferred::new();
    assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Stopped(_)));
    d.discard();
    assert!(fx.fake.log().is_empty(), "{:?}", fx.fake.keys());
}

#[test]
fn movie_close_after_arming_disarms() {
    let fx = fixture();
    fx.fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    send(&fx.push, &[start(1), image(1, 1)]);
    let movie = Movie::new(fx.drv.clone(), 5, 0.1);
    let mut d = Deferred::new();
    assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Frame(_)));
    // the internal trigger is non-blocking: wait until its PUT reached the fake
    assert!(fx
        .fake
        .wait_for(Duration::from_secs(2), |st| st.state == "acquire"));
    movie.close();
    assert_eq!(fx.fake.count("PUT detector/command/disarm"), 1);
    assert_eq!(fx.fake.state(), "idle");
    movie.close();
    assert_eq!(
        fx.fake.count("PUT detector/command/disarm"),
        1,
        "close is idempotent"
    );
    assert!(matches!(movie.next(&mut d).unwrap(), FrameStep::Stopped(_)));
    d.discard();
    assert_eq!(fx.drv.stream().registry().live_count(), 0);
}

// ---------------------------------------------------------------------------------------------
// FrameIter core, message_receiver, acquire_single_frame
// ---------------------------------------------------------------------------------------------

#[test]
fn frame_iter_silent_stop_on_end() {
    let fx = fixture();
    fx.drv.detector().arm().unwrap();
    send(&fx.push, &series(1, 2));
    let it = FrameRecv::new(fx.drv.clone(), 5, FrameMode::Frames);
    let mut d = Deferred::new();
    assert!(matches!(it.next(&mut d).unwrap(), FrameStep::Frame(_)));
    assert!(matches!(it.next(&mut d).unwrap(), FrameStep::Frame(_)));
    assert!(matches!(
        it.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::End)
    ));
    // the stop reason is sticky (review: a later call no longer reports n_frames)
    assert!(matches!(
        it.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::End)
    ));
    d.discard();
    assert_eq!(it.remaining(), 3);
    assert_eq!(fx.drv.stream().registry().live_count(), 0);
}

#[test]
fn frame_iter_releases_registry_with_the_last_frame() {
    let fx = fixture();
    fx.drv.detector().arm().unwrap();
    send(&fx.push, &series(1, 2));
    let it = FrameRecv::new(fx.drv.clone(), 2, FrameMode::Raw);
    let mut d = Deferred::new();
    assert!(matches!(it.next(&mut d).unwrap(), FrameStep::Frame(_)));
    assert_eq!(fx.drv.stream().registry().live_count(), 1);
    assert!(matches!(it.next(&mut d).unwrap(), FrameStep::Frame(_)));
    assert_eq!(fx.drv.stream().registry().live_count(), 0);
    d.discard();
}

#[test]
fn frame_iter_timeout_and_shutdown() {
    let fx = fixture();
    fx.drv.detector().arm().unwrap();
    let it = FrameRecv::new(fx.drv.clone(), 2, FrameMode::Frames);
    let mut d = Deferred::new();
    assert!(matches!(
        it.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::Timeout)
    ));
    let it2 = FrameRecv::new(fx.drv.clone(), 2, FrameMode::Frames);
    let drv = fx.drv.clone();
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        drv.begin_shutdown();
    });
    let t = Instant::now();
    assert!(matches!(
        it2.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::Shutdown)
    ));
    assert!(t.elapsed() < Duration::from_millis(250));
    stopper.join().unwrap();
    d.discard();
}

#[test]
fn message_receiver_start_image_end_none() {
    let fx = fixture();
    fx.drv.detector().arm().unwrap();
    send(&fx.push, &series(1, 1));
    let mut d = Deferred::new();
    assert!(matches!(
        fx.drv.message_receiver(1000, &mut d).unwrap(),
        MessageEvent::Start(_)
    ));
    match fx.drv.message_receiver(1000, &mut d).unwrap() {
        MessageEvent::Image(x) => assert_eq!(x.count_time(), Some(0.25)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        fx.drv.message_receiver(1000, &mut d).unwrap(),
        MessageEvent::End
    ));
    let t = Instant::now();
    assert!(matches!(
        fx.drv.message_receiver(150, &mut d).unwrap(),
        MessageEvent::Nothing
    ));
    assert!(t.elapsed() >= Duration::from_millis(140));
    d.discard();
}

#[test]
fn acquire_single_frame_triggers_once_per_series() {
    let fx = fixture();
    fx.fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    fx.drv.set_single_frame_fresh(false);
    fx.drv
        .detector()
        .ensure_config(99_999, 0.1, 1, None)
        .unwrap();
    fx.drv.detector().arm().unwrap();
    send(&fx.push, &[start(1), image(1, 1), image(1, 2), image(1, 3)]);
    let mut d = Deferred::new();
    for i in 1..=3 {
        let x = fx.drv.acquire_single_frame(&mut d).unwrap();
        assert_eq!(x.meta().image_id, Some(i), "oldest queued frame first");
    }
    assert!(is_timeout(
        &fx.drv.acquire_single_frame(&mut d).unwrap_err()
    ));
    d.discard();
    assert_eq!(fx.fake.count("PUT detector/command/trigger"), 1);
    assert_eq!(
        fx.fake.count("PUT detector/command/disarm"),
        0,
        "autocred owns the series"
    );
    fx.drv.detector().disarm();
}

#[test]
fn acquire_single_frame_fresh_returns_the_newest() {
    let fx = fixture();
    fx.fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    assert!(fx.drv.single_frame_fresh(), "the default (DESIGN §12 #8)");
    fx.drv.detector().arm().unwrap();
    send(&fx.push, &[start(1), image(1, 1), image(1, 2), image(1, 3)]);
    thread::sleep(Duration::from_millis(200));
    let mut d = Deferred::new();
    let x = fx.drv.acquire_single_frame(&mut d).unwrap();
    d.discard();
    assert_eq!(x.meta().image_id, Some(3));
    fx.drv.detector().disarm();
}

// ---------------------------------------------------------------------------------------------
// trig_cv glue: the armed series' end releases a blocking trigger (DESIGN §3.4 step 4)
// ---------------------------------------------------------------------------------------------

#[test]
fn armed_end_from_the_receiver_releases_a_blocking_trigger() {
    let fx = fixture();
    fx.fake.with(|s| s.trigger_hold = TriggerHold::UntilRelease);
    fx.drv.detector().arm().unwrap();
    let det = fx.drv.detector().clone();
    let trig = thread::spawn(move || det.trigger(TriggerRequest::public(None, None)));
    assert!(fx
        .fake
        .wait_for(Duration::from_secs(2), |st| st.triggers_in_flight == 1));
    send(&fx.push, &series(1, 1));
    let it = FrameRecv::new(fx.drv.clone(), 5, FrameMode::Frames);
    let mut d = Deferred::new();
    assert!(matches!(it.next(&mut d).unwrap(), FrameStep::Frame(_)));
    let t = Instant::now();
    assert!(matches!(
        it.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::End)
    ));
    d.discard();
    let r = trig.join().unwrap().unwrap();
    assert_eq!(r, TriggerResult::Released);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(fx.drv.detector().stats().trigger_unblocked, 1);
    fx.drv.detector().disarm();
}

// ---------------------------------------------------------------------------------------------
// ZMQ maintenance
// ---------------------------------------------------------------------------------------------

#[test]
fn reconnect_invalidates_the_ensure_cache() {
    let fx = fixture();
    fx.drv.detector().ensure_config(1, 0.1, 1, None).unwrap();
    assert!(fx.drv.detector().ensure_cache_key().is_some());
    let mut d = Deferred::new();
    let r = fx.drv.reconnect_zmq(&mut d);
    d.discard();
    assert!(r.ok());
    assert!(fx.drv.detector().ensure_cache_key().is_none());
}

#[test]
fn flush_is_busy_while_another_thread_receives() {
    let fx = fixture();
    fx.drv.detector().arm().unwrap();
    let drv = fx.drv.clone();
    let rx = thread::spawn(move || {
        let it = FrameRecv::new(drv, 1, FrameMode::Frames);
        let mut d = Deferred::new();
        let r = it.next(&mut d).unwrap();
        d.discard();
        r
    });
    thread::sleep(Duration::from_millis(100));
    let mut d = Deferred::new();
    assert_eq!(fx.drv.flush_zmq_buffer(&mut d), 0);
    let r = fx.drv.reconnect_zmq(&mut d);
    assert!(!r.ok());
    assert_eq!(
        r.stdout.as_deref(),
        Some("[Dectris] ZMQ reconnect skipped: receiver busy")
    );
    d.discard();
    send(&fx.push, &series(1, 1));
    assert!(matches!(rx.join().unwrap(), FrameStep::Frame(_)));
}

// ---------------------------------------------------------------------------------------------
// review fixes (Phase 1-4 adversarial review): R7 ownership, sticky stop reasons, close wake-up
// ---------------------------------------------------------------------------------------------

fn on_thread<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn review_acquire_image_trigger_failure_disarms_and_releases_owner() {
    let fx = fixture();
    fx.fake.rule(
        Some("PUT"),
        "command/trigger",
        Action::Status(500, "boom".into()),
        None,
    );
    let drv = fx.drv.clone();
    let r = on_thread("worker-A", move || {
        let mut d = Deferred::new();
        let r = drv.acquire_image(0.1, &mut d).map(|_| ());
        d.discard();
        r
    });
    assert!(r.is_err());
    assert_eq!(fx.fake.state(), "idle");
    assert!(fx.fake.count("PUT detector/command/disarm") >= 1);
    assert!(fx.drv.check_ownership().is_ok());
    assert!(fx.drv.detector().series_owner().is_none());
}

#[test]
fn review_movie_trigger_failure_disarms_and_releases_owner() {
    let fx = fixture();
    fx.fake.rule(
        Some("PUT"),
        "command/trigger",
        Action::Status(500, "boom".into()),
        None,
    );
    let drv = fx.drv.clone();
    let r = on_thread("movie-A", move || {
        let m = Movie::new(drv, 3, 0.1);
        let mut d = Deferred::new();
        let r = m.next(&mut d).map(|_| ());
        d.discard();
        r
    });
    assert!(r.is_err());
    assert_eq!(fx.fake.state(), "idle");
    assert!(fx.drv.check_ownership().is_ok());
}

#[test]
fn review_movie_releases_owner_with_the_nth_frame() {
    let fx = fixture();
    send(&fx.push, &series(1, 3));
    let drv = fx.drv.clone();
    let m = on_thread("movie-B", move || {
        let m = Movie::new(drv, 3, 0.1);
        let mut d = Deferred::new();
        for _ in 0..3 {
            assert!(matches!(m.next(&mut d).unwrap(), FrameStep::Frame(_)));
        }
        d.discard();
        m // exactly n calls (zip/islice pattern), kept alive: no drop involved
    });
    assert!(fx.drv.check_ownership().is_ok());
    let disarms = fx.fake.count("PUT detector/command/disarm");
    // the n-th frame disarms; no PUT when the fake DCU is already idle ("skipping disarm")
    assert!(disarms <= 1);
    let mut d = Deferred::new();
    assert!(matches!(
        m.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::NFrames)
    ));
    m.close();
    d.discard();
    // neither the (n+1)-th call nor a close() disarms again
    assert_eq!(fx.fake.count("PUT detector/command/disarm"), disarms);
    assert_eq!(m.stop_reason(), Some(StopReason::NFrames));
}

#[test]
fn review_dropped_movie_releases_owner() {
    let fx = fixture();
    send(&fx.push, &series(1, 5));
    let drv = fx.drv.clone();
    on_thread("movie-C", move || {
        let m = Movie::new(drv, 5, 0.1);
        let mut d = Deferred::new();
        assert!(matches!(m.next(&mut d).unwrap(), FrameStep::Frame(_)));
        d.discard();
        // dropped mid-series, never closed
    });
    assert!(fx.drv.check_ownership().is_ok());
}

#[test]
fn review_concurrent_setups_one_gets_busy() {
    let fx = fixture();
    fx.fake.rule(
        Some("PUT"),
        "config/count_time",
        Action::Delay(Duration::from_millis(300)),
        Some(1),
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut hs = vec![];
    for name in ["grabber", "cred"] {
        let drv = fx.drv.clone();
        let b = barrier.clone();
        hs.push(
            thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    b.wait();
                    let mut d = Deferred::new();
                    let r = drv.acquire_image(0.1, &mut d).map(|x| x.meta().series_id);
                    d.discard();
                    r
                })
                .unwrap(),
        );
    }
    thread::sleep(Duration::from_millis(700));
    send(&fx.push, &series(1, 1));
    let rs: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let busy = rs
        .iter()
        .filter(|r| matches!(r, Err(DriverError::Busy(_))))
        .count();
    assert_eq!(busy, 1, "{rs:?}");
    assert!(rs.iter().any(|r| matches!(r, Ok(Some(1)))), "{rs:?}");
    assert_eq!(fx.fake.count("PUT detector/command/arm"), 1);
}

#[test]
fn review_close_before_or_during_a_movie_is_cancelled() {
    let fx = fixture();
    let m = Movie::new(fx.drv.clone(), 5, 0.1);
    m.close();
    let mut d = Deferred::new();
    assert!(matches!(
        m.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::Cancelled)
    ));
    send(&fx.push, &series(1, 5));
    let m = Movie::new(fx.drv.clone(), 5, 0.1);
    for _ in 0..2 {
        assert!(matches!(m.next(&mut d).unwrap(), FrameStep::Frame(_)));
    }
    m.close();
    assert!(matches!(
        m.next(&mut d).unwrap(),
        FrameStep::Stopped(StopReason::Cancelled)
    ));
    assert_eq!(m.stop_reason(), Some(StopReason::Cancelled));
    assert_eq!(m.delivered(), 2);
    d.discard();
}

#[test]
fn review_close_from_another_thread_wakes_a_blocked_next() {
    let fx = fixture();
    fx.drv.stream().set_receive_timeout(Some(5.0));
    send(&fx.push, &series(1, 1)[..2]); // start + one image, then silence
    let m = Arc::new(Movie::new(fx.drv.clone(), 5, 0.1));
    let mut d = Deferred::new();
    assert!(matches!(m.next(&mut d).unwrap(), FrameStep::Frame(_)));
    d.discard();
    let m2 = m.clone();
    let h = thread::spawn(move || {
        let mut d = Deferred::new();
        let r = m2
            .next(&mut d)
            .map(|s| matches!(s, FrameStep::Stopped(StopReason::Cancelled)));
        d.discard();
        r
    });
    thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    m.close();
    assert!(
        h.join().unwrap().unwrap(),
        "blocked next() did not stop as cancelled"
    );
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

    let it = Arc::new(FrameRecv::new(fx.drv.clone(), 5, FrameMode::Frames));
    let it2 = it.clone();
    let h = thread::spawn(move || {
        let mut d = Deferred::new();
        let r = it2
            .next(&mut d)
            .map(|s| matches!(s, FrameStep::Stopped(StopReason::Cancelled)));
        d.discard();
        r
    });
    thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    it.close();
    assert!(h.join().unwrap().unwrap());
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(it.stop_reason(), Some(StopReason::Cancelled));
}

#[test]
fn review_failed_initialize_is_a_state_error() {
    let fx = fixture();
    fx.drv
        .detector()
        .update_settings(|s| s.auto_initialize = true);
    fx.fake.set_state("error");
    fx.fake.rule(
        Some("PUT"),
        "command/initialize",
        Action::Status(500, "init failed".into()),
        None,
    );
    let mut d = Deferred::new();
    let r = fx.drv.acquire_image(0.1, &mut d);
    d.discard();
    assert!(matches!(r, Err(DriverError::State { .. })), "{r:?}");
    assert_eq!(fx.fake.count("PUT detector/command/arm"), 0);
}
