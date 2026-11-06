//! Property tests (DESIGN §8 Tier 1 "proptest forward→inverse", §9 Phase 1 "proptest 10k"):
//! * bitshuffle forward → inverse identity, 10 000 cases;
//! * bslz4 / lz4 encode → decode identity over random data, element sizes and block sizes;
//! * this crate's LZ4 block decoder vs `lz4_flex` (the DESIGN §4.5 reference): identical output on
//!   valid blocks; on corrupted blocks, whenever either accepts, both accept with identical bytes;
//!   a decode that writes fewer bytes than the slice is reported as such (the scratch tail is
//!   never observable through framing).
//! * CBOR value trees: encode with `encode::Writer` → decode → equal.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code (clippy.toml exempts #[test] fns only, not helpers)

mod common;

use dectris_codec::bitshuffle::{shuffle_block, unshuffle_block};
use dectris_codec::cbor::{Decoder, Value};
use dectris_codec::encode::{compress_bslz4, compress_lz4, Writer};
use dectris_codec::framing::{self, Algorithm};
use dectris_codec::lz4;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, .. ProptestConfig::default() })]

    #[test]
    fn bitshuffle_roundtrip(elem in prop_oneof![Just(1usize), Just(2), Just(4), Just(8), 1usize..=12],
                            groups in 0usize..=80,
                            seed in any::<u64>()) {
        let n = groups * 8;
        let data: Vec<u8> = (0..n * elem).map(|i| (common::splitmix(seed, i as u64) >> 7) as u8).collect();
        let mut sh = vec![0u8; data.len()];
        shuffle_block(&data, &mut sh, elem).unwrap();
        let mut back = vec![0u8; data.len()];
        unshuffle_block(&sh, &mut back, elem).unwrap();
        prop_assert_eq!(back, data);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, .. ProptestConfig::default() })]

    #[test]
    fn bslz4_roundtrip(elem in prop_oneof![Just(1usize), Just(2), Just(4), Just(8), 1usize..=6],
                       len in 0usize..6000,
                       block_units in 1usize..300,
                       sparse in any::<bool>(),
                       seed in any::<u64>()) {
        let data: Vec<u8> = (0..len)
            .map(|i| {
                let r = common::splitmix(seed, i as u64);
                if sparse && r % 5 != 0 { 0 } else { (r >> 9) as u8 }
            })
            .collect();
        let block = block_units * 8 * elem;
        let enc = compress_bslz4(&data, elem, block).unwrap();
        prop_assert_eq!(framing::decompress(Algorithm::Bslz4, elem as u64, &enc).unwrap(), data);
    }

    #[test]
    fn lz4_framing_roundtrip(len in 0usize..6000, block in 1usize..3000, seed in any::<u64>()) {
        let data: Vec<u8> = (0..len).map(|i| (common::splitmix(seed, (i / 3) as u64) % 7) as u8).collect();
        let enc = compress_lz4(&data, block).unwrap();
        prop_assert_eq!(framing::decompress(Algorithm::Lz4, 0, &enc).unwrap(), data);
    }

    #[test]
    fn lz4_decoder_matches_lz4_flex(len in 0usize..9000,
                                    alphabet in 1u64..=256,
                                    runs in any::<bool>(),
                                    seed in any::<u64>()) {
        let data: Vec<u8> = (0..len)
            .map(|i| {
                let k = if runs { i / 37 } else { i };
                (common::splitmix(seed, k as u64) % alphabet) as u8
            })
            .collect();
        let blk = lz4_flex::block::compress(&data);
        let mut a = vec![0u8; len];
        let mut b = vec![0u8; len];
        prop_assert_eq!(lz4::decompress_into(&blk, &mut a).unwrap(), len);
        prop_assert_eq!(lz4_flex::block::decompress_into(&blk, &mut b).unwrap(), len);
        prop_assert_eq!(&a, &data);
        prop_assert_eq!(&b, &data);
    }

    #[test]
    fn lz4_decoder_corrupted_agrees_with_lz4_flex(len in 1usize..3000,
                                                 seed in any::<u64>(),
                                                 flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
                                                 cut in any::<prop::sample::Index>(),
                                                 truncate in any::<bool>()) {
        let data: Vec<u8> = (0..len).map(|i| (common::splitmix(seed, (i / 5) as u64) % 11) as u8).collect();
        let mut blk = lz4_flex::block::compress(&data);
        for (idx, v) in &flips {
            let i = idx.index(blk.len());
            blk[i] = *v;
        }
        if truncate {
            let c = cut.index(blk.len() + 1);
            blk.truncate(c);
        }
        let mut a = vec![0u8; len];
        let mut b = vec![0u8; len];
        let ra = lz4::decompress_into(&blk, &mut a);
        let rb = lz4_flex::block::decompress_into(&blk, &mut b);
        match (ra, rb) {
            (Ok(na), Ok(nb)) => {
                prop_assert_eq!(na, nb);
                // exact-size decodes must be identical; a short decode leaves scratch past `na`
                prop_assert_eq!(&a[..na], &b[..nb]);
            }
            (Err(_), Err(_)) => {}
            (Ok(na), Err(e)) => prop_assert!(false, "we accept ({na} B), lz4_flex rejects: {e:?}"),
            (Err(e), Ok(nb)) => prop_assert!(false, "we reject ({e:?}), lz4_flex accepts ({nb} B)"),
        }
    }
}

