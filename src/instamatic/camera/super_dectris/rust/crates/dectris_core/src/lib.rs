//! Facade crate (docs/DESIGN.md §1).
//!
//! `codec`, `simplon` and `stream` are separate workspace crates; this crate re-exports them so every path
//! in DESIGN (`dectris_core::codec::…`, `dectris_core::simplon::…`, `dectris_core::stream::…`) resolves,
//! and also re-exports `dectris_common` at its root (`dectris_core::log`, `dectris_core::DriverError`,
//! `dectris_core::dlog!`, …). Pure Rust: `cargo test -p dectris_core` needs no Python.

pub use dectris_codec as codec;
pub use dectris_common::*;
pub use dectris_simplon as simplon;
pub use dectris_stream as stream;

pub mod driver;
pub mod preview;
