//! Monitor TIFF decode (docs/DESIGN.md §4.1, §3.3, §3.5; PROTOCOL §11).
//!
//! Spike (f) (the `tiff` crate on a captured DCU monitor TIFF) has not run yet, so decoding sits
//! behind the [`MonitorDecoder`] trait: [`TiffCrateDecoder`] is the default (the `tiff` crate:
//! uncompressed, LZW and deflate), and another decoder can be swapped in through
//! `DetectorBuilder::decoder`. A TIFF the decoder rejects becomes `DriverError::Decode`, and the
//! preview shows zeros with one WARNING.
//!
//! Tag 51192 (DECTRIS private tag) is read in both shapes: as text (`key=value` lines, an older
//! form) or as an offset (LONG / IFD, LONG8 / IFD8 in BigTIFF) to the DECTRIS private IFD
//! of the SIMPLON API reference ("TIFF Header Data", codes 0x0000 `IfdVersion` … 0x0017
//! `DetectorDistance`), which SIMPLON monitor TIFFs carry. Both become `(name, value)` strings
//! (DESIGN §12 #21). The private IFD is parsed from the raw bytes, best-effort and bounded
//! ([`read_dectris_ifd`]).
//!
//! Note: this module shadows the `tiff` crate inside `dectris_simplon`; the crate is `::tiff`.

use std::io::Cursor;

use ::tiff::decoder::ifd::Value;
use ::tiff::decoder::{Decoder, DecodingResult, Limits};
use ::tiff::tags::Tag;
use ::tiff::ColorType;
use dectris_common::{DriverError, Result};

/// Largest decoded image accepted (DESIGN §2.8 size cap).
pub const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;
/// The DECTRIS private TIFF tag (PROTOCOL §11).
pub const DECTRIS_TAG: u16 = 51192;

/// Pixel buffer of a monitor image (row-major).
#[derive(Debug, Clone, PartialEq)]
pub enum Pixels {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    F32(Vec<f32>),
}

