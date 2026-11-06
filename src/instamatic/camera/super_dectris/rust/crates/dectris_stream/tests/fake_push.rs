//! `fake_push` integration tests (docs/DESIGN.md §8 "fake_push", in Rust): a ZeroMQ PUSH bound
//! on an ephemeral 127.0.0.1 port sends Stream V2 start/image/end messages (built with the codec's
//! `encode` helpers) to a real [`Stream`] PULL.
//!
//! Covered: raw identity, attribution rows 1–5 and the three `series_attribution` modes, the [G2-D]
//! self-check, [G2-H] `own_start_seen` / `series_unique_id` pinning and the start-missing fallback,
//! stale series after a cancel (fast_adt run twice without flush) [R12], flush counts incl.
//! `flushed_armed` and the kept start [R22][G1-I][G2-I], busy detection [R5], reconnect, HWM
//! readback, receive windows (Q2), the series tail [R14], the 50 ms drain [R13], shutdown, the
//! single-PULL guard [G1-L] and the registry RAII entries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn me() -> thread::ThreadId {
    thread::current().id()
}

/// Run `f` on a fresh named thread (another "caller thread" for the busy rules).
fn on_other_thread<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn flush_from_other(s: &Arc<Stream>) -> (FlushOutcome, Vec<(Level, String)>) {
    let s = Arc::clone(s);
    on_other_thread("tk-main", move || {
        let mut d = Deferred::new();
        let r = s.flush(thread::current().id(), &mut d);
        (r, records(&mut d))
    })
}

// ---------------------------------------------------------------------------------------------
// Happy path, raw identity, metadata
// ---------------------------------------------------------------------------------------------

#[test]
fn armed_series_delivered_byte_identical_then_end() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    let st = start_uid(5, &uid(5), 0.25, 0.1);
    let imgs: Vec<Vec<u8>> = (1..=3).map(|i| image(5, i)).collect();
    send_all(&fx.push, std::slice::from_ref(&st));
    send_all(&fx.push, &imgs);
    send_all(&fx.push, &[end(5)]);

    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    for (i, want) in imgs.iter().enumerate() {
        let x = recv_image(&fx.stream, &mut sess, &mut d);
        assert_eq!(
            x.bytes(),
            want.as_slice(),
            "raw path must be byte-identical"
        );
        assert_eq!(x.attribution, Attribution::Armed);
        let m = x.meta();
        assert_eq!(m.series_id, Some(5));
        assert_eq!(m.image_id, Some(i as u64 + 1));
        assert_eq!(m.count_time, Some(0.25));
        assert_eq!(m.real_time, Some([4_000_000, 40_000_000]));
        // the wire message decodes to the pushed pixels
        let f = dectris_codec::decode_frame(x.bytes(), dectris_codec::DEFAULT_CHANNEL).unwrap();
        assert_eq!((f.rows, f.cols), (4, 6));
        assert_eq!(f.data, frame_data(5, i as u64 + 1));
    }
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.sig.armed_ends(), 1);
    assert!(fx.stream.armed_end_seen());
    assert!(fx.stream.own_start_seen());
    assert_eq!(fx.stream.armed_series_id(), Some(5));
    assert_eq!(fx.stream.start_cache().unwrap().raw, st);

    let s = fx.stream.stats();
    assert_eq!((s.received, s.delivered, s.stale, s.timeouts), (5, 3, 0, 0));
    assert!(s.last_recv.is_some());
    assert_eq!((sess.received, sess.delivered), (5, 3));
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Warn, ""), 0, "{recs:?}");
}

#[test]
fn large_raw_message_identity() {
    // 512 × 512 u32 uncompressed (1 MiB of data): one memcpy-free RawMsg, bytes unchanged (turbo).
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(9));
    let mut sspec = StartSpec::new(9, (512, 512), "uint32");
    sspec.series_unique_id = uid(9);
    let data: Vec<u8> = (0..512u32 * 512)
        .flat_map(|i| (i.wrapping_mul(2_654_435_761)).to_le_bytes())
        .collect();
    let mut ispec = ImageSpec::new(9, 1, (512, 512), 70, &data, Payload::Raw);
    ispec.series_unique_id = uid(9);
    let img = image_message(&ispec).unwrap();
    send_all(&fx.push, &[start_message(&sspec).unwrap(), img.clone()]);
    let mut sess = RecvSession::new(ms(3000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.bytes().len(), img.len());
    assert!(x.bytes() == img.as_slice());
    let f = dectris_codec::decode_frame(x.bytes(), dectris_codec::DEFAULT_CHANNEL).unwrap();
    assert_eq!(f.data, data);
    records(&mut d);
}

// ---------------------------------------------------------------------------------------------
// [R12] stale series
// ---------------------------------------------------------------------------------------------

