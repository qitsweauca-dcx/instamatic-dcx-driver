//! CBOR decoder (DESIGN §4.4 [R20][R21][G2-K][G2-P]): RFC 8949 Appendix A, malformed items,
//! indefinite lengths and chunked strings, depth limit (decode and skip, depth bombs), the
//! typed-tag table incl. 76/83/87 and float16, tag 56500 rules, tag 1040.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code (clippy.toml exempts #[test] fns only, not helpers)

mod common;

use common::unhex;
use dectris_codec::cbor::{Decoder, Elements, Reader, Value, MAX_DEPTH};
use dectris_codec::encode::Writer;
use dectris_codec::typed_array::Order;
use dectris_codec::Algorithm;
use dectris_common::DriverError;

fn dec(hex: &str) -> Value<'static> {
    let b = unhex(hex);
    Decoder::decode_all(&b)
        .unwrap_or_else(|e| panic!("{hex}: {e}"))
        .into_owned()
}

fn int(i: i128) -> Value<'static> {
    Value::Integer(i)
}
fn text(s: &str) -> Value<'static> {
    Value::Text(s.to_owned().into())
}
fn bytes(b: &[u8]) -> Value<'static> {
    Value::Bytes(b.to_vec().into())
}
fn arr(v: Vec<Value<'static>>) -> Value<'static> {
    Value::Array(v)
}
fn map(v: Vec<(Value<'static>, Value<'static>)>) -> Value<'static> {
    Value::Map(v)
}

fn assert_err(b: &[u8]) {
    match Decoder::decode_all(b) {
        Err(DriverError::Decode { .. }) => {}
        other => panic!("{}: expected Decode error, got {other:?}", common::hex(b)),
    }
    let mut r = Reader::new(b);
    let skipped = r.skip().map(|()| r.at_end());
    assert!(
        !matches!(skipped, Ok(true)),
        "{}: skip accepted a malformed item",
        common::hex(b)
    );
}

#[test]
fn rfc8949_appendix_a_integers() {
    let cases: &[(&str, i128)] = &[
        ("00", 0),
        ("01", 1),
        ("0a", 10),
        ("17", 23),
        ("1818", 24),
        ("1819", 25),
        ("1864", 100),
        ("1903e8", 1000),
        ("1a000f4240", 1_000_000),
        ("1b000000e8d4a51000", 1_000_000_000_000),
        ("1bffffffffffffffff", 18_446_744_073_709_551_615),
        ("3bffffffffffffffff", -18_446_744_073_709_551_616),
        ("20", -1),
        ("29", -10),
        ("3863", -100),
        ("3903e7", -1000),
    ];
    for (h, v) in cases {
        assert_eq!(dec(h), int(*v), "{h}");
    }
    // bignums are pass-through tags 2/3
    assert_eq!(
        dec("c249010000000000000000"),
        Value::Tag(2, Box::new(bytes(&unhex("010000000000000000"))))
    );
    assert_eq!(
        dec("c349010000000000000000"),
        Value::Tag(3, Box::new(bytes(&unhex("010000000000000000"))))
    );
}

#[test]
fn rfc8949_appendix_a_floats_incl_f16() {
    let cases: &[(&str, f64)] = &[
        ("f90000", 0.0),
        ("f93c00", 1.0),
        ("fb3ff199999999999a", 1.1),
        ("f93e00", 1.5),
        ("f97bff", 65504.0),
        ("fa47c35000", 100000.0),
        ("fa7f7fffff", 3.4028234663852886e38),
        ("fb7e37e43c8800759c", 1.0e300),
        ("f90001", 5.960464477539063e-8), // f16 smallest subnormal
        ("f90400", 0.00006103515625),     // f16 smallest normal
        ("f9c400", -4.0),
        ("fbc010666666666666", -4.1),
        ("f97c00", f64::INFINITY),
        ("f9fc00", f64::NEG_INFINITY),
        ("fa7f800000", f64::INFINITY),
        ("faff800000", f64::NEG_INFINITY),
        ("fb7ff0000000000000", f64::INFINITY),
        ("fbfff0000000000000", f64::NEG_INFINITY),
    ];
    for (h, v) in cases {
        match dec(h) {
            Value::Float(f) => assert_eq!(f, *v, "{h}"),
            other => panic!("{h}: {other:?}"),
        }
    }
    match dec("f98000") {
        Value::Float(f) => assert!(f == 0.0 && f.is_sign_negative()),
        other => panic!("{other:?}"),
    }
    for h in ["f97e00", "fa7fc00000", "fb7ff8000000000000"] {
        assert!(matches!(dec(h), Value::Float(f) if f.is_nan()), "{h}");
    }
}

