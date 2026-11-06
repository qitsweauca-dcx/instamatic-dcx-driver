//! Shared fixtures for the `fake_push` integration tests (DESIGN §8 `fake_push`, in Rust).
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

pub use dectris_codec::encode::{
    end_message, image_message, start_message, ImageSpec, Payload, StartSpec,
};
pub use dectris_common::log::{Deferred, Level};
pub use dectris_stream::*;

/// Shrunk budgets: 300 ms window floor, 50 ms slices.
pub fn tuning() -> StreamTuning {
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

pub struct Fx {
    pub push: PushServer,
    pub stream: Arc<Stream>,
    pub sig: Arc<LocalSignals>,
}

/// PUSH on an ephemeral port + a connected stream (HWM `hwm`).
pub fn fixture(hwm: i32) -> Fx {
    let push = PushServer::bind_ephemeral().unwrap();
    let sig = LocalSignals::new();
    let stream = Stream::with_tuning(sig.clone(), tuning());
    let mut d = Deferred::new();
    let r = stream.connect(LinkConfig::new(push.endpoint(), hwm), &mut d);
    assert!(
        matches!(r.outcome, ConnectOutcome::Connected),
        "{:?}",
        r.outcome
    );
    d.discard();
    Fx { push, stream, sig }
}

pub fn uid(sid: u64) -> String {
    format!("01HSERIES{sid:016}")
}

pub fn start(sid: u64) -> Vec<u8> {
    start_uid(sid, &uid(sid), 0.25, 0.1)
}

pub fn start_uid(sid: u64, u: &str, count_time: f64, frame_time: f64) -> Vec<u8> {
    let mut s = StartSpec::new(sid, (4, 6), "uint16");
    s.series_unique_id = u.to_owned();
    s.count_time = count_time;
    s.frame_time = frame_time;
    start_message(&s).unwrap()
}

pub fn frame_data(sid: u64, image_id: u64) -> Vec<u8> {
    (0..24u16)
        .flat_map(|i| (i + (sid as u16) * 100 + (image_id as u16) * 7).to_le_bytes())
        .collect()
}

pub fn image(sid: u64, image_id: u64) -> Vec<u8> {
    image_uid(sid, &uid(sid), image_id)
}

pub fn image_uid(sid: u64, u: &str, image_id: u64) -> Vec<u8> {
    let data = frame_data(sid, image_id);
    let mut spec = ImageSpec::new(sid, image_id, (4, 6), 69, &data, Payload::BSLZ4);
    spec.series_unique_id = u.to_owned();
    image_message(&spec).unwrap()
}

pub fn end(sid: u64) -> Vec<u8> {
    end_message(sid, &uid(sid))
}

pub fn end_uid(sid: u64, u: &str) -> Vec<u8> {
    end_message(sid, u)
}

/// `arm()` as the driver hooks run it.
pub fn arm(s: &Stream, epoch: u64, seq: Option<u64>) {
    s.on_arm_reset(epoch);
    s.on_armed(epoch, seq);
}

pub fn send_all(push: &PushServer, msgs: &[Vec<u8>]) {
    for m in msgs {
        push.send(m).unwrap();
    }
}

/// Let queued messages reach the PULL side (loopback TCP).
pub fn settle() {
    std::thread::sleep(Duration::from_millis(250));
}

pub fn recv(s: &Stream, sess: &mut RecvSession, d: &mut Deferred) -> RecvEvent {
    let dl = sess.deadline();
    s.recv_next(sess, dl, RecvOptions::default(), d).unwrap()
}

pub fn recv_image(s: &Stream, sess: &mut RecvSession, d: &mut Deferred) -> Delivery {
    match recv(s, sess, d) {
        RecvEvent::Image(x) => x,
        other => panic!("expected an image, got {other:?}"),
    }
}

/// Take the records out of `d` (so it can be dropped) as `(level, message)`.
pub fn records(d: &mut Deferred) -> Vec<(Level, String)> {
    let taken = std::mem::take(d);
    taken
        .into_records()
        .into_iter()
        .map(|r| (r.level, r.message))
        .collect()
}

pub fn has(recs: &[(Level, String)], level: Level, needle: &str) -> usize {
    recs.iter()
        .filter(|(l, m)| *l == level && m.contains(needle))
        .count()
}

pub fn elapsed_ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}
