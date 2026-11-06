//! Logging entry points for every driver crate (docs/DESIGN.md §2.9 [R1][R8][R31], §3.2 R6).
//!
//! # Targets
//! [`dlog!`] logs to [`TARGET_DRIVER`] (`instamatic::camera::camera_dectris`) and [`slog!`] to
//! [`TARGET_STREAM`] (`instamatic::camera::camera_dectris::stream`). pyo3-log maps `::` to `.`, so
//! the records land on the Python loggers `instamatic.camera.camera_dectris[.stream]`.
//! `clippy.toml` forbids the bare `log::{trace,debug,info,warn,error,log}` macros; these two macros
//! are the only logging entry points. They do not expand to a `log` macro: they build the
//! `log::Record` themselves through [`__emit`], so the clippy ban never fires on their expansion.
//!
//! ```text
//! use dectris_common::{dlog, slog};
//! dlog!(Info, "armed series {}", id);
//! slog!(Warn, "dropped stale frame {}", n);
//! dlog!(level: lvl, "computed level");
//! if dectris_common::log::log_frames() { slog!(Debug, "frame {}", n); }   // per-frame / Drop records
//! ```
//!
//! # Python-safety flags (enforced by the py crate's `GuardedLogger`)
//! * `NO_PYTHON` — thread-local. [`set_no_python`] is the **first statement of every worker thread**
//!   (`dectris-trigger-<n>`, `dectris-init-<n>`, DESIGN §3.1). A worker never attaches to Python, even
//!   through a dependency's log record (ureq, zmq).
//! * `LOGGING_CLOSED` — process-global. [`close_logging`] is called **only** by the atexit-registered
//!   `_final_release`, after its `release_connection()` [G1-C]. `close()`, `release_connection()`,
//!   `_begin_shutdown()` and `request_stop()` never set it.
//!
//! `GuardedLogger::log`/`enabled` return early if [`python_logging_blocked`] is true, **or** if the
//! inner pyo3-log `enabled()` (attach-free) rejects the record — before `Python::attach` [G1-A].
//! Without the py crate (pure-Rust tests), the macros go to whatever `log::Log` is installed
//! (see `dectris_common::testing` behind the `test-util` feature); the flags are then informational.
//!
//! # Volume
//! Per-frame records, and every record emitted from a `Drop`, are gated by [`log_frames`]
//! (`LOG_FRAMES`, default off).
//!
//! # R6: no logging while a `MutexGuard` is alive
//! Code holding a lock pushes into a [`Deferred`] buffer and calls [`Deferred::emit`] after every
//! guard is dropped.

use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

pub use ::log::Level;

/// `log` target of [`dlog!`]; Python logger `instamatic.camera.camera_dectris`.
pub const TARGET_DRIVER: &str = "instamatic::camera::camera_dectris";
/// `log` target of [`slog!`]; Python logger `instamatic.camera.camera_dectris.stream`.
pub const TARGET_STREAM: &str = "instamatic::camera::camera_dectris::stream";
/// Python logger name that receives [`TARGET_DRIVER`] records.
pub const PY_LOGGER_DRIVER: &str = "instamatic.camera.camera_dectris";
/// Python logger name that receives [`TARGET_STREAM`] records.
pub const PY_LOGGER_STREAM: &str = "instamatic.camera.camera_dectris.stream";
/// The allow-list root: the py crate's pyo3-log `Logger` is built with `filter(Off)` and
/// `filter_target("instamatic", Debug)` (DESIGN §2.9).
pub const ALLOWED_TARGET_ROOT: &str = "instamatic";

thread_local! {
    static NO_PYTHON: Cell<bool> = const { Cell::new(false) };
}

static LOGGING_CLOSED: AtomicBool = AtomicBool::new(false);

/// `log_frames` option (DESIGN §2.2 options table, default `false`). Gates per-frame records and every
/// record emitted from a `Drop` (DESIGN §2.9 Volume, §2.6 R8).
pub static LOG_FRAMES: AtomicBool = AtomicBool::new(false);

/// Marks the current thread as Python-free (DESIGN §3.1). Call it as the first statement of every
/// worker thread. It cannot be unset: a worker thread stays Python-free for its whole life.
pub fn set_no_python() {
    // try_with: during TLS teardown the flag is unreachable, and `no_python()` already reports true.
    let _ = NO_PYTHON.try_with(|f| f.set(true));
}

