//! State-machine policy against the fake SIMPLON server (DESIGN §4.2): ensure_idle [G2-J],
//! initialize [G2-A][G1-H], set_config steps 0-8 [G1-D][G2-B][G2-C][G2-M], re-apply after an
//! implicit initialize [R16] (DESIGN §12 #7), ensure_config, disarm/abort/cancel, getters + back-off
//! [R10], the [#12 keep] wrappers.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{detector, detector_with, Action, FakeSimplon, FtModel};
use dectris_common::DriverError;
use dectris_simplon::{FilewriterConfig, FrameTimePolicy, Settings};
use serde_json::json;

fn idx(keys: &[String], key: &str) -> Option<usize> {
    keys.iter().position(|k| k == key)
}

/// Set the fake's state from another thread after `ms`.
fn set_state_later(
    fake: &Arc<FakeSimplon>,
    state: &'static str,
    ms: u64,
) -> std::thread::JoinHandle<()> {
    let f = Arc::clone(fake);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        f.set_state(state);
    })
}

// ---------------------------------------------------------------------------------------------
// ensure_idle [G2-J]
// ---------------------------------------------------------------------------------------------

#[test]
fn g2j_idle_is_a_single_get() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    assert!(!d.ensure_idle().unwrap());
    assert_eq!(fake.keys(), vec!["GET detector/status/state"]);
}

#[test]
fn g2j_ready_disarms_then_regets() {
    let fake = FakeSimplon::start();
    fake.set_state("ready");
    let d = detector(&fake);
    assert!(!d.ensure_idle().unwrap());
    assert_eq!(
        fake.keys(),
        vec![
            "GET detector/status/state",
            "PUT detector/command/disarm",
            "GET detector/status/state"
        ]
    );
}

#[test]
fn g2j_acquire_cancels_then_disarms() {
    let fake = FakeSimplon::start();
    fake.set_state("acquire");
    let d = detector(&fake);
    assert!(!d.ensure_idle().unwrap());
    assert_eq!(
        fake.keys(),
        vec![
            "GET detector/status/state",
            "PUT detector/command/cancel",
            "PUT detector/command/disarm",
            "GET detector/status/state"
        ]
    );
}

#[test]
fn g2j_failed_cancel_still_disarms() {
    let fake = FakeSimplon::start();
    fake.set_state("acquire");
    fake.rule(
        Some("PUT"),
        "command/cancel",
        Action::Status(500, "x".into()),
        Some(1),
    );
    let d = detector(&fake);
    assert!(!d.ensure_idle().unwrap());
    assert_eq!(fake.count("PUT detector/command/disarm"), 1);
    assert_eq!(fake.count("PUT detector/command/initialize"), 0);
}

#[test]
fn g2j_failed_disarm_initializes() {
    let fake = FakeSimplon::start();
    fake.set_state("acquire");
    fake.rule(
        Some("PUT"),
        "command/disarm",
        Action::Status(500, "x".into()),
        Some(1),
    );
    let d = detector(&fake);
    assert!(d.ensure_idle().unwrap());
    let keys = fake.keys();
    assert_eq!(fake.count("PUT detector/command/initialize"), 1, "{keys:?}");
    // stream + monitor re-enabled after initialize (PROTOCOL §3.6)
    let i = idx(&keys, "PUT detector/command/initialize").unwrap();
    assert!(keys[i..].contains(&"PUT stream/config/mode".to_owned()));
    assert!(keys[i..].contains(&"PUT stream/config/format".to_owned()));
    assert!(keys[i..].contains(&"PUT monitor/config/mode".to_owned()));
}

#[test]
fn g2j_state_not_idle_after_disarm_initializes() {
    let fake = FakeSimplon::start();
    fake.set_state("ready");
    // disarm "succeeds" but the detector reports error
    fake.rule(
        Some("PUT"),
        "command/disarm",
        Action::Status(200, String::new()),
        Some(1),
    );
    let fake = Arc::new(fake);
    let d = detector(&fake);
    // after the (fake-intercepted) disarm the state is still ready → initialize
    assert!(d.ensure_idle().unwrap());
    assert_eq!(fake.count("PUT detector/command/initialize"), 1);
}

#[test]
fn g2j_external_initialize_is_waited_for_never_doubled() {
    let fake = Arc::new(FakeSimplon::start());
    fake.set_state("initialize");
    let h = set_state_later(&fake, "idle", 200);
    let d = detector(&fake);
    let t0 = Instant::now();
    assert!(
        d.ensure_idle().unwrap(),
        "an initialize ran: report reinitialized"
    );
    assert!(t0.elapsed() >= Duration::from_millis(180));
    h.join().unwrap();
    assert_eq!(fake.count("PUT detector/command/initialize"), 0);
    assert!(
        fake.count("PUT stream/config/mode") >= 1,
        "stream re-enabled"
    );
}

