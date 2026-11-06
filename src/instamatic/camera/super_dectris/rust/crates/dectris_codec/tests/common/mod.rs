//! Shared helpers for the codec integration tests.
//!
//! The data generator below is mirrored exactly by `tests/gen_vectors.py` (`pattern_bytes`), so
//! oracle vectors only need to store the compressed bytes: the expected output is regenerated
//! here from `(pattern, seed, n_elems, elem)`.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// splitmix64 of `seed` and index `i` (vectorised identically in gen_vectors.py).
pub fn splitmix(seed: u64, i: u64) -> u64 {
    let mut z = seed.wrapping_add((i + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Element value of `pattern` at index `i` for an `elem`-byte unsigned type.
pub fn pattern_value(pattern: &str, seed: u64, i: u64, elem: usize) -> u64 {
    let mask: u64 = if elem >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * elem)) - 1
    };
    let r = splitmix(seed, i);
    match pattern {
        // detector-like: mostly 0..7, 1/64 hot pixels with random high values
        "low" => {
            if r & 63 != 0 {
                (r >> 61) & mask
            } else {
                (r >> 8) & mask
            }
        }
        "random" => r & mask,
        "zeros" => 0,
        "ramp" => i & mask,
        // saturated: mostly small, 1/16 at the type maximum
        "sat" => {
            if r & 15 == 0 {
                mask
            } else {
                (r >> 62) & mask
            }
        }
        other => panic!("unknown pattern {other}"),
    }
}

/// Little-endian bytes of `n` elements of `pattern`.
pub fn pattern_bytes(pattern: &str, seed: u64, n: usize, elem: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * elem);
    for i in 0..n as u64 {
        let v = pattern_value(pattern, seed, i, elem);
        out.extend_from_slice(&v.to_le_bytes()[..elem]);
    }
    out
}

/// Decode a hex string (whitespace ignored).
pub fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(s.len() % 2 == 0, "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The crate directory.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The committed vectors directory.
pub fn vectors_dir() -> PathBuf {
    crate_dir().join("tests").join("vectors")
}

/// The Python with the oracle packages, named by `DECTRIS_TEST_PYTHON`.
/// `None` → the Python-dependent tests skip (print and return).
pub fn python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DECTRIS_TEST_PYTHON") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    None
}

/// A scratch directory for regenerated (large) vectors: under the cargo target dir, never in
/// `tests/vectors/`.
pub fn regen_dir(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let d = base.join("dectris_codec_vectors").join(name);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

/// Run `tests/gen_vectors.py <args>` with the oracle Python; returns stdout. Panics if the
/// script fails (the caller already checked that Python exists).
pub fn run_gen(py: &Path, args: &[&str]) -> String {
    let script = crate_dir().join("tests").join("gen_vectors.py");
    let out = Command::new(py)
        .arg(&script)
        .args(args)
        .output()
        .expect("spawn python");
    assert!(
        out.status.success(),
        "gen_vectors.py {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
