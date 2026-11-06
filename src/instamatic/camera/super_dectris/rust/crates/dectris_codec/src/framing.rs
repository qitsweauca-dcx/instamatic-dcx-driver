//! bslz4 / lz4 HDF5-filter framing, bounds-checked (docs/DESIGN.md §4.5 [R15][R17][R18]
//! [G2-E][G2-F]; byte layout PROTOCOL §9.1–§9.4).
//!
//! Container: 12-byte header (`u64 BE orig_size`, `u32 BE block_size` in bytes), then blocks of
//! `u32 BE compressed_size` + one raw LZ4 block.
//!
//! | check | bslz4 | lz4 |
//! |---|---|---|
//! | element size | `1 ≤ elem ≤ 0x1FFF_FFFF`, checked before the header (also when `orig_size = 0`) | ignored |
//! | `orig_size` | ≤ [`MAX_DECODED_BYTES`] (allocation guard) | same |
//! | `block_size` | `≠ 0` if `orig ≠ 0`, `≤ i32::MAX`, `% (8·elem) == 0` | `≠ 0` if `orig ≠ 0`, `≤ i32::MAX` |
//! | blocks | `orig / block` full, then `(orig % block) − leftover`, `leftover = orig % (8·elem)` raw | `orig / block` full, then `orig % block` |
//! | per block | `compressed_size ≤ i32::MAX` and ≤ remaining; LZ4 yields exactly the block | raw copy if `compressed_size ==` that block's size, else LZ4 to exactly that size |
//! | trailing bytes | rejected (stricter than the vendor C code, deliberately) | rejected |
//!
//! Every error is `DriverError::Decode { offset, .. }` with `offset` relative to the start of the
//! compressed stream; callers that know the stream's position in a message rebase it
//! ([`rebase`]).

use dectris_common::{DriverError, Result};

use crate::bitshuffle;

/// Upper bound for any decoded payload (DESIGN §2.8, §4.5): 256 MiB.
pub const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// Largest bslz4 element size (`UINT32_MAX / 8`, vendor `compression.c:92`) [G2-E].
pub const MAX_BSLZ4_ELEM: u64 = 0x1FFF_FFFF;

/// `i32::MAX` as the vendor bound for `block_size` and per-block `compressed_size`.
const INT_MAX: u64 = i32::MAX as u64;

/// Container header length.
pub const HEADER_LEN: usize = 12;

/// A DECTRIS tag-56500 compression algorithm. The wire name is compared byte-exactly (PROTOCOL §9.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// `"bslz4"`: bitshuffle + LZ4, HDF5 bitshuffle-filter framing.
    Bslz4,
    /// `"lz4"`: LZ4 blocks, HDF5 LZ4-filter framing.
    Lz4,
}

impl Algorithm {
    /// Parse the wire name (`"bslz4"` / `"lz4"`, byte-exact; `"LZ4"`, `"lz4 "` … are rejected).
    pub fn from_wire(name: &[u8]) -> Result<Self> {
        match name {
            b"bslz4" => Ok(Algorithm::Bslz4),
            b"lz4" => Ok(Algorithm::Lz4),
            other => Err(DriverError::decode(
                0,
                format!(
                    "unsupported compression algorithm {:?}",
                    String::from_utf8_lossy(other)
                ),
            )),
        }
    }

    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            Algorithm::Bslz4 => "bslz4",
            Algorithm::Lz4 => "lz4",
        }
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The 12-byte container header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Total decompressed size in bytes.
    pub orig_size: u64,
    /// Block size in bytes.
    pub block_size: u32,
}

/// Reusable per-iterator scratch: one LZ4 output block for bslz4, plus a full-frame buffer for
/// the tag-1040 (F→C) image path.
#[derive(Debug, Default)]
pub struct Scratch {
    block: Vec<u8>,
    full: Vec<u8>,
}

impl Scratch {
    /// Empty scratch; grows on first use to `min(block_size, orig_size)`.
    pub const fn new() -> Self {
        Scratch {
            block: Vec::new(),
            full: Vec::new(),
        }
    }

    /// Take the full-frame buffer, resized to `n` zero bytes (give it back with
    /// [`put_full`](Self::put_full)).
    pub fn take_full(&mut self, n: usize) -> Vec<u8> {
        let mut v = std::mem::take(&mut self.full);
        v.clear();
        v.resize(n, 0);
        v
    }

    /// Return the buffer taken by [`take_full`](Self::take_full).
    pub fn put_full(&mut self, v: Vec<u8>) {
        self.full = v;
    }

    fn block(&mut self, len: usize) -> &mut [u8] {
        if self.block.len() < len {
            self.block.resize(len, 0);
        }
        // `len <= self.block.len()` after the resize
        self.block.get_mut(..len).unwrap_or_default()
    }
}

