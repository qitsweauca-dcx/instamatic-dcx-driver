//! Image payload: validate, then decode into a caller-owned buffer
//! (docs/DESIGN.md §2.6–§2.8 [R9][R18][R19], §4.5–§4.6; PROTOCOL §8.5, §10).
//!
//! The hot path is two steps so the py layer can allocate the numpy array between them:
//!
//! 1. [`parse_image_message`] scans the image message once (header fields + the
//!    `data[channel]` MultiDimArray) and returns an [`ImagePayload`] that has been **fully
//!    validated without decompressing or allocating**: tag 40/1040, two non-zero dims, element
//!    tag in {64, 68, 69, 70}, `rows·cols·itemsize` with checked arithmetic ≤ 256 MiB and equal to
//!    the raw payload length or to the 12-byte header `orig_size` (whose framing rules are also
//!    checked: element size, block size).
//! 2. [`ImagePayload::decode_into`] fills an output slice of exactly
//!    [`ImagePayload::byte_len`] bytes: one memcpy for raw C-order data, bslz4/lz4 straight into
//!    the destination for compressed C-order data, scratch + F→C copy for tag 1040.
//!
//! [`ImagePayload::from_parts`] builds the same validated payload from already separated parts
//! (the Python `decompress_image` duck-typed `.tag/.value` input and `tag_decoders`, DESIGN §2.7).
//!
//! Output bytes are the little-endian element data of the dtype [`ElemType::native_dtype`]
//! (`u1`, `<u2`, `<u4`), shape `(rows, cols)`, C order.

use std::borrow::Cow;

use dectris_common::{DriverError, Result};

use crate::cbor::{major, read_dims, Len, Reader};
use crate::framing::{self, Algorithm, Scratch};
use crate::message::{walk_top, Header, MessageKind};
use crate::typed_array::{checked_elems, f_to_c, ElemType, TAG_COMPRESSED, TAG_MULTI_DIM};

pub use crate::typed_array::Order;

/// The image channel the driver reads (PROTOCOL §10).
pub const DEFAULT_CHANNEL: &str = "threshold_1";

/// How the element bytes are stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoding<'a> {
    /// Uncompressed byte string (`compression: none`).
    Raw(Cow<'a, [u8]>),
    /// Tag 56500.
    Compressed {
        /// Algorithm.
        algorithm: Algorithm,
        /// Wire modifier (bslz4 element size, used for decompression [R18]).
        modifier: u64,
        /// The compressed stream (12-byte header + blocks).
        data: Cow<'a, [u8]>,
    },
}

/// A validated 2-D image payload (not yet decoded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePayload<'a> {
    /// Rows (`dims[0]`, = `image_size_y`).
    pub rows: usize,
    /// Columns (`dims[1]`, = `image_size_x`).
    pub cols: usize,
    /// Element type (one of 64, 68, 69, 70).
    pub elem: ElemType,
    /// Wire order (40 → C, 1040 → F).
    pub order: Order,
    /// Payload.
    pub encoding: Encoding<'a>,
    /// Offset of the payload bytes in the message (0 for [`from_parts`](Self::from_parts)),
    /// used to place decode errors.
    pub offset: usize,
}

/// A bslz4 modifier that differs from the element size (DESIGN §4.5 [R18]): decoded with the
/// modifier anyway; the stream layer logs one WARNING per series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierMismatch {
    /// The wire modifier.
    pub modifier: u64,
    /// The typed tag's element size.
    pub itemsize: usize,
}

/// Facts about one decode the caller may want to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeNotes {
    /// Set when the bslz4 modifier ≠ itemsize.
    pub modifier_mismatch: Option<ModifierMismatch>,
}

/// An image message: header fields + the validated payload of the requested channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedImage<'a> {
    /// `type`, `series_id`, `series_unique_id`, `image_id`, `real_time`.
    pub header: Header,
    /// `data[channel]`.
    pub payload: ImagePayload<'a>,
}

fn derr(offset: usize, msg: impl Into<String>) -> DriverError {
    DriverError::decode(offset, msg)
}

