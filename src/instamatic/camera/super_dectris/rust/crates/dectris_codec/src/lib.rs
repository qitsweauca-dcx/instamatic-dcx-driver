//! Pure decoders for Stream V2 payloads (docs/DESIGN.md §4.4-§4.6). No I/O, no pyo3.
//! Reached as `dectris_core::codec::…`.
//!
//! | module | contents | DESIGN |
//! |---|---|---|
//! | [`cbor`] | hand-rolled zero-copy CBOR reader, value tree, depth-limited iterative skip | §4.4 |
//! | [`typed_array`] | RFC 8746 typed-array tags 64–87, `TYPED_ARRAY_DTYPES`, byte swap, F→C copy | §2.8, §4.4 |
//! | [`framing`] | bslz4 / lz4 HDF5 framing, every validation rule, `decompress` oracle hook | §4.5 |
//! | [`bitshuffle`] | inverse (hot path) and forward bitshuffle of one block | §4.5, PROTOCOL §9.5 |
//! | [`message`] | Stream V2 messages: header scan (turbo classify), generic decode, start summary | §2.6, §2.7, §4.3, §4.4 |
//! | [`image`] | image payload: validate-then-decode-into (`threshold_1`), parts constructor, `decode_frame` | §2.7, §2.8, §4.6 |
//! | [`lz4`] | safe LZ4 raw-block decoder used by [`framing`] (replaces lz4_flex's decoder for speed, DESIGN §12 #20) | §4.5 |
//! | [`encode`] | CBOR writer, bslz4/lz4 encoders, Stream V2 message builders (tests and fakes) | §8 |
//!
//! Errors are `dectris_common::DriverError::Decode { offset, msg }` (Python `StreamDecodeError`)
//! unless stated otherwise. Nothing here logs: conditions that DESIGN wants logged once per series
//! (modifier ≠ itemsize, [R18]) are returned as data for the stream / py layers.
//!
//! Design tags such as `[R18]`, `[G2-K]` in these docs refer to docs/DESIGN.md, not to items.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#![allow(rustdoc::broken_intra_doc_links)] // the [Rn]/[G2-x] design tags are not links

pub mod bitshuffle;
pub mod cbor;
pub mod encode;
pub mod framing;
pub mod image;
pub mod lz4;
pub mod message;
pub mod typed_array;

pub use cbor::{Value, MAX_DEPTH};
pub use framing::{decompress_named as decompress, Algorithm, Scratch, MAX_DECODED_BYTES};
pub use image::{
    decode_frame, parse_image_message, DecodeNotes, DecodedFrame, Encoding, ImagePayload,
    ModifierMismatch, Order, ParsedImage, DEFAULT_CHANNEL,
};
pub use message::{
    decode_message, decode_message_with, scan_header, DecodeOptions, Header, Message, MessageKind,
    StartSummary,
};
pub use typed_array::{ElemKind, ElemType, TypedArray, TYPED_ARRAY_DTYPES};