impl Pixels {
    pub fn len(&self) -> usize {
        match self {
            Pixels::U8(v) => v.len(),
            Pixels::U16(v) => v.len(),
            Pixels::U32(v) => v.len(),
            Pixels::F32(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// numpy dtype string of the buffer.
    pub fn dtype(&self) -> &'static str {
        match self {
            Pixels::U8(_) => "uint8",
            Pixels::U16(_) => "uint16",
            Pixels::U32(_) => "uint32",
            Pixels::F32(_) => "float32",
        }
    }

    /// Little-endian bytes (tests, py conversion helpers).
    pub fn to_le_bytes(&self) -> Vec<u8> {
        match self {
            Pixels::U8(v) => v.clone(),
            Pixels::U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Pixels::U32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Pixels::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }
}

/// A decoded (not yet oriented) monitor image.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedImage {
    /// Rows.
    pub height: usize,
    /// Columns.
    pub width: usize,
    pub pixels: Pixels,
    /// Tag 51192 as `(name, value)` strings (`_last_monitor_metadata`), best-effort: the `key=value`
    /// lines of a text tag, or the entries of the DECTRIS private IFD the tag points to.
    pub metadata: Vec<(String, String)>,
}

/// Swappable monitor TIFF decoder.
pub trait MonitorDecoder: Send + Sync + std::fmt::Debug {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage>;
}

/// The default decoder: the `tiff` crate (uncompressed grayscale 8/16/32-bit int, 32-bit float).
#[derive(Debug, Default, Clone, Copy)]
pub struct TiffCrateDecoder;

fn derr(msg: impl std::fmt::Display) -> DriverError {
    DriverError::decode(0, format!("monitor TIFF: {msg}"))
}

impl MonitorDecoder for TiffCrateDecoder {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage> {
        let mut limits = Limits::default();
        limits.decoding_buffer_size = MAX_IMAGE_BYTES;
        limits.intermediate_buffer_size = MAX_IMAGE_BYTES;
        let mut dec = Decoder::new(Cursor::new(bytes))
            .map_err(derr)?
            .with_limits(limits);
        let (w, h) = dec.dimensions().map_err(derr)?;
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 {
            return Err(derr("zero image dimension"));
        }
        match dec.colortype().map_err(derr)? {
            ColorType::Gray(8 | 16 | 32) => {}
            other => return Err(derr(format!("unsupported color type {other:?}"))),
        }
        let metadata = read_metadata(&mut dec, bytes);
        let pixels = match dec.read_image().map_err(derr)? {
            DecodingResult::U8(v) => Pixels::U8(v),
            DecodingResult::U16(v) => Pixels::U16(v),
            DecodingResult::U32(v) => Pixels::U32(v),
            DecodingResult::F32(v) => Pixels::F32(v),
            _ => return Err(derr("unsupported sample format")),
        };
        let n = w
            .checked_mul(h)
            .ok_or_else(|| derr("image dimensions overflow"))?;
        if pixels.len() != n {
            return Err(derr(format!(
                "pixel count {} does not match {w}x{h}",
                pixels.len()
            )));
        }
        Ok(DecodedImage {
            height: h,
            width: w,
            pixels,
            metadata,
        })
    }
}

fn read_metadata<R: std::io::Read + std::io::Seek>(
    dec: &mut Decoder<R>,
    bytes: &[u8],
) -> Vec<(String, String)> {
    let value = match dec.find_tag(Tag::Unknown(DECTRIS_TAG)) {
        Ok(Some(v)) => v,
        _ => return Vec::new(),
    };
    if let Some(offset) = ifd_offset(&value) {
        return read_dectris_ifd(bytes, offset);
    }
    match value.into_string() {
        Ok(text) => parse_key_values(&text),
        Err(_) => Vec::new(),
    }
}

/// The offset carried by a LONG / IFD (LONG8 / IFD8) tag 51192.
fn ifd_offset(value: &Value) -> Option<u64> {
    match value {
        Value::Unsigned(o) | Value::Ifd(o) => Some(u64::from(*o)),
        Value::UnsignedBig(o) | Value::IfdBig(o) => Some(*o),
        Value::List(items) if items.len() == 1 => items.first().and_then(ifd_offset),
        _ => None,
    }
}

/// Most entries accepted in the DECTRIS private IFD (the vendor table has 13); more → no metadata.
pub const MAX_IFD_ENTRIES: u64 = 256;
/// Largest value of one private-IFD entry, in bytes; a larger entry is skipped.
pub const MAX_IFD_VALUE_BYTES: usize = 4096;
/// Total value bytes read from the private IFD; reading stops before the entry that exceeds it.
pub const MAX_IFD_TOTAL_BYTES: usize = 64 * 1024;

/// DECTRIS private IFD codes (SIMPLON API reference, "TIFF Header Data"). Other codes are named `Tag<decimal code>`.
const DECTRIS_IFD_NAMES: [(u16, &str); 13] = [
    (0x0000, "IfdVersion"),
    (0x0001, "SeriesUniqueId"),
    (0x0002, "SeriesNumber"),
    (0x0003, "ImageNumber"),
    (0x0004, "ImageDateTime"),
    (0x0005, "ThresholdId"),
    (0x0006, "ThresholdEnergy"),
    (0x0007, "ExposureTime"),
    (0x0009, "IncidentEnergy"),
    (0x000A, "IncidentWavelength"),
    (0x0012, "LostPixelCount"),
    (0x0016, "BeamCenter"),
    (0x0017, "DetectorDistance"),
];

fn ifd_entry_name(code: u16) -> String {
    DECTRIS_IFD_NAMES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or_else(|| format!("Tag{code}"), |(_, n)| (*n).to_owned())
}

/// Byte order and flavour (classic / BigTIFF) of a TIFF file.
#[derive(Debug, Clone, Copy)]
struct Layout {
    big_endian: bool,
    big_tiff: bool,
}

fn take<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at.checked_add(N)?)?.try_into().ok()
}

impl Layout {
    fn from_header(b: &[u8]) -> Option<Layout> {
        let big_endian = match b.get(..2)? {
            b"II" => false,
            b"MM" => true,
            _ => return None,
        };
        let l = Layout {
            big_endian,
            big_tiff: false,
        };
        match l.u16(b, 2)? {
            42 => Some(l),
            43 => Some(Layout {
                big_tiff: true,
                ..l
            }),
            _ => None,
        }
    }

    /// File-order bytes → little-endian order.
    fn le<const N: usize>(self, mut a: [u8; N]) -> [u8; N] {
        if self.big_endian {
            a.reverse();
        }
        a
    }

    fn u16(self, b: &[u8], at: usize) -> Option<u16> {
        take(b, at).map(|a| u16::from_le_bytes(self.le(a)))
    }

    fn u32(self, b: &[u8], at: usize) -> Option<u32> {
        take(b, at).map(|a| u32::from_le_bytes(self.le(a)))
    }

    fn u64(self, b: &[u8], at: usize) -> Option<u64> {
        take(b, at).map(|a| u64::from_le_bytes(self.le(a)))
    }

    /// A count or offset field (4 bytes classic, 8 bytes BigTIFF).
    fn word(self, b: &[u8], at: usize) -> Option<u64> {
        if self.big_tiff {
            self.u64(b, at)
        } else {
            self.u32(b, at).map(u64::from)
        }
    }

