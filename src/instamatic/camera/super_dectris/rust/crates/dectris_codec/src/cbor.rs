//! Hand-rolled CBOR decoder for Stream V2 messages, depth-limited
//! (docs/DESIGN.md §4.4 [R20][R37][G2-K][G2-P]; RFC 8949; PROTOCOL §8).
//!
//! * [`Reader`]: the byte-level cursor. Heads (major types 0–7, additional info 0–27, 31),
//!   definite and indefinite (chunked) byte/text strings returned as `Cow` (borrowed when
//!   definite), and an **iterative** [`Reader::skip`] with an explicit stack, so a depth bomb
//!   cannot overflow the native stack.
//! * [`Decoder`]: the recursive value-tree decoder ([`Value`]), depth-limited to [`MAX_DEPTH`].
//!   Data-model tags are resolved while decoding: 0 → [`Value::DateTime`], 40/1040 →
//!   [`Value::MultiDim`] (1040 copied to C order), 64–87 → [`Value::TypedArray`] (big-endian
//!   swapped to little-endian), 56500 → decompressed wherever a byte string is expected (inside a
//!   typed array, or as [`Value::Bytes`]). Every other tag passes through as [`Value::Tag`].
//!
//! Bounds: every length uses checked arithmetic and every read is bounds-checked; errors are
//! `DriverError::Decode { offset, .. }` with the byte offset in the message. Additional info
//! 28–30 is an error; simple values 20–23 are `false`/`true`/`null`/`null` (undefined maps to
//! null, [G2-P]); a one-byte simple value below 32 is malformed (RFC 8949 §3.3).

use std::borrow::Cow;

use dectris_common::{DriverError, Result};

use crate::framing::{self, Algorithm};
use crate::typed_array::{
    checked_elems, f_order_indices, f_to_c, Compression, ElemType, Order, TypedArray,
    TAG_COMPRESSED, TAG_DATETIME,
};

/// Maximum nesting depth (containers and tags) for decode and skip (DESIGN §4.4).
pub const MAX_DEPTH: usize = 32;

/// Decoded bytes all tag-56500 payloads of one message may expand to together (review #11):
/// each payload is already capped at [`framing::MAX_DECODED_BYTES`], this bounds their sum.
pub const MAX_MESSAGE_DECODED_BYTES: usize = 512 * 1024 * 1024;

/// Maximum number of MultiDimArray dimensions accepted (numpy itself stops at 64).
pub const MAX_DIMS: usize = 32;

/// Major types (RFC 8949 §3.1).
pub mod major {
    /// unsigned integer
    pub const UINT: u8 = 0;
    /// negative integer
    pub const NINT: u8 = 1;
    /// byte string
    pub const BYTES: u8 = 2;
    /// text string
    pub const TEXT: u8 = 3;
    /// array
    pub const ARRAY: u8 = 4;
    /// map
    pub const MAP: u8 = 5;
    /// tag
    pub const TAG: u8 = 6;
    /// simple / float / break
    pub const SIMPLE: u8 = 7;
}

/// Additional-info value for indefinite length / break.
pub const AI_INDEFINITE: u8 = 31;

