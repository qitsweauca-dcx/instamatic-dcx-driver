//! Encoders for tests and fake streams (docs/DESIGN.md §8): a CBOR [`Writer`]
//! (including indefinite lengths, chunked strings, every float width), the bslz4 / lz4
//! HDF5-framing encoders (forward bitshuffle + `lz4_flex` block compression), and Stream V2
//! start / image / end message builders.
//!
//! Nothing on the driver's receive path uses this module. Other crates' tests (fake_push,
//! receiver tests) can build wire-exact messages with it.

use dectris_common::{DriverError, Result};

use crate::bitshuffle;
use crate::cbor::{major, AI_INDEFINITE};
use crate::framing::{Algorithm, MAX_BSLZ4_ELEM};
use crate::typed_array::{ElemType, TAG_COMPRESSED, TAG_DATETIME};

/// Minimal CBOR writer (always minimal-length heads unless [`Writer::head_wide`] is used).
#[derive(Debug, Default, Clone)]
pub struct Writer {
    /// Encoded bytes.
    pub buf: Vec<u8>,
}

impl Writer {
    /// Empty writer.
    pub fn new() -> Self {
        Writer::default()
    }

    /// The encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// A head with the minimal argument width.
    pub fn head(&mut self, major: u8, arg: u64) -> &mut Self {
        let m = major << 5;
        if arg < 24 {
            self.buf.push(m | arg as u8);
        } else if arg <= u64::from(u8::MAX) {
            self.buf.push(m | 24);
            self.buf.push(arg as u8);
        } else if arg <= u64::from(u16::MAX) {
            self.buf.push(m | 25);
            self.buf.extend_from_slice(&(arg as u16).to_be_bytes());
        } else if arg <= u64::from(u32::MAX) {
            self.buf.push(m | 26);
            self.buf.extend_from_slice(&(arg as u32).to_be_bytes());
        } else {
            self.buf.push(m | 27);
            self.buf.extend_from_slice(&arg.to_be_bytes());
        }
        self
    }

    /// A head with an explicit additional info 24–27 (non-minimal encodings, PROTOCOL §8.1).
    pub fn head_wide(&mut self, major: u8, ai: u8, arg: u64) -> &mut Self {
        self.buf.push((major << 5) | (ai & 0x1F));
        match ai {
            24 => self.buf.push(arg as u8),
            25 => self.buf.extend_from_slice(&(arg as u16).to_be_bytes()),
            26 => self.buf.extend_from_slice(&(arg as u32).to_be_bytes()),
            27 => self.buf.extend_from_slice(&arg.to_be_bytes()),
            _ => {}
        }
        self
    }

    /// Unsigned integer.
    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.head(major::UINT, v)
    }

    /// Signed integer.
    pub fn int(&mut self, v: i64) -> &mut Self {
        if v >= 0 {
            self.head(major::UINT, v as u64)
        } else {
            self.head(major::NINT, (-1 - v) as u64)
        }
    }

    /// Byte string.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.head(major::BYTES, b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }

    /// Text string.
    pub fn text(&mut self, s: &str) -> &mut Self {
        self.head(major::TEXT, s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }

    /// Indefinite byte string made of `chunks`.
    pub fn bytes_chunked(&mut self, chunks: &[&[u8]]) -> &mut Self {
        self.buf.push((major::BYTES << 5) | AI_INDEFINITE);
        for c in chunks {
            self.bytes(c);
        }
        self.brk()
    }

    /// Indefinite text string made of `chunks`.
    pub fn text_chunked(&mut self, chunks: &[&str]) -> &mut Self {
        self.buf.push((major::TEXT << 5) | AI_INDEFINITE);
        for c in chunks {
            self.text(c);
        }
        self.brk()
    }

    /// Definite array head.
    pub fn array(&mut self, n: u64) -> &mut Self {
        self.head(major::ARRAY, n)
    }

    /// Definite map head (`n` pairs).
    pub fn map(&mut self, n: u64) -> &mut Self {
        self.head(major::MAP, n)
    }

    /// Indefinite array head (close with [`brk`](Self::brk)).
    pub fn array_indef(&mut self) -> &mut Self {
        self.buf.push((major::ARRAY << 5) | AI_INDEFINITE);
        self
    }

    /// Indefinite map head (close with [`brk`](Self::brk)).
    pub fn map_indef(&mut self) -> &mut Self {
        self.buf.push((major::MAP << 5) | AI_INDEFINITE);
        self
    }

    /// Break stop code.
    pub fn brk(&mut self) -> &mut Self {
        self.buf.push(0xFF);
        self
    }

    /// Tag head.
    pub fn tag(&mut self, t: u64) -> &mut Self {
        self.head(major::TAG, t)
    }

    /// `false` / `true`.
    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.buf.push(if b { 0xF5 } else { 0xF4 });
        self
    }

    /// `null`.
    pub fn null(&mut self) -> &mut Self {
        self.buf.push(0xF6);
        self
    }

    /// `undefined`.
    pub fn undefined(&mut self) -> &mut Self {
        self.buf.push(0xF7);
        self
    }

    /// Simple value (`v < 24` inline, else one-byte form).
    pub fn simple(&mut self, v: u8) -> &mut Self {
        if v < 24 {
            self.buf.push(0xE0 | v);
        } else {
            self.buf.push(0xF8);
            self.buf.push(v);
        }
        self
    }

    /// float16 from raw bits.
    pub fn f16_bits(&mut self, bits: u16) -> &mut Self {
        self.buf.push(0xF9);
        self.buf.extend_from_slice(&bits.to_be_bytes());
        self
    }

    /// float32.
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.buf.push(0xFA);
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
        self
    }

    /// float64.
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.buf.push(0xFB);
        self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
        self
    }

    /// Raw pre-encoded bytes.
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Tag 0 date/time.
    pub fn datetime(&mut self, s: &str) -> &mut Self {
        self.tag(TAG_DATETIME).text(s)
    }

    /// Tag 56500 `[algorithm, modifier, data]`.
    pub fn compressed(&mut self, algorithm: &str, modifier: u64, data: &[u8]) -> &mut Self {
        self.tag(TAG_COMPRESSED)
            .array(3)
            .text(algorithm)
            .uint(modifier)
            .bytes(data)
    }

    /// `[num, den]` rational.
    pub fn rational(&mut self, num: u64, den: u64) -> &mut Self {
        self.array(2).uint(num).uint(den)
    }
}

