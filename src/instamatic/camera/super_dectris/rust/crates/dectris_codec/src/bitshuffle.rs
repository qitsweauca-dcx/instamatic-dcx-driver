//! Bitshuffle transform of one block (docs/DESIGN.md §4.5; normative algorithm PROTOCOL §9.5).
//!
//! A shuffled block of `n` elements (`n` a multiple of 8) of `E` bytes each is laid out as `8·E`
//! bit-rows of `n/8` bytes. Row `r = j·8 + k` holds bit `k` of byte `j` (memory order) of every
//! element; byte `g` of a row packs elements `8g … 8g+7` LSB-first.
//!
//! [`unshuffle_block`] is the inverse the driver runs on every compressed frame. It goes straight
//! from the shuffled block to the destination (no second scratch). The normative per-group form
//! (PROTOCOL §9.5) gathers the 8 row bytes at offset `g` of plane `j` into one little-endian word,
//! applies `TRANS_BIT_8X8` and scatters byte `k` to byte `j` of element `8g + k`; that is
//! `unshuffle_scalar`, used for element sizes other than 1/2/4/8 and for the tail groups.
//! The fast kernel computes the same permutation 64 elements (one `u64` per bit-row) × `LANES`
//! column blocks at a time: an in-lane 8×8 bit transpose across the 8 row words of a plane, an
//! 8×8 byte transpose (now word `g'` = plane-`j` bytes of elements `8g' … 8g'+7`), then a word-level
//! interleave of the `E` planes. All of it is safe Rust with bounds-checked access
//! (`#![deny(clippy::indexing_slicing)]`); the lane loops vectorise to SSE2.
//!
//! [`shuffle_block`] is the forward transform, used by the encoder in [`crate::encode`] (tests and
//! fake streams). It is checked against the "turing" vectors of dectris/compression v0.3.1 (MIT licence), where the LZ4-decoded
//! block of a real bitshuffle stream is exactly the forward transform of the plain text.

use dectris_common::{DriverError, Result};

/// `TRANS_BIT_8X8` (PROTOCOL §9.5): transpose the 8×8 bit matrix held in a little-endian word
/// (byte `i` = row `i`, bit `b` = column `b`). It is an involution.
#[inline(always)]
pub const fn trans_bit_8x8(mut x: u64) -> u64 {
    let mut t = (x ^ (x >> 7)) & 0x00AA_00AA_00AA_00AA;
    x ^= t ^ (t << 7);
    t = (x ^ (x >> 14)) & 0x0000_CCCC_0000_CCCC;
    x ^= t ^ (t << 14);
    t = (x ^ (x >> 28)) & 0x0000_0000_F0F0_F0F0;
    x ^= t ^ (t << 28);
    x
}

/// Transpose the 8×8 byte matrix `w[row]` byte `col` in place (single-lane reference of
/// [`trans_byte_v`], kept for its unit test).
#[cfg(test)]
fn trans_byte_8x8(w: &mut [u64; 8]) {
    // Three stages of block swaps (4×4, 2×2, 1×1) written as fixed pairs so no index is computed.
    let [w0, w1, w2, w3, w4, w5, w6, w7] = w;
    const M32: u64 = 0x0000_0000_FFFF_FFFF;
    const M16: u64 = 0x0000_FFFF_0000_FFFF;
    const M8: u64 = 0x00FF_00FF_00FF_00FF;
    swap_stage(w0, w4, 32, M32);
    swap_stage(w1, w5, 32, M32);
    swap_stage(w2, w6, 32, M32);
    swap_stage(w3, w7, 32, M32);
    swap_stage(w0, w2, 16, M16);
    swap_stage(w1, w3, 16, M16);
    swap_stage(w4, w6, 16, M16);
    swap_stage(w5, w7, 16, M16);
    swap_stage(w0, w1, 8, M8);
    swap_stage(w2, w3, 8, M8);
    swap_stage(w4, w5, 8, M8);
    swap_stage(w6, w7, 8, M8);
}

fn internal(msg: &str) -> DriverError {
    DriverError::Internal(format!("bitshuffle: {msg}"))
}