/// A decoded CBOR data item.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    /// Major types 0 and 1 (`-2^64 ..= 2^64-1`).
    Integer(i128),
    /// Byte string (also the decompressed content of a bare tag 56500).
    Bytes(Cow<'a, [u8]>),
    /// Text string.
    Text(Cow<'a, str>),
    /// Array.
    Array(Vec<Value<'a>>),
    /// Map, in wire order (duplicate keys are kept; a dict built from it keeps the last).
    Map(Vec<(Value<'a>, Value<'a>)>),
    /// `false` / `true`.
    Bool(bool),
    /// `null` and `undefined`.
    Null,
    /// An unassigned simple value (0–19, 32–255).
    Simple(u8),
    /// float16/32/64, widened to f64.
    Float(f64),
    /// Tag 0: RFC 3339 text (kept as text, DESIGN §5 Q17).
    DateTime(Cow<'a, str>),
    /// Tags 64–87 (payload raw or tag-56500-compressed).
    TypedArray(TypedArray<'a>),
    /// Tags 40 / 1040.
    MultiDim(MultiDimArray<'a>),
    /// Any tag outside the data model, with its content.
    Tag(u64, Box<Value<'a>>),
    /// A start-message array skipped by `DecodeOptions::skip_start_arrays` (DESIGN §4.4): the
    /// item's byte span in the message.
    Skipped {
        /// Offset of the item in the message.
        offset: usize,
        /// Encoded length of the item.
        len: usize,
    },
}

/// A MultiDimArray (tag 40 / 1040), always held in C order after decoding.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiDimArray<'a> {
    /// Logical shape (RFC 8746 §3.1: unchanged by the order).
    pub dims: Vec<usize>,
    /// The order on the wire (1040 data was copied F→C during decode).
    pub source_order: Order,
    /// Elements in C order.
    pub elements: Elements<'a>,
}

/// MultiDimArray elements.
#[derive(Debug, Clone, PartialEq)]
pub enum Elements<'a> {
    /// A typed array (the normal case).
    Typed(TypedArray<'a>),
    /// A plain CBOR array (PROTOCOL §8.4; becomes an object ndarray in Python) [G2-P].
    Values(Vec<Value<'a>>),
}

impl<'a> Value<'a> {
    /// Detach from the message buffer.
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::Integer(i) => Value::Integer(i),
            Value::Bytes(b) => Value::Bytes(Cow::Owned(b.into_owned())),
            Value::Text(t) => Value::Text(Cow::Owned(t.into_owned())),
            Value::Array(v) => Value::Array(v.into_iter().map(Value::into_owned).collect()),
            Value::Map(v) => Value::Map(
                v.into_iter()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            ),
            Value::Bool(b) => Value::Bool(b),
            Value::Null => Value::Null,
            Value::Simple(s) => Value::Simple(s),
            Value::Float(f) => Value::Float(f),
            Value::DateTime(t) => Value::DateTime(Cow::Owned(t.into_owned())),
            Value::TypedArray(t) => Value::TypedArray(t.into_owned()),
            Value::MultiDim(m) => Value::MultiDim(MultiDimArray {
                dims: m.dims,
                source_order: m.source_order,
                elements: match m.elements {
                    Elements::Typed(t) => Elements::Typed(t.into_owned()),
                    Elements::Values(v) => {
                        Elements::Values(v.into_iter().map(Value::into_owned).collect())
                    }
                },
            }),
            Value::Tag(t, v) => Value::Tag(t, Box::new(v.into_owned())),
            Value::Skipped { offset, len } => Value::Skipped { offset, len },
        }
    }

    /// Strip pass-through tags outside the data model (DESIGN §4.4 [G2-P] scalar rule).
    pub fn peel_foreign_tags(&self) -> &Value<'a> {
        let mut v = self;
        while let Value::Tag(_, inner) = v {
            v = inner;
        }
        v
    }

    /// Non-negative integer as `u64` (foreign tags peeled).
    pub fn as_u64(&self) -> Option<u64> {
        match self.peel_foreign_tags() {
            Value::Integer(i) => u64::try_from(*i).ok(),
            _ => None,
        }
    }

    /// Float or integer as `f64` (foreign tags peeled).
    pub fn as_f64(&self) -> Option<f64> {
        match self.peel_foreign_tags() {
            Value::Float(f) => Some(*f),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Text (foreign tags peeled; a tag-0 date is text too).
    pub fn as_str(&self) -> Option<&str> {
        match self.peel_foreign_tags() {
            Value::Text(t) | Value::DateTime(t) => Some(t),
            _ => None,
        }
    }

    /// Bool (foreign tags peeled).
    pub fn as_bool(&self) -> Option<bool> {
        match self.peel_foreign_tags() {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Map lookup by text key (last duplicate wins).
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self.peel_foreign_tags() {
            Value::Map(entries) => entries
                .iter()
                .rev()
                .find(|(k, _)| matches!(k, Value::Text(t) if t == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }
}

/// One item head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// Major type 0–7.
    pub major: u8,
    /// Additional info 0–27 or 31.
    pub ai: u8,
    /// The argument (value, length, tag number, simple value or float bits); 0 if indefinite.
    pub arg: u64,
    /// Offset of the initial byte.
    pub offset: usize,
}

impl Head {
    /// Indefinite length (strings, arrays, maps) or break (major 7).
    pub const fn is_indefinite(&self) -> bool {
        self.ai == AI_INDEFINITE
    }

    /// The break stop code `0xFF`.
    pub const fn is_break(&self) -> bool {
        self.major == major::SIMPLE && self.ai == AI_INDEFINITE
    }
}

/// Decode an IEEE 754 binary16 value.
pub fn f16_to_f64(h: u16) -> f64 {
    let negative = h & 0x8000 != 0;
    let exp = i32::from((h >> 10) & 0x1F);
    let mant = f64::from(h & 0x3FF);
    let v = match exp {
        0 => mant * 2f64.powi(-24),
        31 => {
            if mant == 0.0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + mant / 1024.0) * 2f64.powi(exp - 15),
    };
    if negative {
        -v
    } else {
        v
    }
}

/// Byte cursor over one message.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

/// One frame of the iterative skip stack.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// Items still to read in a definite container (a tag is a definite frame of 1).
    Definite(u64),
    /// Indefinite array.
    IndefArray,
    /// Indefinite map; the count of items read so far (must be even at the break).
    IndefMap(u64),
}

impl<'a> Reader<'a> {
    /// Cursor at offset 0.
    pub const fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// The whole buffer.
    pub const fn buf(&self) -> &'a [u8] {
        self.buf
    }

    /// Current offset.
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// Rewind/advance to `pos` (an offset previously returned by [`pos`](Self::pos)).
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.buf.len());
    }

    /// True if every byte was consumed.
    pub const fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// `Decode` error at `offset`.
    pub fn err(offset: usize, msg: impl Into<String>) -> DriverError {
        DriverError::decode(offset, msg)
    }

    /// Consume `n` bytes.
    pub fn take(&mut self, n: u64) -> Result<&'a [u8]> {
        let at = self.pos;
        let have = self.buf.len().saturating_sub(at);
        let end = usize::try_from(n)
            .ok()
            .and_then(|n| at.checked_add(n))
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| {
                Self::err(
                    at,
                    format!("truncated: item needs {n} bytes, {have} remain"),
                )
            })?;
        let s = self
            .buf
            .get(at..end)
            .ok_or_else(|| Self::err(at, "truncated"))?;
        self.pos = end;
        Ok(s)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self.take(N as u64)?;
        s.try_into().map_err(|_| Self::err(self.pos, "truncated"))
    }

    /// Read one head.
    pub fn head(&mut self) -> Result<Head> {
        let offset = self.pos;
        let ib = *self
            .buf
            .get(offset)
            .ok_or_else(|| Self::err(offset, "truncated: unexpected end of message"))?;
        self.pos += 1;
        let major = ib >> 5;
        let ai = ib & 0x1F;
        let arg = match ai {
            0..=23 => u64::from(ai),
            24 => u64::from(u8::from_be_bytes(self.take_array::<1>()?)),
            25 => u64::from(u16::from_be_bytes(self.take_array::<2>()?)),
            26 => u64::from(u32::from_be_bytes(self.take_array::<4>()?)),
            27 => u64::from_be_bytes(self.take_array::<8>()?),
            28..=30 => {
                return Err(Self::err(
                    offset,
                    format!("reserved additional info {ai} (major type {major})"),
                ))
            }
            _ => {
                // 31
                if !matches!(major, 2..=5 | 7) {
                    return Err(Self::err(
                        offset,
                        format!("indefinite length is not allowed for major type {major}"),
                    ));
                }
                0
            }
        };
        if major == major::SIMPLE && ai == 24 && arg < 32 {
            return Err(Self::err(
                offset,
                format!("malformed one-byte simple value {arg} (< 32)"),
            ));
        }
        Ok(Head {
            major,
            ai,
            arg,
            offset,
        })
    }

    /// Read a head without consuming it.
    pub fn peek_head(&self) -> Result<Head> {
        let mut c = self.clone();
        c.head()
    }

    /// True if the next byte is the break code (does not consume).
    pub fn peek_break(&self) -> bool {
        self.buf.get(self.pos) == Some(&0xFF)
    }

    /// Content of a byte string whose head `h` (major 2) was just read. Borrowed when definite;
    /// chunked strings are concatenated (each chunk must be a definite byte string).
    pub fn bytes_item(&mut self, h: Head) -> Result<Cow<'a, [u8]>> {
        if h.major != major::BYTES {
            return Err(Self::err(
                h.offset,
                format!("expected a byte string, found major type {}", h.major),
            ));
        }
        if !h.is_indefinite() {
            return Ok(Cow::Borrowed(self.take(h.arg)?));
        }
        let mut v = Vec::new();
        loop {
            let c = self.head()?;
            if c.is_break() {
                return Ok(Cow::Owned(v));
            }
            if c.major != major::BYTES || c.is_indefinite() {
                return Err(Self::err(
                    c.offset,
                    "chunk of an indefinite byte string must be a definite byte string",
                ));
            }
            v.extend_from_slice(self.take(c.arg)?);
        }
    }

    /// Content of a text string whose head `h` (major 3) was just read; UTF-8 checked per chunk.
    pub fn text_item(&mut self, h: Head) -> Result<Cow<'a, str>> {
        if h.major != major::TEXT {
            return Err(Self::err(
                h.offset,
                format!("expected a text string, found major type {}", h.major),
            ));
        }
        let utf8 = |b: &'a [u8], at: usize| {
            std::str::from_utf8(b)
                .map_err(|e| Self::err(at, format!("invalid UTF-8 in text string: {e}")))
        };
        if !h.is_indefinite() {
            let at = self.pos;
            let b = self.take(h.arg)?;
            return Ok(Cow::Borrowed(utf8(b, at)?));
        }
        let mut s = String::new();
        loop {
            let c = self.head()?;
            if c.is_break() {
                return Ok(Cow::Owned(s));
            }
            if c.major != major::TEXT || c.is_indefinite() {
                return Err(Self::err(
                    c.offset,
                    "chunk of an indefinite text string must be a definite text string",
                ));
            }
            let at = self.pos;
            let b = self.take(c.arg)?;
            s.push_str(utf8(b, at)?);
        }
    }

    /// Skip one complete item at nesting depth 0. Iterative; enforces [`MAX_DEPTH`] and every
    /// structural rule (heads, lengths, chunk types, breaks, map parity), but does not validate
    /// UTF-8 inside skipped text strings (strings are skipped in O(1)).
    pub fn skip(&mut self) -> Result<()> {
        self.skip_at_depth(0).map(|_| ())
    }

    /// Skip one complete item that sits inside `base` enclosing containers/tags (the depth limit
    /// counts them). Returns the item's start offset.
    pub fn skip_at_depth(&mut self, base: usize) -> Result<usize> {
        let start = self.pos;
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            let h = self.head()?;
            let mut complete = true;
            match h.major {
                major::UINT | major::NINT => {}
                major::BYTES | major::TEXT => {
                    if h.is_indefinite() {
                        loop {
                            let c = self.head()?;
                            if c.is_break() {
                                break;
                            }
                            if c.major != h.major || c.is_indefinite() {
                                return Err(Self::err(
                                    c.offset,
                                    "chunk of an indefinite string must be a definite string of the same type",
                                ));
                            }
                            self.take(c.arg)?;
                        }
                    } else {
                        self.take(h.arg)?;
                    }
                }
                major::ARRAY | major::MAP | major::TAG => {
                    if base + stack.len() + 1 > MAX_DEPTH {
                        return Err(Self::err(h.offset, "nesting too deep"));
                    }
                    let frame = if h.major == major::TAG {
                        Some(Frame::Definite(1))
                    } else if h.is_indefinite() {
                        Some(if h.major == major::MAP {
                            Frame::IndefMap(0)
                        } else {
                            Frame::IndefArray
                        })
                    } else {
                        let n = if h.major == major::MAP {
                            h.arg
                                .checked_mul(2)
                                .ok_or_else(|| Self::err(h.offset, "map length overflow"))?
                        } else {
                            h.arg
                        };
                        (n > 0).then_some(Frame::Definite(n))
                    };
                    if let Some(f) = frame {
                        stack.push(f);
                        complete = false;
                    }
                }
                _ => {
                    // major 7
                    if h.is_break() {
                        match stack.pop() {
                            Some(Frame::IndefArray) => {}
                            Some(Frame::IndefMap(n)) if n % 2 == 0 => {}
                            Some(Frame::IndefMap(_)) => {
                                return Err(Self::err(
                                    h.offset,
                                    "indefinite map has a key without a value",
                                ))
                            }
                            _ => return Err(Self::err(h.offset, "unexpected break")),
                        }
                    }
                }
            }
            if complete {
                // propagate completion up the stack
                loop {
                    match stack.last_mut() {
                        None => return Ok(start),
                        Some(Frame::Definite(n)) => {
                            *n -= 1;
                            if *n == 0 {
                                stack.pop();
                                continue;
                            }
                            break;
                        }
                        Some(Frame::IndefArray) => break,
                        Some(Frame::IndefMap(n)) => {
                            *n += 1;
                            break;
                        }
                    }
                }
            }
        }
    }

    /// After the head of a tag 56500 was read: the content `[algorithm tstr, modifier uint,
    /// data bstr]` — exactly three items [G2-K]. Returns the algorithm, modifier, data and the
    /// data's offset. No decompression.
    pub fn compressed_triple(&mut self) -> Result<(Algorithm, u64, Cow<'a, [u8]>, usize)> {
        let h = self.head()?;
        if h.major != major::ARRAY || (!h.is_indefinite() && h.arg != 3) {
            return Err(Self::err(
                h.offset,
                "tag 56500 content must be a 3-element array [algorithm, modifier, data]",
            ));
        }
        let ah = self.head()?;
        if ah.major != major::TEXT {
            return Err(Self::err(
                ah.offset,
                "tag 56500 algorithm must be a text string",
            ));
        }
        let name = self.text_item(ah)?;
        let algorithm =
            Algorithm::from_wire(name.as_bytes()).map_err(|e| framing::rebase(e, ah.offset))?;
        let mh = self.head()?;
        if mh.major != major::UINT {
            return Err(Self::err(
                mh.offset,
                "tag 56500 modifier must be an unsigned integer",
            ));
        }
        let dh = self.head()?;
        if dh.major != major::BYTES {
            return Err(Self::err(dh.offset, "tag 56500 data must be a byte string"));
        }
        let data_offset = self.pos;
        let data = self.bytes_item(dh)?;
        if h.is_indefinite() {
            let b = self.head()?;
            if !b.is_break() {
                return Err(Self::err(
                    b.offset,
                    "tag 56500 content must be a 3-element array [algorithm, modifier, data]",
                ));
            }
        }
        Ok((algorithm, mh.arg, data, data_offset))
    }
}