#[test]
fn stale_series_after_cancel_second_run_collects_all_frames() {
    // fast_adt run twice without a flush: run 1 stops after 2 of 5 frames (cancel); its remaining
    // images and its END are still queued when run 2 arms.
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(1));
    send_all(&fx.push, &[start_uid(1, &uid(1), 0.5, 0.1)]);
    send_all(&fx.push, &(1..=5).map(|i| image(1, i)).collect::<Vec<_>>());
    send_all(&fx.push, &[end(1)]);
    let mut d = Deferred::new();
    let mut s1 = RecvSession::new(ms(2000));
    for i in 1..=2 {
        let x = recv_image(&fx.stream, &mut s1, &mut d);
        assert_eq!(x.meta().image_id, Some(i));
    }
    settle();

    arm(&fx.stream, 2, Some(2));
    send_all(&fx.push, &[start_uid(2, &uid(2), 0.25, 0.1)]);
    send_all(&fx.push, &(1..=3).map(|i| image(2, i)).collect::<Vec<_>>());
    send_all(&fx.push, &[end(2)]);
    let mut s2 = RecvSession::new(ms(2000));
    for i in 1..=3 {
        let x = recv_image(&fx.stream, &mut s2, &mut d);
        let m = x.meta();
        assert_eq!((m.series_id, m.image_id), (Some(2), Some(i)));
        assert_eq!(
            m.count_time,
            Some(0.25),
            "never paired with the stale start"
        );
    }
    assert!(matches!(recv(&fx.stream, &mut s2, &mut d), RecvEvent::End));
    // the stale END of run 1 did not end run 2; only run 2's end notified the waiters
    assert_eq!(fx.sig.armed_ends(), 1);
    assert_eq!(s2.stale, 4, "3 leftover images + 1 leftover end");
    assert_eq!(fx.stream.stats().stale, 4);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "of series Some(1)"),
        1,
        "one WARNING per stale series: {recs:?}"
    );
}

#[test]
fn stale_end_before_new_start_is_not_terminal() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(3));
    send_all(&fx.push, &[end(2), start(3), image(3, 1), end(3)]);
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().series_id, Some(3));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.sig.armed_ends(), 1);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Debug, "stale end of series Some(2) ignored"),
        1,
        "{recs:?}"
    );
    assert_eq!(has(&recs, Level::Warn, "stale end"), 0, "{recs:?}");
}

#[test]
fn stale_start_and_images_dropped_start_never_cached() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(8));
    send_all(
        &fx.push,
        &[start_uid(7, &uid(7), 0.9, 0.1), image(7, 1), image(7, 2)],
    );
    let mut sess = RecvSession::new(ms(400));
    let mut d = Deferred::new();
    // nothing deliverable yet; the stale start was not cached
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    assert!(fx.stream.start_cache().is_none());
    assert!(!fx.stream.own_start_seen());

    send_all(&fx.push, &[start_uid(8, &uid(8), 0.3, 0.1), image(8, 1)]);
    sess.window = ms(2000);
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().series_id, Some(8));
    assert_eq!(x.count_time(), Some(0.3));
    assert_eq!(fx.stream.start_cache().unwrap().series_id(), Some(8));
    assert_eq!(fx.stream.stats().stale, 3);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "stale start of series Some(7) ignored"),
        1,
        "{recs:?}"
    );
}

#[test]
fn stale_policy_deliver_hands_out_stale_images_without_count_time() {
    let fx = fixture(1000);
    fx.stream.set_stale_policy(StalePolicy::Deliver);
    arm(&fx.stream, 1, Some(5));
    send_all(
        &fx.push,
        &[start(4), image(4, 1), end(4), start(5), image(5, 1)],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.attribution, Attribution::Stale);
    assert_eq!(x.meta().series_id, Some(4));
    assert_eq!(x.count_time(), None);
    // the stale start is still never cached, the stale end never terminal
    let y = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(y.attribution, Attribution::Armed);
    assert_eq!(y.count_time(), Some(0.25));
    assert_eq!(fx.sig.armed_ends(), 0);
    let st = fx.stream.stats();
    assert_eq!((st.stale, st.delivered), (3, 2));
    records(&mut d);
}

// ---------------------------------------------------------------------------------------------
// [G2-H] own_start_seen + series_unique_id pinning, start-missing fallback
// ---------------------------------------------------------------------------------------------

#[test]
fn g2h_unique_id_pinning() {
    // Sequence ids reset on initialize: an unflushed earlier series ("old") carries the armed id.
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(
        &fx.push,
        &[
            image_uid(5, "old", 1),        // before the armed start → stale
            end_uid(5, "old"),             // before the armed start → stale, not terminal
            start_uid(5, "new", 0.2, 0.1), // armed start: pins "new"
            image_uid(5, "old", 2),        // pinned id mismatch → stale
            image_uid(5, "new", 3),        // deliver
            end_uid(5, "old"),             // pinned id mismatch → stale
            end_uid(5, "new"),             // End
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().image_id, Some(3));
    assert_eq!(x.header.series_unique_id.as_deref(), Some("new"));
    assert_eq!(x.count_time(), Some(0.2));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(sess.stale, 4);
    assert_eq!(fx.sig.armed_ends(), 1);
    assert_eq!(
        fx.stream.start_cache().unwrap().series_unique_id(),
        Some("new")
    );
    records(&mut d);
}

#[test]
// [G1-R] the warning when no start comes
fn g2h_start_missing_fallback_after_one_window() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[image(5, 1)]);
    let mut sess = RecvSession::new(ms(300));
    let mut d = Deferred::new();
    // the first armed-id image before any start is stale and starts the [G2-H] clock
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    thread::sleep(ms(50));
    send_all(&fx.push, &[image(5, 2), image(5, 3), end(5)]);
    // keep `sess.window` unchanged: it is also the [G2-H] orphan window, which has now elapsed
    // (one timed-out receive + 50 ms), so images 2 and 3 take the start-missing fallback
    for i in 2..=3 {
        let x = recv_image(&fx.stream, &mut sess, &mut d);
        assert_eq!(x.attribution, Attribution::StartMissing);
        assert_eq!(x.meta().image_id, Some(i));
        assert_eq!(x.count_time(), None);
    }
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.sig.armed_ends(), 1);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "armed start missing (dropped by DCU?)"),
        1,
        "{recs:?}"
    );
    assert_eq!(
        has(&recs, Level::Warn, "no Stream V2 start within"),
        1,
        "{recs:?}"
    );
    // armed-id messages were seen: the [G2-D] self-check must not fire
    assert_eq!(has(&recs, Level::Error, ""), 0);
    assert_eq!(fx.stream.stats().attribution_mismatch, 0);
}