#[test]
fn g2j_configure_waits_then_idle() {
    let fake = Arc::new(FakeSimplon::start());
    fake.set_state("configure");
    let h = set_state_later(&fake, "idle", 100);
    let d = detector(&fake);
    assert!(!d.ensure_idle().unwrap());
    h.join().unwrap();
    assert_eq!(fake.count("PUT detector/command/initialize"), 0);
}

#[test]
fn g2j_configure_stuck_initializes() {
    let fake = FakeSimplon::start();
    fake.set_state("configure");
    let d = detector(&fake);
    let t0 = Instant::now();
    assert!(d.ensure_idle().unwrap());
    assert!(
        t0.elapsed() >= Duration::from_millis(550),
        "waited the transient budget"
    );
    assert_eq!(fake.count("PUT detector/command/initialize"), 1);
}

#[test]
fn g2j_bad_states_initialize() {
    for st in ["na", "error", "test", "weird"] {
        let fake = FakeSimplon::start();
        fake.set_state(st);
        let d = detector(&fake);
        assert!(d.ensure_idle().unwrap(), "{st}");
        assert_eq!(fake.count("PUT detector/command/initialize"), 1, "{st}");
        assert_eq!(fake.state(), "idle");
    }
}

#[test]
fn g2j_unknown_state_initializes() {
    let fake = FakeSimplon::start();
    fake.rule(Some("GET"), "status/state", Action::Close, Some(1));
    let d = detector(&fake);
    assert!(d.ensure_idle().unwrap());
    assert_eq!(fake.count("PUT detector/command/initialize"), 1);
}

#[test]
fn g2j_auto_initialize_off_raises_state_error() {
    let fake = FakeSimplon::start();
    fake.set_state("na");
    let d = detector_with(
        &fake,
        Settings {
            auto_initialize: false,
            ..Settings::default()
        },
    );
    let e = d.ensure_idle().unwrap_err();
    assert!(
        matches!(e, DriverError::State { ref state, .. } if state == "na"),
        "{e:?}"
    );
    assert_eq!(fake.count("PUT detector/command/initialize"), 0);
    // arm propagates it, without an arm PUT
    assert!(matches!(d.arm(), Err(DriverError::State { .. })));
    assert_eq!(fake.count("PUT detector/command/arm"), 0);
}

#[test]
fn ensure_idle_shutdown() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.request_stop();
    assert!(matches!(d.ensure_idle(), Err(DriverError::Shutdown)));
    assert!(fake.keys().is_empty());
}

// ---------------------------------------------------------------------------------------------
// initialize [G2-A] [G1-H]
// ---------------------------------------------------------------------------------------------

fn assert_quiet_during_initialize(fake: &FakeSimplon) {
    let log = fake.log();
    let i = log
        .iter()
        .position(|l| l.key() == "PUT detector/command/initialize")
        .expect("initialize PUT");
    let t_put = log[i].at;
    for l in &log[i + 1..] {
        assert!(
            l.at >= t_put + Duration::from_millis(450),
            "request {} issued {:?} after the initialize PUT, while it was in flight",
            l.key(),
            l.at - t_put
        );
    }
}

#[test]
fn g2a_cancel_then_initialize_waits_only_for_outcome() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(500));
    let d = Arc::new(detector(&fake));
    d.cancel();
    fake.clear_log();
    let t0 = Instant::now();
    d.initialize().unwrap();
    assert!(
        t0.elapsed() >= Duration::from_millis(480),
        "caller waited for the PUT"
    );
    assert_quiet_during_initialize(&fake);
    let keys = fake.keys();
    let i = idx(&keys, "PUT detector/command/initialize").unwrap();
    let rest = &keys[i + 1..];
    assert!(
        rest.contains(&"GET detector/status/state".to_owned()),
        "{keys:?}"
    );
    assert!(rest.contains(&"PUT stream/config/mode".to_owned()));
    assert!(rest.contains(&"PUT stream/config/format".to_owned()));
    assert!(rest.contains(&"PUT monitor/config/mode".to_owned()));
    assert_eq!(fake.stream_value("mode"), Some(json!("enabled")));
}

#[test]
fn g1h_getters_answer_without_io_during_initialize() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(500));
    let d = Arc::new(detector(&fake));
    assert_eq!(d.get_hv_state(), "READY");
    assert_eq!(d.get_countrate_correction(), Some(true));
    assert_eq!(d.get_stream_mode(), "enabled");
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || d2.initialize());
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.state == "initialize"));
    let n0 = fake.log().len();
    for _ in 0..5 {
        assert_eq!(d.get_detector_state(), "initialize");
        assert_eq!(d.get_hv_state(), "READY");
        assert_eq!(d.get_stream_mode(), "enabled");
        assert_eq!(d.get_countrate_correction(), Some(true));
        assert!(d.monitor_frame().unwrap().is_none());
        assert_eq!(d.get_bit_depth(), 0);
    }
    // best-effort commands stay silent too
    d.cancel();
    d.abort();
    assert_eq!(fake.log().len(), n0, "{:?}", &fake.keys()[n0..]);
    h.join().unwrap().unwrap();
    assert_quiet_during_initialize(&fake);
    assert_eq!(d.get_detector_state(), "idle");
}