#[test]
fn rfc8949_appendix_a_simple_tags_strings() {
    assert_eq!(dec("f4"), Value::Bool(false));
    assert_eq!(dec("f5"), Value::Bool(true));
    assert_eq!(dec("f6"), Value::Null);
    assert_eq!(dec("f7"), Value::Null); // undefined → None [G2-P]
    assert_eq!(dec("f0"), Value::Simple(16));
    assert_eq!(dec("f8ff"), Value::Simple(255));
    assert_eq!(
        dec("c074323031332d30332d32315432303a30343a30305a"),
        Value::DateTime("2013-03-21T20:04:00Z".into())
    );
    assert_eq!(
        dec("c11a514b67b0"),
        Value::Tag(1, Box::new(int(1_363_896_240)))
    );
    assert_eq!(
        dec("c1fb41d452d9ec200000"),
        Value::Tag(1, Box::new(Value::Float(1_363_896_240.5)))
    );
    assert_eq!(
        dec("d74401020304"),
        Value::Tag(23, Box::new(bytes(&[1, 2, 3, 4])))
    );
    assert_eq!(
        dec("d818456449455446"),
        Value::Tag(24, Box::new(bytes(&unhex("6449455446"))))
    );
    assert_eq!(
        dec("d82076687474703a2f2f7777772e6578616d706c652e636f6d"),
        Value::Tag(32, Box::new(text("http://www.example.com")))
    );
    assert_eq!(dec("40"), bytes(&[]));
    assert_eq!(dec("4401020304"), bytes(&[1, 2, 3, 4]));
    assert_eq!(dec("60"), text(""));
    assert_eq!(dec("6161"), text("a"));
    assert_eq!(dec("6449455446"), text("IETF"));
    assert_eq!(dec("62225c"), text("\"\\"));
    assert_eq!(dec("62c3bc"), text("ü"));
    assert_eq!(dec("63e6b0b4"), text("水"));
    assert_eq!(dec("64f0908591"), text("𐅑"));
}