    /// The elements of a value, each converted to little-endian order.
    fn elems<const N: usize>(self, v: &[u8]) -> impl Iterator<Item = [u8; N]> + '_ {
        v.chunks_exact(N)
            .filter_map(move |c| c.try_into().ok().map(|a| self.le(a)))
    }
}

/// Element size of a TIFF field type; `None` for an unknown type.
fn type_size(typ: u16) -> Option<usize> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),              // BYTE ASCII SBYTE UNDEFINED
        3 | 8 => Some(2),                      // SHORT SSHORT
        4 | 9 | 11 | 13 => Some(4),            // LONG SLONG FLOAT IFD
        5 | 10 | 12 | 16 | 17 | 18 => Some(8), // RATIONAL SRATIONAL DOUBLE LONG8 SLONG8 IFD8
        _ => None,
    }
}

fn join<T: ToString>(items: impl Iterator<Item = T>) -> String {
    items.map(|x| x.to_string()).collect::<Vec<_>>().join(",")
}

fn ratio(n: f64, d: f64, raw: impl FnOnce() -> String) -> String {
    if d == 0.0 {
        raw()
    } else {
        (n / d).to_string()
    }
}

/// One entry's value as a string: text NUL-split (empty pieces dropped) and joined with `,`;
/// numbers in Rust `Display` form (`8041.0_f64` → `8041`), arrays joined with `,`; RATIONAL as the
/// decimal quotient (`n/d` when `d == 0`). IFD / IFD8 entries give their offset (no recursion).
fn format_value(l: Layout, typ: u16, v: &[u8]) -> Option<String> {
    Some(match typ {
        2 | 7 => v
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(","),
        1 => join(v.iter()),
        6 => join(v.iter().map(|&x| i8::from_le_bytes([x]))),
        3 => join(l.elems(v).map(u16::from_le_bytes)),
        8 => join(l.elems(v).map(i16::from_le_bytes)),
        4 | 13 => join(l.elems(v).map(u32::from_le_bytes)),
        9 => join(l.elems(v).map(i32::from_le_bytes)),
        16 | 18 => join(l.elems(v).map(u64::from_le_bytes)),
        17 => join(l.elems(v).map(i64::from_le_bytes)),
        11 => join(l.elems(v).map(f32::from_le_bytes)),
        12 => join(l.elems(v).map(f64::from_le_bytes)),
        5 => join(
            l.elems::<4>(v)
                .collect::<Vec<_>>()
                .chunks_exact(2)
                .map(|p| {
                    let (n, d) = (u32::from_le_bytes(p[0]), u32::from_le_bytes(p[1]));
                    ratio(f64::from(n), f64::from(d), || format!("{n}/{d}"))
                }),
        ),
        10 => join(
            l.elems::<4>(v)
                .collect::<Vec<_>>()
                .chunks_exact(2)
                .map(|p| {
                    let (n, d) = (i32::from_le_bytes(p[0]), i32::from_le_bytes(p[1]));
                    ratio(f64::from(n), f64::from(d), || format!("{n}/{d}"))
                }),
        ),
        _ => return None,
    })
}

/// Read the DECTRIS private IFD at `offset` of the TIFF `bytes` (the value of a LONG / IFD tag 51192)
/// as `(name, value)` strings, in IFD order. Best-effort, never panics: a bad header, an offset or
/// entry table outside the file, or more than [`MAX_IFD_ENTRIES`] entries → empty; an entry with an
/// unknown type, a value outside the file or larger than [`MAX_IFD_VALUE_BYTES`] is skipped; reading
/// stops at [`MAX_IFD_TOTAL_BYTES`]. Nested IFDs are not followed.
pub fn read_dectris_ifd(bytes: &[u8], offset: u64) -> Vec<(String, String)> {
    read_ifd_entries(bytes, offset).unwrap_or_default()
}

