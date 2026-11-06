//! Stream V2 messages (docs/DESIGN.md §2.6–§2.7, §4.3 header scan, §4.4 Messages; PROTOCOL §8.1–§8.2, §13).
//!
//! * [`scan_header`]: the µs top-level scan used to classify every message (start / image / end,
//!   `series_id`, `series_unique_id`, `image_id`, `real_time`) without decoding or decompressing
//!   `data` or the start-message arrays (turbo raw path, DESIGN §5 Q9).
//! * [`decode_message`] / [`decode_message_with`]: the generic value tree of the whole message
//!   (Python `decode_message`), optionally skipping `flatfield`, `pixel_mask` and
//!   `countrate_correction_lookup_table` with an O(1) length skip ([`DecodeOptions`]).
//! * [`StartSummary`]: the start-message scalars the receive paths need (count_time, frame_time,
//!   image size and dtype, …), from a scan or from a decoded message.
//!
//! Scalar fields are read leniently: a known key whose value has another type yields `None`
//! (the item is skipped), and pass-through tags outside the data model that precede a scalar are
//! skipped, as the vendor parser does ([G2-P]). Structural CBOR errors (truncation, bad heads,
//! nesting) are errors. A message is exactly one top-level map: trailing bytes are an error.
//! Leading pass-through tags before the top-level map (e.g. 55799 self-describe) are skipped.

use std::borrow::Cow;

use dectris_common::Result;

use crate::cbor::{major, Decoder, Head, Len, Reader, Value, MAX_DEPTH};
use crate::typed_array::is_data_model_tag;

/// Message `type`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MessageKind {
    /// `"start"`
    Start,
    /// `"image"`
    Image,
    /// `"end"`
    End,
    /// Any other `type` text (the receivers log one WARNING and continue).
    Other(String),
    /// No text `type` key.
    #[default]
    Missing,
}

impl MessageKind {
    /// From the `type` text.
    pub fn from_text(t: &str) -> Self {
        match t {
            "start" => MessageKind::Start,
            "image" => MessageKind::Image,
            "end" => MessageKind::End,
            other => MessageKind::Other(other.to_owned()),
        }
    }

    /// The `type` text (`""` when missing).
    pub fn as_str(&self) -> &str {
        match self {
            MessageKind::Start => "start",
            MessageKind::Image => "image",
            MessageKind::End => "end",
            MessageKind::Other(s) => s,
            MessageKind::Missing => "",
        }
    }
}

/// Result of the top-level header scan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// `type`.
    pub kind: MessageKind,
    /// `series_id`.
    pub series_id: Option<u64>,
    /// `series_unique_id`.
    pub series_unique_id: Option<String>,
    /// `image_id` (image messages).
    pub image_id: Option<u64>,
    /// `real_time` `[numerator, denominator]` (image messages).
    pub real_time: Option<[u64; 2]>,
}

/// Skip pass-through tags (outside the data model) in front of the next item. At most
/// [`MAX_DEPTH`] of them.
fn skip_foreign_tags(r: &mut Reader<'_>) -> Result<()> {
    for _ in 0..=MAX_DEPTH {
        let h = r.peek_head()?;
        if h.major == major::TAG && !is_data_model_tag(h.arg) {
            r.head()?;
        } else {
            return Ok(());
        }
    }
    Err(Reader::err(r.pos(), "nesting too deep"))
}

/// Lenient scalar read: `f` gets the item's head (after foreign tags); returning `Ok(None)`
/// means "other type" and the whole item is skipped instead.
fn scalar<'a, T>(
    r: &mut Reader<'a>,
    depth: usize,
    f: impl FnOnce(Head, &mut Reader<'a>) -> Result<Option<T>>,
) -> Result<Option<T>> {
    let start = r.pos();
    skip_foreign_tags(r)?;
    let h = r.head()?;
    match f(h, r)? {
        Some(v) => Ok(Some(v)),
        None => {
            r.set_pos(start);
            r.skip_at_depth(depth)?;
            Ok(None)
        }
    }
}

pub(crate) fn scalar_u64(r: &mut Reader<'_>, depth: usize) -> Result<Option<u64>> {
    scalar(r, depth, |h, _| {
        Ok((h.major == major::UINT).then_some(h.arg))
    })
}

