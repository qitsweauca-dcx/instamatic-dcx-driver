//! RFC 8746 typed-array tags (64–87) and multi-dimensional array helpers
//! (docs/DESIGN.md §2.8 [R19][R21], §4.4; PROTOCOL §8.3–§8.4).
//!
//! Tag number = `64 + 16·f + 8·s + 4·e + ll` (RFC 8746 §2.1). Element data is always handed out
//! in **little-endian** (native on Windows x64): big-endian tags are byte-swapped at decode time,
//! so the py layer can allocate the native dtype ([`ElemType::native_dtype`]). float16 stays as raw
//! `u16` bits (the py layer views it as `np.float16`). Tags 76 (unassigned) and 83/87 (float128,
//! no Windows numpy dtype) are errors.

use std::borrow::Cow;

use dectris_common::{DriverError, Result};

use crate::framing::{Algorithm, MAX_DECODED_BYTES};

/// Element kind of a typed array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElemKind {
    /// Unsigned integer (`u1`/`u2`/`u4`/`u8`).
    Unsigned,
    /// Signed integer (`i1`/`i2`/`i4`/`i8`).
    Signed,
    /// IEEE float (`f2`/`f4`/`f8`).
    Float,
}

/// Element type of an RFC 8746 typed-array tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElemType {
    /// The tag number (64–87).
    pub tag: u64,
    /// Integer / float.
    pub kind: ElemKind,
    /// Bytes per element (1, 2, 4, 8).
    pub size: usize,
    /// Wire byte order (false for 1-byte types).
    pub big_endian: bool,
}

/// `TYPED_ARRAY_DTYPES` [#12 keep]: tag → numpy dtype string, exactly the DESIGN §2.8 table
/// (wire byte order; 76/83/87 absent).
pub const TYPED_ARRAY_DTYPES: &[(u64, &str)] = &[
    (64, "u1"),
    (65, ">u2"),
    (66, ">u4"),
    (67, ">u8"),
    (68, "u1"),
    (69, "<u2"),
    (70, "<u4"),
    (71, "<u8"),
    (72, "i1"),
    (73, ">i2"),
    (74, ">i4"),
    (75, ">i8"),
    (77, "<i2"),
    (78, "<i4"),
    (79, "<i8"),
    (80, ">f2"),
    (81, ">f4"),
    (82, ">f8"),
    (84, "<f2"),
    (85, "<f4"),
    (86, "<f8"),
];

/// Element order of a MultiDimArray on the wire (RFC 8746 §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Order {
    /// Tag 40: row-major, last dimension contiguous.
    C,
    /// Tag 1040: column-major, first dimension contiguous.
    F,
}

impl Order {
    /// 40 → C, 1040 → F.
    pub const fn from_tag(tag: u64) -> Option<Order> {
        match tag {
            TAG_MULTI_DIM => Some(Order::C),
            TAG_MULTI_DIM_COL_MAJOR => Some(Order::F),
            _ => None,
        }
    }

    /// The tag number.
    pub const fn tag(self) -> u64 {
        match self {
            Order::C => TAG_MULTI_DIM,
            Order::F => TAG_MULTI_DIM_COL_MAJOR,
        }
    }
}

/// Tag 0: RFC 3339 date/time text.
pub const TAG_DATETIME: u64 = 0;
/// Tag 40: row-major MultiDimArray.
pub const TAG_MULTI_DIM: u64 = 40;
/// Tag 1040: column-major MultiDimArray.
pub const TAG_MULTI_DIM_COL_MAJOR: u64 = 1040;
/// Tag 56500: DECTRIS compressed byte string `[algorithm, modifier, bytes]`.
pub const TAG_COMPRESSED: u64 = 56500;

/// True for the Stream V2 data-model tags {0, 40, 1040, 56500, 64–87} (PROTOCOL §8.4).
pub const fn is_data_model_tag(tag: u64) -> bool {
    matches!(
        tag,
        TAG_DATETIME | TAG_MULTI_DIM | TAG_MULTI_DIM_COL_MAJOR | TAG_COMPRESSED
    ) || ElemType::is_typed_array_tag(tag)
}

/// Tags accepted on the image hot path (DESIGN §2.8): u8, u8-clamped, `<u2`, `<u4`.
pub const IMAGE_TAGS: [u64; 4] = [64, 68, 69, 70];