#[test]
fn g2a_concurrent_initialize_joins_one_put() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(300));
    let d = Arc::new(detector(&fake));
    let hs: Vec<_> = (0..3)
        .map(|_| {
            let d = Arc::clone(&d);
            std::thread::spawn(move || d.initialize())
        })
        .collect();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    assert_eq!(fake.count("PUT detector/command/initialize"), 1);
}

#[test]
fn g2a_arm_from_na_after_cancel() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(500));
    let d = detector(&fake);
    d.cancel();
    fake.set_state("na");
    fake.clear_log();
    let out = d.arm().unwrap();
    assert!(out.reinitialized);
    assert_eq!(
        out.sequence_id,
        Some(1),
        "sequence id reset by initialize, then arm"
    );
    assert_quiet_during_initialize(&fake);
    let keys = fake.keys();
    assert!(idx(&keys, "PUT detector/command/initialize") < idx(&keys, "PUT detector/command/arm"));
}

#[test]
fn initialize_failure_is_logged_not_raised() {
    let fake = FakeSimplon::start();
    fake.rule(
        Some("PUT"),
        "command/initialize",
        Action::Status(500, "boom".into()),
        Some(1),
    );
    let d = detector(&fake);
    d.initialize().unwrap();
    // no idle wait / re-enable after a failed initialize
    assert_eq!(fake.count("PUT stream/config/mode"), 0);
    assert!(!d.init_in_flight());
}

#[test]
fn initialize_put_timeout_releases_gate() {
    let fake = FakeSimplon::start();
    fake.rule(Some("PUT"), "command/initialize", Action::Hang, Some(1));
    let mut b = common::fast_budgets();
    b.init_put = Duration::from_millis(300);
    let d = dectris_simplon::Detector::builder(fake.endpoint())
        .budgets(b)
        .pool(dectris_simplon::WorkerPool::new(8))
        .build()
        .unwrap();
    let t0 = Instant::now();
    d.initialize().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(!d.init_in_flight());
    // the PUT timeout opened the offline back-off: the Tk getter answers 'unknown' without I/O,
    // the policy idle wait does real I/O
    assert_eq!(d.get_detector_state(), "unknown");
    assert_eq!(
        d.wait_for_idle(Duration::from_secs(1), Duration::from_millis(20)),
        "idle"
    );
    fake.release_hangs();
}

#[test]
fn initialize_caller_released_by_shutdown() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.init_delay = Duration::from_millis(2000));
    let d = Arc::new(detector(&fake));
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || {
        let t0 = Instant::now();
        (d2.initialize(), t0.elapsed())
    });
    assert!(fake.wait_for(Duration::from_secs(2), |s| s.state == "initialize"));
    d.request_stop();
    let (r, el) = h.join().unwrap();
    assert!(matches!(r, Err(DriverError::Shutdown)), "{r:?}");
    assert!(el < Duration::from_millis(1000), "{el:?}");
}

// ---------------------------------------------------------------------------------------------
// set_config / ensure_config
// ---------------------------------------------------------------------------------------------

fn put_keys(fake: &FakeSimplon) -> Vec<String> {
    common::keys_matching(fake, |k| k.starts_with("PUT detector/config/"))
}

#[test]
fn set_config_order_readout_policy_and_last_applied() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(10, 0.1, 1, None).unwrap();
    assert_eq!(
        put_keys(&fake),
        vec![
            "PUT detector/config/frame_time",
            "PUT detector/config/count_time",
            "PUT detector/config/frame_time",
            "PUT detector/config/nimages",
            "PUT detector/config/ntrigger",
            "PUT detector/config/trigger_mode",
            "PUT detector/config/compression",
        ]
    );
    let ft = fake.detector_value("frame_time").unwrap().as_f64().unwrap();
    assert!((ft - 0.104).abs() < 1e-9, "{ft}");
    assert_eq!(fake.detector_value("nimages"), Some(json!(10)));
    let la = d.last_applied().unwrap();
    assert_eq!((la.nimages, la.ntrigger), (10, 1));
    assert!((la.frame_time - 0.104).abs() < 1e-9);
    assert_eq!(la.requested_frame_time, None);
    // readback GETs happened (step 7)
    assert!(fake.count("GET detector/config/count_time") >= 1);
    assert!(fake.count("GET detector/config/frame_time") >= 1);
    // no header_appendix configured → never PUT
    assert_eq!(fake.count("PUT stream/config/header_appendix"), 0);
}