#[test]
fn rfc8949_appendix_a_containers_definite_and_indefinite() {
    let nested = arr(vec![
        int(1),
        arr(vec![int(2), int(3)]),
        arr(vec![int(4), int(5)]),
    ]);
    let seq25 = arr((1..=25).map(int).collect());
    assert_eq!(dec("80"), arr(vec![]));
    assert_eq!(dec("83010203"), arr(vec![int(1), int(2), int(3)]));
    assert_eq!(dec("8301820203820405"), nested);
    assert_eq!(
        dec("98190102030405060708090a0b0c0d0e0f101112131415161718181819"),
        seq25
    );
    assert_eq!(dec("a0"), map(vec![]));
    assert_eq!(
        dec("a201020304"),
        map(vec![(int(1), int(2)), (int(3), int(4))])
    );
    let ab = map(vec![
        (text("a"), int(1)),
        (text("b"), arr(vec![int(2), int(3)])),
    ]);
    assert_eq!(dec("a26161016162820203"), ab);
    let a_bc = arr(vec![text("a"), map(vec![(text("b"), text("c"))])]);
    assert_eq!(dec("826161a161626163"), a_bc);
    assert_eq!(
        dec("a56161614161626142616361436164614461656145"),
        map(vec![
            (text("a"), text("A")),
            (text("b"), text("B")),
            (text("c"), text("C")),
            (text("d"), text("D")),
            (text("e"), text("E")),
        ])
    );
    // indefinite / chunked
    assert_eq!(dec("5f42010243030405ff"), bytes(&[1, 2, 3, 4, 5]));
    assert_eq!(dec("7f657374726561646d696e67ff"), text("streaming"));
    assert_eq!(dec("9fff"), arr(vec![]));
    assert_eq!(dec("9f018202039f0405ffff"), nested);
    assert_eq!(dec("9f01820203820405ff"), nested);
    assert_eq!(dec("83018202039f0405ff"), nested);
    assert_eq!(dec("83019f0203ff820405"), nested);
    assert_eq!(
        dec("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff"),
        seq25
    );
    assert_eq!(dec("bf61610161629f0203ffff"), ab);
    assert_eq!(dec("826161bf61626163ff"), a_bc);
    assert_eq!(
        dec("bf6346756ef563416d7421ff"),
        map(vec![
            (text("Fun"), Value::Bool(true)),
            (text("Amt"), int(-2))
        ])
    );
    // chunked strings are owned, definite ones borrowed (zero-copy)
    let b = unhex("4401020304");
    assert!(matches!(
        Decoder::decode_all(&b).unwrap(),
        Value::Bytes(std::borrow::Cow::Borrowed(_))
    ));
    let b = unhex("5f42010243030405ff");
    assert!(matches!(
        Decoder::decode_all(&b).unwrap(),
        Value::Bytes(std::borrow::Cow::Owned(_))
    ));
}

#[test]
fn non_minimal_heads_accepted() {
    // PROTOCOL §8.1: any argument width
    let mut w = Writer::new();
    w.head_wide(0, 27, 5);
    assert_eq!(Decoder::decode_all(&w.buf).unwrap(), int(5));
    let mut w = Writer::new();
    w.head_wide(3, 26, 2);
    w.raw(b"hi");
    assert_eq!(Decoder::decode_all(&w.buf).unwrap(), text("hi"));
    let mut w = Writer::new();
    w.head_wide(4, 25, 1).uint(9);
    assert_eq!(Decoder::decode_all(&w.buf).unwrap(), arr(vec![int(9)]));
}

#[test]
fn malformed_items_are_errors() {
    for h in [
        "",
        "1c",
        "1d",
        "1e",
        "3c",
        "5c",
        "7d",
        "9e",
        "be",
        "dc",
        "fc",
        "fd",
        "fe", // reserved ai 28-30
        "1f",
        "3f",
        "df", // indefinite ints / tags
        "ff",
        "81ff",
        "82ff", // misplaced break
        "18",
        "1900",
        "1a000000",
        "1b00000000000000",
        "41",
        "61",
        "81",
        "a100",
        "c0",
        "f9",
        "fa0000",
        "fb00",
        "5f",
        "5f41",
        "5f4100",
        "5f60ff",
        "5f5f41ff",
        "7f41ff",
        "7f6100", // chunked strings
        "9f",
        "9f01",
        "bf",
        "bf01",
        "bf00ff",
        "bf0102", // indefinite containers
        "f800",
        "f81f", // one-byte simple < 32
    ] {
        assert_err(&unhex(h));
    }
    // invalid UTF-8 and tag-0 content that is not text: decode errors; `skip` is structural only
    // (strings skipped in O(1), tag semantics not checked) and accepts them
    for h in ["62c328", "61ff", "7f61ff6161ff", "c0f5", "c040"] {
        assert!(
            matches!(
                Decoder::decode_all(&unhex(h)),
                Err(DriverError::Decode { .. })
            ),
            "{h}"
        );
    }
    // trailing bytes after the item
    assert_err(&unhex("0000"));
}

