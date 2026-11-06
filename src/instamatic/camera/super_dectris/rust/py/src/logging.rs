//! Allow-list logger `GuardedLogger` (docs/DESIGN.md §2.9 [R1][R8][G1-A]).
//!
//! pyo3-log's `Logger` with `filter(Off)` + `filter_target("instamatic", Debug)`, wrapped so that
//! `log()` runs the attach-free filter (`NO_PYTHON`, `LOGGING_CLOSED`, `inner.enabled()`) before
//! it ever calls `Python::attach`. The `log` macros call `log()` directly once `max_level` passes,
//! so filtering only in `enabled()` would not be enough [G1-A].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use dectris_core::log::{python_logging_blocked, ALLOWED_TARGET_ROOT};
use log::{LevelFilter, Log, Metadata, Record};
use pyo3::exceptions::PyKeyboardInterrupt;
use pyo3::prelude::*;

/// Records whose Python handler raised (`StreamStats.log_errors`).
pub static LOG_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Test hook [G1-A]: `Python::attach` calls made for a record whose target is NOT under
/// `instamatic` (must stay 0 on every thread).
pub static FOREIGN_ATTACHES: AtomicU64 = AtomicU64::new(0);

/// Test hook: `Python::attach` calls made for allowed records.
pub static ALLOWED_ATTACHES: AtomicU64 = AtomicU64::new(0);

/// ResetHandle taken from the pyo3-log `Logger` before boxing it (`reset_log_cache()`).
static RESET_HANDLE: OnceLock<pyo3_log::ResetHandle> = OnceLock::new();

struct GuardedLogger {
    inner: pyo3_log::Logger,
}

impl Log for GuardedLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // pyo3-log's enabled() reads only its arc-swap cache and never attaches.
        !python_logging_blocked() && self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if python_logging_blocked() || !self.inner.enabled(record.metadata()) {
            return;
        }
        if record.target().starts_with(ALLOWED_TARGET_ROOT) {
            ALLOWED_ATTACHES.fetch_add(1, Ordering::Relaxed);
        } else {
            FOREIGN_ATTACHES.fetch_add(1, Ordering::Relaxed);
        }
        Python::attach(|py| {
            let had = PyErr::occurred(py);
            self.inner.log(record);
            // pyo3-log restores a handler's exception onto the thread state; take it so the
            // caller never sees `SystemError: … returned a result with an exception set` [R8].
            if !had {
                if let Some(e) = PyErr::take(py) {
                    if e.is_instance_of::<PyKeyboardInterrupt>(py) {
                        // Ctrl+C landed in a logging handler: never swallow it, re-arm it so the
                        // eval loop raises it on the main thread (review #2)
                        let _ = py
                            .import("_thread")
                            .and_then(|t| t.call_method0("interrupt_main"));
                    } else {
                        LOG_ERRORS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    fn flush(&self) {}
}

/// Install the allow-list logger (once). If another extension already owns the global `log`
/// slot, keep going: our records then go to that logger (DESIGN §2.9).
pub fn install(py: Python<'_>) -> PyResult<()> {
    if RESET_HANDLE.get().is_some() {
        return Ok(());
    }
    let logger = pyo3_log::Logger::new(py, pyo3_log::Caching::LoggersAndLevels)?
        .filter(LevelFilter::Off)
        .filter_target(ALLOWED_TARGET_ROOT.to_owned(), LevelFilter::Debug);
    let handle = logger.reset_handle();
    if log::set_boxed_logger(Box::new(GuardedLogger { inner: logger })).is_ok() {
        log::set_max_level(LevelFilter::Debug);
        let _ = RESET_HANDLE.set(handle);
    }
    Ok(())
}

/// `reset_log_cache()`: forget pyo3-log's cached loggers and levels (after the application
/// changed logging levels at runtime).
#[pyfunction]
pub fn reset_log_cache() {
    if let Some(h) = RESET_HANDLE.get() {
        h.reset();
    }
}

/// `_test_attach_counts() -> (allowed, foreign)` (DESIGN §8 [R1][G1-A]).
#[pyfunction]
pub fn _test_attach_counts() -> (u64, u64) {
    (
        ALLOWED_ATTACHES.load(Ordering::Relaxed),
        FOREIGN_ATTACHES.load(Ordering::Relaxed),
    )
}