fn check_args(inp_len: usize, out_len: usize, elem: usize) -> Result<usize> {
    if elem == 0 {
        return Err(internal("element size 0"));
    }
    if inp_len != out_len {
        return Err(internal("input and output lengths differ"));
    }
    let row_unit = elem
        .checked_mul(8)
        .ok_or_else(|| internal("element size overflow"))?;
    if inp_len % row_unit != 0 {
        return Err(internal("block length is not a multiple of 8 elements"));
    }
    // bytes per bit-row = n / 8 = len / (8·E)
    Ok(inp_len / row_unit)
}

/// Inverse bitshuffle of one block: `inp` (shuffled) → `out` (elements in memory order).
///
/// `inp.len() == out.len() == n·elem` with `n % 8 == 0`. The caller (framing) guarantees the
/// shape; a violation returns `Internal`, never panics.
pub fn unshuffle_block(inp: &[u8], out: &mut [u8], elem: usize) -> Result<()> {
    let m = check_args(inp.len(), out.len(), elem)?;
    if m == 0 {
        return Ok(());
    }
    let blocks = m / 8; // column blocks of 8 groups = 64 elements
    let done = match elem {
        1 => unshuffle_fast_all::<1>(inp, out, m, blocks),
        2 => unshuffle_fast_all::<2>(inp, out, m, blocks),
        4 => unshuffle_fast_all::<4>(inp, out, m, blocks),
        8 => unshuffle_fast_all::<8>(inp, out, m, blocks),
        _ => Some(0),
    }
    .ok_or_else(|| internal("fast kernel bounds"))?;
    unshuffle_scalar(inp, out, elem, m, done)
}

/// Block swap used by both 8×8 transposes: exchange the `lo_mask` part of `b` with the
/// complementary part of `a` (one butterfly stage).
#[inline(always)]
fn swap_stage(a: &mut u64, b: &mut u64, shift: u32, lo_mask: u64) {
    let (x, y) = (*a, *b);
    *a = (x & lo_mask) | ((y & lo_mask) << shift);
    *b = ((x >> shift) & lo_mask) | (y & !lo_mask);
}

/// Spread 4 bytes to the even byte positions of a word (`b0 0 b1 0 b2 0 b3 0`).
#[inline(always)]
const fn spread8(x: u64) -> u64 {
    let mut v = x & 0xFFFF_FFFF;
    v = (v | (v << 16)) & 0x0000_FFFF_0000_FFFF;
    (v | (v << 8)) & 0x00FF_00FF_00FF_00FF
}

/// Spread 2 u16 to the even u16 positions of a word.
#[inline(always)]
const fn spread16(x: u64) -> u64 {
    let v = x & 0xFFFF_FFFF;
    (v | (v << 16)) & 0x0000_FFFF_0000_FFFF
}

/// Byte-interleave two words: `[a0 b0 a1 b1 a2 b2 a3 b3]`, `[a4 b4 … a7 b7]`.
#[inline(always)]
const fn interleave8(a: u64, b: u64) -> (u64, u64) {
    (
        spread8(a) | (spread8(b) << 8),
        spread8(a >> 32) | (spread8(b >> 32) << 8),
    )
}

/// u16-interleave two words.
#[inline(always)]
const fn interleave16(a: u64, b: u64) -> (u64, u64) {
    (
        spread16(a) | (spread16(b) << 16),
        spread16(a >> 32) | (spread16(b >> 32) << 16),
    )
}

#[inline(always)]
fn store_words(dst: &mut [u8], words: &[u64]) {
    for (d, w) in dst.chunks_exact_mut(8).zip(words) {
        d.copy_from_slice(&w.to_le_bytes());
    }
}

/// Number of 64-element column blocks processed together; the per-lane loops let LLVM use
/// 128-bit SSE2 (x86-64 baseline) for the butterfly stages without `unsafe` or intrinsics.
/// 8 ≈ 16 in measurements (4 is ~10 % slower); the plane buffer is `E·8·L·8` bytes of stack
/// (2 KiB for u32), and an 8 KiB u32 block has 32 column blocks.
const LANES: usize = 8;