fn read_ifd_entries(bytes: &[u8], offset: u64) -> Option<Vec<(String, String)>> {
    let l = Layout::from_header(bytes)?;
    let start = usize::try_from(offset).ok()?;
    let (count, first, entry_len, inline) = if l.big_tiff {
        (l.u64(bytes, start)?, start.checked_add(8)?, 20, 8)
    } else {
        (
            u64::from(l.u16(bytes, start)?),
            start.checked_add(2)?,
            12,
            4,
        )
    };
    if start < 8 || count > MAX_IFD_ENTRIES {
        return None;
    }
    let count = usize::try_from(count).ok()?;
    let table_end = first.checked_add(count.checked_mul(entry_len)?)?;
    bytes.get(first..table_end)?;
    let mut out = Vec::new();
    let mut total = 0usize;
    for e in (first..table_end).step_by(entry_len) {
        let (Some(code), Some(typ), Some(n)) =
            (l.u16(bytes, e), l.u16(bytes, e + 2), l.word(bytes, e + 4))
        else {
            break;
        };
        let field = e + 4 + inline;
        let Some(len) = type_size(typ)
            .zip(usize::try_from(n).ok())
            .and_then(|(size, n)| size.checked_mul(n))
            .filter(|&len| len <= MAX_IFD_VALUE_BYTES)
        else {
            continue;
        };
        let value = if len <= inline {
            bytes.get(field..field + len)
        } else {
            l.word(bytes, field)
                .and_then(|o| usize::try_from(o).ok())
                .and_then(|o| bytes.get(o..o.checked_add(len)?))
        };
        let Some(value) = value else { continue };
        total += len;
        if total > MAX_IFD_TOTAL_BYTES {
            break;
        }
        if let Some(text) = format_value(l, typ, value) {
            out.push((ifd_entry_name(code), text));
        }
    }
    Some(out)
}