/// bslz4 encode (bitshuffle HDF5-filter framing). `block_size` in bytes must be a non-zero
/// multiple of `8·elem` (8192 is the encoder default, PROTOCOL §9.5). `data.len()` need not be a
/// multiple of `elem`: as in the vendor format, the `orig % (8·elem)` tail is stored raw.
pub fn compress_bslz4(data: &[u8], elem: usize, block_size: usize) -> Result<Vec<u8>> {
    if elem == 0 || elem as u64 > MAX_BSLZ4_ELEM {
        return Err(DriverError::InvalidArg(format!(
            "bslz4 element size {elem}"
        )));
    }
    let unit = elem * 8;
    if block_size == 0 || block_size % unit != 0 || block_size > i32::MAX as usize {
        return Err(DriverError::InvalidArg(format!(
            "bslz4 block size {block_size} must be a non-zero multiple of {unit}"
        )));
    }
    let n = data.len();
    let mut out = Vec::with_capacity(n / 2 + 64);
    out.extend_from_slice(&(n as u64).to_be_bytes());
    out.extend_from_slice(&(block_size as u32).to_be_bytes());
    let leftover = n % unit;
    let body = data.get(..n - leftover).unwrap_or_default();
    let mut shuffled = vec![0u8; block_size.min(body.len())];
    for blk in body.chunks(block_size) {
        let sh = shuffled
            .get_mut(..blk.len())
            .ok_or_else(|| DriverError::Internal("encode scratch".into()))?;
        bitshuffle::shuffle_block(blk, sh, elem)?;
        let c = lz4_flex::block::compress(sh);
        out.extend_from_slice(&(c.len() as u32).to_be_bytes());
        out.extend_from_slice(&c);
    }
    out.extend_from_slice(data.get(n - leftover..).unwrap_or_default());
    Ok(out)
}

/// lz4 encode (HDF5 LZ4-filter framing). A block whose LZ4 form is not smaller than the block is
/// stored raw (`compressed_size == block size`), as the HDF5 filter does.
pub fn compress_lz4(data: &[u8], block_size: usize) -> Result<Vec<u8>> {
    if block_size == 0 || block_size > i32::MAX as usize {
        return Err(DriverError::InvalidArg(format!(
            "lz4 block size {block_size}"
        )));
    }
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    out.extend_from_slice(&(data.len() as u64).to_be_bytes());
    out.extend_from_slice(&(block_size as u32).to_be_bytes());
    for blk in data.chunks(block_size) {
        let c = lz4_flex::block::compress(blk);
        if c.len() >= blk.len() {
            out.extend_from_slice(&(blk.len() as u32).to_be_bytes());
            out.extend_from_slice(blk);
        } else {
            out.extend_from_slice(&(c.len() as u32).to_be_bytes());
            out.extend_from_slice(&c);
        }
    }
    Ok(out)
}