/// Shift a `Decode` error's offset by `base` (the stream's position inside a message).
pub fn rebase(err: DriverError, base: usize) -> DriverError {
    match err {
        DriverError::Decode { offset, msg } => DriverError::Decode {
            offset: offset.saturating_add(base),
            msg,
        },
        other => other,
    }
}

fn err(offset: usize, msg: impl Into<String>) -> DriverError {
    DriverError::decode(offset, msg)
}

fn read_u32_be(data: &[u8], at: usize) -> Option<u32> {
    let s = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(s.try_into().ok()?))
}

fn read_u64_be(data: &[u8], at: usize) -> Option<u64> {
    let s = data.get(at..at.checked_add(8)?)?;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

/// Parse the 12-byte header (PROTOCOL §9.1). Shorter input is an error.
pub fn parse_header(data: &[u8]) -> Result<FrameHeader> {
    let orig_size = read_u64_be(data, 0)
        .ok_or_else(|| err(0, "compressed stream shorter than the 12-byte header"))?;
    let block_size = read_u32_be(data, 8)
        .ok_or_else(|| err(0, "compressed stream shorter than the 12-byte header"))?;
    Ok(FrameHeader {
        orig_size,
        block_size,
    })
}

/// bslz4 element-size rule, applied before the header is read [G2-E].
fn check_bslz4_elem(elem: u64) -> Result<usize> {
    if elem == 0 || elem > MAX_BSLZ4_ELEM {
        return Err(err(
            0,
            format!("bslz4 element size {elem} out of range 1..={MAX_BSLZ4_ELEM}"),
        ));
    }
    usize::try_from(elem).map_err(|_| err(0, "bslz4 element size does not fit usize"))
}

/// Validated layout of one stream.
#[derive(Debug, Clone, Copy)]
struct Layout {
    orig: usize,
    block: usize,
    full_blocks: usize,
    last_block: usize,
    leftover: usize,
    elem: usize,
}

fn layout(alg: Algorithm, elem: u64, data: &[u8]) -> Result<Layout> {
    let elem = match alg {
        Algorithm::Bslz4 => check_bslz4_elem(elem)?,
        Algorithm::Lz4 => 1, // modifier ignored (PROTOCOL §9.4) [R17]
    };
    let h = parse_header(data)?;
    if h.orig_size > MAX_DECODED_BYTES as u64 {
        return Err(err(
            0,
            format!(
                "orig_size {} exceeds the {} MiB limit",
                h.orig_size,
                MAX_DECODED_BYTES >> 20
            ),
        ));
    }
    let orig = h.orig_size as usize; // ≤ 256 MiB
    if orig != 0 && h.block_size == 0 {
        return Err(err(8, "block_size 0 with non-zero orig_size"));
    }
    if u64::from(h.block_size) > INT_MAX {
        return Err(err(
            8,
            format!("block_size {} exceeds i32::MAX", h.block_size),
        ));
    }
    let block = h.block_size as usize;
    let (full_blocks, last_block, leftover) = if orig == 0 {
        (0, 0, 0)
    } else {
        match alg {
            Algorithm::Bslz4 => {
                // elem ≤ 0x1FFF_FFFF so 8·elem ≤ u32::MAX: no overflow on 64-bit usize
                let unit = elem
                    .checked_mul(8)
                    .ok_or_else(|| err(0, "bslz4 element size overflow"))?;
                if block % unit != 0 {
                    return Err(err(
                        8,
                        format!(
                            "bslz4 block_size {block} is not a multiple of 8·elem_size ({unit})"
                        ),
                    ));
                }
                let leftover = orig % unit;
                // block is a multiple of unit, so (orig % block) % unit == leftover
                (orig / block, (orig % block) - leftover, leftover)
            }
            Algorithm::Lz4 => (orig / block, orig % block, 0),
        }
    };
    Ok(Layout {
        orig,
        block,
        full_blocks,
        last_block,
        leftover,
        elem,
    })
}

/// Decoded size of a stream after full header validation (no decompression). Use it to size the
/// destination before calling [`decompress_into`].
pub fn decoded_len(alg: Algorithm, elem: u64, data: &[u8]) -> Result<usize> {
    Ok(layout(alg, elem, data)?.orig)
}

/// Read one block header at `*pos`; returns the payload slice and advances `*pos` past it.
fn next_block<'d>(data: &'d [u8], pos: &mut usize) -> Result<&'d [u8]> {
    let at = *pos;
    let csize = read_u32_be(data, at).ok_or_else(|| err(at, "truncated block header"))?;
    if u64::from(csize) > INT_MAX {
        return Err(err(
            at,
            format!("block compressed_size {csize} exceeds i32::MAX"),
        ));
    }
    let start = at + 4;
    let end = start
        .checked_add(csize as usize)
        .ok_or_else(|| err(at, "block length overflow"))?;
    let payload = data.get(start..end).ok_or_else(|| {
        err(
            at,
            format!(
                "block compressed_size {csize} exceeds the {} remaining bytes",
                data.len().saturating_sub(start)
            ),
        )
    })?;
    *pos = end;
    Ok(payload)
}

