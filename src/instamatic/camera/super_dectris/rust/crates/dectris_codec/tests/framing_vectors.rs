//! bslz4 / lz4 framing against the MIT vendor vectors (`src/dectris_compression/compression_test.cc`
//! :28-134, PROTOCOL §9.6–§9.7), the upstream dectris-compression 0.3.1 "turing" vectors, and the
//! DESIGN §4.5 validation table ([G2-E], [G2-F], [G2-K], [R15], [R17], [R18]).

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code (clippy.toml exempts #[test] fns only, not helpers)

mod common;

use common::unhex;
use dectris_codec::bitshuffle;
use dectris_codec::encode::{compress_bslz4, compress_lz4};
use dectris_codec::framing::{self, decoded_len, Algorithm, Scratch, MAX_BSLZ4_ELEM};
use dectris_common::DriverError;

fn bs(data: &[u8], elem: u64) -> Result<Vec<u8>, DriverError> {
    framing::decompress(Algorithm::Bslz4, elem, data)
}
fn lz(data: &[u8], modifier: u64) -> Result<Vec<u8>, DriverError> {
    framing::decompress(Algorithm::Lz4, modifier, data)
}
fn assert_decode_err<T: std::fmt::Debug>(r: Result<T, DriverError>) {
    match r {
        Err(DriverError::Decode { .. }) => {}
        other => panic!("expected Decode error, got {other:?}"),
    }
}

// ---- compression_test.cc (MIT) ----

#[test]
fn cc_decompress_block_underflow() {
    // :29-31 bslz4 E=1, leftover-only
    assert_eq!(
        bs(&unhex("0000000000000001 00002000 89"), 1).unwrap(),
        unhex("89")
    );
    // :32-35 bslz4 E=2
    assert_eq!(
        bs(&unhex("0000000000000002 00004000 8900"), 2).unwrap(),
        unhex("8900")
    );
    // :36-39 bslz4 E=4
    assert_eq!(
        bs(&unhex("0000000000000004 00008000 89000000"), 4).unwrap(),
        unhex("89000000")
    );
    // :40-43 lz4, 1-byte last block stored raw
    assert_eq!(
        lz(&unhex("0000000000000001 00002000 00000001 89"), 0).unwrap(),
        unhex("89")
    );
}

#[test]
fn cc_decompress_lz4_hdf5_compressed_blocks() {
    // :46-51
    let enc = unhex(
        "000000000000001d 00000010 0000000a 16ff010050ffffffffff 0000000a 13ff010050ffffffffff",
    );
    assert_eq!(lz(&enc, 0).unwrap(), vec![0xFF; 29]);
}

#[test]
fn cc_decompress_lz4_hdf5_uncompressed_blocks() {
    // :54-58
    let enc = unhex("0000000000000003 00000002 00000002 ffff 00000001 ff");
    assert_eq!(lz(&enc, 0).unwrap(), vec![0xFF; 3]);
}

#[test]
fn cc_decompress_empty() {
    // :61-67
    let enc = [0u8; 12];
    assert_eq!(bs(&enc, 1).unwrap(), Vec::<u8>::new());
    assert_eq!(lz(&enc, 0).unwrap(), Vec::<u8>::new());
}

#[test]
fn cc_decompress_error_empty() {
    // :70-77
    assert_decode_err(bs(&[], 1));
    assert_decode_err(lz(&[], 0));
}

#[test]
fn cc_decompress_error_zero_block_size() {
    // :79-89
    let enc = unhex("0000000000000001 00000000");
    assert_decode_err(bs(&enc, 1));
    assert_decode_err(lz(&enc, 0));
}

#[test]
fn cc_decompress_error_truncated_block_header() {
    // :91-102
    let enc = unhex("0000000000000001 00000001 00");
    assert_decode_err(bs(&enc, 1));
    assert_decode_err(lz(&enc, 0));
}

#[test]
fn cc_decompress_error_bslz4_zero_elem_size() {
    // :104-112
    assert_decode_err(bs(&[0u8; 12], 0));
}

#[test]
fn cc_decompress_error_bslz4_elem_size_overflow() {
    // :114-124 [G2-E]: checked before the header, so it fails even with orig_size = 0
    assert_decode_err(bs(&[0u8; 12], 0xFFFF_FFFE));
    assert_decode_err(bs(&[0u8; 12], MAX_BSLZ4_ELEM + 1));
    assert_decode_err(decoded_len(Algorithm::Bslz4, 0xFFFF_FFFE, &[0u8; 12]));
    // the bound itself is accepted
    assert_eq!(bs(&[0u8; 12], MAX_BSLZ4_ELEM).unwrap(), Vec::<u8>::new());
}

#[test]
fn cc_decompress_error_lz4_block_size_overflow() {
    // :126-134 (orig 0xFFFFFFFE, block 0xFFFFFFFE > i32::MAX)
    let enc = unhex("00000000fffffffe fffffffe 00000001 00");
    assert_decode_err(lz(&enc, 0));
}

// ---- from dectris/compression v0.3.1, python/tests/test_compression.py, under the MIT licence ----

const TURING: &[u8] = b"My hair is shingled, and the longest strands are about nine inches long.";

const TURING_BS1: &str =
    "00000000000000480000004000000040f21b33ab224827a91a5540ca40c0a54a8c6201c0cfea4b86b8262b7149e4000288b3420a0002ec483000feff0100f0017bfbe7eeefefbef70000000000000000000000098053f2f9b802ff7b00";
const TURING_BS2: &str =
    "00000000000000480000004000000042f031158013f488888382818b2924d1a9005008008a04feffffffddabbbf600000000f525e503b0803c5a80fb935e47c210da31102e04fffffffff7fdffdf000000006573206c6f6e672e";
const TURING_BS4: &str =
    "00000000000000480000004000000042f03107e5000111211dc00020feff1fe500003f1b04c6d0e58bc44522ffffffff000080c1aa99b846e800020bfffffadf00004c1c8c36f83991b00407ffffedbf00006573206c6f6e672e";
const TURING_LZ4: &str =
    "000000000000004800000040000000404d792068616972206973207368696e676c65642c20616e6420746865206c6f6e6765737420737472616e6473206172652061626f7574206e696e6520696e6368000000086573206c6f6e672e";

#[test]
fn turing_vectors() {
    assert_eq!(bs(&unhex(TURING_BS1), 1).unwrap(), TURING);
    assert_eq!(bs(&unhex(TURING_BS2), 2).unwrap(), TURING);
    assert_eq!(bs(&unhex(TURING_BS4), 4).unwrap(), TURING);
    assert_eq!(lz(&unhex(TURING_LZ4), 0).unwrap(), TURING);
    // lz4 modifier is ignored [R17]
    assert_eq!(lz(&unhex(TURING_LZ4), 4).unwrap(), TURING);
    assert_eq!(lz(&unhex(TURING_LZ4), u64::MAX).unwrap(), TURING);
}

/// The forward transform is the exact inverse of what the vendor encoder produced: the
/// LZ4-decoded first block of each turing stream equals `shuffle_block(plain block)`.
#[test]
fn forward_bitshuffle_matches_vendor_blocks() {
    for (hexs, elem) in [(TURING_BS1, 1usize), (TURING_BS2, 2), (TURING_BS4, 4)] {
        let enc = unhex(hexs);
        let csize = u32::from_be_bytes(enc[12..16].try_into().unwrap()) as usize;
        let block = 64usize; // header block_size 0x40
        let mut shuffled = vec![0u8; block];
        let n = lz4_flex::block::decompress_into(&enc[16..16 + csize], &mut shuffled).unwrap();
        assert_eq!(n, block);
        let mut mine = vec![0u8; block];
        bitshuffle::shuffle_block(&TURING[..block], &mut mine, elem).unwrap();
        assert_eq!(mine, shuffled, "elem {elem}");
    }
}

#[test]
fn upstream_error_vectors() {
    let enc = unhex("0000000000000001 00000000");
    for e in [1, 2, 4] {
        assert_decode_err(bs(&enc, e));
    }
    assert_decode_err(lz(&enc, 0));
}

#[test]
fn algorithm_names_byte_exact() {
    // [G2-K] / PROTOCOL §9.7
    assert_eq!(Algorithm::from_wire(b"bslz4").unwrap(), Algorithm::Bslz4);
    assert_eq!(Algorithm::from_wire(b"lz4").unwrap(), Algorithm::Lz4);
    for bad in [
        "", "LZ4", "lz", "lz5", "lz4 ", " lz4", "BSLZ4", "none", "bslz4\0",
    ] {
        assert_decode_err(Algorithm::from_wire(bad.as_bytes()));
        assert_decode_err(framing::decompress_named(bad.as_bytes(), 4, &[0u8; 12]));
    }
    assert_eq!(
        framing::decompress_named(b"lz4", 0, &[0u8; 12]).unwrap(),
        Vec::<u8>::new()
    );
}

// ---- DESIGN §4.5 validation table ----

#[test]
fn g2f_lz4_block_size_1gib_orig_3_decodes() {
    let enc = unhex("0000000000000003 40000000 00000003 abcdef");
    assert_eq!(lz(&enc, 0).unwrap(), unhex("abcdef"));
    // i32::MAX accepted, i32::MAX + 1 rejected
    let enc = unhex("0000000000000003 7fffffff 00000003 abcdef");
    assert_eq!(lz(&enc, 0).unwrap(), unhex("abcdef"));
    let enc = unhex("0000000000000003 80000000 00000003 abcdef");
    assert_decode_err(lz(&enc, 0));
}

#[test]
fn orig_size_cap_is_the_allocation_guard() {
    // 256 MiB + 1 is rejected before any allocation
    let mut enc = ((256u64 << 20) + 1).to_be_bytes().to_vec();
    enc.extend_from_slice(&8192u32.to_be_bytes());
    assert_decode_err(lz(&enc, 0));
    assert_decode_err(bs(&enc, 1));
    let mut enc = u64::MAX.to_be_bytes().to_vec();
    enc.extend_from_slice(&8192u32.to_be_bytes());
    assert_decode_err(decoded_len(Algorithm::Lz4, 0, &enc));
}

#[test]
fn bslz4_block_size_rules() {
    // not a multiple of 8·elem
    let mut enc = 64u64.to_be_bytes().to_vec();
    enc.extend_from_slice(&36u32.to_be_bytes());
    assert_decode_err(bs(&enc, 4));
    // > i32::MAX
    let mut enc = 64u64.to_be_bytes().to_vec();
    enc.extend_from_slice(&0x8000_0000u32.to_be_bytes());
    assert_decode_err(bs(&enc, 1));
}

#[test]
fn trailing_bytes_rejected_both_algorithms() {
    // the vendor C code accepts these (checked against the oracle in oracle_vectors.rs)
    let mut enc = unhex(TURING_LZ4);
    enc.push(0);
    assert_decode_err(lz(&enc, 0));
    let mut enc = unhex(TURING_BS1);
    enc.push(0);
    assert_decode_err(bs(&enc, 1));
    let mut enc = vec![0u8; 12];
    enc.push(0);
    assert_decode_err(bs(&enc, 1));
    assert_decode_err(lz(&enc, 0));
    // bslz4 leftover longer / shorter than the remaining input
    let mut enc = unhex(TURING_BS2); // leftover 8 bytes
    enc.pop();
    assert_decode_err(bs(&enc, 2));
}

#[test]
fn lz4_block_must_decode_to_exact_size() {
    // a valid LZ4 block for 16 bytes, framed as an 8-byte block → error
    let blk = lz4_flex::block::compress(&[7u8; 16]);
    let mut enc = 16u64.to_be_bytes().to_vec();
    enc.extend_from_slice(&8u32.to_be_bytes());
    enc.extend_from_slice(&(blk.len() as u32).to_be_bytes());
    enc.extend_from_slice(&blk);
    assert_decode_err(lz(&enc, 0));
    // framed as one 32-byte block (decodes short) → error
    let mut enc = 32u64.to_be_bytes().to_vec();
    enc.extend_from_slice(&32u32.to_be_bytes());
    enc.extend_from_slice(&(blk.len() as u32).to_be_bytes());
    enc.extend_from_slice(&blk);
    assert_decode_err(lz(&enc, 0));
}

#[test]
fn lz4_raw_last_partial_block() {
    // [R17]: raw shortcut compares with the last block's own size
    let enc = compress_lz4(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], 8).unwrap();
    assert_eq!(
        lz(&enc, 0).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    );
    // explicit: full block raw (8) + last block raw (3)
    let enc = unhex("000000000000000b 00000008 00000008 0102030405060708 00000003 090a0b");
    assert_eq!(lz(&enc, 7).unwrap(), (1..=11).collect::<Vec<u8>>());
}

