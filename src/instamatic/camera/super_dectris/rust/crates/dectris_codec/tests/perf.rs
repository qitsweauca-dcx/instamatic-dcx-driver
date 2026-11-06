//! Phase 1 performance acceptance (docs/DESIGN.md §9 Phase 1, §8 Timing):
//! * 1028×1062 u32 bslz4 image message: parse + decompress ≤ 3 ms;
//! * raw classify (header scan) ≤ 50 µs, also on a 4.4 MB uncompressed frame;
//! * 4.4 MB start-array skip < 1 ms.
//!
//! Thresholds are asserted only in optimised builds (`cargo test -p dectris_codec --release
//! --test perf -- --nocapture --test-threads=1`, or `--profile test-opt`); debug builds just print.
//!
//! Fixtures (compressed with this crate's encoder, 8192-byte blocks = the DCU/bitshuffle default):
//! * **diffraction** (asserted): a synthetic electron-diffraction frame — Poisson-like background
//!   (λ ≈ 0.25: P(0) 0.78, P(1) 0.19, P(2) 0.024, P(3) 0.002), 300 Bragg spots (7×7, peak 50–5050
//!   counts), a direct-beam disc (r = 20 px, up to 60 000 counts), 0.05 % hot pixels at 0xFFFFF.
//!   ≈ 0.33 MB compressed for u32 (7.6 %).
//! * **dense** (printed, not asserted): every pixel uniformly 0–7 plus 1/64 pixels with random
//!   32-bit values (`common::pattern_bytes("low")`), ≈ 1.28 MB compressed — a stress case well
//!   above the 0.5–1 MB PROTOCOL §7.5 estimate.
//!
//! The hardware golden capture (Phase 6) settles which is representative.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code (clippy.toml exempts #[test] fns only, not helpers)

mod common;

use std::time::{Duration, Instant};

use dectris_codec::encode::{image_message, start_message, ImageSpec, Payload, StartSpec};
use dectris_codec::{
    decode_message_with, parse_image_message, scan_header, DecodeOptions, Scratch, StartSummary,
    DEFAULT_CHANNEL,
};

const ROWS: u64 = 1028;
const COLS: u64 = 1062;

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time<F: FnMut()>(iters: usize, mut f: F) -> Duration {
    for _ in 0..3 {
        f();
    }
    median(
        (0..iters)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed()
            })
            .collect(),
    )
}

fn optimised() -> bool {
    !cfg!(debug_assertions)
}

/// The "diffraction" fixture (see the module doc), `rows × cols` u32 little-endian bytes.
fn diffraction(rows: usize, cols: usize, seed: u64, elem: usize) -> Vec<u8> {
    let sm = common::splitmix;
    let mut img = vec![0u64; rows * cols];
    for (i, p) in img.iter_mut().enumerate() {
        let u = sm(seed, i as u64) & 0xFFFF;
        *p = match u {
            0..=51117 => 0,
            51118..=63569 => 1,
            63570..=65126 => 2,
            _ => 3,
        };
    }
    for s in 0..300u64 {
        let r = sm(seed ^ 0xABCD, s);
        let (cy, cx) = ((r % rows as u64) as i64, ((r >> 20) % cols as u64) as i64);
        let peak = 50 + (r >> 40) % 5000;
        for dy in -3i64..=3 {
            for dx in -3i64..=3 {
                let (y, x) = (cy + dy, cx + dx);
                if y < 0 || x < 0 || y >= rows as i64 || x >= cols as i64 {
                    continue;
                }
                let d2 = (dy * dy + dx * dx) as u64;
                img[y as usize * cols + x as usize] += peak >> d2.min(30);
            }
        }
    }
    let (cy, cx) = (rows as i64 / 2, cols as i64 / 2);
    for y in cy - 20..cy + 20 {
        for x in cx - 20..cx + 20 {
            let d2 = ((y - cy).pow(2) + (x - cx).pow(2)) as u64;
            if d2 < 400 {
                img[y as usize * cols + x as usize] += 60000 - d2 * 100;
            }
        }
    }
    for i in 0..(rows * cols / 2000) as u64 {
        let r = sm(seed ^ 0x55, i);
        img[(r % (rows * cols) as u64) as usize] = 0x000F_FFFF;
    }
    let max = if elem >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * elem)) - 1
    };
    img.iter()
        .flat_map(|v| (*v).min(max).to_le_bytes()[..elem].to_vec())
        .collect()
}

