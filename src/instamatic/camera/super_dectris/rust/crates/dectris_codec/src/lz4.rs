//! LZ4 raw-block decoder, safe Rust, bounds-checked (LZ4 Block Format, lz4_Block_format.md).
//!
//! Why not only `lz4_flex::block::decompress_into` (DESIGN §4.5): with `safe-decode`, lz4_flex copies
//! overlapping matches one byte at a time (`SliceSink::extend_from_within_overlapping`). Bitshuffled
//! detector frames are mostly long zero runs, i.e. overlapping matches with offset 1–8, so that
//! loop dominated the frame budget (≈ 1.6 ms of LZ4 for a 1028×1062 u32 frame). This decoder copies
//! overlapping matches by doubling `copy_within` chunks (the match source is periodic with the
//! offset), and non-overlapping ones with one `copy_within`.
//!
//! It never uses `unsafe`. Every read/write is bounds-checked; any malformed input is an error,
//! never a panic. The result is differentially tested against `lz4_flex` (valid and corrupted
//! streams) and the Python oracle (`tests/`). The caller (framing) still requires the output to be
//! exactly the expected block size.

/// Why a block failed to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lz4Error {
    /// Input ended inside a token, length extension, literal run or offset.
    Truncated,
    /// A literal run or match would write past the end of the output.
    OutputOverrun,
    /// Match offset before the start of the output.
    BadOffset,
    /// Match offset 0 ("invalid (corrupted) block" per the LZ4 block format). The vendor
    /// liblz4 in dectris-compression 0.3.1 accepts it and copies uninitialised scratch bytes;
    /// lz4_flex and this decoder reject it.
    ZeroOffset,
}

impl std::fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Lz4Error::Truncated => "input truncated",
            Lz4Error::OutputOverrun => "output overrun",
            Lz4Error::BadOffset => "invalid match offset (before the start of the block)",
            Lz4Error::ZeroOffset => "invalid match offset 0",
        })
    }
}

#[inline(always)]
fn read_len(input: &[u8], ip: &mut usize, mut len: usize) -> Result<usize, Lz4Error> {
    loop {
        let b = *input.get(*ip).ok_or(Lz4Error::Truncated)?;
        *ip += 1;
        len = len
            .checked_add(usize::from(b))
            .ok_or(Lz4Error::OutputOverrun)?;
        if b != 255 {
            return Ok(len);
        }
    }
}

#[inline(always)]
fn copy16(input: &[u8], ip: usize, out: &mut [u8], op: usize) -> Result<(), Lz4Error> {
    let src: &[u8; 16] = input
        .get(ip..ip + 16)
        .and_then(|s| s.try_into().ok())
        .ok_or(Lz4Error::Truncated)?;
    let dst: &mut [u8; 16] = out
        .get_mut(op..op + 16)
        .and_then(|s| s.try_into().ok())
        .ok_or(Lz4Error::OutputOverrun)?;
    *dst = *src;
    Ok(())
}

#[inline(always)]
fn copy8_within(out: &mut [u8], src: usize, dst: usize) -> Result<(), Lz4Error> {
    let t: [u8; 8] = out
        .get(src..src + 8)
        .and_then(|s| s.try_into().ok())
        .ok_or(Lz4Error::BadOffset)?;
    let d: &mut [u8; 8] = out
        .get_mut(dst..dst + 8)
        .and_then(|s| s.try_into().ok())
        .ok_or(Lz4Error::OutputOverrun)?;
    *d = t;
    Ok(())
}

/// Decode one LZ4 raw block into `out`; returns the number of bytes written.
pub fn decompress_into(input: &[u8], out: &mut [u8]) -> Result<usize, Lz4Error> {
    let mut ip = 0usize;
    let mut op = 0usize;
    loop {
        let token = *input.get(ip).ok_or(Lz4Error::Truncated)?;
        ip += 1;
        // literals
        let mut lit = usize::from(token >> 4);
        if lit == 15 {
            lit = read_len(input, &mut ip, lit)?;
        }
        if lit <= 16 && ip + 16 <= input.len() && op + 16 <= out.len() {
            // short literal run: one fixed 16-byte copy; bytes past `op + lit` are scratch that
            // later sequences overwrite before anything reads them
            copy16(input, ip, out, op)?;
            ip += lit;
            op += lit;
        } else if lit > 0 {
            let src_end = ip.checked_add(lit).ok_or(Lz4Error::Truncated)?;
            let src = input.get(ip..src_end).ok_or(Lz4Error::Truncated)?;
            let dst_end = op.checked_add(lit).ok_or(Lz4Error::OutputOverrun)?;
            let dst = out.get_mut(op..dst_end).ok_or(Lz4Error::OutputOverrun)?;
            dst.copy_from_slice(src);
            ip = src_end;
            op = dst_end;
        }
        if ip == input.len() {
            return Ok(op); // the last sequence has literals only
        }
        // match
        let o = input.get(ip..ip + 2).ok_or(Lz4Error::Truncated)?;
        let offset = usize::from(u16::from_le_bytes([
            *o.first().ok_or(Lz4Error::Truncated)?,
            *o.get(1).ok_or(Lz4Error::Truncated)?,
        ]));
        ip += 2;
        if offset == 0 {
            return Err(Lz4Error::ZeroOffset);
        }
        if offset > op {
            return Err(Lz4Error::BadOffset);
        }
        let mut mlen = usize::from(token & 15);
        if mlen == 15 {
            mlen = read_len(input, &mut ip, mlen)?;
        }
        mlen = mlen.checked_add(4).ok_or(Lz4Error::OutputOverrun)?;
        let end = op.checked_add(mlen).ok_or(Lz4Error::OutputOverrun)?;
        if end > out.len() {
            return Err(Lz4Error::OutputOverrun);
        }
        let src = op - offset;
        if mlen <= 16 && offset >= 8 && op + 16 <= out.len() {
            // short match, offset ≥ 8: two 8-byte steps (each step's source ends at or before its
            // destination, so sequential semantics hold); bytes past `end` are scratch
            copy8_within(out, src, op)?;
            copy8_within(out, src + 8, op + 8)?;
        } else if offset >= mlen {
            out.copy_within(src..src + mlen, op);
        } else if mlen <= 32 {
            // short overlapping match: byte loop (no call overhead)
            for i in op..end {
                let b = *out.get(i - offset).ok_or(Lz4Error::BadOffset)?;
                *out.get_mut(i).ok_or(Lz4Error::OutputOverrun)? = b;
            }
        } else if offset == 1 {
            let b = *out.get(src).ok_or(Lz4Error::BadOffset)?;
            out.get_mut(op..end).ok_or(Lz4Error::OutputOverrun)?.fill(b);
        } else {
            // out[src..op] is one period of the pattern; [src, op + done) stays periodic, so each
            // step can copy `offset + done` bytes from `src` without overlapping its destination.
            let mut done = 0usize;
            while done < mlen {
                let n = (offset + done).min(mlen - done);
                out.copy_within(src..src + n, op + done);
                done += n;
            }
        }
        op = end;
    }
}