/// All full column blocks: [`LANES`] at a time, the rest one at a time. Returns the number of
/// groups (of 8 elements) done.
#[inline(always)]
fn unshuffle_fast_all<const E: usize>(
    inp: &[u8],
    out: &mut [u8],
    m: usize,
    blocks: usize,
) -> Option<usize> {
    let wide = blocks / LANES * LANES;
    unshuffle_fast::<E, LANES>(inp, out, m, 0, wide)?;
    unshuffle_fast::<E, 1>(inp, out, m, wide, blocks)?;
    Some(blocks * 8)
}

#[inline(always)]
fn swap_v<const L: usize>(a: &mut [u64; L], b: &mut [u64; L], shift: u32, lo_mask: u64) {
    for (x, y) in a.iter_mut().zip(b.iter_mut()) {
        swap_stage(x, y, shift, lo_mask);
    }
}

/// [`trans_bit_lanes`] on `L` independent column blocks.
#[inline(always)]
fn trans_bit_lanes_v<const L: usize>(w: &mut [[u64; L]; 8]) {
    const M4: u64 = 0x0F0F_0F0F_0F0F_0F0F;
    const M2: u64 = 0x3333_3333_3333_3333;
    const M1: u64 = 0x5555_5555_5555_5555;
    let [w0, w1, w2, w3, w4, w5, w6, w7] = w;
    swap_v(w0, w4, 4, M4);
    swap_v(w1, w5, 4, M4);
    swap_v(w2, w6, 4, M4);
    swap_v(w3, w7, 4, M4);
    swap_v(w0, w2, 2, M2);
    swap_v(w1, w3, 2, M2);
    swap_v(w4, w6, 2, M2);
    swap_v(w5, w7, 2, M2);
    swap_v(w0, w1, 1, M1);
    swap_v(w2, w3, 1, M1);
    swap_v(w4, w5, 1, M1);
    swap_v(w6, w7, 1, M1);
}

/// [`trans_byte_8x8`] on `L` independent column blocks.
#[inline(always)]
fn trans_byte_v<const L: usize>(w: &mut [[u64; L]; 8]) {
    const M32: u64 = 0x0000_0000_FFFF_FFFF;
    const M16: u64 = 0x0000_FFFF_0000_FFFF;
    const M8: u64 = 0x00FF_00FF_00FF_00FF;
    let [w0, w1, w2, w3, w4, w5, w6, w7] = w;
    swap_v(w0, w4, 32, M32);
    swap_v(w1, w5, 32, M32);
    swap_v(w2, w6, 32, M32);
    swap_v(w3, w7, 32, M32);
    swap_v(w0, w2, 16, M16);
    swap_v(w1, w3, 16, M16);
    swap_v(w4, w6, 16, M16);
    swap_v(w5, w7, 16, M16);
    swap_v(w0, w1, 8, M8);
    swap_v(w2, w3, 8, M8);
    swap_v(w4, w5, 8, M8);
    swap_v(w6, w7, 8, M8);
}