fn measure(name: &str, msg: &[u8], expect: &[u8], iters: usize) -> (Duration, Duration) {
    let mut out = vec![0u8; expect.len()];
    let mut scratch = Scratch::new();
    // steady state: reused output buffer (an iterator's preallocated array)
    let t_reuse = time(iters, || {
        let p = parse_image_message(msg, DEFAULT_CHANNEL).unwrap();
        p.payload.decode_into(&mut out, &mut scratch).unwrap();
    });
    assert_eq!(out, expect, "{name}");
    // fresh allocation per frame (every delivered frame is a new numpy array)
    let t_fresh = time(iters, || {
        let p = parse_image_message(msg, DEFAULT_CHANNEL).unwrap();
        let mut o = vec![0u8; p.payload.byte_len()];
        p.payload.decode_into(&mut o, &mut scratch).unwrap();
        std::hint::black_box(&o);
    });
    println!(
        "PERF {name} ({} B message): parse+decode reuse {t_reuse:?}, fresh {t_fresh:?}",
        msg.len()
    );
    (t_reuse, t_fresh)
}

#[test]
fn perf_full_frame_decode() {
    let n = (ROWS * COLS) as usize;
    let iters = if optimised() { 50 } else { 2 };
    for (tag, elem, payload, pname) in [
        (70u64, 4usize, Payload::BSLZ4, "u32 bslz4"),
        (69, 2, Payload::BSLZ4, "u16 bslz4"),
        (70, 4, Payload::LZ4, "u32 lz4"),
        (70, 4, Payload::Raw, "u32 raw"),
    ] {
        let data = diffraction(ROWS as usize, COLS as usize, 7, elem);
        let msg = image_message(&ImageSpec::new(3, 1, (ROWS, COLS), tag, &data, payload)).unwrap();
        let (_, fresh) = measure(
            &format!("1028x1062 diffraction {pname}"),
            &msg,
            &data,
            iters,
        );
        // The DESIGN §9 bound is asserted for bslz4 (the DCU default) and raw. lz4 without
        // bitshuffle on u32 data is sequence-heavy (≈ 2.3 ms reuse / 2.9 ms fresh measured):
        // printed only, to keep the suite deterministic.
        if optimised() && !matches!(payload, Payload::Lz4 { .. }) {
            assert!(
                fresh <= Duration::from_millis(3),
                "{pname} decode {fresh:?} > 3 ms"
            );
        }
        // dense stress fixture: recorded, not asserted
        let dense = common::pattern_bytes("low", 7, n, elem);
        let msg = image_message(&ImageSpec::new(3, 1, (ROWS, COLS), tag, &dense, payload)).unwrap();
        measure(&format!("1028x1062 dense {pname}"), &msg, &dense, iters);
    }
}

#[test]
fn perf_raw_classify() {
    let n = (ROWS * COLS) as usize;
    let data = diffraction(ROWS as usize, COLS as usize, 11, 4);
    let _ = n;
    let iters = if optimised() { 200 } else { 5 };
    for (payload, name) in [(Payload::BSLZ4, "bslz4"), (Payload::Raw, "raw 4.4 MB")] {
        let msg = image_message(&ImageSpec::new(3, 1, (ROWS, COLS), 70, &data, payload)).unwrap();
        let t = time(iters, || {
            std::hint::black_box(scan_header(&msg).unwrap());
        });
        println!("PERF raw classify image {name} ({} B): {t:?}", msg.len());
        if optimised() {
            assert!(t <= Duration::from_micros(50), "classify {t:?} > 50 µs");
        }
    }
    // header_detail=all start message with two 4.4 MB arrays + LUT (≈ 8.7 MB)
    let mut s = StartSpec::new(3, (ROWS, COLS), "uint32");
    s.header_detail_all = Some(Payload::Raw);
    let start = start_message(&s).unwrap();
    let t = time(iters, || {
        std::hint::black_box(scan_header(&start).unwrap());
    });
    println!(
        "PERF raw classify start header_detail=all ({} B): {t:?}",
        start.len()
    );
    let t_skip = time(iters, || {
        std::hint::black_box(
            decode_message_with(
                &start,
                DecodeOptions {
                    skip_start_arrays: true,
                },
            )
            .unwrap(),
        );
    });
    let t_summary = time(iters, || {
        std::hint::black_box(StartSummary::scan(&start).unwrap());
    });
    println!("PERF start decode with array skip: {t_skip:?}; StartSummary::scan: {t_summary:?}");
    if optimised() {
        assert!(
            t <= Duration::from_micros(50),
            "start classify {t:?} > 50 µs"
        );
        assert!(
            t_skip <= Duration::from_millis(1),
            "start skip {t_skip:?} > 1 ms"
        );
    }
}