/// How a builder stores an image or array payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// Plain byte string (`compression: none`).
    Raw,
    /// Tag 56500 bslz4 with this block size (modifier = itemsize).
    Bslz4 {
        /// Block size in bytes.
        block_size: usize,
    },
    /// Tag 56500 bslz4 with an explicit (possibly wrong) modifier [R18].
    Bslz4Modifier {
        /// Block size in bytes (a multiple of `8·modifier`).
        block_size: usize,
        /// Wire modifier.
        modifier: u64,
    },
    /// Tag 56500 lz4 with this block size and modifier 0.
    Lz4 {
        /// Block size in bytes.
        block_size: usize,
    },
}

impl Payload {
    /// bslz4 with the encoder's default 8192-byte blocks.
    pub const BSLZ4: Payload = Payload::Bslz4 { block_size: 8192 };
    /// lz4 with one 1 GiB block (HDF5 LZ4 default: the whole chunk is one block).
    pub const LZ4: Payload = Payload::Lz4 {
        block_size: 1 << 30,
    };
}

/// Write the typed-array item (`tag (bytes | 56500[...])`) for little-endian element bytes.
pub fn write_typed(w: &mut Writer, elem_tag: u64, le_bytes: &[u8], payload: Payload) -> Result<()> {
    let e = ElemType::from_tag(elem_tag)?;
    w.tag(elem_tag);
    let wire: std::borrow::Cow<'_, [u8]> = if e.big_endian {
        let mut v = le_bytes.to_vec();
        crate::typed_array::swap_bytes_in_place(&mut v, e.size);
        std::borrow::Cow::Owned(v)
    } else {
        std::borrow::Cow::Borrowed(le_bytes)
    };
    match payload {
        Payload::Raw => {
            w.bytes(&wire);
        }
        Payload::Bslz4 { block_size } => {
            let c = compress_bslz4(&wire, e.size, block_size)?;
            w.compressed(Algorithm::Bslz4.name(), e.size as u64, &c);
        }
        Payload::Bslz4Modifier {
            block_size,
            modifier,
        } => {
            let m = usize::try_from(modifier)
                .map_err(|_| DriverError::InvalidArg("modifier".into()))?;
            let c = compress_bslz4(&wire, m, block_size)?;
            w.compressed(Algorithm::Bslz4.name(), modifier, &c);
        }
        Payload::Lz4 { block_size } => {
            let c = compress_lz4(&wire, block_size)?;
            w.compressed(Algorithm::Lz4.name(), 0, &c);
        }
    }
    Ok(())
}

/// Write a tag-40 MultiDimArray `[dims, typed]` (C-order little-endian element bytes).
pub fn write_multidim(
    w: &mut Writer,
    dims: &[u64],
    elem_tag: u64,
    le_bytes: &[u8],
    payload: Payload,
) -> Result<()> {
    w.tag(crate::typed_array::TAG_MULTI_DIM)
        .array(2)
        .array(dims.len() as u64);
    for &d in dims {
        w.uint(d);
    }
    write_typed(w, elem_tag, le_bytes, payload)
}

/// Fields of a built image message.
#[derive(Debug, Clone)]
pub struct ImageSpec<'d> {
    /// `series_id`.
    pub series_id: u64,
    /// `series_unique_id`.
    pub series_unique_id: String,
    /// `image_id`.
    pub image_id: u64,
    /// `real_time`.
    pub real_time: [u64; 2],
    /// `(rows, cols)`.
    pub shape: (u64, u64),
    /// Element tag (64, 68, 69, 70, …).
    pub elem_tag: u64,
    /// Little-endian C-order element bytes.
    pub data: &'d [u8],
    /// Storage.
    pub payload: Payload,
    /// Channel key (`threshold_1`).
    pub channel: String,
}

impl<'d> ImageSpec<'d> {
    /// A spec with typical values for a `(rows, cols)` frame of `elem_tag`.
    pub fn new(
        series_id: u64,
        image_id: u64,
        shape: (u64, u64),
        elem_tag: u64,
        data: &'d [u8],
        payload: Payload,
    ) -> Self {
        ImageSpec {
            series_id,
            series_unique_id: format!("01HSERIES{series_id:016}"),
            image_id,
            real_time: [4_000_000, 40_000_000],
            shape,
            elem_tag,
            data,
            payload,
            channel: crate::image::DEFAULT_CHANNEL.to_owned(),
        }
    }
}