impl ElemType {
    /// True if `tag` is in the typed-array range 64–87 (including the rejected 76/83/87).
    pub const fn is_typed_array_tag(tag: u64) -> bool {
        tag >= 64 && tag <= 87
    }

    /// The element type of a typed-array tag. Errors for 76/83/87 and for tags outside 64–87.
    pub fn from_tag(tag: u64) -> Result<Self> {
        if !Self::is_typed_array_tag(tag) {
            return Err(DriverError::decode(
                0,
                format!("tag {tag} is not a typed-array tag"),
            ));
        }
        let bits = tag - 64;
        let f = bits & 0b1_0000 != 0;
        let s = bits & 0b1000 != 0;
        let e = bits & 0b100 != 0;
        let ll = (bits & 0b11) as u32;
        match tag {
            76 => {
                return Err(DriverError::decode(
                    0,
                    "typed-array tag 76 is unassigned (RFC 8746)",
                ))
            }
            83 | 87 => {
                return Err(DriverError::decode(
                    0,
                    format!("typed-array tag {tag} (float128) is not supported on this platform"),
                ))
            }
            _ => {}
        }
        let (kind, size) = if f {
            (ElemKind::Float, 2usize << ll)
        } else if s {
            (ElemKind::Signed, 1usize << ll)
        } else {
            (ElemKind::Unsigned, 1usize << ll)
        };
        Ok(ElemType {
            tag,
            kind,
            size,
            big_endian: !e && size > 1,
        })
    }

    /// numpy dtype string in wire byte order (the `TYPED_ARRAY_DTYPES` spelling).
    pub fn wire_dtype(&self) -> &'static str {
        TYPED_ARRAY_DTYPES
            .iter()
            .find(|(t, _)| *t == self.tag)
            .map_or("", |(_, d)| d)
    }

    /// numpy dtype string of the decoded (little-endian) data, e.g. `"<u2"`, `"u1"`, `"<f2"`.
    pub fn native_dtype(&self) -> &'static str {
        match (self.kind, self.size) {
            (ElemKind::Unsigned, 1) => "u1",
            (ElemKind::Unsigned, 2) => "<u2",
            (ElemKind::Unsigned, 4) => "<u4",
            (ElemKind::Unsigned, 8) => "<u8",
            (ElemKind::Signed, 1) => "i1",
            (ElemKind::Signed, 2) => "<i2",
            (ElemKind::Signed, 4) => "<i4",
            (ElemKind::Signed, 8) => "<i8",
            (ElemKind::Float, 2) => "<f2",
            (ElemKind::Float, 4) => "<f4",
            (ElemKind::Float, 8) => "<f8",
            _ => "",
        }
    }

    /// True for the tags the image hot path accepts ({64, 68, 69, 70}).
    pub fn is_image_type(&self) -> bool {
        IMAGE_TAGS.contains(&self.tag)
    }
}

/// Tag-56500 compression of a payload as seen on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compression {
    /// Algorithm.
    pub algorithm: Algorithm,
    /// The wire modifier (bslz4 element size; lz4 placeholder).
    pub modifier: u64,
}

/// A decoded typed array: element type + little-endian element bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedArray<'a> {
    /// Element type from the tag.
    pub elem: ElemType,
    /// Element bytes, little-endian, `len() % elem.size == 0`. Borrowed from the message when
    /// the payload was a definite, uncompressed, little-endian (or 1-byte) byte string.
    pub data: Cow<'a, [u8]>,
    /// The 56500 wrapper, if the payload was compressed.
    pub compression: Option<Compression>,
}

impl<'a> TypedArray<'a> {
    /// Build from the tag and the (already decompressed) wire bytes: checks the length and
    /// byte-swaps big-endian data to little-endian.
    pub fn from_wire(
        tag: u64,
        bytes: Cow<'a, [u8]>,
        compression: Option<Compression>,
    ) -> Result<Self> {
        let elem = ElemType::from_tag(tag)?;
        if bytes.len() % elem.size != 0 {
            return Err(DriverError::decode(
                0,
                format!(
                    "typed array (tag {tag}) of {} bytes is not a multiple of the {}-byte element",
                    bytes.len(),
                    elem.size
                ),
            ));
        }
        let data = if elem.big_endian {
            let mut v = bytes.into_owned();
            swap_bytes_in_place(&mut v, elem.size);
            Cow::Owned(v)
        } else {
            bytes
        };
        Ok(TypedArray {
            elem,
            data,
            compression,
        })
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.data.len() / self.elem.size
    }