/// Column blocks `first..last` (`last − first` a multiple of `L`), `L` blocks per step.
///
/// Per byte plane `j`: load the 8 bit-rows `j·8+k` (8 bytes = 64 elements per block), transpose
/// bits inside every byte lane, then transpose bytes: word `g'` now holds plane-`j` bytes of
/// elements `8g' … 8g'+7` in order. The planes are then interleaved into elements with word
/// operations (E = 1, 2, 4) or byte stores (E = 8).
#[inline(always)]
fn unshuffle_fast<const E: usize, const L: usize>(
    inp: &[u8],
    out: &mut [u8],
    m: usize,
    first: usize,
    last: usize,
) -> Option<()> {
    let block_out = 64 * E;
    let region = out.get_mut(first * block_out..last * block_out)?;
    for (step, out_chunk) in region.chunks_exact_mut(block_out * L).enumerate() {
        let col = (first + step * L) * 8;
        // planes[j][g'][l] = plane-j bytes of elements 8g'..8g'+7 of block l (LE word)
        let mut planes = [[[0u64; L]; 8]; E];
        for (j, w) in planes.iter_mut().enumerate() {
            for (k, wk) in w.iter_mut().enumerate() {
                let off = (j * 8 + k) * m + col;
                let row = inp.get(off..off + 8 * L)?;
                for (x, b) in wk.iter_mut().zip(row.chunks_exact(8)) {
                    *x = u64::from_le_bytes(b.try_into().ok()?);
                }
            }
            trans_bit_lanes_v(w);
            trans_byte_v(w);
        }
        match E {
            1 => {
                let p0 = planes.first()?;
                for (l, blk) in out_chunk.chunks_exact_mut(block_out).enumerate() {
                    for (d, wg) in blk.chunks_exact_mut(8).zip(p0.iter()) {
                        d.copy_from_slice(&wg.get(l)?.to_le_bytes());
                    }
                }
            }
            2 => {
                let [p0, p1] = planes.get(..2)? else {
                    return None;
                };
                for (g, (a, b)) in p0.iter().zip(p1.iter()).enumerate() {
                    // lane-parallel compute, then per-lane stores
                    let mut lo = [0u64; L];
                    let mut hi = [0u64; L];
                    for (((x, y), o0), o1) in a.iter().zip(b).zip(&mut lo).zip(&mut hi) {
                        (*o0, *o1) = interleave8(*x, *y);
                    }
                    for (l, blk) in out_chunk.chunks_exact_mut(block_out).enumerate() {
                        let d = blk.get_mut(g * 16..g * 16 + 16)?;
                        store_words(d, &[*lo.get(l)?, *hi.get(l)?]);
                    }
                }
            }
            4 => {
                let [p0, p1, p2, p3] = planes.get(..4)? else {
                    return None;
                };
                for (g, (((a, b), c), e)) in p0.iter().zip(p1).zip(p2).zip(p3).enumerate() {
                    let mut o = [[0u64; L]; 4];
                    let [o0, o1, o2, o3] = &mut o;
                    for l in 0..L {
                        let (Some(&x0), Some(&x1), Some(&x2), Some(&x3)) =
                            (a.get(l), b.get(l), c.get(l), e.get(l))
                        else {
                            return None;
                        };
                        let (ab_lo, ab_hi) = interleave8(x0, x1);
                        let (cd_lo, cd_hi) = interleave8(x2, x3);
                        let (w0, w1) = interleave16(ab_lo, cd_lo);
                        let (w2, w3) = interleave16(ab_hi, cd_hi);
                        let (Some(s0), Some(s1), Some(s2), Some(s3)) =
                            (o0.get_mut(l), o1.get_mut(l), o2.get_mut(l), o3.get_mut(l))
                        else {
                            return None;
                        };
                        (*s0, *s1, *s2, *s3) = (w0, w1, w2, w3);
                    }
                    for (l, blk) in out_chunk.chunks_exact_mut(block_out).enumerate() {
                        let d = blk.get_mut(g * 32..g * 32 + 32)?;
                        store_words(d, &[*o0.get(l)?, *o1.get(l)?, *o2.get(l)?, *o3.get(l)?]);
                    }
                }
            }
            _ => {
                for (l, blk) in out_chunk.chunks_exact_mut(block_out).enumerate() {
                    for (e, elem_bytes) in blk.chunks_exact_mut(E).enumerate() {
                        for (j, b) in elem_bytes.iter_mut().enumerate() {
                            let w = planes.get(j)?.get(e / 8)?.get(l)?;
                            *b = (w >> (8 * (e % 8))) as u8;
                        }
                    }
                }
            }
        }
    }
    Some(())
}

/// Per-group inverse for groups `start..m` (any element size).
pub(crate) fn unshuffle_scalar(
    inp: &[u8],
    out: &mut [u8],
    elem: usize,
    m: usize,
    start: usize,
) -> Result<()> {
    let oob = || internal("scalar kernel bounds");
    for g in start..m {
        for j in 0..elem {
            let mut word = 0u64;
            for k in 0..8usize {
                let idx = (j * 8 + k) * m + g;
                let b = *inp.get(idx).ok_or_else(oob)?;
                word |= u64::from(b) << (8 * k);
            }
            let x = trans_bit_8x8(word);
            for k in 0..8usize {
                let idx = (g * 8 + k) * elem + j;
                *out.get_mut(idx).ok_or_else(oob)? = (x >> (8 * k)) as u8;
            }
        }
    }
    Ok(())
}