impl<'a> ImagePayload<'a> {
    /// Validate separated parts (DESIGN §2.8 "validation before any allocation").
    pub fn from_parts(
        dims: &[u64],
        order: Order,
        elem_tag: u64,
        encoding: Encoding<'a>,
    ) -> Result<Self> {
        Self::validate(dims, order, elem_tag, encoding, 0)
    }

    fn validate(
        dims: &[u64],
        order: Order,
        elem_tag: u64,
        encoding: Encoding<'a>,
        offset: usize,
    ) -> Result<Self> {
        let at = |e: DriverError| framing::rebase(e, offset);
        let [rows, cols] = *dims else {
            return Err(derr(
                offset,
                format!("image must have 2 dimensions, got {dims:?}"),
            ));
        };
        if rows == 0 || cols == 0 {
            return Err(derr(offset, format!("image dimension is zero: {dims:?}")));
        }
        let elem = ElemType::from_tag(elem_tag).map_err(at)?;
        if !elem.is_image_type() {
            return Err(derr(
                offset,
                format!(
                    "unsupported image element tag {elem_tag} ({}); the image path accepts 64, 68, 69, 70",
                    elem.wire_dtype()
                ),
            ));
        }
        let n = checked_elems(dims, elem.size).map_err(at)?;
        let byte_len = n * elem.size; // checked ≤ 256 MiB above
        match &encoding {
            Encoding::Raw(b) => {
                if b.len() != byte_len {
                    return Err(derr(
                        offset,
                        format!(
                            "raw image payload is {} bytes, dims {rows}x{cols} x {} B = {byte_len}",
                            b.len(),
                            elem.size
                        ),
                    ));
                }
            }
            Encoding::Compressed {
                algorithm,
                modifier,
                data,
            } => {
                let orig = framing::decoded_len(*algorithm, *modifier, data).map_err(at)?;
                if orig != byte_len {
                    return Err(derr(
                        offset,
                        format!(
                            "{algorithm} orig_size {orig} != dims {rows}x{cols} x {} B = {byte_len}",
                            elem.size
                        ),
                    ));
                }
            }
        }
        Ok(ImagePayload {
            rows: rows as usize, // ≤ n
            cols: cols as usize,
            elem,
            order,
            encoding,
            offset,
        })
    }

    /// Decoded size in bytes (`rows·cols·itemsize`).
    pub fn byte_len(&self) -> usize {
        self.rows * self.cols * self.elem.size
    }

    /// `(rows, cols)`.
    pub fn shape(&self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    /// numpy dtype of the decoded bytes (`u1`, `<u2`, `<u4`).
    pub fn dtype(&self) -> &'static str {
        self.elem.native_dtype()
    }

    /// The bslz4 modifier ≠ itemsize condition [R18].
    pub fn modifier_mismatch(&self) -> Option<ModifierMismatch> {
        match self.encoding {
            Encoding::Compressed {
                algorithm: Algorithm::Bslz4,
                modifier,
                ..
            } if modifier != self.elem.size as u64 => Some(ModifierMismatch {
                modifier,
                itemsize: self.elem.size,
            }),
            _ => None,
        }
    }

    /// True if the payload is tag-56500 compressed.
    pub fn is_compressed(&self) -> bool {
        matches!(self.encoding, Encoding::Compressed { .. })
    }

