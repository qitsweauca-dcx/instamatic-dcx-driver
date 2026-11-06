//! Shared foundation for every driver crate (docs/DESIGN.md §2.9, §3.2, §6).
//!
//! * [`log`] — the only logging entry points ([`dlog!`], [`slog!`]), the `NO_PYTHON` thread flag,
//!   the process-global `LOGGING_CLOSED` flag, `LOG_FRAMES`, and the [`log::Deferred`] buffer used
//!   while a `MutexGuard` is alive (R6).
//! * [`error`] — [`DriverError`], the typed error every crate returns (DESIGN §6).
//! * [`sync`] — poison-recovering lock helper (DESIGN §3.2 R5).
//!
//! Pure Rust: no pyo3. The Python bridge (`GuardedLogger` around pyo3-log) lives in the `py` crate
//! and consults [`log::python_logging_blocked`] before it attaches.

pub mod error;
pub mod log;
pub mod sync;

#[cfg(any(test, feature = "test-util"))]
pub mod testing;

pub use error::{DriverError, ExceptionKind, Result};

/// Re-export used by the `dlog!`/`slog!` expansions; not public API.
#[doc(hidden)]
pub use ::log as __log;
