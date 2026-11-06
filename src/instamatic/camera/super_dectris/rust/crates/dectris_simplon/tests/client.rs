//! REST client against the fake SIMPLON server: wire conventions, error mapping, per-call
//! timeouts, offline back-off [R10], downloads (DESIGN §4.1).

#![allow(clippy::unwrap_used, clippy::expect_used)] // test crate: helpers outside #[test] fns

mod common;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use common::{fast_budgets, Action, FakeSimplon};
use dectris_common::DriverError;
use dectris_simplon::client::{Client, Module, Task};
use dectris_simplon::PlainSession;
use serde_json::json;

fn client(fake: &FakeSimplon) -> Client {
    Client::new(&fake.endpoint(), "instamatic-dcx7/test", fast_budgets()).unwrap()
}

/// A local port with nothing listening (connection refused at once).
fn dead_endpoint() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

#[test]
fn get_value_and_headers() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    let url = c.url(Module::Detector, Task::Status, "state");
    assert_eq!(c.get_string(&url, Duration::from_secs(1)).unwrap(), "idle");
    let f = c
        .get_f64(
            &c.url(Module::Detector, Task::Config, "count_time"),
            Duration::from_secs(1),
        )
        .unwrap();
    assert!((f - 0.5).abs() < 1e-12);
    let log = fake.log();
    let h = &log[0].headers;
    let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("user-agent").as_deref(), Some("instamatic-dcx7/test"));
    assert_eq!(get("accept").as_deref(), Some("application/json"));
}

/// ureq builds the User-Agent header at every request and would panic on a control character,
/// so the test sends requests, through the client and the plain session.
#[test]
fn control_characters_in_user_agent_become_spaces() {
    let fake = FakeSimplon::start();
    let c = Client::new(&fake.endpoint(), "DCX7\n/1.0\u{7f}b", fast_budgets()).unwrap();
    let url = c.url(Module::Detector, Task::Status, "state");
    assert_eq!(c.get_string(&url, Duration::from_secs(1)).unwrap(), "idle");
    let s = PlainSession::new("DCX7\r/1.0\tb");
    assert_eq!(s.get(&url, Duration::from_secs(1)).unwrap().status, 200);
    let agents: Vec<_> = fake
        .log()
        .iter()
        .map(|l| l.headers.iter().find(|(n, _)| n == "user-agent").map(|(_, v)| v.clone()))
        .collect();
    assert_eq!(agents, [Some("DCX7 /1.0 b".to_owned()), Some("DCX7 /1.0\tb".to_owned())]);
}

#[test]
fn config_put_body_is_value_object_and_changed_list_optional() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    let url = c.url(Module::Detector, Task::Config, "count_time");
    let changed = c
        .put_config(&url, json!(1.0), Duration::from_secs(1))
        .unwrap();
    // rewrite model: frame_time changed too
    assert!(changed.contains(&"count_time".to_owned()));
    let log = fake.log();
    assert_eq!(log[0].method, "PUT");
    assert_eq!(log[0].body, Some(json!({"value": 1.0})));
    // an empty 200 body is still success [G2-L]
    fake.rule(
        Some("PUT"),
        "config/nimages",
        Action::Status(200, String::new()),
        Some(1),
    );
    let changed = c
        .put_config(
            &c.url(Module::Detector, Task::Config, "nimages"),
            json!(3),
            Duration::from_secs(1),
        )
        .unwrap();
    assert!(changed.is_empty());
}

#[test]
fn command_without_body_and_lenient_reply() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    let body = c
        .put_command(
            &c.url(Module::Detector, Task::Command, "arm"),
            None,
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(body, Some(json!({"sequence_id": 1})));
    let log = fake.log();
    assert_eq!(log[0].body, None);
    let len = log[0]
        .headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .map(|(_, v)| v.clone());
    assert!(len.is_none() || len.as_deref() == Some("0"), "{len:?}");
    // empty 200 body → None
    let r = c
        .put_command(
            &c.url(Module::Detector, Task::Command, "initialize"),
            None,
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(r, None);
}

#[test]
fn non_200_maps_to_http_error_with_status_and_url() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    for status in [400_u16, 404, 405, 408, 500] {
        fake.clear_rules();
        fake.rule(
            None,
            "config/trigger_mode",
            Action::Status(status, "nope".into()),
            None,
        );
        let url = c.url(Module::Detector, Task::Config, "trigger_mode");
        let e = c.get_value(&url, Duration::from_secs(1)).unwrap_err();
        match e {
            DriverError::Http {
                status: s,
                url: u,
                body,
            } => {
                assert_eq!(s, status);
                assert_eq!(u, url);
                assert_eq!(body, "nope");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            e_kind(&c.put_config(&url, json!("ints"), Duration::from_secs(1))),
            "Http"
        );
    }
    // an HTTP error is a response: no back-off
    assert!(!c.in_backoff());
}

fn e_kind<T: std::fmt::Debug>(r: &Result<T, DriverError>) -> &'static str {
    match r {
        Ok(_) => "Ok",
        Err(DriverError::Http { .. }) => "Http",
        Err(DriverError::Transport(_)) => "Transport",
        Err(DriverError::Timeout(_)) => "Timeout",
        Err(DriverError::State { .. }) => "State",
        Err(_) => "Other",
    }
}