fn nested_arrays(depth: usize, indefinite: bool) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..depth {
        v.push(if indefinite { 0x9F } else { 0x81 });
    }
    v.push(0x00);
    if indefinite {
        v.extend(std::iter::repeat_n(0xFF, depth));
    }
    v
}

#[test]
fn depth_limit_decode_and_skip() {
    for indefinite in [false, true] {
        let ok = nested_arrays(MAX_DEPTH, indefinite);
        assert!(Decoder::decode_all(&ok).is_ok());
        let mut r = Reader::new(&ok);
        r.skip().unwrap();
        assert!(r.at_end());
        let bad = nested_arrays(MAX_DEPTH + 1, indefinite);
        match Decoder::decode_all(&bad) {
            Err(DriverError::Decode { msg, .. }) => assert!(msg.contains("nesting too deep")),
            other => panic!("{other:?}"),
        }
        let mut r = Reader::new(&bad);
        assert!(r.skip().is_err());
    }
    // tags count as levels
    let mut t = vec![0xC1u8; MAX_DEPTH];
    t.push(0);
    assert!(Decoder::decode_all(&t).is_ok());
    let mut t = vec![0xC1u8; MAX_DEPTH + 1];
    t.push(0);
    assert!(Decoder::decode_all(&t).is_err());
    assert!(Reader::new(&t).skip().is_err());
}

#[test]
fn depth_bombs_do_not_overflow_the_stack() {
    // DESIGN §4.4 [R20]: ≥ 33 levels → error; a 1 M-level bomb must not overflow the native stack in
    // either the recursive decoder or the iterative skip (debug builds included)
    for byte in [0x81u8, 0x9F, 0xA1, 0xBF, 0xC1, 0xD8] {
        let mut bomb = vec![byte; 1_000_000];
        if byte == 0xD8 {
            // tag 24.. one-byte tag numbers: 0xD8 0xD8 … reads as tag 216 chains
            bomb.push(0);
        }
        assert!(Decoder::decode_all(&bomb).is_err(), "{byte:#x}");
        assert!(Reader::new(&bomb).skip().is_err(), "{byte:#x}");
        let mut w = Writer::new();
        w.map(1).text("data").raw(&bomb);
        assert!(dectris_codec::decode_message(&w.buf).is_err());
        assert!(dectris_codec::scan_header(&w.buf).is_err());
    }
}

#[test]
fn huge_declared_lengths_fail_without_allocation() {
    for h in [
        "9bffffffffffffffff",
        "bbffffffffffffffff",
        "5bffffffffffffffff",
        "7bffffffffffffffff",
        "9b7fffffffffffffff00",
        "bb4000000000000000",
    ] {
        assert_err(&unhex(h));
    }
}

// ---- typed arrays, tags 40/1040/56500 ----

fn typed_msg(build: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    build(&mut w);
    w.into_bytes()
}

#[test]
fn typed_tag_table_and_rejected_tags() {
    // every assigned tag decodes; 76/83/87 are errors in any position
    for &(tag, _) in dectris_codec::TYPED_ARRAY_DTYPES {
        let e = dectris_codec::ElemType::from_tag(tag).unwrap();
        let b = typed_msg(|w| {
            w.tag(tag).bytes(&vec![0u8; e.size * 3]);
        });
        match Decoder::decode_all(&b).unwrap() {
            Value::TypedArray(t) => {
                assert_eq!(t.elem, e);
                assert_eq!(t.len(), 3);
            }
            other => panic!("{tag}: {other:?}"),
        }
    }
    for tag in [76u64, 83, 87] {
        let b = typed_msg(|w| {
            w.tag(tag).bytes(&[0u8; 16]);
        });
        let e = Decoder::decode_all(&b).unwrap_err();
        assert!(e.to_string().contains(&tag.to_string()), "{e}");
        let b = typed_msg(|w| {
            w.tag(40)
                .array(2)
                .array(1)
                .uint(1)
                .tag(tag)
                .bytes(&[0u8; 16]);
        });
        assert!(Decoder::decode_all(&b).is_err());
    }
    // length not a multiple of the element size
    let b = typed_msg(|w| {
        w.tag(70).bytes(&[0u8; 5]);
    });
    assert!(Decoder::decode_all(&b).is_err());
    // typed tag over a non-bytes item
    let b = typed_msg(|w| {
        w.tag(70).array(1).uint(1);
    });
    assert!(Decoder::decode_all(&b).is_err());
}