    /// True if there are no elements.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// True if a 56500 modifier differs from the element size (bslz4 only; DESIGN §4.5 [R18]).
    pub fn modifier_mismatch(&self) -> bool {
        matches!(self.compression, Some(c) if c.algorithm == Algorithm::Bslz4 && c.modifier != self.elem.size as u64)
    }

    /// Detach from the message buffer.
    pub fn into_owned(self) -> TypedArray<'static> {
        TypedArray {
            elem: self.elem,
            data: Cow::Owned(self.data.into_owned()),
            compression: self.compression,
        }
    }
}

/// Reverse the bytes of every `size`-byte element (big-endian ↔ little-endian).
pub fn swap_bytes_in_place(data: &mut [u8], size: usize) {
    if size <= 1 {
        return;
    }
    for ch in data.chunks_exact_mut(size) {
        ch.reverse();
    }
}

/// Product of `dims` with checked arithmetic; also checks `product · itemsize ≤ 256 MiB`.
pub fn checked_elems(dims: &[u64], itemsize: usize) -> Result<usize> {
    let mut n: u64 = 1;
    for &d in dims {
        n = n
            .checked_mul(d)
            .ok_or_else(|| DriverError::decode(0, format!("array dimensions {dims:?} overflow")))?;
    }
    let bytes = n
        .checked_mul(itemsize as u64)
        .ok_or_else(|| DriverError::decode(0, format!("array dimensions {dims:?} overflow")))?;
    if bytes > MAX_DECODED_BYTES as u64 {
        return Err(DriverError::decode(
            0,
            format!(
                "array of {bytes} bytes (dims {dims:?}) exceeds the {} MiB limit",
                MAX_DECODED_BYTES >> 20
            ),
        ));
    }
    usize::try_from(n).map_err(|_| DriverError::decode(0, "array too large"))
}

/// For each element in C (row-major) order, its index in F (column-major) order.
/// `dims` must already have passed [`checked_elems`].
pub fn f_order_indices(dims: &[usize]) -> FOrderIndices {
    let mut fstride = Vec::with_capacity(dims.len());
    let mut s = 1usize;
    for &d in dims {
        fstride.push(s);
        s = s.saturating_mul(d);
    }
    let total = if dims.contains(&0) { 0 } else { s };
    FOrderIndices {
        dims: dims.to_vec(),
        fstride,
        idx: vec![0; dims.len()],
        off: 0,
        left: total,
    }
}

/// Iterator returned by [`f_order_indices`].
#[derive(Debug, Clone)]
pub struct FOrderIndices {
    dims: Vec<usize>,
    fstride: Vec<usize>,
    idx: Vec<usize>,
    off: usize,
    left: usize,
}