#[test]
fn set_config_explicit_frame_time_and_detector_policy() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(1, 0.2, 1, Some(0.5)).unwrap();
    assert_eq!(fake.detector_value("frame_time"), Some(json!(0.5)));
    assert_eq!(d.last_applied().unwrap().requested_frame_time, Some(0.5));
    // policy detector: no frame_time PUT at all; effective ft from the readback
    let fake = FakeSimplon::start();
    let d = detector_with(
        &fake,
        Settings {
            frame_time_policy: FrameTimePolicy::Detector,
            ..Settings::default()
        },
    );
    d.set_config(1, 0.1, 1, None).unwrap();
    assert_eq!(fake.count("PUT detector/config/frame_time"), 0);
    assert_eq!(fake.count("GET detector/config/detector_readout_time"), 0);
    let la = d.last_applied().unwrap();
    assert!(
        (la.frame_time - 0.504).abs() < 1e-9,
        "DCU value kept: {}",
        la.frame_time
    );
}

#[test]
fn set_config_readout_get_failure_falls_back() {
    let fake = FakeSimplon::start();
    fake.rule(
        Some("GET"),
        "detector_readout_time",
        Action::Status(404, "x".into()),
        None,
    );
    let d = detector(&fake);
    d.set_config(1, 0.3, 1, None).unwrap();
    let ft = fake.detector_value("frame_time").unwrap().as_f64().unwrap();
    assert!((ft - 0.304).abs() < 1e-9);
}

#[test]
fn g2b_enable_modes_map_nimages_to_ntrigger() {
    for mode in ["inte", "exte"] {
        let fake = FakeSimplon::start();
        let d = detector(&fake);
        d.set_trigger_mode(mode).unwrap();
        d.set_config(5, 0.1, 1, None).unwrap();
        assert_eq!(fake.detector_value("nimages"), Some(json!(1)), "{mode}");
        assert_eq!(fake.detector_value("ntrigger"), Some(json!(5)), "{mode}");
        let la = d.last_applied().unwrap();
        assert_eq!((la.nimages, la.ntrigger), (1, 5));
        // given ntrigger: nimages forced to 1, ntrigger as given
        d.set_config(1, 0.1, 99_999, None).unwrap();
        assert_eq!(fake.detector_value("ntrigger"), Some(json!(99_999)));
        // the ensure key holds the mapped values
        d.ensure_config(7, 0.1, 1, None).unwrap();
        let k = d.ensure_cache_key().unwrap();
        assert_eq!((k.nimages, k.ntrigger), (1, 7));
        fake.clear_log();
        d.ensure_config(7, 0.1, 1, None).unwrap();
        assert!(fake.keys().is_empty(), "cache hit");
    }
}

#[test]
fn g2c_reject_model_first_frame_time_is_best_effort() {
    let fake = FakeSimplon::start();
    fake.with(|s| {
        s.ft_model = FtModel::Reject;
        s.detector.insert("count_time".into(), json!(1.0));
        s.detector.insert("frame_time".into(), json!(1.004));
    });
    let d = detector(&fake);
    d.set_config(1, 0.1, 1, None).unwrap();
    assert_eq!(fake.count("PUT detector/config/frame_time"), 2);
    assert_eq!(fake.detector_value("count_time"), Some(json!(0.1)));
    let ft = fake.detector_value("frame_time").unwrap().as_f64().unwrap();
    assert!((ft - 0.104).abs() < 1e-9);
    assert!((d.last_applied().unwrap().frame_time - 0.104).abs() < 1e-9);
    // and back up: 0.1 → 2.0 works in either model
    d.set_config(1, 2.0, 1, None).unwrap();
    assert_eq!(fake.detector_value("count_time"), Some(json!(2.0)));
}

#[test]
fn set_config_strict_failure_raises_and_keeps_last_applied() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_config(1, 0.1, 1, None).unwrap();
    let before = d.last_applied();
    fake.rule(
        Some("PUT"),
        "config/count_time",
        Action::Status(500, "x".into()),
        Some(1),
    );
    let e = d.set_config(3, 0.2, 1, None).unwrap_err();
    assert!(matches!(e, DriverError::Http { status: 500, .. }));
    assert_eq!(d.last_applied(), before);
}

#[test]
fn trigger_mode_405_fallback_compares_and_continues() {
    let fake = FakeSimplon::start();
    fake.rule(
        Some("PUT"),
        "config/trigger_mode",
        Action::Status(405, String::new()),
        None,
    );
    let d = detector(&fake);
    d.set_config(1, 0.1, 1, None).unwrap();
    assert!(fake.count("GET detector/config/trigger_mode") >= 1);
    // 405 + failing readback GET → raise
    fake.rule(
        Some("GET"),
        "config/trigger_mode",
        Action::Status(500, String::new()),
        None,
    );
    assert!(d.set_config(1, 0.1, 1, None).is_err());
    // non-405 error → raise
    fake.clear_rules();
    fake.rule(
        Some("PUT"),
        "config/compression",
        Action::Status(400, String::new()),
        None,
    );
    assert!(matches!(
        d.set_config(1, 0.1, 1, None),
        Err(DriverError::Http { status: 400, .. })
    ));
}