/// Container length: definite count or indefinite (ends with a break).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Len {
    /// Definite item count (pairs for maps).
    Definite(u64),
    /// Indefinite.
    Indefinite,
}

impl Len {
    /// From an array/map head.
    pub const fn of(h: &Head) -> Len {
        if h.is_indefinite() {
            Len::Indefinite
        } else {
            Len::Definite(h.arg)
        }
    }

    /// True if another item follows `read` items (consumes the break of an indefinite container).
    pub fn more(&self, r: &mut Reader<'_>, read: u64) -> Result<bool> {
        match *self {
            Len::Definite(n) => Ok(read < n),
            Len::Indefinite => {
                if r.peek_break() {
                    r.head()?;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
        }
    }

    /// Capacity hint bounded by the remaining input (each item is at least one byte).
    pub fn capacity(&self, r: &Reader<'_>) -> usize {
        match *self {
            Len::Definite(n) => usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(r.buf.len().saturating_sub(r.pos)),
            Len::Indefinite => 0,
        }
    }
}

/// Recursive value decoder (depth-limited).
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    /// The cursor.
    pub r: Reader<'a>,
    depth: usize,
    /// Decoded bytes still allowed for this message's compressed payloads.
    budget: usize,
}

impl<'a> Decoder<'a> {
    /// Decoder at offset 0, depth 0.
    pub const fn new(buf: &'a [u8]) -> Self {
        Self::with_budget(buf, MAX_MESSAGE_DECODED_BYTES)
    }