// ---------------------------------------------------------------------------------------------
// [G2-D] attribution modes + self-check
// ---------------------------------------------------------------------------------------------

#[test]
fn g2d_self_check_fires_once_per_epoch() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(6), image(6, 1)]);
    let mut sess = RecvSession::new(ms(300));
    let mut d = Deferred::new();
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let recs = records(&mut d);
    let needle =
        "series_id never matched arm sequence_id (armed=5, seen=6) — set series_attribution: fifo";
    assert_eq!(has(&recs, Level::Error, needle), 1, "{recs:?}");
    assert_eq!(fx.stream.stats().attribution_mismatch, 1);
    assert_eq!(fx.stream.stats().timeouts, 2);

    // a new epoch re-arms the check
    arm(&fx.stream, 2, Some(7));
    send_all(&fx.push, &[start(8)]);
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Error, "(armed=7, seen=8)"), 1, "{recs:?}");
    assert_eq!(fx.stream.stats().attribution_mismatch, 2);
}

#[test]
fn g2d_self_check_ignores_stale_start_before_arm_returned() {
    // A stale start read between the arm reset and the arm PUT's return does not count.
    let fx = fixture(1000);
    fx.stream.on_arm_reset(1);
    // armed id still unknown: row 4 would adopt a start, so feed only an image + end here
    send_all(&fx.push, &[image(4, 1), end(4)]);
    let mut sess = RecvSession::new(ms(300));
    let mut d = Deferred::new();
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    fx.stream.on_armed(1, Some(5));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Error, ""), 0, "{recs:?}");
    assert_eq!(fx.stream.stats().attribution_mismatch, 0);
}

#[test]
fn fifo_mode_adopts_first_start_after_arm() {
    let fx = fixture(1000);
    fx.stream.set_attribution_mode(AttributionMode::Fifo);
    arm(&fx.stream, 1, Some(5)); // body id ignored in fifo
    assert_eq!(fx.stream.armed_series_id(), None);
    send_all(
        &fx.push,
        &[
            image(4, 9),
            end(4),
            start_uid(6, &uid(6), 0.4, 0.1),
            image(6, 1),
            end(6),
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().series_id, Some(6));
    assert_eq!(x.count_time(), Some(0.4));
    assert_eq!(x.attribution, Attribution::Armed);
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.stream.armed_series_id(), Some(6));
    assert_eq!(sess.stale, 2);
    // no self-check in fifo mode
    assert!(matches!(
        fx.stream
            .recv_next(
                &mut sess,
                Instant::now() + ms(100),
                RecvOptions::default(),
                &mut d
            )
            .unwrap(),
        RecvEvent::Timeout
    ));
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Error, ""), 0, "{recs:?}");
}

#[test]
fn arm_body_without_id_falls_back_to_first_start() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, None);
    send_all(&fx.push, &[image(2, 1), start(3), image(3, 1)]);
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().series_id, Some(3));
    assert_eq!(x.attribution, Attribution::Armed);
    assert_eq!(fx.stream.armed_series_id(), Some(3));
    assert_eq!(sess.stale, 1);
    records(&mut d);
}

#[test]
fn off_mode_delivers_everything_every_end_terminal() {
    let fx = fixture(1000);
    fx.stream.set_attribution_mode(AttributionMode::Off);
    arm(&fx.stream, 1, Some(5));
    send_all(
        &fx.push,
        &[
            start_uid(6, &uid(6), 0.7, 0.1),
            image(6, 1),
            image(9, 1),
            end(9),
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    for sid in [6, 9] {
        let x = recv_image(&fx.stream, &mut sess, &mut d);
        assert_eq!(x.meta().series_id, Some(sid));
        assert_eq!(x.attribution, Attribution::Unattributed);
        assert_eq!(
            x.count_time(),
            Some(0.7),
            "pairs with the last cached start"
        );
    }
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.sig.armed_ends(), 1);
    assert_eq!(sess.stale, 0);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "series attribution unavailable"),
        0,
        "{recs:?}"
    );
}

#[test]
fn row5_no_driver_arm_delivers_with_one_warning() {
    let fx = fixture(1000);
    send_all(
        &fx.push,
        &[
            start_uid(3, &uid(3), 0.6, 0.1),
            image(3, 1),
            image(3, 2),
            end(3),
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    for _ in 0..2 {
        let x = recv_image(&fx.stream, &mut sess, &mut d);
        assert_eq!(x.attribution, Attribution::Unattributed);
        assert_eq!(x.count_time(), Some(0.6));
    }
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert!(!fx.stream.driver_armed());
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "series attribution unavailable"),
        1,
        "{recs:?}"
    );
}

