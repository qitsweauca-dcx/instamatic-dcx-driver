//! SIMPLON 1.8 REST side of the driver (docs/DESIGN.md §4.1-§4.2, §3.4). No pyo3.
//! Reached as `dectris_core::simplon::…`.
//!
//! * [`client`] — ureq REST client: per-call timeouts ([`client::Budgets`]), error mapping,
//!   HTTP-200-only policy, offline back-off [R10], initialize gate [G1-H].
//! * [`detector`] — [`Detector`]: the state-machine policy (§4.2), `trigger()` (§3.4), the
//!   `[#12 keep]` wrappers, the constructor checks.
//! * [`stream_cfg`], [`filewriter`], [`monitor`] — the other SIMPLON modules (`impl Detector`).
//! * [`signals`] — `trig_cv` + release flags (R3a [G1-B]) shared with the driver/stream crates.
//! * [`worker`] — Python-free trigger/initialize workers, outcome cells, the worker ceiling.
//! * [`tiff`] — swappable monitor TIFF decoder.
//!
//! Note: the module `tiff` shadows the `tiff` crate inside this crate; refer to the crate as `::tiff`.

pub mod client;
pub mod detector;
pub mod filewriter;
pub mod monitor;
pub mod session;
pub mod signals;
pub mod stream_cfg;
#[cfg(feature = "test-util")]
pub mod testing;
pub mod tiff;
pub mod worker;

pub use client::{Budgets, Client};
pub use detector::{
    AppliedConfig, ArmOutcome, Detector, DetectorBuilder, DriverHooks, EnsureKey, FrameTimePolicy,
    ModeKind, NoHooks, Settings, SimplonStats, StartupReport, TriggerCaller, TriggerRequest,
    TriggerResult,
};
pub use filewriter::FilewriterConfig;
pub use monitor::MonitorFrame;
pub use session::PlainSession;
pub use signals::{InterruptHook, Signals, WaitEnd};
pub use tiff::{DecodedImage, MonitorDecoder, Pixels, TiffCrateDecoder};
pub use worker::{Outcome, WorkerPool};