/// Forward bitshuffle of one block: `inp` (elements in memory order) → `out` (shuffled).
/// Same shape rules as [`unshuffle_block`].
pub fn shuffle_block(inp: &[u8], out: &mut [u8], elem: usize) -> Result<()> {
    let m = check_args(inp.len(), out.len(), elem)?;
    let oob = || internal("shuffle bounds");
    for g in 0..m {
        for j in 0..elem {
            let mut word = 0u64;
            for k in 0..8usize {
                let idx = (g * 8 + k) * elem + j;
                let b = *inp.get(idx).ok_or_else(oob)?;
                word |= u64::from(b) << (8 * k);
            }
            let x = trans_bit_8x8(word);
            for k in 0..8usize {
                let idx = (j * 8 + k) * m + g;
                *out.get_mut(idx).ok_or_else(oob)? = (x >> (8 * k)) as u8;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-level definition of the shuffled layout (PROTOCOL §9.5), independent of the word tricks.
    fn shuffle_reference(inp: &[u8], elem: usize) -> Vec<u8> {
        let n = inp.len() / elem;
        let m = n / 8;
        let mut out = vec![0u8; inp.len()];
        for i in 0..n {
            for j in 0..elem {
                let byte = inp[i * elem + j];
                for k in 0..8 {
                    let bit = (byte >> k) & 1;
                    let r = j * 8 + k;
                    let g = i / 8;
                    let pos = i % 8;
                    out[r * m + g] |= bit << pos;
                }
            }
        }
        out
    }

    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn trans_bit_is_involution_and_transposes() {
        for seed in 0..64u64 {
            let x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            assert_eq!(trans_bit_8x8(trans_bit_8x8(x)), x);
            let t = trans_bit_8x8(x);
            for r in 0..8 {
                for c in 0..8 {
                    let a = (x >> (8 * r + c)) & 1;
                    let b = (t >> (8 * c + r)) & 1;
                    assert_eq!(a, b);
                }
            }
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn byte_transpose() {
        let mut w = [0u64; 8];
        for (r, wr) in w.iter_mut().enumerate() {
            for c in 0..8 {
                *wr |= ((r * 8 + c) as u64) << (8 * c);
            }
        }
        let orig = w;
        trans_byte_8x8(&mut w);
        for r in 0..8 {
            for c in 0..8 {
                assert_eq!((w[r] >> (8 * c)) & 0xFF, (orig[c] >> (8 * r)) & 0xFF);
            }
        }
    }

    #[test]
    fn forward_matches_bit_definition_and_inverse_roundtrips() {
        for &elem in &[1usize, 2, 3, 4, 5, 8, 12] {
            for &n in &[8usize, 16, 56, 64, 72, 128, 136, 1024, 1032] {
                let inp = pseudo(n * elem, (n * 31 + elem) as u64);
                let want = shuffle_reference(&inp, elem);
                let mut sh = vec![0u8; inp.len()];
                shuffle_block(&inp, &mut sh, elem).unwrap();
                assert_eq!(sh, want, "forward elem={elem} n={n}");
                let mut back = vec![0u8; inp.len()];
                unshuffle_block(&sh, &mut back, elem).unwrap();
                assert_eq!(back, inp, "inverse elem={elem} n={n}");
            }
        }
    }

    #[test]
    fn shape_errors_are_typed() {
        let mut out = [0u8; 8];
        assert!(unshuffle_block(&[0u8; 8], &mut out, 0).is_err());
        assert!(unshuffle_block(&[0u8; 7], &mut out[..7], 1).is_err());
        assert!(unshuffle_block(&[0u8; 8], &mut out[..4], 1).is_err());
        assert!(unshuffle_block(&[0u8; 8], &mut out, 2).is_err());
        assert!(unshuffle_block(&[], &mut [], 4).is_ok());
    }
}