#[test]
fn big_endian_and_f16_payloads() {
    let b = typed_msg(|w| {
        w.tag(66)
            .bytes(&[0x01, 0x02, 0x03, 0x04, 0xAA, 0xBB, 0xCC, 0xDD]);
    });
    match Decoder::decode_all(&b).unwrap() {
        Value::TypedArray(t) => {
            assert_eq!(&*t.data, &[0x04, 0x03, 0x02, 0x01, 0xDD, 0xCC, 0xBB, 0xAA]);
            assert_eq!(t.elem.native_dtype(), "<u4");
            assert_eq!(t.elem.wire_dtype(), ">u4");
        }
        other => panic!("{other:?}"),
    }
    // float16 BE (tag 80): 1.5 = 0x3E00 → LE bits 00 3E
    let b = typed_msg(|w| {
        w.tag(80).bytes(&[0x3E, 0x00, 0x7C, 0x00]);
    });
    match Decoder::decode_all(&b).unwrap() {
        Value::TypedArray(t) => {
            assert_eq!(&*t.data, &[0x00, 0x3E, 0x00, 0x7C]);
            assert_eq!(t.elem.native_dtype(), "<f2");
            let bits: Vec<u16> = t
                .data
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            assert_eq!(dectris_codec::cbor::f16_to_f64(bits[0]), 1.5);
            assert_eq!(dectris_codec::cbor::f16_to_f64(bits[1]), f64::INFINITY);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nestings_40_70_raw_and_56500() {
    let le: Vec<u8> = (0..12u32).flat_map(|v| (v * 1000).to_le_bytes()).collect();
    let check = |b: &[u8], compressed: Option<Algorithm>| match Decoder::decode_all(b).unwrap() {
        Value::MultiDim(m) => {
            assert_eq!(m.dims, vec![3, 4]);
            assert_eq!(m.source_order, Order::C);
            match m.elements {
                Elements::Typed(t) => {
                    assert_eq!(&*t.data, &le[..]);
                    assert_eq!(t.compression.map(|c| c.algorithm), compressed);
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    };
    for (payload, alg) in [
        (dectris_codec::encode::Payload::Raw, None),
        (
            dectris_codec::encode::Payload::Bslz4 { block_size: 32 },
            Some(Algorithm::Bslz4),
        ),
        (dectris_codec::encode::Payload::LZ4, Some(Algorithm::Lz4)),
    ] {
        let mut w = Writer::new();
        dectris_codec::encode::write_multidim(&mut w, &[3, 4], 70, &le, payload).unwrap();
        check(&w.buf, alg);
    }
    // a bare 56500 where a byte string is expected → Bytes (decompressed)
    let enc = dectris_codec::encode::compress_lz4(b"abcabcabcabc", 1 << 30).unwrap();
    let b = typed_msg(|w| {
        w.compressed("lz4", 0, &enc);
    });
    assert_eq!(Decoder::decode_all(&b).unwrap(), bytes(b"abcabcabcabc"));
    // modifier ≠ itemsize: decoded with the modifier, flagged [R18]
    let mut w = Writer::new();
    dectris_codec::encode::write_multidim(
        &mut w,
        &[3, 4],
        70,
        &le,
        dectris_codec::encode::Payload::Bslz4Modifier {
            block_size: 16,
            modifier: 2,
        },
    )
    .unwrap();
    match Decoder::decode_all(&w.buf).unwrap() {
        Value::MultiDim(m) => match m.elements {
            Elements::Typed(t) => {
                assert_eq!(&*t.data, &le[..]);
                assert!(t.modifier_mismatch());
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn tag_56500_content_rules_g2k() {
    let enc = dectris_codec::encode::compress_lz4(&[1, 2, 3, 4], 1 << 30).unwrap();
    let ok = typed_msg(|w| {
        w.tag(70)
            .tag(56500)
            .array(3)
            .text("lz4")
            .uint(0)
            .bytes(&enc);
    });
    assert!(Decoder::decode_all(&ok).is_ok());
    let indef = typed_msg(|w| {
        w.tag(70)
            .tag(56500)
            .array_indef()
            .text("lz4")
            .uint(0)
            .bytes(&enc)
            .brk();
    });
    assert!(Decoder::decode_all(&indef).is_ok());
    let bad: Vec<Vec<u8>> = vec![
        typed_msg(|w| {
            w.tag(70).tag(56500).array(2).text("lz4").bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(4)
                .text("lz4")
                .uint(0)
                .bytes(&enc)
                .uint(0);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array_indef()
                .text("lz4")
                .uint(0)
                .bytes(&enc)
                .uint(0)
                .brk();
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("LZ4")
                .uint(0)
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("lz4 ")
                .uint(0)
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("zstd")
                .uint(0)
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .bytes(b"lz4")
                .uint(0)
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("lz4")
                .text("0")
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("lz4")
                .int(-1)
                .bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70).tag(56500).array(3).text("lz4").uint(0).text("x");
        }),
        typed_msg(|w| {
            w.tag(70).tag(56500).bytes(&enc);
        }),
        typed_msg(|w| {
            w.tag(70)
                .tag(56500)
                .array(3)
                .text("bslz4")
                .uint(0)
                .bytes(&enc);
        }),
    ];
    for b in bad {
        let e = Decoder::decode_all(&b).unwrap_err();
        assert!(matches!(e, DriverError::Decode { .. }), "{e}");
    }
    let e = Decoder::decode_all(&typed_msg(|w| {
        w.tag(70)
            .tag(56500)
            .array(3)
            .text("zstd")
            .uint(0)
            .bytes(&enc);
    }))
    .unwrap_err();
    assert!(
        e.to_string().contains("unsupported compression algorithm"),
        "{e}"
    );
}

#[test]
fn multidim_validation_and_1040() {
    // dims product must match
    let b = typed_msg(|w| {
        w.tag(40)
            .array(2)
            .array(2)
            .uint(2)
            .uint(3)
            .tag(70)
            .bytes(&[0u8; 20]);
    });
    assert!(Decoder::decode_all(&b).is_err());
    // overflowing dims → error before allocation
    let b = typed_msg(|w| {
        w.tag(40)
            .array(2)
            .array(2)
            .uint(u64::MAX)
            .uint(3)
            .tag(70)
            .bytes(&[0u8; 4]);
    });
    assert!(Decoder::decode_all(&b).is_err());
    let b = typed_msg(|w| {
        w.tag(40)
            .array(2)
            .array(2)
            .uint(1 << 20)
            .uint(1 << 20)
            .tag(64)
            .bytes(&[]);
    });
    assert!(Decoder::decode_all(&b).is_err());
    // wrong arity, non-uint dim, bare bytes elements, unknown element tag
    for b in [
        typed_msg(|w| {
            w.tag(40)
                .array(3)
                .array(1)
                .uint(1)
                .tag(64)
                .bytes(&[1])
                .uint(0);
        }),
        typed_msg(|w| {
            w.tag(40).array(2).array(1).text("1").tag(64).bytes(&[1]);
        }),
        typed_msg(|w| {
            w.tag(40).array(2).array(1).uint(1).bytes(&[1]);
        }),
        typed_msg(|w| {
            w.tag(40).array(2).array(1).uint(1).tag(99).bytes(&[1]);
        }),
        typed_msg(|w| {
            w.tag(40).array(2).uint(1).tag(64).bytes(&[1]);
        }),
    ] {
        assert!(Decoder::decode_all(&b).is_err(), "{}", common::hex(&b));
    }
    // 1040 typed: F → C
    let b = typed_msg(|w| {
        w.tag(1040)
            .array(2)
            .array(2)
            .uint(2)
            .uint(3)
            .tag(64)
            .bytes(&[0, 3, 1, 4, 2, 5]);
    });
    match Decoder::decode_all(&b).unwrap() {
        Value::MultiDim(m) => {
            assert_eq!(m.source_order, Order::F);
            assert_eq!(m.dims, vec![2, 3]);
            match m.elements {
                Elements::Typed(t) => assert_eq!(&*t.data, &[0, 1, 2, 3, 4, 5]),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    // 1040 object array [G2-P] → permuted to C order
    let b = typed_msg(|w| {
        w.tag(1040)
            .array(2)
            .array(2)
            .uint(2)
            .uint(3)
            .array(6)
            .uint(1)
            .uint(2)
            .uint(3)
            .uint(4)
            .uint(5)
            .uint(6);
    });
    match Decoder::decode_all(&b).unwrap() {
        Value::MultiDim(m) => match m.elements {
            Elements::Values(v) => assert_eq!(v, [1, 3, 5, 2, 4, 6].map(int).to_vec()),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // N-D and empty arrays are fine in decode_message
    let b = typed_msg(|w| {
        w.tag(40)
            .array(2)
            .array(3)
            .uint(2)
            .uint(2)
            .uint(2)
            .tag(69)
            .bytes(&[0u8; 16]);
    });
    assert!(Decoder::decode_all(&b).is_ok());
    let b = typed_msg(|w| {
        w.tag(40)
            .array(2)
            .array(2)
            .uint(0)
            .uint(5)
            .tag(70)
            .bytes(&[]);
    });
    assert!(Decoder::decode_all(&b).is_ok());
}

/// Truncation at every offset of a message full of features never panics.
#[test]
fn truncation_at_every_offset() {
    let le: Vec<u8> = (0..300u32).flat_map(|v| (v % 17).to_le_bytes()).collect();
    let mut w = Writer::new();
    w.map_indef()
        .text("type")
        .text("image")
        .text("series_id")
        .uint(7);
    w.text("s")
        .text_chunked(&["ab", "cd"])
        .text("b")
        .bytes_chunked(&[b"x", b"yz"]);
    w.text("f")
        .f16_bits(0x3E00)
        .text("g")
        .f32(1.5)
        .text("h")
        .f64(2.5)
        .text("n")
        .int(-5);
    w.text("t").tag(1234).array_indef().uint(1).null().brk();
    w.text("data").map(1).text("threshold_1");
    dectris_codec::encode::write_multidim(
        &mut w,
        &[15, 20],
        70,
        &le,
        dectris_codec::encode::Payload::Bslz4 { block_size: 256 },
    )
    .unwrap();
    w.brk();
    let full = w.into_bytes();
    assert!(dectris_codec::decode_message(&full).is_ok());
    assert!(dectris_codec::parse_image_message(&full, "threshold_1").is_ok());
    for cut in 0..full.len() {
        let b = &full[..cut];
        assert!(dectris_codec::decode_message(b).is_err(), "cut {cut}");
        assert!(dectris_codec::scan_header(b).is_err(), "cut {cut}");
        assert!(
            dectris_codec::parse_image_message(b, "threshold_1").is_err(),
            "cut {cut}"
        );
        assert!(Decoder::decode_all(b).is_err(), "cut {cut}");
    }
}