/// One LZ4 block to exactly `dst.len()` bytes. The single swap point between this crate's
/// decoder ([`crate::lz4`], used for speed; DESIGN §4.5, §12 #20) and `lz4_flex::block::decompress_into`
/// (DESIGN §4.5's reference, still the encoder and the differential-test reference).
fn lz4_exact(src: &[u8], dst: &mut [u8], at: usize) -> Result<()> {
    match crate::lz4::decompress_into(src, dst) {
        Ok(n) if n == dst.len() => Ok(()),
        Ok(n) => Err(err(
            at,
            format!("LZ4 block decoded to {n} bytes, expected {}", dst.len()),
        )),
        Err(e) => Err(err(at, format!("LZ4 block corrupt: {e}"))),
    }
}

/// Decompress `data` into `out`, which must be exactly the decoded length
/// ([`decoded_len`]). `elem` is the 56500 modifier (wire value): the bitshuffle element size for
/// bslz4, ignored for lz4.
pub fn decompress_into(
    alg: Algorithm,
    elem: u64,
    data: &[u8],
    out: &mut [u8],
    scratch: &mut Scratch,
) -> Result<()> {
    let l = layout(alg, elem, data)?;
    if out.len() != l.orig {
        return Err(err(
            0,
            format!(
                "destination is {} bytes, stream decodes to {}",
                out.len(),
                l.orig
            ),
        ));
    }
    let mut pos = HEADER_LEN;
    let mut dst = 0usize;
    match alg {
        Algorithm::Bslz4 => {
            let tmp = scratch.block(l.block.min(l.orig));
            let mut one = |size: usize, pos: &mut usize, dst: &mut usize| -> Result<()> {
                let at = *pos;
                let payload = next_block(data, pos)?;
                let t = tmp
                    .get_mut(..size)
                    .ok_or_else(|| err(at, "scratch too small"))?;
                lz4_exact(payload, t, at)?;
                let o = out
                    .get_mut(*dst..*dst + size)
                    .ok_or_else(|| err(at, "block overruns the destination"))?;
                bitshuffle::unshuffle_block(t, o, l.elem).map_err(|e| match e {
                    DriverError::Internal(m) => err(at, m),
                    other => other,
                })?;
                *dst += size;
                Ok(())
            };
            for _ in 0..l.full_blocks {
                one(l.block, &mut pos, &mut dst)?;
            }
            if l.last_block > 0 {
                one(l.last_block, &mut pos, &mut dst)?;
            }
            if l.leftover > 0 {
                let rest = data.get(pos..).unwrap_or_default();
                if rest.len() != l.leftover {
                    return Err(err(
                        pos,
                        format!(
                            "bslz4 leftover is {} bytes but {} input bytes remain",
                            l.leftover,
                            rest.len()
                        ),
                    ));
                }
                let o = out
                    .get_mut(dst..dst + l.leftover)
                    .ok_or_else(|| err(pos, "leftover overruns the destination"))?;
                o.copy_from_slice(rest);
                pos += l.leftover;
                dst += l.leftover;
            }
        }
        Algorithm::Lz4 => {
            let mut one = |size: usize, pos: &mut usize, dst: &mut usize| -> Result<()> {
                let at = *pos;
                let payload = next_block(data, pos)?;
                let o = out
                    .get_mut(*dst..*dst + size)
                    .ok_or_else(|| err(at, "block overruns the destination"))?;
                if payload.len() == size {
                    o.copy_from_slice(payload); // stored raw [compression.c:161]
                } else {
                    lz4_exact(payload, o, at)?;
                }
                *dst += size;
                Ok(())
            };
            for _ in 0..l.full_blocks {
                one(l.block, &mut pos, &mut dst)?;
            }
            if l.last_block > 0 {
                one(l.last_block, &mut pos, &mut dst)?;
            }
        }
    }
    if pos != data.len() {
        return Err(err(
            pos,
            format!("trailing bytes: {} after the last block", data.len() - pos),
        ));
    }
    if dst != l.orig {
        return Err(err(
            pos,
            format!("decoded {dst} bytes, expected {}", l.orig),
        ));
    }
    Ok(())
}

/// Decompress into a fresh `Vec`.
pub fn decompress(alg: Algorithm, elem: u64, data: &[u8]) -> Result<Vec<u8>> {
    let n = decoded_len(alg, elem, data)?;
    let mut out = vec![0u8; n];
    decompress_into(alg, elem, data, &mut out, &mut Scratch::new())?;
    Ok(out)
}

/// The Python `decompress(algorithm, elem_size, data)` oracle hook (DESIGN §2.1): the algorithm name
/// is compared byte-exactly; `elem_size` is the bitshuffle element size (ignored for lz4).
pub fn decompress_named(algorithm: &[u8], elem_size: u64, data: &[u8]) -> Result<Vec<u8>> {
    decompress(Algorithm::from_wire(algorithm)?, elem_size, data)
}