impl Iterator for FOrderIndices {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.left == 0 {
            return None;
        }
        let cur = self.off;
        self.left -= 1;
        if self.left > 0 {
            // odometer: last index fastest (C order)
            for ((i, &d), &st) in self
                .idx
                .iter_mut()
                .zip(self.dims.iter())
                .zip(self.fstride.iter())
                .rev()
            {
                *i += 1;
                self.off += st;
                if *i < d {
                    break;
                }
                *i = 0;
                self.off -= st * d;
            }
        }
        Some(cur)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

/// Copy column-major (`order='F'`) element bytes `src` of shape `dims` into C-contiguous `out`
/// (DESIGN §2.8 tag 1040 [R19]): `out == np.reshape(src, dims, order='F').copy(order='C')`.
pub fn f_to_c(src: &[u8], dims: &[usize], itemsize: usize, out: &mut [u8]) -> Result<()> {
    let n: usize = dims
        .iter()
        .try_fold(1usize, |a, &d| a.checked_mul(d))
        .unwrap_or(usize::MAX);
    let need = n.checked_mul(itemsize);
    if itemsize == 0 || need != Some(src.len()) || src.len() != out.len() {
        return Err(DriverError::decode(0, "F→C copy: size mismatch"));
    }
    if n == 0 {
        // an empty array (a zero dim): nothing to copy, and chunks_exact_mut(0) would panic
        return Ok(());
    }
    if let [rows, cols] = *dims {
        // 2-D fast path: out[r, c] = src[c·rows + r]
        for (r, out_row) in out.chunks_exact_mut(cols * itemsize).enumerate() {
            for (c, dst) in out_row.chunks_exact_mut(itemsize).enumerate() {
                let at = (c * rows + r) * itemsize;
                let s = src
                    .get(at..at + itemsize)
                    .ok_or_else(|| DriverError::decode(0, "F→C copy: index"))?;
                dst.copy_from_slice(s);
            }
        }
        return Ok(());
    }
    for (dst, fi) in out.chunks_exact_mut(itemsize).zip(f_order_indices(dims)) {
        let at = fi * itemsize;
        let s = src
            .get(at..at + itemsize)
            .ok_or_else(|| DriverError::decode(0, "F→C copy: index"))?;
        dst.copy_from_slice(s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_table() {
        for &(tag, dt) in TYPED_ARRAY_DTYPES {
            let e = ElemType::from_tag(tag).unwrap();
            assert_eq!(e.wire_dtype(), dt, "tag {tag}");
            let size: usize = dt
                .trim_start_matches(['<', '>'])
                .get(1..)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(e.size, size, "tag {tag}");
            assert_eq!(e.big_endian, dt.starts_with('>'), "tag {tag}");
            let kind = match dt.trim_start_matches(['<', '>']).as_bytes()[0] {
                b'u' => ElemKind::Unsigned,
                b'i' => ElemKind::Signed,
                b'f' => ElemKind::Float,
                _ => unreachable!(),
            };
            assert_eq!(e.kind, kind);
            assert!(!e.native_dtype().starts_with('>'));
        }
        for tag in [76u64, 83, 87] {
            assert!(ElemType::from_tag(tag).is_err());
        }
        for tag in [0u64, 40, 63, 88, 1040, 56500] {
            assert!(ElemType::from_tag(tag).is_err());
        }
        assert_eq!(TYPED_ARRAY_DTYPES.len(), 21);
        assert!(ElemType::from_tag(70).unwrap().is_image_type());
        assert!(!ElemType::from_tag(71).unwrap().is_image_type());
    }

    #[test]
    fn byteswap_from_wire() {
        let t = TypedArray::from_wire(65, Cow::Borrowed(&[0x12, 0x34, 0xAB, 0xCD]), None).unwrap();
        assert_eq!(&*t.data, &[0x34, 0x12, 0xCD, 0xAB]);
        assert_eq!(t.len(), 2);
        let t = TypedArray::from_wire(69, Cow::Borrowed(&[0x12, 0x34]), None).unwrap();
        assert!(matches!(t.data, Cow::Borrowed(_)));
        assert!(TypedArray::from_wire(70, Cow::Borrowed(&[1, 2, 3]), None).is_err());
    }

    #[test]
    fn f_to_c_empty_is_not_a_panic() {
        // review #4: dims [5, 0] made chunks_exact_mut(0) panic
        let mut out = [];
        f_to_c(&[], &[5, 0], 4, &mut out).unwrap();
        f_to_c(&[], &[0, 3, 2], 2, &mut out).unwrap();
    }

    #[test]
    fn f_to_c_matches_reshape() {
        // dims (2,3), F order source = column-major listing of [[0,1,2],[3,4,5]]
        let src: Vec<u8> = vec![0, 3, 1, 4, 2, 5];
        let mut out = vec![0u8; 6];
        f_to_c(&src, &[2, 3], 1, &mut out).unwrap();
        assert_eq!(out, vec![0, 1, 2, 3, 4, 5]);
        // 3-D via the generic path vs. a direct formula
        let dims = [2usize, 3, 4];
        let n = 24;
        let src: Vec<u8> = (0..n as u8).collect();
        let mut out = vec![0u8; n];
        f_to_c(&src, &dims, 1, &mut out).unwrap();
        for i in 0..2 {
            for j in 0..3 {
                for k in 0..4 {
                    let c = (i * 3 + j) * 4 + k;
                    let f = i + 2 * (j + 3 * k);
                    assert_eq!(out[c], src[f]);
                }
            }
        }
        assert_eq!(f_order_indices(&[0, 3]).count(), 0);
        assert_eq!(f_order_indices(&[]).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn checked_elems_limits() {
        assert_eq!(checked_elems(&[1028, 1062], 4).unwrap(), 1028 * 1062);
        assert!(checked_elems(&[u64::MAX, 2], 1).is_err());
        assert!(checked_elems(&[1 << 20, 1 << 20], 1).is_err());
        assert!(checked_elems(&[1 << 26], 4).is_ok());
        assert!(checked_elems(&[(1 << 26) + 1], 4).is_err());
    }
}