#[test]
fn row3_newer_series_armed_outside_driver_is_adopted() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(
        &fx.push,
        &[
            start_uid(5, &uid(5), 0.25, 0.1),
            image(5, 1),
            image(6, 0), // before the newer start: stale
            start_uid(6, &uid(6), 0.75, 0.1),
            image(6, 1),
            image(5, 2), // old armed series: now stale
            end(5),      // stale, not terminal
            end(6),
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let a = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!((a.meta().series_id, a.count_time()), (Some(5), Some(0.25)));
    let b = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!((b.meta().series_id, b.count_time()), (Some(6), Some(0.75)));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    assert_eq!(fx.stream.armed_series_id(), Some(6));
    assert_eq!(sess.stale, 3);
    assert_eq!(fx.sig.armed_ends(), 1);
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "series armed outside the driver"),
        1,
        "{recs:?}"
    );
}

#[test]
fn message_receiver_returns_on_armed_start_and_skips_unknown_types() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    // an unknown message type: CBOR map {"type": "status"}
    let mut w = dectris_codec::encode::Writer::new();
    w.map(2)
        .text("type")
        .text("status")
        .text("series_id")
        .uint(5);
    let unknown = w.into_bytes();
    send_all(
        &fx.push,
        &[
            unknown.clone(),
            unknown,
            b"\xff\x00garbage".to_vec(),
            start(5),
            image(5, 1),
        ],
    );
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let opts = RecvOptions {
        return_on_start: true,
    };
    let dl = sess.deadline();
    match fx.stream.recv_next(&mut sess, dl, opts, &mut d).unwrap() {
        RecvEvent::Start(s) => assert_eq!(s.series_id(), Some(5)),
        other => panic!("expected Start, got {other:?}"),
    }
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().image_id, Some(1));
    let recs = records(&mut d);
    assert_eq!(
        has(
            &recs,
            Level::Warn,
            "unexpected Stream V2 message type 'status'"
        ),
        1,
        "{recs:?}"
    );
    assert_eq!(
        has(&recs, Level::Warn, "undecodable Stream V2 message skipped"),
        1,
        "{recs:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Flush [R22][G1-I][G2-I][R5]
// ---------------------------------------------------------------------------------------------

#[test]
fn r22_flush_keeps_armed_start() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start_uid(5, &uid(5), 0.33, 0.1)]);
    settle();
    let mut d = Deferred::new();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(0));
    assert!(fx.stream.own_start_seen());
    assert_eq!(fx.stream.start_cache().unwrap().count_time(), Some(0.33));
    send_all(&fx.push, &[image(5, 1)]);
    let mut sess = RecvSession::new(ms(2000));
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(
        x.count_time(),
        Some(0.33),
        "count_time survives the flush (Q28)"
    );
    let st = fx.stream.stats();
    assert_eq!((st.flushed, st.flushed_armed), (0, 0));
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Info, "Flushed"), 0, "{recs:?}");
}

#[test]
fn flush_drains_the_backlog_behind_a_small_hwm() {
    // Seen in testing: `until EAGAIN` stopped after about 3 of a few hundred queued images,
    // and the rest then pushed the next series' start out of the sender's queue.
    let fx = fixture(4);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5)]);
    send_all(&fx.push, &(1..=200).map(|i| image(5, i)).collect::<Vec<_>>());
    settle();
    let mut d = Deferred::new();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(200));
    records(&mut d);
}

#[test]
fn g2i_flush_counts_flushed_armed_and_warns_before_end() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5)]);
    send_all(&fx.push, &(1..=4).map(|i| image(5, i)).collect::<Vec<_>>());
    settle();
    let mut d = Deferred::new();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(4));
    let st = fx.stream.stats();
    assert_eq!((st.flushed, st.flushed_armed, st.stale), (4, 4, 0));
    assert_eq!(st.received, 0, "flushed messages are not 'received'");
    let recs = records(&mut d);
    assert_eq!(
        has(
            &recs,
            Level::Warn,
            "flush discarded 4 image(s) of the current series"
        ),
        1,
        "{recs:?}"
    );
    assert_eq!(
        has(
            &recs,
            Level::Info,
            "Flushed 4 stale messages from ZMQ buffer"
        ),
        1,
        "{recs:?}"
    );

    // the armed end is consumed by a receive path; a later flush of armed images does not warn
    send_all(&fx.push, &[end(5)]);
    let mut sess = RecvSession::new(ms(2000));
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::End
    ));
    send_all(&fx.push, &[image(5, 5)]);
    settle();
    assert_eq!(fx.stream.flush(me(), &mut d).count(), 1);
    assert_eq!(fx.stream.stats().flushed_armed, 5);
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Warn, "flush discarded"), 0, "{recs:?}");
}

#[test]
fn flush_counts_stale_and_consumes_armed_end() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[image(4, 1), end(4), image_uid(5, "early", 1)]);
    settle();
    let mut d = Deferred::new();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(3));
    let st = fx.stream.stats();
    assert_eq!((st.flushed, st.flushed_armed, st.stale), (3, 0, 3));
    // an armed series' end drained by a flush notifies the trigger waiters
    send_all(&fx.push, &[start(5), image(5, 1), end(5)]);
    settle();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(2));
    assert_eq!(fx.sig.armed_ends(), 1);
    assert!(fx.stream.armed_end_seen());
    assert!(
        fx.stream.start_cache().is_some(),
        "flush never clears the start cache"
    );
    records(&mut d);
}