    /// Decode into `out` (exactly [`byte_len`](Self::byte_len) bytes, C order). `scratch` is
    /// reused across frames.
    pub fn decode_into(&self, out: &mut [u8], scratch: &mut Scratch) -> Result<DecodeNotes> {
        let n = self.byte_len();
        if out.len() != n {
            return Err(derr(
                self.offset,
                format!("destination is {} bytes, image needs {n}", out.len()),
            ));
        }
        let dims = [self.rows, self.cols];
        let size = self.elem.size;
        let at = |e: DriverError| framing::rebase(e, self.offset);
        match (&self.encoding, self.order) {
            (Encoding::Raw(b), Order::C) => out.copy_from_slice(b),
            (Encoding::Raw(b), Order::F) => f_to_c(b, &dims, size, out).map_err(at)?,
            (
                Encoding::Compressed {
                    algorithm,
                    modifier,
                    data,
                },
                Order::C,
            ) => framing::decompress_into(*algorithm, *modifier, data, out, scratch).map_err(at)?,
            (
                Encoding::Compressed {
                    algorithm,
                    modifier,
                    data,
                },
                Order::F,
            ) => {
                let mut tmp = scratch.take_full(n);
                let r = framing::decompress_into(*algorithm, *modifier, data, &mut tmp, scratch)
                    .and_then(|()| f_to_c(&tmp, &dims, size, out));
                scratch.put_full(tmp);
                r.map_err(at)?;
            }
        }
        Ok(DecodeNotes {
            modifier_mismatch: self.modifier_mismatch(),
        })
    }

    /// Decode into a fresh `Vec` (tests, post-processing).
    pub fn decode_to_vec(&self) -> Result<(Vec<u8>, DecodeNotes)> {
        let mut out = vec![0u8; self.byte_len()];
        let notes = self.decode_into(&mut out, &mut Scratch::new())?;
        Ok((out, notes))
    }

    /// Detach from the message buffer.
    pub fn into_owned(self) -> ImagePayload<'static> {
        ImagePayload {
            rows: self.rows,
            cols: self.cols,
            elem: self.elem,
            order: self.order,
            encoding: match self.encoding {
                Encoding::Raw(b) => Encoding::Raw(Cow::Owned(b.into_owned())),
                Encoding::Compressed {
                    algorithm,
                    modifier,
                    data,
                } => Encoding::Compressed {
                    algorithm,
                    modifier,
                    data: Cow::Owned(data.into_owned()),
                },
            },
            offset: self.offset,
        }
    }
}

/// Parse `data[channel]` (tag 40/1040 → [dims, typed tag → bytes | 56500]) at the reader.
fn parse_channel<'a>(r: &mut Reader<'a>) -> Result<ImagePayload<'a>> {
    let th = r.head()?;
    let order = match (th.major, Order::from_tag(th.arg)) {
        (major::TAG, Some(o)) => o,
        _ => {
            return Err(derr(
                th.offset,
                format!(
                    "image channel must be a MultiDimArray (tag {TAG_MULTI_DIM}/1040), found major type {} arg {}",
                    th.major, th.arg
                ),
            ))
        }
    };
    let ah = r.head()?;
    if ah.major != major::ARRAY || (!ah.is_indefinite() && ah.arg != 2) {
        return Err(derr(
            ah.offset,
            "MultiDimArray content must be a 2-element array [dimensions, elements]",
        ));
    }
    let dims = read_dims(r)?;
    let eh = r.head()?;
    if eh.major == major::ARRAY {
        return Err(derr(
            eh.offset,
            "MultiDimArray elements given as a plain array are not supported on the image path",
        ));
    }
    if eh.major != major::TAG || !ElemType::is_typed_array_tag(eh.arg) {
        return Err(derr(
            eh.offset,
            format!(
                "image elements must be a typed array (tags 64-87), found major type {} arg {}",
                eh.major, eh.arg
            ),
        ));
    }
    let elem_tag = eh.arg;
    // reject unsupported element tags before touching the payload
    let elem = ElemType::from_tag(elem_tag).map_err(|e| framing::rebase(e, eh.offset))?;
    if !elem.is_image_type() {
        return Err(derr(
            eh.offset,
            format!(
                "unsupported image element tag {elem_tag} ({}); the image path accepts 64, 68, 69, 70",
                elem.wire_dtype()
            ),
        ));
    }
    let ch = r.head()?;
    let (encoding, offset) = match ch.major {
        major::BYTES => {
            let off = r.pos();
            (Encoding::Raw(r.bytes_item(ch)?), off)
        }
        major::TAG if ch.arg == TAG_COMPRESSED => {
            let (algorithm, modifier, data, off) = r.compressed_triple()?;
            (
                Encoding::Compressed {
                    algorithm,
                    modifier,
                    data,
                },
                off,
            )
        }
        _ => {
            return Err(derr(
                ch.offset,
                format!("typed array tag {elem_tag} content must be a byte string or tag 56500"),
            ))
        }
    };
    if ah.is_indefinite() {
        let b = r.head()?;
        if !b.is_break() {
            return Err(derr(
                b.offset,
                "MultiDimArray content must be a 2-element array [dimensions, elements]",
            ));
        }
    }
    ImagePayload::validate(&dims, order, elem_tag, encoding, offset)
}