pub(crate) fn scalar_text<'a>(r: &mut Reader<'a>, depth: usize) -> Result<Option<Cow<'a, str>>> {
    scalar(r, depth, |h, r| {
        if h.major == major::TEXT {
            Ok(Some(r.text_item(h)?))
        } else {
            Ok(None)
        }
    })
}

pub(crate) fn scalar_f64(r: &mut Reader<'_>, depth: usize) -> Result<Option<f64>> {
    scalar(r, depth, |h, _| {
        Ok(match (h.major, h.ai) {
            (major::UINT, _) => Some(h.arg as f64),
            (major::NINT, _) => Some(-1.0 - h.arg as f64),
            (major::SIMPLE, 25) => Some(crate::cbor::f16_to_f64(h.arg as u16)),
            (major::SIMPLE, 26) => Some(f64::from(f32::from_bits(h.arg as u32))),
            (major::SIMPLE, 27) => Some(f64::from_bits(h.arg)),
            _ => None,
        })
    })
}

/// `[u64, u64]` (Rational).
pub(crate) fn scalar_rational(r: &mut Reader<'_>, depth: usize) -> Result<Option<[u64; 2]>> {
    scalar(r, depth, |h, r| {
        if h.major != major::ARRAY || (!h.is_indefinite() && h.arg != 2) {
            return Ok(None);
        }
        let mut out = [0u64; 2];
        for slot in &mut out {
            match scalar_u64(r, depth + 1)? {
                Some(v) => *slot = v,
                None => return Ok(None),
            }
        }
        if h.is_indefinite() {
            if !r.peek_break() {
                return Ok(None);
            }
            r.head()?;
        }
        Ok(Some(out))
    })
}

/// Array of text (e.g. `channels`).
pub(crate) fn scalar_text_list(r: &mut Reader<'_>, depth: usize) -> Result<Option<Vec<String>>> {
    scalar(r, depth, |h, r| {
        if h.major != major::ARRAY {
            return Ok(None);
        }
        let len = Len::of(&h);
        let mut v = Vec::new();
        let mut i = 0u64;
        while len.more(r, i)? {
            match scalar_text(r, depth + 1)? {
                Some(t) => v.push(t.into_owned()),
                None => return Ok(None),
            }
            i += 1;
        }
        Ok(Some(v))
    })
}

/// Open the top-level map (leading pass-through tags skipped).
fn open_top_map(r: &mut Reader<'_>) -> Result<Len> {
    skip_foreign_tags(r)?;
    let h = r.head()?;
    if h.major != major::MAP {
        return Err(Reader::err(
            h.offset,
            format!("message is not a CBOR map (major type {})", h.major),
        ));
    }
    Ok(Len::of(&h))
}

fn check_trailing(r: &Reader<'_>) -> Result<()> {
    if r.at_end() {
        Ok(())
    } else {
        Err(Reader::err(
            r.pos(),
            format!(
                "trailing bytes: {} after the message map",
                r.buf().len() - r.pos()
            ),
        ))
    }
}

/// Walk the top-level map: the five header keys are parsed here, every other text key is
/// offered to `other(key, reader)`, which returns `true` if it consumed the value (else it is
/// skipped). Values sit at depth 1.
pub(crate) fn walk_top<'a>(
    raw: &'a [u8],
    mut other: impl FnMut(&str, &mut Reader<'a>) -> Result<bool>,
) -> Result<Header> {
    let mut r = Reader::new(raw);
    let len = open_top_map(&mut r)?;
    let mut h = Header::default();
    let mut i = 0u64;
    while len.more(&mut r, i)? {
        i += 1;
        let key = scalar_text(&mut r, 1)?;
        if r.peek_break() {
            return Err(Reader::err(
                r.pos(),
                "indefinite map has a key without a value",
            ));
        }
        let Some(key) = key else {
            r.skip_at_depth(1)?;
            continue;
        };
        match &*key {
            "type" => {
                h.kind = scalar_text(&mut r, 1)?
                    .map_or(MessageKind::Missing, |t| MessageKind::from_text(&t));
            }
            "series_id" => h.series_id = scalar_u64(&mut r, 1)?,
            "series_unique_id" => h.series_unique_id = scalar_text(&mut r, 1)?.map(Cow::into_owned),
            "image_id" => h.image_id = scalar_u64(&mut r, 1)?,
            "real_time" => h.real_time = scalar_rational(&mut r, 1)?,
            k => {
                if !other(k, &mut r)? {
                    r.skip_at_depth(1)?;
                }
            }
        }
    }
    check_trailing(&r)?;
    Ok(h)
}