#[test]
fn r5_flush_busy_from_other_thread_owner_flush_drains() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5), image(5, 1), image(5, 2)]);
    settle();
    // this thread is the receiver (e.g. cRED keeps frame_iter referenced)
    let reg = fx.stream.registry().register(ms(2000));
    let (r, recs) = flush_from_other(&fx.stream);
    assert!(matches!(r, FlushOutcome::Busy(_)), "{r:?}");
    assert_eq!(r.count(), 0);
    assert_eq!(
        has(
            &recs,
            Level::Warn,
            "flush_zmq_buffer skipped: receiver busy"
        ),
        1,
        "{recs:?}"
    );
    // the owner thread may flush (cRED's post-disarm flush)
    let mut d = Deferred::new();
    assert_eq!(fx.stream.flush(me(), &mut d), FlushOutcome::Flushed(2));
    records(&mut d);
    // RAII: dropping the entry makes the other thread's flush work again
    drop(reg);
    send_all(&fx.push, &[image(5, 3)]);
    settle();
    let (r, _) = flush_from_other(&fx.stream);
    assert_eq!(r, FlushOutcome::Flushed(1));
}

#[test]
fn r5_frames_survive_flush_attempts_from_another_thread() {
    // A FrameIter on thread A mid-series, across 20 flush attempts from thread B: no frame lost.
    let Fx {
        push,
        stream,
        sig: _sig,
    } = fixture(1000);
    arm(&stream, 1, Some(5));
    let n = 8u64;
    let ready = Arc::new(std::sync::Barrier::new(2));
    let receiver = {
        let stream = Arc::clone(&stream);
        let ready = Arc::clone(&ready);
        thread::Builder::new()
            .name("frame-iter".into())
            .spawn(move || {
                let reg = stream.registry().register(ms(2000));
                ready.wait();
                let mut sess = RecvSession::new(ms(3000));
                let mut d = Deferred::new();
                let mut got = Vec::new();
                for _ in 0..n {
                    reg.touch();
                    let x = recv_image(&stream, &mut sess, &mut d);
                    got.push(x.meta().image_id.unwrap());
                    thread::sleep(ms(20));
                }
                reg.release();
                records(&mut d);
                got
            })
            .unwrap()
    };
    ready.wait();
    let flusher = {
        let stream = Arc::clone(&stream);
        thread::spawn(move || {
            let mut busy = 0;
            for _ in 0..20 {
                let mut d = Deferred::new();
                if let FlushOutcome::Busy(_) = stream.flush(thread::current().id(), &mut d) {
                    busy += 1;
                }
                records(&mut d);
                thread::sleep(ms(5));
            }
            busy
        })
    };
    send_all(&push, &[start(5)]);
    send_all(&push, &(1..=n).map(|i| image(5, i)).collect::<Vec<_>>());
    let busy = flusher.join().unwrap();
    let got = receiver.join().unwrap();
    assert_eq!(busy, 20, "every flush from the other thread is refused");
    assert_eq!(got, (1..=n).collect::<Vec<_>>(), "no frame lost");
    assert_eq!(stream.stats().flushed, 0);
}

// ---------------------------------------------------------------------------------------------
// Socket: HWM readback, reconnect, try_lock busy
// ---------------------------------------------------------------------------------------------

#[test]
fn hwm_linger_keepalive_readback() {
    for hwm in [3, 1000] {
        let fx = fixture(hwm);
        let (h, linger, (ka, _)) = fx.stream.socket_readback().unwrap();
        assert_eq!((h, linger, ka), (hwm, 0, -1));
        assert_eq!(fx.stream.rcvhwm(), Some(hwm));
    }
    let push = PushServer::bind_ephemeral().unwrap();
    let s = Stream::with_tuning(LocalSignals::new(), tuning());
    let mut cfg = LinkConfig::new(push.endpoint(), 25);
    cfg.tcp_keepalive = true;
    let mut d = Deferred::new();
    let r = s.connect(cfg, &mut d);
    assert!(matches!(r.outcome, ConnectOutcome::Connected));
    assert_eq!(
        r.stdout.as_deref(),
        Some(
            format!(
                "[Dectris] ZMQ connected: {} (buffer HWM=25 messages)",
                push.endpoint()
            )
            .as_str()
        )
    );
    assert_eq!(s.socket_readback().unwrap(), (25, 0, (1, 60)));
    // a second connect is a no-op
    let r2 = s.connect(LinkConfig::new(push.endpoint(), 25), &mut d);
    assert!(matches!(r2.outcome, ConnectOutcome::AlreadyConnected));
    records(&mut d);
}

#[test]
fn reconnect_gives_fresh_buffer_same_hwm() {
    let fx = fixture(7);
    send_all(&fx.push, &[image(2, 1), image(2, 2)]);
    settle();
    let mut d = Deferred::new();
    let r = fx.stream.reconnect(me(), &mut d);
    assert!(r.ok(), "{:?}", r.outcome);
    assert_eq!(
        r.stdout.as_deref(),
        Some("[Dectris] ZMQ reconnected: fresh buffer")
    );
    assert_eq!(fx.stream.stats().reconnects, 1);
    assert_eq!(fx.stream.rcvhwm(), Some(7));
    settle();
    send_all(&fx.push, &[image(3, 1)]);
    let mut sess = RecvSession::new(ms(2000));
    let x = recv_image(&fx.stream, &mut sess, &mut d);
    assert_eq!(x.meta().series_id, Some(3), "the old buffer was discarded");
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Info, "ZMQ reconnected: fresh buffer"),
        1,
        "{recs:?}"
    );
}