/// Build a Stream V2 image message (`type` first, as the DCU sends it).
pub fn image_message(s: &ImageSpec<'_>) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    w.map(9).text("type").text("image");
    w.text("series_id").uint(s.series_id);
    w.text("series_unique_id").text(&s.series_unique_id);
    w.text("image_id").uint(s.image_id);
    w.text("real_time").rational(s.real_time[0], s.real_time[1]);
    w.text("series_date")
        .datetime("2026-10-21T01:02:03.456789+00:00");
    w.text("start_time")
        .rational(s.image_id * 40_000_000, 40_000_000);
    w.text("stop_time")
        .rational(s.image_id * 40_000_000 + s.real_time[0], 40_000_000);
    w.text("data").map(1).text(&s.channel);
    write_multidim(
        &mut w,
        &[s.shape.0, s.shape.1],
        s.elem_tag,
        s.data,
        s.payload,
    )?;
    Ok(w.into_bytes())
}

/// Fields of a built start message.
#[derive(Debug, Clone)]
pub struct StartSpec {
    /// `series_id`.
    pub series_id: u64,
    /// `series_unique_id`.
    pub series_unique_id: String,
    /// `count_time` (s).
    pub count_time: f64,
    /// `frame_time` (s).
    pub frame_time: f64,
    /// `(image_size_y, image_size_x)` = `(rows, cols)`.
    pub shape: (u64, u64),
    /// `image_dtype`.
    pub image_dtype: String,
    /// `number_of_images`.
    pub number_of_images: u64,
    /// Include `flatfield` (tag 85), `pixel_mask` (tag 70) and
    /// `countrate_correction_lookup_table` (header_detail=all), stored with this payload.
    pub header_detail_all: Option<Payload>,
}

impl StartSpec {
    /// Typical start spec.
    pub fn new(series_id: u64, shape: (u64, u64), image_dtype: &str) -> Self {
        StartSpec {
            series_id,
            series_unique_id: format!("01HSERIES{series_id:016}"),
            count_time: 0.099_999,
            frame_time: 0.1,
            shape,
            image_dtype: image_dtype.to_owned(),
            number_of_images: 10,
            header_detail_all: None,
        }
    }
}

/// The deterministic flatfield (`f32`) used by [`start_message`].
pub fn flatfield_values(n: usize) -> Vec<f32> {
    (0..n).map(|i| 1.0 + (i % 7) as f32 * 0.01).collect()
}

/// The deterministic pixel mask (`u32`) used by [`start_message`].
pub fn pixel_mask_values(n: usize) -> Vec<u32> {
    (0..n).map(|i| if i % 97 == 0 { 1 } else { 0 }).collect()
}

/// Build a Stream V2 start message.
pub fn start_message(s: &StartSpec) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    let extra = if s.header_detail_all.is_some() { 3 } else { 0 };
    w.map(15 + extra).text("type").text("start");
    w.text("series_id").uint(s.series_id);
    w.text("series_unique_id").text(&s.series_unique_id);
    w.text("arm_date")
        .datetime("2026-10-21T01:02:03.000000+00:00");
    w.text("beam_center_x").f64(514.25);
    w.text("beam_center_y").f64(531.5);
    w.text("channels")
        .array(1)
        .text(crate::image::DEFAULT_CHANNEL);
    w.text("count_time").f64(s.count_time);
    w.text("frame_time").f64(s.frame_time);
    w.text("image_size_x").uint(s.shape.1);
    w.text("image_size_y").uint(s.shape.0);
    w.text("image_dtype").text(&s.image_dtype);
    w.text("number_of_images").uint(s.number_of_images);
    w.text("threshold_energy")
        .map(1)
        .text(crate::image::DEFAULT_CHANNEL)
        .f64(4000.0);
    if let Some(p) = s.header_detail_all {
        let n = usize::try_from(s.shape.0 * s.shape.1)
            .map_err(|_| DriverError::InvalidArg("shape".into()))?;
        let ff: Vec<u8> = flatfield_values(n)
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let pm: Vec<u8> = pixel_mask_values(n)
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        w.text("flatfield")
            .map(1)
            .text(crate::image::DEFAULT_CHANNEL);
        write_multidim(&mut w, &[s.shape.0, s.shape.1], 85, &ff, p)?;
        w.text("pixel_mask")
            .map(1)
            .text(crate::image::DEFAULT_CHANNEL);
        write_multidim(&mut w, &[s.shape.0, s.shape.1], 70, &pm, p)?;
        let lut: Vec<u8> = (0..1024u32).flat_map(|v| (v * 3).to_le_bytes()).collect();
        w.text("countrate_correction_lookup_table");
        write_typed(&mut w, 70, &lut, Payload::Raw)?;
    }
    w.text("user_data").null();
    Ok(w.into_bytes())
}

/// Build a Stream V2 end message.
pub fn end_message(series_id: u64, series_unique_id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.map(3)
        .text("type")
        .text("end")
        .text("series_id")
        .uint(series_id)
        .text("series_unique_id")
        .text(series_unique_id);
    w.into_bytes()
}