/// The µs header scan (DESIGN §4.3, §4.4 raw path): `type`, `series_id`, `series_unique_id`,
/// `image_id`, `real_time`; `data` and every array are skipped without decoding or
/// decompression.
pub fn scan_header(raw: &[u8]) -> Result<Header> {
    walk_top(raw, |_, _| Ok(false))
}

/// Options of [`decode_message_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Replace the top-level `flatfield`, `pixel_mask` and `countrate_correction_lookup_table`
    /// values by [`Value::Skipped`] (O(1) length skip, DESIGN §4.4; the receive paths set this unless
    /// the `stream_decode_start_arrays` option is on).
    pub skip_start_arrays: bool,
}

/// Top-level keys skipped by [`DecodeOptions::skip_start_arrays`].
pub const START_ARRAY_KEYS: [&str; 3] = [
    "flatfield",
    "pixel_mask",
    "countrate_correction_lookup_table",
];

/// A decoded message: its `type` and every top-level entry in wire order.
#[derive(Debug, Clone, PartialEq)]
pub struct Message<'a> {
    /// `type`.
    pub kind: MessageKind,
    /// All top-level entries, including `type` (Python drops nothing; DESIGN §2.7).
    pub entries: Vec<(Value<'a>, Value<'a>)>,
}

impl<'a> Message<'a> {
    /// Value of a text key (last duplicate wins).
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| matches!(k, Value::Text(t) if t == key))
            .map(|(_, v)| v)
    }

    /// The header fields, from the decoded entries (same leniency as [`scan_header`]).
    pub fn header(&self) -> Header {
        Header {
            kind: self.kind.clone(),
            series_id: self.get("series_id").and_then(Value::as_u64),
            series_unique_id: self
                .get("series_unique_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            image_id: self.get("image_id").and_then(Value::as_u64),
            real_time: self.get("real_time").and_then(rational),
        }
    }

    /// Detach from the message buffer (e.g. to cache a start message).
    pub fn into_owned(self) -> Message<'static> {
        Message {
            kind: self.kind,
            entries: self
                .entries
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        }
    }
}

fn rational(v: &Value<'_>) -> Option<[u64; 2]> {
    match v.peel_foreign_tags() {
        Value::Array(a) => match a.as_slice() {
            [n, d] => Some([n.as_u64()?, d.as_u64()?]),
            _ => None,
        },
        _ => None,
    }
}

/// Decode a whole message (Python `decode_message`, DESIGN §2.7): every top-level key, typed
/// arrays decompressed, tags resolved.
pub fn decode_message(raw: &[u8]) -> Result<Message<'_>> {
    decode_message_with(raw, DecodeOptions::default())
}

/// [`decode_message`] with options.
pub fn decode_message_with(raw: &[u8], opts: DecodeOptions) -> Result<Message<'_>> {
    let mut d = Decoder::new(raw);
    let len = open_top_map(&mut d.r)?;
    d.enter(0)?;
    let mut entries = Vec::with_capacity(len.capacity(&d.r).min(64));
    let mut i = 0u64;
    while len.more(&mut d.r, i)? {
        i += 1;
        let key = d.value()?;
        if d.r.peek_break() {
            return Err(Reader::err(
                d.r.pos(),
                "indefinite map has a key without a value",
            ));
        }
        let skip = opts.skip_start_arrays
            && matches!(&key, Value::Text(t) if START_ARRAY_KEYS.contains(&&**t));
        let val = if skip {
            let (offset, len) = d.skip_span()?;
            Value::Skipped { offset, len }
        } else {
            d.value()?
        };
        entries.push((key, val));
    }
    d.leave();
    check_trailing(&d.r)?;
    let kind = entries
        .iter()
        .rev()
        .find(|(k, _)| matches!(k, Value::Text(t) if t == "type"))
        .and_then(|(_, v)| v.as_str())
        .map_or(MessageKind::Missing, MessageKind::from_text);
    Ok(Message { kind, entries })
}