#[test]
fn reconnect_busy_via_registry_and_via_socket_lock() {
    let push = PushServer::bind_ephemeral().unwrap();
    let mut t = tuning();
    t.slice = Duration::from_secs(5); // one long poll slice holds the socket mutex
    let stream = Stream::with_tuning(LocalSignals::new(), t);
    let mut d = Deferred::new();
    assert!(matches!(
        stream
            .connect(LinkConfig::new(push.endpoint(), 10), &mut d)
            .outcome,
        ConnectOutcome::Connected
    ));

    // (1) registry: a live receiver on this thread → another thread's reconnect is refused
    let reg = stream.registry().register(ms(2000));
    let s2 = Arc::clone(&stream);
    let (ok, stdout, recs) = on_other_thread("tk-main", move || {
        let mut d = Deferred::new();
        let r = s2.reconnect(thread::current().id(), &mut d);
        (r.ok(), r.stdout, records(&mut d))
    });
    assert!(!ok);
    assert_eq!(
        stdout.as_deref(),
        Some("[Dectris] ZMQ reconnect skipped: receiver busy")
    );
    assert_eq!(
        has(&recs, Level::Warn, "reconnect_zmq skipped: receiver busy"),
        1,
        "{recs:?}"
    );
    drop(reg);

    // (2) socket lock: a receiver without a registry entry is inside a poll slice
    let s3 = Arc::clone(&stream);
    let poller = thread::spawn(move || {
        let mut sess = RecvSession::new(ms(1500));
        let mut d = Deferred::new();
        let ev = recv(&s3, &mut sess, &mut d);
        records(&mut d);
        matches!(ev, RecvEvent::Timeout)
    });
    thread::sleep(ms(300));
    let r = stream.reconnect(me(), &mut d);
    assert!(
        matches!(r.outcome, ReconnectOutcome::Busy(_)),
        "{:?}",
        r.outcome
    );
    assert_eq!(
        stream.flush(me(), &mut d),
        FlushOutcome::Busy("socket in use".into())
    );
    assert!(poller.join().unwrap());
    assert_eq!(stream.stats().reconnects, 0);
    records(&mut d);
}

// ---------------------------------------------------------------------------------------------
// Windows (Q2), shutdown
// ---------------------------------------------------------------------------------------------