/// Parse `key=value` lines (PROTOCOL §11: today's driver keeps them as `_last_monitor_metadata`).
pub fn parse_key_values(text: &str) -> Vec<(String, String)> {
    text.split(['\n', '\r', '\0'])
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_owned(), v.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_values() {
        let kv = parse_key_values("A=1\nB = x y \n\nnoeq\n=v\0");
        assert_eq!(
            kv,
            vec![("A".into(), "1".into()), ("B".into(), "x y".into())]
        );
    }

    #[test]
    fn garbage_is_decode_error() {
        let e = TiffCrateDecoder.decode(b"not a tiff").unwrap_err();
        assert!(matches!(e, DriverError::Decode { .. }));
        let e = TiffCrateDecoder.decode(&[]).unwrap_err();
        assert!(matches!(e, DriverError::Decode { .. }));
    }

    /// A classic little-endian TIFF header (IFD0 offset unused) followed by a private IFD at 8 with
    /// `entries` = (code, type, count, 4-byte value field), then `tail` bytes (at 8 + 2 + 12·n + 4).
    fn private_ifd_le(declared: u16, entries: &[(u16, u16, u32, [u8; 4])], tail: &[u8]) -> Vec<u8> {
        let mut b = b"II".to_vec();
        b.extend(42u16.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(declared.to_le_bytes());
        for &(code, typ, n, field) in entries {
            b.extend(code.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(n.to_le_bytes());
            b.extend(field);
        }
        b.extend(0u32.to_le_bytes());
        b.extend(tail);
        b
    }

    fn tail_at(n_entries: usize) -> u32 {
        (8 + 2 + 12 * n_entries + 4) as u32
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn private_ifd_inline_and_out_of_line_values() {
        let tail = [0.05f64.to_le_bytes(), 8041.0f64.to_le_bytes()].concat();
        let b = private_ifd_le(
            4,
            &[
                (0x0000, 4, 1, 0u32.to_le_bytes()),
                (0x0004, 2, 3, *b"ab\0\0"),
                (0x0007, 12, 1, tail_at(4).to_le_bytes()),
                (0x0099, 12, 1, (tail_at(4) + 8).to_le_bytes()),
            ],
            &tail,
        );
        assert_eq!(
            read_dectris_ifd(&b, 8),
            kv(&[
                ("IfdVersion", "0"),
                ("ImageDateTime", "ab"),
                ("ExposureTime", "0.05"),
                ("Tag153", "8041"),
            ])
        );
    }

    #[test]
    fn private_ifd_malformed_header_offset_or_count_is_empty() {
        let good = private_ifd_le(1, &[(0x0007, 4, 1, 7u32.to_le_bytes())], &[]);
        assert_eq!(read_dectris_ifd(&good, 8), kv(&[("ExposureTime", "7")]));
        // offset outside the file, inside the header, absurd
        for off in [
            0,
            4,
            7,
            good.len() as u64,
            good.len() as u64 + 100,
            u64::MAX,
        ] {
            assert!(read_dectris_ifd(&good, off).is_empty(), "offset {off}");
        }
        // not a TIFF header
        let mut bad = good.clone();
        bad[..2].copy_from_slice(b"XX");
        assert!(read_dectris_ifd(&bad, 8).is_empty());
        let mut bad = good.clone();
        bad[2] = 41;
        assert!(read_dectris_ifd(&bad, 8).is_empty());
        // entry count above the cap, and an entry table running past the end of the file
        let over = private_ifd_le(257, &[(0x0007, 4, 1, 7u32.to_le_bytes())], &[0; 4096]);
        assert!(read_dectris_ifd(&over, 8).is_empty());
        let short = private_ifd_le(3, &[(0x0007, 4, 1, 7u32.to_le_bytes())], &[]);
        assert!(read_dectris_ifd(&short, 8).is_empty());
        assert!(read_dectris_ifd(&[], 8).is_empty());
        // BigTIFF with an 8-byte entry count of u64::MAX
        let mut big = b"II".to_vec();
        big.extend(43u16.to_le_bytes());
        big.extend([8, 0, 0, 0, 0, 0, 0, 0]);
        big.extend([16, 0, 0, 0, 0, 0, 0, 0]);
        big.extend(u64::MAX.to_le_bytes());
        assert!(read_dectris_ifd(&big, 16).is_empty());
    }

    #[test]
    fn private_ifd_bad_entries_are_skipped() {
        let b = private_ifd_le(
            6,
            &[
                (0x0001, 2, 10, 0xFFFF_FF00u32.to_le_bytes()), // value offset outside the file
                (0x0002, 12, u32::MAX, tail_at(6).to_le_bytes()), // count × size above the value cap
                (0x0003, 99, 1, [1, 2, 3, 4]),                    // unknown field type
                (0x0005, 12, 1, (tail_at(6) + 4).to_le_bytes()),  // value runs past the end
                (0x0006, 3, 2, [1, 0, 2, 0]),
                (0x0010, 13, 1, 8u32.to_le_bytes()), // nested IFD: offset only, never followed
            ],
            &[0; 8],
        );
        assert_eq!(
            read_dectris_ifd(&b, 8),
            kv(&[("ThresholdEnergy", "1,2"), ("Tag16", "8")])
        );
    }

    #[test]
    fn private_ifd_total_cap_stops_reading() {
        // 20 entries of 4096 bytes each, all pointing at the same data: 16 fit in 64 KiB
        let n = 20;
        let entries: Vec<_> = (0..n as u16)
            .map(|i| (0x0100 + i, 1, 4096, tail_at(n).to_le_bytes()))
            .collect();
        let b = private_ifd_le(n as u16, &entries, &[7; 4096]);
        let kv = read_dectris_ifd(&b, 8);
        assert_eq!(kv.len(), MAX_IFD_TOTAL_BYTES / MAX_IFD_VALUE_BYTES);
        assert_eq!(kv[0].0, "Tag256");
        // one byte over the per-value cap → skipped
        let b = private_ifd_le(
            1,
            &[(0x0001, 1, 4097, tail_at(1).to_le_bytes())],
            &[7; 4097],
        );
        assert!(read_dectris_ifd(&b, 8).is_empty());
    }

    #[test]
    fn private_ifd_rational_and_signed_formatting() {
        let tail = [
            1u32.to_le_bytes(),
            4u32.to_le_bytes(),
            3u32.to_le_bytes(),
            0u32.to_le_bytes(),
            (-1i32).to_le_bytes(),
            2i32.to_le_bytes(),
        ]
        .concat();
        let b = private_ifd_le(
            3,
            &[
                (0x0020, 5, 2, tail_at(3).to_le_bytes()),
                (0x0021, 10, 1, (tail_at(3) + 16).to_le_bytes()),
                (0x0022, 6, 2, [0xFF, 0x05, 0, 0]),
            ],
            &tail,
        );
        assert_eq!(
            read_dectris_ifd(&b, 8),
            kv(&[("Tag32", "0.25,3/0"), ("Tag33", "-0.5"), ("Tag34", "-1,5")])
        );
    }

    #[test]
    fn private_ifd_truncation_never_panics() {
        let tail = [0.05f64.to_le_bytes(), 8041.0f64.to_le_bytes()].concat();
        let b = private_ifd_le(
            3,
            &[
                (0x0000, 4, 1, 0u32.to_le_bytes()),
                (0x0007, 12, 1, tail_at(3).to_le_bytes()),
                (0x0016, 12, 2, tail_at(3).to_le_bytes()),
            ],
            &tail,
        );
        for cut in 0..b.len() {
            for off in [0, 8, cut as u64, u64::MAX] {
                let _ = read_dectris_ifd(&b[..cut], off);
            }
        }
        assert_eq!(read_dectris_ifd(&b, 8).len(), 3);
    }

    #[test]
    fn truncated_never_panics() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/vectors/mono16.tif"
        ))
        .unwrap();
        for cut in 0..data.len() {
            let _ = TiffCrateDecoder.decode(&data[..cut]);
        }
    }
}