/// The start-message scalars the receive paths use (DESIGN §2.6 prealloc, §2.7 count_time, §4.3
/// window). Every field is `None` when absent or of another type.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StartSummary {
    /// `series_id`.
    pub series_id: Option<u64>,
    /// `series_unique_id`.
    pub series_unique_id: Option<String>,
    /// `count_time` (s).
    pub count_time: Option<f64>,
    /// `frame_time` (s).
    pub frame_time: Option<f64>,
    /// `image_size_x` (width = cols).
    pub image_size_x: Option<u64>,
    /// `image_size_y` (height = rows).
    pub image_size_y: Option<u64>,
    /// `image_dtype` (`uint8` / `uint16` / `uint32`).
    pub image_dtype: Option<String>,
    /// `number_of_images`.
    pub number_of_images: Option<u64>,
    /// `channels`.
    pub channels: Option<Vec<String>>,
    /// `arm_date` (RFC 3339 text).
    pub arm_date: Option<String>,
    /// `saturation_value`.
    pub saturation_value: Option<u64>,
}

impl StartSummary {
    /// Scan a raw start message (arrays skipped in O(1); works for any message type).
    pub fn scan(raw: &[u8]) -> Result<(Header, StartSummary)> {
        let mut s = StartSummary::default();
        let h = walk_top(raw, |key, r| {
            match key {
                "count_time" => s.count_time = scalar_f64(r, 1)?,
                "frame_time" => s.frame_time = scalar_f64(r, 1)?,
                "image_size_x" => s.image_size_x = scalar_u64(r, 1)?,
                "image_size_y" => s.image_size_y = scalar_u64(r, 1)?,
                "image_dtype" => s.image_dtype = scalar_text(r, 1)?.map(Cow::into_owned),
                "number_of_images" => s.number_of_images = scalar_u64(r, 1)?,
                "channels" => s.channels = scalar_text_list(r, 1)?,
                "saturation_value" => s.saturation_value = scalar_u64(r, 1)?,
                "arm_date" => {
                    // tag 0 + text (a data-model tag, so read it explicitly)
                    let start = r.pos();
                    let h = r.peek_head()?;
                    if h.major == major::TAG && h.arg == 0 {
                        r.head()?;
                        s.arm_date = scalar_text(r, 2)?.map(Cow::into_owned);
                    } else {
                        r.set_pos(start);
                        s.arm_date = scalar_text(r, 1)?.map(Cow::into_owned);
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        s.series_id = h.series_id;
        s.series_unique_id = h.series_unique_id.clone();
        Ok((h, s))
    }

    /// From a decoded message.
    pub fn from_message(m: &Message<'_>) -> StartSummary {
        let f = |k: &str| m.get(k).and_then(Value::as_f64);
        let u = |k: &str| m.get(k).and_then(Value::as_u64);
        let t = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
        StartSummary {
            series_id: u("series_id"),
            series_unique_id: t("series_unique_id"),
            count_time: f("count_time"),
            frame_time: f("frame_time"),
            image_size_x: u("image_size_x"),
            image_size_y: u("image_size_y"),
            image_dtype: t("image_dtype"),
            number_of_images: u("number_of_images"),
            channels: m.get("channels").and_then(|v| match v.peel_foreign_tags() {
                Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_owned)).collect(),
                _ => None,
            }),
            arm_date: t("arm_date"),
            saturation_value: u("saturation_value"),
        }
    }

    /// `(itemsize, rows, cols)` predicted from `image_dtype` and the image size (DESIGN §2.6 R9
    /// prealloc), if all three are present and valid.
    pub fn predicted_frame(&self) -> Option<(usize, usize, usize)> {
        let size = match self.image_dtype.as_deref()? {
            "uint8" => 1,
            "uint16" => 2,
            "uint32" => 4,
            _ => return None,
        };
        let rows = usize::try_from(self.image_size_y?).ok()?;
        let cols = usize::try_from(self.image_size_x?).ok()?;
        (rows > 0 && cols > 0).then_some((size, rows, cols))
    }
}