/// A short decode (fewer bytes than the destination) must surface as an error in framing.
#[test]
fn short_lz4_block_is_an_error_in_framing() {
    let blk = lz4_flex::block::compress(&[5u8; 40]);
    let mut enc = 64u64.to_be_bytes().to_vec();
    enc.extend_from_slice(&64u32.to_be_bytes());
    enc.extend_from_slice(&(blk.len() as u32).to_be_bytes());
    enc.extend_from_slice(&blk);
    assert!(framing::decompress(Algorithm::Lz4, 0, &enc).is_err());
    let mut out = vec![0xAAu8; 64];
    assert_eq!(lz4::decompress_into(&blk, &mut out).unwrap(), 40);
}

// ---- CBOR value trees ----

fn arb_value() -> impl Strategy<Value = Value<'static>> {
    let leaf = prop_oneof![
        any::<u64>().prop_map(|v| Value::Integer(i128::from(v))),
        any::<i64>().prop_map(|v| Value::Integer(i128::from(v))),
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(|b| Value::Bytes(b.into())),
        "[a-zA-Z0-9 _\\-é漢]{0,20}".prop_map(|s| Value::Text(s.into())),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
        any::<f64>()
            .prop_filter("NaN != NaN", |f| !f.is_nan())
            .prop_map(Value::Float),
        (0u8..20).prop_map(Value::Simple),
    ];
    leaf.prop_recursive(6, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
            proptest::collection::vec(
                (
                    "[a-z]{1,6}".prop_map(|s| Value::Text(s.into())),
                    inner.clone()
                ),
                0..6
            )
            .prop_map(Value::Map),
            (2000u64..3000, inner).prop_map(|(t, v)| Value::Tag(t, Box::new(v))),
        ]
    })
}

fn write_value(w: &mut Writer, v: &Value<'_>, indefinite: bool) {
    match v {
        Value::Integer(i) => {
            if *i >= 0 {
                w.uint(*i as u64);
            } else {
                w.head(1, (-1 - *i) as u64);
            }
        }
        Value::Bytes(b) => {
            if indefinite && b.len() > 1 {
                let (x, y) = b.split_at(b.len() / 2);
                w.bytes_chunked(&[x, y]);
            } else {
                w.bytes(b);
            }
        }
        Value::Text(t) => {
            if indefinite && t.len() > 1 && t.is_char_boundary(t.len() / 2) {
                let (x, y) = t.split_at(t.len() / 2);
                w.text_chunked(&[x, y]);
            } else {
                w.text(t);
            }
        }
        Value::Array(a) => {
            if indefinite {
                w.array_indef();
            } else {
                w.array(a.len() as u64);
            }
            for x in a {
                write_value(w, x, indefinite);
            }
            if indefinite {
                w.brk();
            }
        }
        Value::Map(m) => {
            if indefinite {
                w.map_indef();
            } else {
                w.map(m.len() as u64);
            }
            for (k, x) in m {
                write_value(w, k, indefinite);
                write_value(w, x, indefinite);
            }
            if indefinite {
                w.brk();
            }
        }
        Value::Bool(b) => {
            w.bool(*b);
        }
        Value::Null => {
            w.null();
        }
        Value::Float(f) => {
            w.f64(*f);
        }
        Value::Simple(s) => {
            w.simple(*s);
        }
        Value::Tag(t, x) => {
            w.tag(*t);
            write_value(w, x, indefinite);
        }
        _ => unreachable!(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3_000, .. ProptestConfig::default() })]

    #[test]
    fn cbor_roundtrip(v in arb_value(), indefinite in any::<bool>()) {
        let mut w = Writer::new();
        write_value(&mut w, &v, indefinite);
        let buf = w.into_bytes();
        let d = Decoder::decode_all(&buf);
        // the arbitrary tree may nest deeper than MAX_DEPTH only via prop_recursive depth 6 +
        // tags; it never does, so every tree must decode
        prop_assert_eq!(d.unwrap(), v.clone());
        // skip covers exactly the item
        let mut r = dectris_codec::cbor::Reader::new(&buf);
        r.skip().unwrap();
        prop_assert!(r.at_end());
        // every strict prefix is an error, never a panic
        for cut in [0, buf.len() / 3, buf.len() / 2, buf.len().saturating_sub(1)] {
            if cut < buf.len() {
                prop_assert!(Decoder::decode_all(&buf[..cut]).is_err());
            }
        }
    }
}