#[test]
fn q2_timeout_window_deadline() {
    let Fx {
        push,
        stream,
        sig: _sig,
    } = fixture(1000);
    arm(&stream, 1, Some(5));
    // stale messages keep arriving during the window: they must not extend the deadline
    let pusher = thread::spawn(move || {
        for i in 0..12 {
            push.send(&image(4, i)).unwrap();
            thread::sleep(ms(50));
        }
        push
    });
    let mut sess = RecvSession::new(ms(400));
    let mut d = Deferred::new();
    let t = Instant::now();
    assert!(matches!(
        recv(&stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let el = elapsed_ms(t);
    assert!((390..1000).contains(&el), "timeout after {el} ms");
    assert_eq!(stream.stats().timeouts, 1);
    assert!(sess.stale >= 2);
    let _push = pusher.join().unwrap();
    records(&mut d);

    // silent stream
    let mut sess = RecvSession::new(ms(300));
    let t = Instant::now();
    assert!(matches!(
        recv(&stream, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let el = elapsed_ms(t);
    assert!((290..900).contains(&el), "timeout after {el} ms");
    records(&mut d);
}

#[test]
fn q2_window_from_attributed_start_applied_or_receive_timeout() {
    let push = PushServer::bind_ephemeral().unwrap();
    let stream = Stream::new(LocalSignals::new()); // DESIGN budgets: floor 2 s, cap 60 s
    let mut d = Deferred::new();
    let _ = stream.connect(LinkConfig::new(push.endpoint(), 1000), &mut d);
    assert_eq!(stream.window(None), ms(2000));
    assert_eq!(stream.window(Some(0.2)), ms(2000));
    assert_eq!(stream.window(Some(1.5)), ms(4000));
    arm(&stream, 1, Some(5));
    // a stale start's frame_time is never used
    push.send(&start_uid(4, &uid(4), 0.1, 9.0)).unwrap();
    push.send(&start_uid(5, &uid(5), 0.1, 0.9)).unwrap();
    let mut sess = RecvSession::new(ms(2000));
    let opts = RecvOptions {
        return_on_start: true,
    };
    let dl = sess.deadline();
    assert!(matches!(
        stream.recv_next(&mut sess, dl, opts, &mut d).unwrap(),
        RecvEvent::Start(_)
    ));
    assert_eq!(
        stream.window(Some(10.0)),
        ms(2800),
        "attributed start beats last_applied"
    );
    stream.set_receive_timeout(Some(0.5));
    assert_eq!(stream.window(Some(10.0)), ms(500));
    stream.set_receive_timeout(None);
    arm(&stream, 2, Some(6));
    assert_eq!(
        stream.window(Some(30.0)),
        ms(60_000),
        "cap; cleared start no longer used"
    );
    records(&mut d);
}

#[test]
fn shutdown_releases_a_waiting_receiver_within_a_slice() {
    let fx = fixture(1000);
    let s = Arc::clone(&fx.stream);
    let waiter = thread::spawn(move || {
        let mut sess = RecvSession::new(Duration::from_secs(10));
        let mut d = Deferred::new();
        let t = Instant::now();
        let ev = recv(&s, &mut sess, &mut d);
        records(&mut d);
        (matches!(ev, RecvEvent::Shutdown), t.elapsed())
    });
    thread::sleep(ms(200));
    fx.sig.begin_shutdown();
    let (ok, el) = waiter.join().unwrap();
    assert!(ok);
    assert!(el < ms(1500), "returned after {el:?}");
    // already shut down: immediate
    let mut sess = RecvSession::new(Duration::from_secs(10));
    let mut d = Deferred::new();
    assert!(matches!(
        recv(&fx.stream, &mut sess, &mut d),
        RecvEvent::Shutdown
    ));
    let mut deadline = Instant::now() + ms(100);
    let mut disarm = || panic!("no disarm after shutdown");
    assert!(matches!(
        fx.stream
            .recv_owned(
                &mut sess,
                &mut deadline,
                RecvOptions::default(),
                &mut disarm,
                &mut d
            )
            .unwrap(),
        RecvEvent::Shutdown
    ));
    records(&mut d);
}

// ---------------------------------------------------------------------------------------------
// Owned paths: series tail [R14], 50 ms drain [R13], fresh frame [R24]
// ---------------------------------------------------------------------------------------------

#[test]
fn r14_series_tail_disarms_once_and_gets_last_image() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5), image(5, 1)]);
    let mut sess = RecvSession::new(ms(300));
    let mut d = Deferred::new();
    let disarms = AtomicUsize::new(0);
    let push = &fx.push;
    // SIMPLON Information #4: the last image is available only after a disarm
    let mut disarm = || {
        disarms.fetch_add(1, Ordering::SeqCst);
        push.send(&image(5, 2)).unwrap();
        push.send(&end(5)).unwrap();
    };
    let mut deadline = sess.deadline();
    let a = fx
        .stream
        .recv_owned(
            &mut sess,
            &mut deadline,
            RecvOptions::default(),
            &mut disarm,
            &mut d,
        )
        .unwrap();
    assert!(matches!(a, RecvEvent::Image(ref x) if x.meta().image_id == Some(1)));
    let t = Instant::now();
    let b = fx
        .stream
        .recv_owned(
            &mut sess,
            &mut deadline,
            RecvOptions::default(),
            &mut disarm,
            &mut d,
        )
        .unwrap();
    assert!(
        matches!(b, RecvEvent::Image(ref x) if x.meta().image_id == Some(2)),
        "{b:?}"
    );
    assert!(
        t.elapsed() >= ms(250),
        "the tail starts when the window expires"
    );
    assert!(sess.tail_used);
    let c = fx
        .stream
        .recv_owned(
            &mut sess,
            &mut deadline,
            RecvOptions::default(),
            &mut disarm,
            &mut d,
        )
        .unwrap();
    assert!(matches!(c, RecvEvent::End));
    // a second expiry is a timeout (the caller raises DectrisTimeout); no second disarm
    let mut deadline = sess.deadline();
    let e = fx
        .stream
        .recv_owned(
            &mut sess,
            &mut deadline,
            RecvOptions::default(),
            &mut disarm,
            &mut d,
        )
        .unwrap();
    assert!(matches!(e, RecvEvent::Timeout));
    assert_eq!(disarms.load(Ordering::SeqCst), 1);
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Info, "series tail"), 1, "{recs:?}");
}

#[test]
fn r13_ten_consecutive_single_image_series() {
    let fx = fixture(1000);
    let mut d = Deferred::new();
    for k in 1..=10u64 {
        arm(&fx.stream, k, Some(k));
        // odd series: the end arrives late (after the next arm) and must not end the next series
        if k % 2 == 1 {
            send_all(&fx.push, &[start(k), image(k, 1)]);
        } else {
            send_all(&fx.push, &[end(k - 1), start(k), image(k, 1), end(k)]);
        }
        let mut sess = RecvSession::new(ms(2000));
        let x = recv_image(&fx.stream, &mut sess, &mut d);
        assert_eq!(x.meta().series_id, Some(k));
        assert_eq!(x.count_time(), Some(0.25));
        let ended = fx.stream.drain_after_single(&mut sess, &mut d).unwrap();
        assert_eq!(ended, k % 2 == 0, "series {k}");
    }
    assert_eq!(fx.sig.armed_ends(), 5);
    assert_eq!(fx.stream.stats().stale, 5, "5 late ends");
    records(&mut d);
}

#[test]
fn r13_drain_counts_an_extra_image_and_is_bounded() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5), image(5, 1), image(5, 2)]);
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let _ = recv_image(&fx.stream, &mut sess, &mut d);
    settle();
    assert!(!fx.stream.drain_after_single(&mut sess, &mut d).unwrap());
    assert_eq!(fx.stream.stats().stale, 1);
    let t = Instant::now();
    assert!(!fx.stream.drain_after_single(&mut sess, &mut d).unwrap());
    assert!(t.elapsed() < ms(500), "drain is ≤ 50 ms (+ scheduling)");
    records(&mut d);
}