/// True if this thread must never attach to Python. Also true while thread-locals are being
/// destroyed (the flag cannot be read), which errs on the safe side.
pub fn no_python() -> bool {
    NO_PYTHON.try_with(Cell::get).unwrap_or(true)
}

/// Closes Python logging for the rest of the process. Interpreter-exit path only (`_final_release`,
/// DESIGN §2.9 [G1-C], §3.7 step 5). There is deliberately no way to reopen it.
pub fn close_logging() {
    LOGGING_CLOSED.store(true, Ordering::SeqCst);
}

/// True once [`close_logging`] has run.
pub fn logging_closed() -> bool {
    LOGGING_CLOSED.load(Ordering::SeqCst)
}

/// The attach-free pre-check of `GuardedLogger` (DESIGN §2.9 [G1-A]): `NO_PYTHON || LOGGING_CLOSED`.
pub fn python_logging_blocked() -> bool {
    no_python() || logging_closed()
}

/// True if `target` is inside the `instamatic` allow-list (`instamatic`, `instamatic::…`,
/// `instamatic.…`). A cheap belt-and-braces check for the py crate; pyo3-log's own
/// `filter_target` remains the authority.
pub fn target_allowed(target: &str) -> bool {
    match target.strip_prefix(ALLOWED_TARGET_ROOT) {
        Some(rest) => rest.is_empty() || rest.starts_with("::") || rest.starts_with('.'),
        None => false,
    }
}

/// Current value of `LOG_FRAMES`.
#[inline]
pub fn log_frames() -> bool {
    LOG_FRAMES.load(Ordering::Relaxed)
}

/// Sets `LOG_FRAMES` (the `log_frames` option setter).
pub fn set_log_frames(on: bool) {
    LOG_FRAMES.store(on, Ordering::Relaxed);
}

/// True if a record at `level` would pass the `log` crate's static and dynamic max level.
#[doc(hidden)]
#[inline]
pub fn __enabled(level: Level) -> bool {
    level <= ::log::STATIC_MAX_LEVEL && level <= ::log::max_level()
}

/// Builds the `log::Record` and hands it to the installed logger. Used by [`dlog!`], [`slog!`] and
/// [`Deferred::emit`]; not public API. The level check is repeated here so direct calls are safe.
#[doc(hidden)]
pub fn __emit(
    level: Level,
    target: &str,
    args: fmt::Arguments<'_>,
    module_path: &str,
    file: &str,
    line: u32,
) {
    if !__enabled(level) {
        return;
    }
    ::log::logger().log(
        &::log::Record::builder()
            .level(level)
            .target(target)
            .args(args)
            .module_path(Some(module_path))
            .file(Some(file))
            .line(Some(line))
            .build(),
    );
}

/// Log to [`TARGET_DRIVER`] (`instamatic.camera.camera_dectris`).
///
/// `dlog!(Info, "fmt {}", x)` with a `log::Level` variant name, or `dlog!(level: expr, "fmt")`.
/// Arguments are evaluated only if the level is enabled. Never call it while a `MutexGuard` is
/// alive (DESIGN §3.2 R6) — use [`log::Deferred`](crate::log::Deferred) there.
#[macro_export]
macro_rules! dlog {
    (level: $lvl:expr, $($arg:tt)+) => {
        $crate::__dectris_log_impl!($lvl, $crate::log::TARGET_DRIVER, $($arg)+)
    };
    ($lvl:ident, $($arg:tt)+) => {
        $crate::__dectris_log_impl!($crate::log::Level::$lvl, $crate::log::TARGET_DRIVER, $($arg)+)
    };
}