#[test]
fn bslz4_decodes_with_the_wire_modifier() {
    // [R18]: u32 data encoded with elem 2 decodes correctly only with modifier 2
    let data: Vec<u8> = (0..4096u32)
        .flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes())
        .collect();
    let enc = compress_bslz4(&data, 2, 1024).unwrap();
    assert_eq!(bs(&enc, 2).unwrap(), data);
    // decoding with 4 gives a same-length different buffer (the oracle's silent case)
    let wrong = bs(&enc, 4);
    if let Ok(w) = wrong {
        assert_ne!(w, data);
    }
}

#[test]
fn decompress_into_requires_exact_destination() {
    let enc = unhex(TURING_LZ4);
    let mut out = vec![0u8; 71];
    assert_decode_err(framing::decompress_into(
        Algorithm::Lz4,
        0,
        &enc,
        &mut out,
        &mut Scratch::new(),
    ));
    let mut out = vec![0u8; 72];
    framing::decompress_into(Algorithm::Lz4, 0, &enc, &mut out, &mut Scratch::new()).unwrap();
    assert_eq!(out, TURING);
}

/// [R15]: truncation at every offset and byte flips in the LZ4 payload never panic.
#[test]
fn truncation_and_payload_corruption_never_panic() {
    let data: Vec<u8> = (0..3000u32)
        .flat_map(|i| ((i / 7) % 13).to_le_bytes())
        .collect();
    for (alg, enc, elem) in [
        (
            Algorithm::Bslz4,
            compress_bslz4(&data, 4, 512).unwrap(),
            4u64,
        ),
        (Algorithm::Lz4, compress_lz4(&data, 1000).unwrap(), 0),
        (Algorithm::Bslz4, unhex(TURING_BS4), 4),
        (Algorithm::Lz4, unhex(TURING_LZ4), 0),
    ] {
        let full = framing::decompress(alg, elem, &enc).unwrap();
        for cut in 0..enc.len() {
            assert!(
                framing::decompress(alg, elem, &enc[..cut]).is_err(),
                "{alg} cut {cut}"
            );
        }
        let mut s = 0x1234_5678u64;
        for i in 12..enc.len() {
            for _ in 0..3 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                let mut c = enc.clone();
                c[i] ^= (s >> 56) as u8 | 1;
                // any outcome but a panic; a successful decode must keep the length
                if let Ok(v) = framing::decompress(alg, elem, &c) {
                    assert_eq!(v.len(), full.len());
                }
            }
        }
    }
}

#[test]
fn encoder_roundtrip_block_layouts() {
    for elem in [1usize, 2, 4] {
        for n in [0usize, 1, 7, 8, 63, 64, 65, 1000, 8191, 8192, 8193, 20000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            for block in [8 * elem, 64 * elem, 8192] {
                let enc = compress_bslz4(&data, elem, block).unwrap();
                assert_eq!(
                    bs(&enc, elem as u64).unwrap(),
                    data,
                    "bslz4 elem {elem} n {n} block {block}"
                );
            }
            for block in [1usize, 7, 64, 8192, 1 << 30] {
                let enc = compress_lz4(&data, block).unwrap();
                assert_eq!(lz(&enc, 0).unwrap(), data, "lz4 n {n} block {block}");
            }
        }
    }
}