    /// Like [`Self::new`] with a different decoded-bytes budget (tests).
    pub const fn with_budget(buf: &'a [u8], budget: usize) -> Self {
        Decoder {
            r: Reader::new(buf),
            depth: 0,
            budget,
        }
    }

    /// Decompress one tag-56500 payload at `at`, charging its decoded size to the message budget
    /// before anything is allocated.
    fn decompress_charged(
        &mut self,
        alg: Algorithm,
        modifier: u64,
        data: &[u8],
        at: usize,
    ) -> Result<Vec<u8>> {
        let n = framing::decoded_len(alg, modifier, data).map_err(|e| framing::rebase(e, at))?;
        if n > self.budget {
            return Err(Reader::err(
                at,
                format!(
                    "compressed payloads of one message expand beyond {} MiB",
                    MAX_MESSAGE_DECODED_BYTES >> 20
                ),
            ));
        }
        self.budget -= n;
        framing::decompress(alg, modifier, data).map_err(|e| framing::rebase(e, at))
    }

    /// Current nesting depth.
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Enter a container/tag at `offset`.
    pub fn enter(&mut self, offset: usize) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Reader::err(offset, "nesting too deep"));
        }
        Ok(())
    }

    /// Leave a container/tag.
    pub fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Skip one item at the current depth; returns its `(offset, len)`.
    pub fn skip_span(&mut self) -> Result<(usize, usize)> {
        let start = self.r.skip_at_depth(self.depth)?;
        Ok((start, self.r.pos() - start))
    }

    /// Decode one item.
    pub fn value(&mut self) -> Result<Value<'a>> {
        let h = self.r.head()?;
        self.value_from_head(h)
    }

    /// Decode one item whose head was already read.
    pub fn value_from_head(&mut self, h: Head) -> Result<Value<'a>> {
        Ok(match h.major {
            major::UINT => Value::Integer(i128::from(h.arg)),
            major::NINT => Value::Integer(-1 - i128::from(h.arg)),
            major::BYTES => Value::Bytes(self.r.bytes_item(h)?),
            major::TEXT => Value::Text(self.r.text_item(h)?),
            major::ARRAY => {
                self.enter(h.offset)?;
                let len = Len::of(&h);
                let mut v = Vec::with_capacity(len.capacity(&self.r));
                let mut i = 0u64;
                while len.more(&mut self.r, i)? {
                    v.push(self.value()?);
                    i += 1;
                }
                self.leave();
                Value::Array(v)
            }
            major::MAP => {
                self.enter(h.offset)?;
                let len = Len::of(&h);
                let mut v = Vec::with_capacity(len.capacity(&self.r) / 2);
                let mut i = 0u64;
                while len.more(&mut self.r, i)? {
                    let k = self.value()?;
                    if self.r.peek_break() {
                        return Err(Reader::err(
                            self.r.pos(),
                            "indefinite map has a key without a value",
                        ));
                    }
                    let val = self.value()?;
                    v.push((k, val));
                    i += 1;
                }
                self.leave();
                Value::Map(v)
            }
            major::TAG => {
                self.enter(h.offset)?;
                let v = self.tagged(h)?;
                self.leave();
                v
            }
            _ => match h.ai {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 | 23 => Value::Null,
                25 => Value::Float(f16_to_f64(h.arg as u16)),
                26 => Value::Float(f64::from(f32::from_bits(h.arg as u32))),
                27 => Value::Float(f64::from_bits(h.arg)),
                AI_INDEFINITE => return Err(Reader::err(h.offset, "unexpected break")),
                _ => Value::Simple(h.arg as u8),
            },
        })
    }

    fn tagged(&mut self, h: Head) -> Result<Value<'a>> {
        let tag = h.arg;
        Ok(match tag {
            TAG_DATETIME => {
                let c = self.r.head()?;
                if c.major != major::TEXT {
                    return Err(Reader::err(c.offset, "tag 0 content must be a text string"));
                }
                Value::DateTime(self.r.text_item(c)?)
            }
            _ if Order::from_tag(tag).is_some() => {
                let order = Order::from_tag(tag).unwrap_or(Order::C);
                Value::MultiDim(self.multi_dim(order, h.offset)?)
            }
            _ if ElemType::is_typed_array_tag(tag) => {
                Value::TypedArray(self.typed_array(tag, h.offset)?)
            }
            TAG_COMPRESSED => {
                let (alg, modifier, data, at) = self.r.compressed_triple()?;
                Value::Bytes(Cow::Owned(
                    self.decompress_charged(alg, modifier, &data, at)?,
                ))
            }
            _ => Value::Tag(tag, Box::new(self.value()?)),
        })
    }

    /// Content of a typed-array tag (head already read): a byte string or a tag 56500.
    fn typed_array(&mut self, tag: u64, offset: usize) -> Result<TypedArray<'a>> {
        ElemType::from_tag(tag).map_err(|e| framing::rebase(e, offset))?;
        let c = self.r.head()?;
        let (bytes, compression) = match c.major {
            major::BYTES => (self.r.bytes_item(c)?, None),
            major::TAG if c.arg == TAG_COMPRESSED => {
                self.enter(c.offset)?;
                let (algorithm, modifier, data, at) = self.r.compressed_triple()?;
                let out = self.decompress_charged(algorithm, modifier, &data, at)?;
                self.leave();
                (
                    Cow::Owned(out),
                    Some(Compression {
                        algorithm,
                        modifier,
                    }),
                )
            }
            _ => {
                return Err(Reader::err(
                    c.offset,
                    format!("typed array tag {tag} content must be a byte string or tag 56500"),
                ))
            }
        };
        TypedArray::from_wire(tag, bytes, compression).map_err(|e| framing::rebase(e, offset))
    }

    /// Content of tag 40/1040 (head already read): `[dims, elements]`.
    fn multi_dim(&mut self, order: Order, offset: usize) -> Result<MultiDimArray<'a>> {
        let h = self.r.head()?;
        if h.major != major::ARRAY || (!h.is_indefinite() && h.arg != 2) {
            return Err(Reader::err(
                h.offset,
                "MultiDimArray content must be a 2-element array [dimensions, elements]",
            ));
        }
        self.enter(h.offset)?;
        let dims = read_dims(&mut self.r)?;
        let eh = self.r.head()?;
        let elements = match eh.major {
            major::TAG if ElemType::is_typed_array_tag(eh.arg) => {
                self.enter(eh.offset)?;
                let t = self.typed_array(eh.arg, eh.offset)?;
                self.leave();
                Elements::Typed(t)
            }
            major::TAG => {
                return Err(Reader::err(
                    eh.offset,
                    format!("unsupported MultiDimArray element tag {}", eh.arg),
                ))
            }
            major::ARRAY => match self.value_from_head(eh)? {
                Value::Array(v) => Elements::Values(v),
                _ => return Err(Reader::err(eh.offset, "MultiDimArray elements")),
            },
            _ => {
                return Err(Reader::err(
                    eh.offset,
                    "MultiDimArray elements must be a typed array or an array",
                ))
            }
        };
        if h.is_indefinite() {
            let b = self.r.head()?;
            if !b.is_break() {
                return Err(Reader::err(
                    b.offset,
                    "MultiDimArray content must be a 2-element array [dimensions, elements]",
                ));
            }
        }
        self.leave();
        let (itemsize, have) = match &elements {
            Elements::Typed(t) => (t.elem.size, t.len()),
            Elements::Values(v) => (1, v.len()),
        };
        let n = checked_elems(&dims, itemsize).map_err(|e| framing::rebase(e, offset))?;
        if n != have {
            return Err(Reader::err(
                offset,
                format!("MultiDimArray dims {dims:?} need {n} elements, payload has {have}"),
            ));
        }
        let udims: Vec<usize> = dims.iter().map(|&d| d as usize).collect(); // each ≤ n, fits
        let elements = match (order, elements) {
            (Order::C, e) => e,
            (Order::F, Elements::Typed(t)) => {
                let mut out = vec![0u8; t.data.len()];
                f_to_c(&t.data, &udims, t.elem.size, &mut out)
                    .map_err(|e| framing::rebase(e, offset))?;
                Elements::Typed(TypedArray {
                    elem: t.elem,
                    data: Cow::Owned(out),
                    compression: t.compression,
                })
            }
            (Order::F, Elements::Values(v)) => {
                let mut slots: Vec<Option<Value<'a>>> = v.into_iter().map(Some).collect();
                let c: Option<Vec<Value<'a>>> = f_order_indices(&udims)
                    .map(|fi| slots.get_mut(fi).and_then(Option::take))
                    .collect();
                Elements::Values(c.ok_or_else(|| Reader::err(offset, "F→C permutation"))?)
            }
        };
        Ok(MultiDimArray {
            dims: udims,
            source_order: order,
            elements,
        })
    }

    /// Decode exactly one item spanning the whole buffer.
    pub fn decode_all(buf: &'a [u8]) -> Result<Value<'a>> {
        let mut d = Decoder::new(buf);
        let v = d.value()?;
        if !d.r.at_end() {
            return Err(Reader::err(
                d.r.pos(),
                format!("trailing bytes: {} after the item", buf.len() - d.r.pos()),
            ));
        }
        Ok(v)
    }
}

/// Read a MultiDimArray dimensions array (unsigned integers, at most [`MAX_DIMS`]).
pub fn read_dims(r: &mut Reader<'_>) -> Result<Vec<u64>> {
    let h = r.head()?;
    if h.major != major::ARRAY {
        return Err(Reader::err(
            h.offset,
            "MultiDimArray dimensions must be an array",
        ));
    }
    let len = Len::of(&h);
    let mut dims = Vec::new();
    let mut i = 0u64;
    while len.more(r, i)? {
        if dims.len() >= MAX_DIMS {
            return Err(Reader::err(
                h.offset,
                format!("more than {MAX_DIMS} dimensions"),
            ));
        }
        let d = r.head()?;
        if d.major != major::UINT {
            return Err(Reader::err(
                d.offset,
                "MultiDimArray dimension must be an unsigned integer",
            ));
        }
        dims.push(d.arg);
        i += 1;
    }
    Ok(dims)
}