#[test]
fn per_call_timeout_maps_to_timeout_and_opens_backoff() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    fake.rule(
        None,
        "status/state",
        Action::Delay(Duration::from_millis(800)),
        Some(1),
    );
    let t0 = Instant::now();
    let r = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_millis(200),
    );
    let el = t0.elapsed();
    assert_eq!(e_kind(&r), "Timeout", "{r:?}");
    assert!(el < Duration::from_millis(700), "{el:?}");
    assert!(c.in_backoff());
    // back-off closes after its window (300 ms in fast budgets) …
    std::thread::sleep(Duration::from_millis(350));
    assert!(!c.in_backoff());
    // … and any response closes it at once
    let _ = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_millis(50),
    );
    std::thread::sleep(Duration::from_millis(900));
    let _ = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_secs(1),
    );
    assert!(!c.in_backoff());
}

#[test]
fn hang_maps_to_timeout() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    fake.rule(None, "status/state", Action::Hang, None);
    let t0 = Instant::now();
    let r = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_millis(300),
    );
    assert_eq!(e_kind(&r), "Timeout");
    assert!(t0.elapsed() < Duration::from_secs(2));
    fake.release_hangs();
}

#[test]
fn closed_connection_maps_to_transport() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    fake.rule(None, "status/state", Action::Close, None);
    let r = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_secs(1),
    );
    assert_eq!(e_kind(&r), "Transport", "{r:?}");
    assert!(c.in_backoff());
}

#[test]
fn refused_connection_is_transport_and_fast() {
    let c = Client::new(&dead_endpoint(), "ua", fast_budgets()).unwrap();
    let t0 = Instant::now();
    let r = c.get_string(
        &c.url(Module::Detector, Task::Status, "state"),
        Duration::from_secs(3),
    );
    assert!(
        matches!(
            r,
            Err(DriverError::Transport(_)) | Err(DriverError::Timeout(_))
        ),
        "{r:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert!(c.in_backoff());
}

#[test]
fn invalid_json_is_an_error_not_a_panic() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    fake.rule(
        None,
        "config/count_time",
        Action::Status(200, "{not json".into()),
        None,
    );
    let r = c.get_f64(
        &c.url(Module::Detector, Task::Config, "count_time"),
        Duration::from_secs(1),
    );
    assert_eq!(e_kind(&r), "Transport");
    // wrong type
    fake.clear_rules();
    let r = c.get_bool(
        &c.url(Module::Detector, Task::Config, "count_time"),
        Duration::from_secs(1),
    );
    assert_eq!(e_kind(&r), "Transport");
}

#[test]
fn download_streams_to_file_and_cleans_up_on_error() {
    let fake = FakeSimplon::start();
    let big: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
    fake.with(|st| st.files.push(("x_master.h5".into(), big.clone())));
    let c = client(&fake);
    let dir = std::env::temp_dir().join(format!("simplon-dl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("x_master.h5");
    let n = c
        .download_to(&c.data_url("x_master.h5"), Duration::from_secs(5), &dest)
        .unwrap();
    assert_eq!(n, big.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), big);
    let missing = dir.join("missing.h5");
    let r = c.download_to(&c.data_url("missing.h5"), Duration::from_secs(5), &missing);
    assert_eq!(e_kind(&r), "Http");
    assert!(!missing.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keep_alive_reuses_connections() {
    let fake = FakeSimplon::start();
    let c = client(&fake);
    let url = c.url(Module::Detector, Task::Status, "state");
    for _ in 0..200 {
        c.get_string(&url, Duration::from_secs(1)).unwrap();
    }
    assert_eq!(fake.count("GET detector/status/state"), 200);
}