#[test]
fn compression_none_is_never_sent() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.set_compression("none").unwrap();
    assert_eq!(fake.count("PUT detector/config/compression"), 0);
    d.set_config(1, 0.1, 1, None).unwrap();
    assert_eq!(fake.count("PUT detector/config/compression"), 0);
    d.set_compression("lz4").unwrap();
    assert_eq!(fake.detector_value("compression"), Some(json!("lz4")));
}

#[test]
fn g2m_header_appendix_falls_back_to_string_and_remembers() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.reject_object_header_appendix = true);
    let obj = json!({"id": "super_dectris Data Collection", "version": "x"});
    let d = detector_with(
        &fake,
        Settings {
            header_appendix: Some(obj.clone()),
            ..Settings::default()
        },
    );
    d.set_config(1, 0.1, 1, None).unwrap();
    let puts: Vec<_> = fake
        .log()
        .into_iter()
        .filter(|l| l.key() == "PUT stream/config/header_appendix")
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0].body, Some(json!({"value": obj})));
    assert_eq!(puts[1].body, Some(json!({"value": obj.to_string()})));
    fake.clear_log();
    d.set_config(1, 0.1, 1, None).unwrap();
    let puts: Vec<_> = fake
        .log()
        .into_iter()
        .filter(|l| l.key() == "PUT stream/config/header_appendix")
        .collect();
    assert_eq!(puts.len(), 1, "string form remembered");
    assert_eq!(puts[0].body, Some(json!({"value": obj.to_string()})));
    // a failing header_appendix never fails set_config
    fake.rule(
        Some("PUT"),
        "header_appendix",
        Action::Status(500, String::new()),
        None,
    );
    d.set_config(1, 0.1, 1, None).unwrap();
}

#[test]
fn g1d_set_config_invalidates_ensure_cache() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.ensure_config(1, 0.1, 1, None).unwrap();
    assert!(d.ensure_cache_key().is_some());
    d.set_config(5, 0.2, 1, None).unwrap();
    assert!(d.ensure_cache_key().is_none());
    fake.clear_log();
    d.ensure_config(1, 0.1, 1, None).unwrap();
    let ct: Vec<_> = fake
        .log()
        .into_iter()
        .filter(|l| l.key() == "PUT detector/config/count_time")
        .collect();
    assert_eq!(ct.len(), 1, "A re-applied");
    assert_eq!(ct[0].body, Some(json!({"value": 0.1})));
    // a set_config that fails at nimages also leaves the cache empty
    fake.rule(
        Some("PUT"),
        "config/nimages",
        Action::Status(500, String::new()),
        Some(1),
    );
    assert!(d.set_config(500, 0.1, 1, None).is_err());
    assert!(d.ensure_cache_key().is_none());
}

#[test]
fn ensure_config_cache_semantics() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.ensure_config(1, 0.1, 1, None).unwrap();
    fake.clear_log();
    d.ensure_config(1, 0.1, 1, Some(0.0)).unwrap(); // 0.0 and None collide (PROTOCOL §3.7)
    assert!(fake.keys().is_empty());
    d.set_trigger_mode("ints").unwrap(); // invalidates
    fake.clear_log();
    d.ensure_config(1, 0.1, 1, None).unwrap();
    assert!(fake.count("PUT detector/config/count_time") == 1);
    // stream enable failure → not cached
    fake.rule(
        Some("PUT"),
        "stream/api/1.8.0/config/mode",
        Action::Status(500, String::new()),
        None,
    );
    d.invalidate_ensure_cache();
    d.ensure_config(2, 0.1, 1, None).unwrap();
    assert!(d.ensure_cache_key().is_none());
}

#[test]
fn r16_reapply_after_implicit_initialize_on_cache_hit() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.ensure_config(3, 0.1, 1, None).unwrap();
    fake.set_state("na");
    fake.clear_log();
    d.ensure_config(3, 0.1, 1, None).unwrap(); // cache hit, no I/O
    assert!(fake.keys().is_empty());
    let out = d.arm().unwrap();
    assert!(out.reinitialized);
    let keys = fake.keys();
    let i_init = idx(&keys, "PUT detector/command/initialize").unwrap();
    let i_ct = idx(&keys, "PUT detector/config/count_time").unwrap();
    let i_ni = idx(&keys, "PUT detector/config/nimages").unwrap();
    let i_arm = idx(&keys, "PUT detector/command/arm").unwrap();
    assert!(i_init < i_ct && i_ct < i_ni && i_ni < i_arm, "{keys:?}");
    assert_eq!(fake.detector_value("nimages"), Some(json!(3)));
    assert!(
        d.ensure_cache_key().is_none(),
        "invalidated after re-initialize"
    );
}