/// Walk the `data` map; parse `channel`, record every key.
fn parse_data<'a>(
    r: &mut Reader<'a>,
    channel: &str,
    found: &mut Option<ImagePayload<'a>>,
    keys: &mut Vec<String>,
) -> Result<()> {
    let h = r.head()?;
    if h.major != major::MAP {
        return Err(derr(h.offset, "image `data` must be a map"));
    }
    let len = Len::of(&h);
    let mut i = 0u64;
    while len.more(r, i)? {
        i += 1;
        let kh = r.peek_head()?;
        let key = if kh.major == major::TEXT {
            r.head()?;
            Some(r.text_item(kh)?)
        } else {
            r.skip_at_depth(2)?;
            None
        };
        if r.peek_break() {
            return Err(derr(r.pos(), "indefinite map has a key without a value"));
        }
        match key {
            Some(k) if k == channel => {
                keys.push(k.into_owned());
                *found = Some(parse_channel(r)?);
            }
            Some(k) => {
                keys.push(k.into_owned());
                r.skip_at_depth(2)?;
            }
            None => {
                r.skip_at_depth(2)?;
            }
        }
    }
    Ok(())
}

/// Parse an image message (DESIGN §2.6 hot path): header fields + the validated `data[channel]`
/// payload, in one pass, without decompressing. A message whose `type` is not `image`, a missing
/// `data`, or a missing channel (the error lists the channel keys) is a `Decode` error.
pub fn parse_image_message<'a>(raw: &'a [u8], channel: &str) -> Result<ParsedImage<'a>> {
    let mut found: Option<ImagePayload<'a>> = None;
    let mut keys: Option<Vec<String>> = None;
    let header = walk_top(raw, |key, r| {
        if key != "data" {
            return Ok(false);
        }
        let mut k = Vec::new();
        parse_data(r, channel, &mut found, &mut k)?;
        keys = Some(k);
        Ok(true)
    })?;
    if header.kind != MessageKind::Image {
        return Err(derr(
            0,
            format!("not an image message (type {:?})", header.kind.as_str()),
        ));
    }
    let Some(keys) = keys else {
        return Err(derr(0, "image message has no `data` map"));
    };
    let payload = found.ok_or_else(|| {
        derr(
            0,
            format!("image channel {channel:?} not found; available channels: {keys:?}"),
        )
    })?;
    Ok(ParsedImage { header, payload })
}

/// A fully decoded frame (owned): the turbo post-processing / `decompress_image(bytes)` path
/// (DESIGN §2.7, §4.6) in one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Header fields of the message.
    pub header: Header,
    /// Rows.
    pub rows: usize,
    /// Columns.
    pub cols: usize,
    /// Element type (dtype = [`ElemType::native_dtype`]).
    pub elem: ElemType,
    /// `rows·cols·itemsize` little-endian bytes, C order.
    pub data: Vec<u8>,
    /// Decode notes ([R18] modifier mismatch).
    pub notes: DecodeNotes,
}

/// Parse + validate + decode an image message into an owned frame.
pub fn decode_frame(raw: &[u8], channel: &str) -> Result<DecodedFrame> {
    let p = parse_image_message(raw, channel)?;
    let (data, notes) = p.payload.decode_to_vec()?;
    Ok(DecodedFrame {
        header: p.header,
        rows: p.payload.rows,
        cols: p.payload.cols,
        elem: p.payload.elem,
        data,
        notes,
    })
}