#[test]
fn r24_fresh_frame_returns_newest_queued_image() {
    let fx = fixture(1000);
    arm(&fx.stream, 1, Some(5));
    send_all(&fx.push, &[start(5)]);
    send_all(&fx.push, &(1..=4).map(|i| image(5, i)).collect::<Vec<_>>());
    settle();
    let mut sess = RecvSession::new(ms(2000));
    let mut d = Deferred::new();
    let dl = sess.deadline();
    match fx.stream.recv_freshest(&mut sess, dl, &mut d).unwrap() {
        RecvEvent::Image(x) => assert_eq!(x.meta().image_id, Some(4)),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.stream.stats().stale, 3);
    records(&mut d);
}

// ---------------------------------------------------------------------------------------------
// Single-PULL guard [G1-L], release, registry RAII
// ---------------------------------------------------------------------------------------------

#[test]
fn g1l_second_instance_stays_disconnected_until_release() {
    let fx = fixture(1000);
    let ep = fx.push.endpoint().to_owned();
    let second = Stream::with_tuning(LocalSignals::new(), tuning());
    let mut d = Deferred::new();
    let r = second.connect(LinkConfig::new(ep.clone(), 1000), &mut d);
    assert!(
        matches!(r.outcome, ConnectOutcome::HeldByOther),
        "{:?}",
        r.outcome
    );
    assert!(!second.is_connected() && !second.holds_guard());
    assert!(fx.stream.holds_guard());
    let recs = records(&mut d);
    assert_eq!(
        has(
            &recs,
            Level::Warn,
            "another CameraDectris already holds the PULL for"
        ),
        1,
        "{recs:?}"
    );
    // reconnect does not override a live holder
    assert!(matches!(
        second.reconnect(me(), &mut d).outcome,
        ReconnectOutcome::HeldByOther
    ));
    // receive on the disconnected instance: timeout with one WARNING, no panic
    let mut sess = RecvSession::new(ms(150));
    assert!(matches!(
        recv(&second, &mut sess, &mut d),
        RecvEvent::Timeout
    ));
    let recs = records(&mut d);
    assert_eq!(
        has(&recs, Level::Warn, "ZMQ stream not connected"),
        1,
        "{recs:?}"
    );

    // the holder releases (close()/release_connection) → establish connects the second
    let rel = fx.stream.release(&mut d);
    assert!(rel.was_connected && rel.close_panic.is_none());
    assert!(!fx.stream.is_connected() && !fx.stream.holds_guard());
    assert!(
        fx.stream.release(&mut d)
            == ReleaseReport {
                was_connected: false,
                close_panic: None
            }
    );
    let r = second.establish(me(), &mut d);
    assert!(
        matches!(r.outcome, ConnectOutcome::Connected),
        "{:?}",
        r.outcome
    );
    settle();
    send_all(&fx.push, &[image(1, 1)]);
    let x = recv_image(&second, &mut RecvSession::new(ms(2000)), &mut d);
    assert_eq!(x.meta().series_id, Some(1));

    // dropping the holder (owner gone) frees the endpoint too
    drop(second);
    let third = Stream::with_tuning(LocalSignals::new(), tuning());
    assert!(matches!(
        third.connect(LinkConfig::new(ep, 1000), &mut d).outcome,
        ConnectOutcome::Connected
    ));
    records(&mut d);
}

#[test]
fn establish_without_connect_is_not_configured() {
    let s = Stream::with_tuning(LocalSignals::new(), tuning());
    let mut d = Deferred::new();
    assert!(matches!(
        s.establish(me(), &mut d).outcome,
        ConnectOutcome::NotConfigured
    ));
    records(&mut d);
}

#[test]
fn connect_failure_releases_the_guard() {
    let s = Stream::with_tuning(LocalSignals::new(), tuning());
    let mut d = Deferred::new();
    let r = s.connect(LinkConfig::new("tcp://127.0.0.1:notaport", 10), &mut d);
    assert!(
        matches!(r.outcome, ConnectOutcome::Failed(_)),
        "{:?}",
        r.outcome
    );
    assert!(!s.holds_guard());
    let recs = records(&mut d);
    assert_eq!(has(&recs, Level::Error, "ZMQ connect to"), 1, "{recs:?}");
}

#[test]
fn registry_entries_are_raii_and_owner_scoped() {
    let fx = fixture(1000);
    let reg = fx.stream.registry().register(ms(2000));
    assert_eq!(fx.stream.registry().live_count(), 1);
    assert!(fx.stream.busy_for(me()).is_none());
    let s = Arc::clone(&fx.stream);
    let busy = on_other_thread("tk-main", move || s.busy_for(thread::current().id()));
    assert_eq!(busy.unwrap().thread, me());
    drop(reg);
    assert_eq!(fx.stream.registry().live_count(), 0);
    let s = Arc::clone(&fx.stream);
    assert!(on_other_thread("tk-main", move || s.busy_for(thread::current().id())).is_none());
    // an entry moved into another thread's iterator and dropped there is released too
    let reg2 = fx.stream.registry().register(ms(2000));
    thread::spawn(move || drop(reg2)).join().unwrap();
    assert_eq!(fx.stream.registry().live_count(), 0);
}