/// Log to [`TARGET_STREAM`] (`instamatic.camera.camera_dectris.stream`). Same forms as [`dlog!`].
#[macro_export]
macro_rules! slog {
    (level: $lvl:expr, $($arg:tt)+) => {
        $crate::__dectris_log_impl!($lvl, $crate::log::TARGET_STREAM, $($arg)+)
    };
    ($lvl:ident, $($arg:tt)+) => {
        $crate::__dectris_log_impl!($crate::log::Level::$lvl, $crate::log::TARGET_STREAM, $($arg)+)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __dectris_log_impl {
    ($lvl:expr, $target:expr, $($arg:tt)+) => {{
        let __lvl: $crate::log::Level = $lvl;
        if $crate::log::__enabled(__lvl) {
            $crate::log::__emit(
                __lvl,
                $target,
                ::core::format_args!($($arg)+),
                ::core::module_path!(),
                ::core::file!(),
                ::core::line!(),
            );
        }
    }};
}

/// One record held by a [`Deferred`] buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredRecord {
    pub level: Level,
    pub target: &'static str,
    pub message: String,
}

/// Log records collected while a `MutexGuard` is alive, emitted after every guard is dropped
/// (DESIGN §3.2 R6 [R2]).
///
/// ```text
/// let mut d = Deferred::new();
/// {
///     let g = lock_unpoisoned(&inner.receivers);
///     if busy(&g) { d.warn("flush skipped: receiver busy"); }
/// } // guard dropped
/// d.emit();
/// ```
///
/// Dropping a non-empty buffer without [`emit`](Self::emit) or [`discard`](Self::discard) is a bug;
/// debug builds panic on it (unless already unwinding). Drop never logs.
#[must_use = "a Deferred buffer must be emitted after every MutexGuard is dropped (DESIGN §3.2 R6)"]
#[derive(Debug, Default)]
pub struct Deferred {
    records: Vec<DeferredRecord>,
}

impl Deferred {
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Queue a record for [`TARGET_DRIVER`].
    pub fn push(&mut self, level: Level, message: impl Into<String>) {
        self.push_target(level, TARGET_DRIVER, message);
    }

    /// Queue a record for [`TARGET_STREAM`].
    pub fn push_stream(&mut self, level: Level, message: impl Into<String>) {
        self.push_target(level, TARGET_STREAM, message);
    }

    /// Queue a record for an explicit target (one of the two constants).
    pub fn push_target(&mut self, level: Level, target: &'static str, message: impl Into<String>) {
        self.records.push(DeferredRecord {
            level,
            target,
            message: message.into(),
        });
    }

    /// Shorthand for `push(Level::Warn, …)` (driver target).
    pub fn warn(&mut self, message: impl Into<String>) {
        self.push(Level::Warn, message);
    }

    /// Shorthand for `push(Level::Info, …)` (driver target).
    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Level::Info, message);
    }

    /// Shorthand for `push(Level::Debug, …)` (driver target).
    pub fn debug(&mut self, message: impl Into<String>) {
        self.push(Level::Debug, message);
    }

    /// Move all of `other`'s records to the end of `self`.
    pub fn append(&mut self, mut other: Deferred) {
        self.records.append(&mut other.records);
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn records(&self) -> &[DeferredRecord] {
        &self.records
    }

    /// Emit every record, in push order. Call only after every `MutexGuard` is dropped.
    #[track_caller]
    pub fn emit(mut self) {
        let loc = std::panic::Location::caller();
        for r in std::mem::take(&mut self.records) {
            __emit(
                r.level,
                r.target,
                format_args!("{}", r.message),
                module_path!(),
                loc.file(),
                loc.line(),
            );
        }
    }

    /// Hand the records to the caller instead of logging them (e.g. the py layer, which emits them
    /// after it has dropped its own guards).
    pub fn into_records(mut self) -> Vec<DeferredRecord> {
        std::mem::take(&mut self.records)
    }

    /// Drop the records on purpose.
    pub fn discard(mut self) {
        self.records.clear();
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        if cfg!(debug_assertions) && !self.records.is_empty() && !std::thread::panicking() {
            panic!(
                "Deferred dropped with {} un-emitted record(s); call emit() after the guard drops",
                self.records.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn targets_match_python_logger_names() {
        assert_eq!(TARGET_DRIVER.replace("::", "."), PY_LOGGER_DRIVER);
        assert_eq!(TARGET_STREAM.replace("::", "."), PY_LOGGER_STREAM);
        assert!(TARGET_STREAM.starts_with(TARGET_DRIVER));
        assert!(target_allowed(TARGET_DRIVER));
        assert!(target_allowed(TARGET_STREAM));
    }

    #[test]
    fn allow_list() {
        assert!(target_allowed("instamatic"));
        assert!(target_allowed("instamatic.camera"));
        assert!(!target_allowed("instamaticx"));
        assert!(!target_allowed("ureq::run"));
        assert!(!target_allowed("ureq::pool"));
        assert!(!target_allowed("zmq"));
        assert!(!target_allowed(""));
    }

    #[test]
    fn dlog_and_slog_route_to_their_targets() {
        testing::install();
        dlog!(Info, "dlog-route-{}", 7);
        slog!(Warn, "slog-route-{}", 8);
        let lvl = Level::Error;
        dlog!(level: lvl, "dlog-expr-route");
        let d = testing::take_matching("dlog-route-7");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].target, TARGET_DRIVER);
        assert_eq!(d[0].level, Level::Info);
        assert!(d[0].file.as_deref().is_some_and(|f| f.ends_with("log.rs")));
        let s = testing::take_matching("slog-route-8");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].target, TARGET_STREAM);
        assert_eq!(s[0].level, Level::Warn);
        let e = testing::take_matching("dlog-expr-route");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].level, Level::Error);
    }

    #[test]
    fn disabled_level_does_not_evaluate_arguments() {
        testing::install(); // max level Debug: Trace is disabled
        let mut evaluated = false;
        let mut side_effect = || {
            evaluated = true;
            "x"
        };
        dlog!(Trace, "trace-not-evaluated {}", side_effect());
        assert!(!evaluated);
        assert!(testing::take_matching("trace-not-evaluated").is_empty());
    }

    #[test]
    fn no_python_is_thread_local() {
        assert!(!no_python());
        let worker = std::thread::spawn(|| {
            set_no_python();
            (no_python(), python_logging_blocked())
        });
        assert_eq!(worker.join().unwrap(), (true, true));
        assert!(!no_python(), "flag leaked to the spawning thread");
    }

    #[test]
    fn records_from_no_python_threads_still_reach_a_rust_logger() {
        // NO_PYTHON is enforced by the py crate's GuardedLogger, not by the macros.
        testing::install();
        std::thread::spawn(|| {
            set_no_python();
            dlog!(Info, "worker-record-tag");
        })
        .join()
        .unwrap();
        let r = testing::take_matching("worker-record-tag");
        assert_eq!(r.len(), 1);
        assert!(r[0].no_python);
    }

    #[test]
    fn log_frames_toggle() {
        // Only this test touches LOG_FRAMES.
        assert!(!log_frames());
        set_log_frames(true);
        assert!(log_frames());
        set_log_frames(false);
        assert!(!log_frames());
    }

    #[test]
    fn deferred_emits_in_order_after_guard() {
        testing::install();
        let m = std::sync::Mutex::new(0_u32);
        let mut d = Deferred::new();
        {
            let _g = crate::sync::lock_unpoisoned(&m);
            d.warn("deferred-order-a");
            d.push_stream(Level::Info, "deferred-order-b");
            d.debug("deferred-order-c");
            // nothing is logged while the guard is alive
            assert!(testing::take_matching("deferred-order-").is_empty());
        }
        assert_eq!(d.len(), 3);
        d.emit();
        let r = testing::take_matching("deferred-order-");
        let got: Vec<_> = r
            .iter()
            .map(|r| (r.level, r.target.as_str(), r.message.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Level::Warn, TARGET_DRIVER, "deferred-order-a"),
                (Level::Info, TARGET_STREAM, "deferred-order-b"),
                (Level::Debug, TARGET_DRIVER, "deferred-order-c"),
            ]
        );
    }

    #[test]
    fn deferred_append_into_records_and_discard() {
        let mut a = Deferred::new();
        a.info("one");
        let mut b = Deferred::new();
        b.warn("two");
        a.append(b);
        assert_eq!(a.len(), 2);
        let recs = a.into_records();
        assert_eq!(recs[1].message, "two");
        let mut c = Deferred::new();
        c.info("dropped on purpose");
        c.discard();
        Deferred::new().emit(); // empty: no-op
    }

    #[test]
    #[cfg(debug_assertions)]
    fn deferred_dropped_unemitted_panics_in_debug() {
        let r = std::panic::catch_unwind(|| {
            let mut d = Deferred::new();
            d.warn("forgotten");
            drop(d);
        });
        assert!(r.is_err());
    }
}
