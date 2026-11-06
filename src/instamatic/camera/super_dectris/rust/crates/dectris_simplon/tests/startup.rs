//! Constructor checks (DESIGN §2.2 `_connect` steps 2-3): offline budget ≤ 3 s (refused and
//! black-hole), VERIFIED / MISMATCH, geometry warning, guarded disarm, stream + monitor enable.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use common::{detector, FakeSimplon};
use dectris_simplon::{Budgets, Detector};
use serde_json::json;

fn spec_detector(endpoint: String) -> Detector {
    Detector::builder(endpoint)
        .budgets(Budgets::spec())
        .build()
        .unwrap()
}

#[test]
fn offline_refused_is_fast_and_opens_backoff() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let d = spec_detector(format!("http://127.0.0.1:{port}"));
    let t0 = Instant::now();
    let rep = d.connect_checks(Some((1028, 1062)));
    assert!(!rep.reachable);
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    let t1 = Instant::now();
    assert_eq!(d.get_detector_state(), "unknown");
    assert_eq!(d.get_hv_state(), "unknown");
    assert!(t1.elapsed() < Duration::from_millis(50), "back-off: no I/O");
}

/// A bound listener that never accepts: the TCP handshake completes in the backlog and the
/// request then times out at the 3 s probe budget.
#[test]
fn offline_black_hole_within_probe_budget() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let d = spec_detector(format!("http://127.0.0.1:{port}"));
    let t0 = Instant::now();
    let rep = d.connect_checks(None);
    let el = t0.elapsed();
    assert!(!rep.reachable);
    assert!(el <= Duration::from_millis(3500), "{el:?}");
    assert_eq!(d.get_detector_state(), "unknown");
    drop(l);
}

#[test]
fn online_checks_verified() {
    dectris_common::testing::install();
    let fake = FakeSimplon::start();
    fake.with(|s| {
        s.stream.insert("mode".into(), json!("disabled"));
    });
    let d = detector(&fake);
    let rep = d.connect_checks(Some((1028, 1062)));
    assert!(rep.reachable);
    assert_eq!(rep.state.as_deref(), Some("idle"));
    assert_eq!(rep.trigger_mode.as_deref(), Some("ints"));
    assert_eq!(rep.pixels, Some((1028, 1062)));
    assert!(rep.stream_enabled);
    assert_eq!(fake.stream_value("mode"), Some(json!("enabled")));
    assert_eq!(
        fake.with(|s| s.monitor.get("mode").cloned()),
        Some(json!("enabled"))
    );
    // idle: no disarm request at all; never initialize, never detector config PUTs
    let keys = fake.keys();
    assert!(!keys.iter().any(|k| k.contains("command/")), "{keys:?}");
    assert!(
        !keys.iter().any(|k| k.starts_with("PUT detector/config")),
        "{keys:?}"
    );
    assert!(!dectris_common::testing::take_matching("Trigger mode VERIFIED: ints").is_empty());
}

#[test]
fn online_checks_mismatch_geometry_and_disarm() {
    dectris_common::testing::install();
    let fake = FakeSimplon::start();
    fake.set_state("ready");
    fake.with(|s| {
        s.detector.insert("trigger_mode".into(), json!("exts"));
        s.detector.insert("x_pixels_in_detector".into(), json!(512));
        s.detector.insert("y_pixels_in_detector".into(), json!(512));
    });
    let d = detector(&fake);
    let rep = d.connect_checks(Some((1028, 1062)));
    assert!(rep.reachable);
    assert_eq!(fake.count("PUT detector/command/disarm"), 1);
    assert_eq!(rep.pixels, Some((512, 512)));
    assert!(!dectris_common::testing::take_matching(
        "Trigger mode MISMATCH: camera=exts, yaml=ints"
    )
    .is_empty());
    assert!(
        !dectris_common::testing::take_matching("detector geometry 512x512 differs").is_empty()
    );
}