#[test]
fn reinitialized_without_config_warns_and_arms() {
    dectris_common::testing::install();
    let fake = FakeSimplon::start();
    fake.set_state("error");
    let d = detector(&fake);
    let out = d.arm().unwrap();
    assert!(out.reinitialized);
    assert_eq!(fake.count("PUT detector/config/count_time"), 0);
    assert!(
        !dectris_common::testing::take_matching("DCU default configuration in effect").is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// arm / disarm / abort / cancel
// ---------------------------------------------------------------------------------------------

#[test]
fn arm_parses_both_sequence_id_spellings_and_empty_body() {
    use common::ArmBody;
    for (style, want) in [
        (ArmBody::SequenceId, Some(1)),
        (ArmBody::SequenceIdSpace, Some(1)),
        (ArmBody::Empty, None),
    ] {
        let fake = FakeSimplon::start();
        fake.with(|s| s.arm_body = style);
        let d = detector(&fake);
        let e0 = d.epoch();
        let out = d.arm().unwrap();
        assert_eq!(out.sequence_id, want, "{style:?}");
        assert_eq!(out.epoch, e0 + 1);
        assert!(!out.reinitialized);
    }
}

#[test]
fn arm_failure_raises_and_clears_owner() {
    let fake = FakeSimplon::start();
    fake.rule(
        Some("PUT"),
        "command/arm",
        Action::Status(500, "x".into()),
        None,
    );
    let d = detector(&fake);
    assert!(matches!(
        d.arm(),
        Err(DriverError::Http { status: 500, .. })
    ));
    assert!(d.series_owner().is_none());
}

#[test]
fn arm_timeout_raises_timeout() {
    let fake = FakeSimplon::start();
    fake.rule(
        Some("PUT"),
        "command/arm",
        Action::Delay(Duration::from_millis(2500)),
        Some(1),
    );
    let d = detector(&fake);
    let t0 = Instant::now();
    assert!(matches!(d.arm(), Err(DriverError::Timeout(_))));
    assert!(t0.elapsed() < Duration::from_millis(2400));
}

#[test]
fn arm_clears_flags_and_records_owner() {
    let fake = FakeSimplon::start();
    let d = Arc::new(detector(&fake));
    d.cancel();
    d.signals().set_end_seen();
    assert!(d.signals().cancel_requested());
    d.arm().unwrap();
    assert!(!d.signals().cancel_requested());
    assert!(!d.signals().end_seen());
    assert!(d.series_owner().is_some());
    assert!(d.armed_by_other_thread().is_none());
    let d2 = Arc::clone(&d);
    let other = std::thread::spawn(move || d2.armed_by_other_thread())
        .join()
        .unwrap();
    assert!(other.is_some());
    d.disarm();
    assert!(d.series_owner().is_none());
}

#[test]
fn disarm_table() {
    let cases = [
        ("idle", false),
        ("na", false),
        ("error", false),
        ("test", false),
        ("ready", true),
        ("acquire", true),
    ];
    for (st, put) in cases {
        let fake = FakeSimplon::start();
        fake.set_state(st);
        let d = detector(&fake);
        d.disarm();
        assert!(d.signals().cancel_requested(), "{st}");
        assert_eq!(
            fake.count("PUT detector/command/disarm"),
            usize::from(put),
            "{st}"
        );
        // a running series is stopped first: a DCU may finish it before answering disarm
        assert_eq!(
            fake.count("PUT detector/command/cancel"),
            usize::from(st == "acquire"),
            "{st}"
        );
    }
    // the FileWriter on: an idle detector is disarmed too, to close the files (cRED stops by cancel)
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    d.configure_filewriter(&FilewriterConfig::default()).unwrap();
    d.disarm();
    assert_eq!(fake.count("PUT detector/command/disarm"), 1);
    d.disable_filewriter();
    d.disarm();
    assert_eq!(fake.count("PUT detector/command/disarm"), 1);
    // initialize/configure: wait ≤ 10 s (0.6 s here) then return without sending
    let fake = FakeSimplon::start();
    fake.set_state("configure");
    let d = detector(&fake);
    let t0 = Instant::now();
    d.disarm();
    assert!(t0.elapsed() >= Duration::from_millis(550));
    assert_eq!(fake.count("PUT detector/command/disarm"), 0);
    // failure is logged, not raised
    let fake = FakeSimplon::start();
    fake.set_state("ready");
    fake.rule(
        Some("PUT"),
        "command/disarm",
        Action::Status(500, String::new()),
        None,
    );
    detector(&fake).disarm();
}

#[test]
fn abort_and_cancel_tables() {
    for (st, put) in [
        ("idle", false),
        ("na", false),
        ("error", false),
        ("ready", true),
        ("acquire", true),
    ] {
        let fake = FakeSimplon::start();
        fake.set_state(st);
        let d = detector(&fake);
        d.abort();
        assert_eq!(
            fake.count("PUT detector/command/abort"),
            usize::from(put),
            "abort {st}"
        );
        assert!(d.signals().cancel_requested());
    }
    for (st, put) in [("idle", false), ("ready", false), ("acquire", true)] {
        let fake = FakeSimplon::start();
        fake.set_state(st);
        let d = detector(&fake);
        d.cancel();
        assert_eq!(
            fake.count("PUT detector/command/cancel"),
            usize::from(put),
            "cancel {st}"
        );
        assert_eq!(
            fake.count("PUT detector/command/initialize"),
            0,
            "never re-initialises"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// getters, back-off, [#12 keep] wrappers
// ---------------------------------------------------------------------------------------------

#[test]
fn getters_return_values_and_unknown_on_error() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    assert_eq!(d.get_detector_state(), "idle");
    assert_eq!(d.get_hv_state(), "READY");
    assert_eq!(d.get_stream_mode(), "enabled");
    assert_eq!(d.get_countrate_correction(), Some(true));
    fake.rule(
        None,
        "high_voltage",
        Action::Status(500, String::new()),
        None,
    );
    fake.rule(
        None,
        "countrate_correction_applied",
        Action::Status(404, String::new()),
        None,
    );
    assert_eq!(d.get_hv_state(), "unknown");
    assert_eq!(d.get_countrate_correction(), None);
}

#[test]
fn r10_offline_backoff_three_polls_one_request() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    fake.rule(None, "status/state", Action::Close, None);
    assert_eq!(d.get_detector_state(), "unknown");
    let n = fake.log().len();
    for _ in 0..3 {
        assert_eq!(d.get_detector_state(), "unknown");
        assert_eq!(d.get_hv_state(), "unknown");
        assert_eq!(d.get_stream_mode(), "unknown");
        assert_eq!(d.get_countrate_correction(), None);
    }
    assert_eq!(fake.log().len(), n, "no I/O during back-off");
    // policy GETs still do real I/O during back-off
    let _ = d.ensure_idle();
    assert!(fake.log().len() > n);
    // after the window, and once the DCU answers, polls resume
    fake.clear_rules();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(d.get_detector_state(), "idle");
}

#[test]
fn keep12_wrappers() {
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    assert_eq!(d.get_bit_depth(), 32);
    fake.rule(
        None,
        "bit_depth_image",
        Action::Status(404, String::new()),
        Some(1),
    );
    assert_eq!(d.get_bit_depth(), 0);
    fake.rule(
        Some("PUT"),
        "monitor/api/1.8.0/config/mode",
        Action::Status(500, String::new()),
        Some(1),
    );
    d.enable_monitor(); // WARNING, no raise
    d.enable_monitor();
    assert_eq!(
        fake.with(|s| s.monitor.get("mode").cloned()),
        Some(json!("enabled"))
    );
    // wait_for_idle returns the last state on timeout, never raises
    fake.set_state("acquire");
    let t0 = Instant::now();
    assert_eq!(
        d.wait_for_idle(Duration::from_millis(150), Duration::from_millis(20)),
        "acquire"
    );
    assert!(t0.elapsed() >= Duration::from_millis(150));
    fake.set_state("idle");
    assert_eq!(
        d.wait_for_idle(Duration::from_secs(1), Duration::from_millis(20)),
        "idle"
    );
    // set_countrate_correction: PUT then bit depth GET, never raises
    fake.clear_log();
    d.set_countrate_correction(false);
    assert_eq!(
        fake.detector_value("countrate_correction_applied"),
        Some(json!(false))
    );
    assert!(fake.count("GET detector/config/bit_depth_image") == 1);
    fake.rule(
        Some("PUT"),
        "countrate_correction_applied",
        Action::Status(500, String::new()),
        None,
    );
    d.set_countrate_correction(true);
    // hv_enabled raises on failure
    d.hv_enabled(false).unwrap();
    assert_eq!(fake.with(|s| s.hv.clone()), "OFF");
    fake.rule(
        Some("PUT"),
        "hv_enabled",
        Action::Status(500, String::new()),
        None,
    );
    assert!(d.hv_enabled(true).is_err());
    // gate: no HTTP
    fake.clear_log();
    d.gate(true);
    d.gate(true);
    assert!(fake.keys().is_empty());
    // set_trigger_mode best effort
    fake.rule(
        Some("PUT"),
        "config/trigger_mode",
        Action::Status(500, String::new()),
        None,
    );
    d.set_trigger_mode("inte").unwrap();
    assert_eq!(d.settings().trigger_mode, "inte");
}

#[test]
fn keep12_arm_for_preview_and_disarm_preview() {
    let fake = FakeSimplon::start();
    fake.with(|s| s.trigger_hold = common::TriggerHold::UntilRelease);
    let d = detector_with(
        &fake,
        Settings {
            default_exposure: 0.05,
            ..Settings::default()
        },
    );
    d.arm_for_preview().unwrap();
    assert!(d.preview_armed());
    assert_eq!(fake.detector_value("nimages"), Some(json!(99_999)));
    assert_eq!(fake.detector_value("count_time"), Some(json!(0.05)));
    assert!(fake.wait_for(Duration::from_secs(2), |s| s
        .log
        .iter()
        .any(|l| l.path.ends_with("command/trigger"))));
    assert_eq!(fake.count("PUT detector/command/trigger"), 1);
    d.disarm_preview();
    d.disarm_preview();
    assert!(!d.preview_armed());
    assert_eq!(fake.count("PUT detector/command/disarm"), 1);
}

#[test]
fn stream_cfg_enable_disable_and_405() {
    let fake = FakeSimplon::start();
    let d = detector_with(
        &fake,
        Settings {
            stream_header_detail: Some("all".into()),
            ..Settings::default()
        },
    );
    fake.set_state("idle");
    fake.with(|s| {
        s.stream.insert("mode".into(), json!("disabled"));
    });
    assert!(d.enable_stream_v2());
    assert_eq!(fake.stream_value("mode"), Some(json!("enabled")));
    assert_eq!(fake.stream_value("format"), Some(json!("cbor")));
    assert_eq!(fake.stream_value("header_detail"), Some(json!("all")));
    fake.rule(
        Some("PUT"),
        "stream/api/1.8.0/config/format",
        Action::Status(405, String::new()),
        None,
    );
    assert!(d.enable_stream_v2(), "405 is not a failure");
    fake.clear_rules();
    fake.rule(
        Some("PUT"),
        "stream/api/1.8.0/config/format",
        Action::Status(500, String::new()),
        None,
    );
    assert!(!d.enable_stream_v2());
    d.disable_stream_v2();
    assert_eq!(fake.stream_value("mode"), Some(json!("disabled")));
    assert_eq!(d.get_stream_mode(), "disabled");
    // header_detail None → never PUT
    let fake = FakeSimplon::start();
    let d = detector(&fake);
    assert!(d.enable_stream_v2());
    assert_eq!(fake.count("PUT stream/config/header_detail"), 0);
}

// ---------------------------------------------------------------------------------------------
// shutdown releases the policy waits (DESIGN §3.7 step 2, [G1-S])
// ---------------------------------------------------------------------------------------------

#[test]
fn g1s_request_stop_releases_disarm_and_ensure_idle_waits() {
    let mut b = common::fast_budgets();
    b.transient_wait = Duration::from_secs(5);
    b.init_put = Duration::from_secs(5);
    for st in ["configure", "initialize"] {
        let fake = FakeSimplon::start();
        fake.set_state(st);
        let d = Arc::new(
            dectris_simplon::Detector::builder(fake.endpoint())
                .budgets(b.clone())
                .pool(dectris_simplon::WorkerPool::new(8))
                .build()
                .unwrap(),
        );
        // disarm waiting in its idle wait
        let d2 = Arc::clone(&d);
        let h = std::thread::spawn(move || {
            let t0 = Instant::now();
            d2.disarm();
            t0.elapsed()
        });
        std::thread::sleep(Duration::from_millis(100));
        let t_stop = Instant::now();
        d.request_stop();
        let el = h.join().unwrap();
        assert!(
            t_stop.elapsed() < Duration::from_millis(300),
            "{st}: {el:?}"
        );
        assert_eq!(fake.count("PUT detector/command/disarm"), 0, "{st}");
        // ensure_idle refuses at once after shutdown; one already waiting returns Shutdown
        assert!(matches!(d.ensure_idle(), Err(DriverError::Shutdown)));
    }
    // an ensure_idle already waiting (configure row) is released with Shutdown
    let fake = FakeSimplon::start();
    fake.set_state("configure");
    let d = Arc::new(
        dectris_simplon::Detector::builder(fake.endpoint())
            .budgets(b)
            .pool(dectris_simplon::WorkerPool::new(8))
            .build()
            .unwrap(),
    );
    let d2 = Arc::clone(&d);
    let h = std::thread::spawn(move || d2.ensure_idle());
    std::thread::sleep(Duration::from_millis(100));
    let t_stop = Instant::now();
    d.request_stop();
    assert!(matches!(h.join().unwrap(), Err(DriverError::Shutdown)));
    assert!(t_stop.elapsed() < Duration::from_millis(300));
    assert_eq!(fake.count("PUT detector/command/initialize"), 0);
}
