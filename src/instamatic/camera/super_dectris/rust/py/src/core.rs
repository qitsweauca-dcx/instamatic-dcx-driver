//! DectrisCore #[pyclass] methods, ffi_guard!, Python::detach around I/O (docs/DESIGN.md §2.1-§2.5, §3.3).
//!
//! `DectrisCore()` is created unconnected with no I/O (§2.2). `load_defaults` then writes the yaml
//! keys through the lenient property setters [R33] into [`Pending`]; `_connect` builds the
//! [`Driver`] from it. After `_connect` the setters write through to the driver and the getters
//! read the driver (the explicit `set_trigger_mode`/`set_compression` change it directly).
//!
//! Every call into `dectris_core` goes through [`call`]: detached, `catch_unwind` inside the
//! detach (so a panic never unwinds through a non-empty `Deferred`), records emitted after the
//! detach returns with no lock held (R6). Even µs accessors run detached, because the member
//! crates lock with plain `lock()` (R6: attached threads never block on a mutex).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::ThreadId;
use std::time::Duration;

use dectris_core::codec::{decode_message_with, DecodeOptions};
use dectris_core::driver::{Driver, DriverConfig, FrameMode, MessageEvent, StopReason};
use dectris_core::log::{close_logging, log_frames, set_log_frames, Deferred, Level};
use dectris_core::simplon::detector::FrameTimePolicy;
use dectris_core::simplon::{FilewriterConfig, InterruptHook, Pixels, Settings, TriggerRequest};
use dectris_core::stream::{AttributionMode, Delivery, StalePolicy, StartCache, DEFAULT_RCVBUF};
use dectris_core::DriverError;
use numpy::{PyArray1, PyArrayMethods};
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyTuple};

use crate::convert::{bin_mean, delivery_to_array, message_to_dict};
use crate::errors::{
    dectris_error, guard, py_result, stash_interrupt, take_stashed, to_pyerr, INTERRUPTED,
};
use crate::iters::{FrameIter, MovieIter};
use crate::stats::{snapshot, StreamStats};

/// The Python main thread (captured at module init when the import runs on it), for the
/// interrupt hook's 250 ms slicing + `check_signals()` (DESIGN §3.2 R4).
pub static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Fallback `default_exposure` when the yaml has none (s).
const DEFAULT_EXPOSURE: f64 = 0.1;

// ---------------------------------------------------------------------------------------------
// Detached calls
// ---------------------------------------------------------------------------------------------

/// Run `f` detached with a fresh `Deferred`; emit its records once attached again (no lock held),
/// then map the result. A panic inside `f` → `DectrisError("internal error: …")`.
pub fn call<T: Send>(
    py: Python<'_>,
    f: impl FnOnce(&mut Deferred) -> dectris_core::Result<T> + Send,
) -> PyResult<T> {
    let (r, d) = py.detach(move || {
        let mut d = Deferred::new();
        let r = catch_unwind(AssertUnwindSafe(|| f(&mut d)));
        (r, d)
    });
    d.emit();
    let out = match r {
        Ok(r) => py_result(py, r),
        Err(p) => Err(to_pyerr(py, DriverError::from_panic(p.as_ref()))),
    };
    // Ctrl+C caught by a sliced wait that never raises (wait_for_idle, disarm's initialize wait):
    // the stashed KeyboardInterrupt still wins (review #2).
    match take_stashed() {
        Some(e) => Err(e),
        None => out,
    }
}

/// [`call`] for infallible operations.
pub fn call_ok<T: Send>(py: Python<'_>, f: impl FnOnce(&mut Deferred) -> T + Send) -> PyResult<T> {
    call(py, move |d| Ok(f(d)))
}

/// Run a property accessor body; a panic becomes an ERROR record and the type's default instead
/// of a `PanicException` (a BaseException that escapes `except Exception` and `hasattr`). The
/// accessors are lenient and never raise [R33] (review #10).
fn shielded<T: Default>(what: &str, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => {
            let mut d = Deferred::new();
            d.push(
                Level::Error,
                format!("{what}: {}", DriverError::from_panic(p.as_ref())),
            );
            d.emit();
            T::default()
        }
    }
}

/// Values the lenient `trigger_mode` / `compression` setters accept (after trim + lowercase).
const TRIGGER_MODES: [&str; 4] = ["ints", "inte", "exts", "exte"];
const COMPRESSIONS: [&str; 3] = ["bslz4", "lz4", "none"];

/// One WARNING record, emitted at once (the caller holds no lock).
pub(crate) fn warn(msg: String) {
    let mut d = Deferred::new();
    d.warn(msg);
    d.emit();
}

/// Write `[Dectris] …` lines to Python `sys.stdout` (the GUI console captures it, DESIGN §2.9).
pub fn print_stdout(py: Python<'_>, line: Option<&str>) {
    let Some(line) = line else { return };
    let Ok(out) = py.import("sys").and_then(|s| s.getattr("stdout")) else {
        return;
    };
    if out.is_none() {
        return;
    }
    let _ = out.call_method1("write", (format!("{line}\n"),));
    let _ = out.call_method0("flush");
}

// ---------------------------------------------------------------------------------------------
// Lenient coercion [R33]
// ---------------------------------------------------------------------------------------------

/// bool from bool, int, or `true/false/yes/no/1/0`.
fn coerce_bool(v: &Bound<'_, PyAny>) -> Option<bool> {
    if let Ok(b) = v.cast::<PyBool>() {
        return Some(b.is_true());
    }
    if v.is_instance_of::<PyInt>() {
        return v.extract::<i64>().ok().map(|i| i != 0);
    }
    let s: String = v.extract().ok()?;
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// A finite number from int, float or a numeric string.
fn coerce_f64(v: &Bound<'_, PyAny>) -> Option<f64> {
    let x = if v.is_instance_of::<PyInt>() || v.is_instance_of::<PyFloat>() {
        v.extract::<f64>().ok()?
    } else {
        v.extract::<String>().ok()?.trim().parse::<f64>().ok()?
    };
    x.is_finite().then_some(x)
}

fn coerce_str(v: &Bound<'_, PyAny>) -> Option<String> {
    v.extract::<String>().ok()
}

/// `<key>=<value!r> ignored; keeping <current>` (one WARNING, no lock held).
fn ignored(v: &Bound<'_, PyAny>, key: &str, keeping: impl std::fmt::Display) {
    let r = v
        .repr()
        .map(|r| r.to_string())
        .unwrap_or_else(|_| "<unrepresentable>".into());
    warn(format!("{key}={r} ignored; keeping {keeping}"));
}

/// Python spelling of a bool / an optional value for the WARNING text.
fn py_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

fn py_opt<T: std::fmt::Display>(o: Option<T>, quote: bool) -> String {
    match o {
        Some(v) if quote => format!("'{v}'"),
        Some(v) => v.to_string(),
        None => "None".into(),
    }
}

/// An optional positive `binning`/`binsize` argument (None → `None`).
fn binning_arg(v: Option<&Bound<'_, PyAny>>) -> Option<u32> {
    let v = v.filter(|v| !v.is_none())?;
    let b = coerce_f64(v)?;
    (b >= 1.0).then_some(b.min(u32::MAX as f64) as u32)
}

fn stop_code(r: Option<StopReason>) -> u8 {
    match r {
        None => 0,
        Some(StopReason::NFrames) => 1,
        Some(StopReason::End) => 2,
        Some(StopReason::Timeout) => 3,
        Some(StopReason::Cancelled) => 4,
        Some(StopReason::Shutdown) => 5,
    }
}

fn stop_str(c: u8) -> Option<&'static str> {
    let r = match c {
        1 => StopReason::NFrames,
        2 => StopReason::End,
        3 => StopReason::Timeout,
        4 => StopReason::Cancelled,
        5 => StopReason::Shutdown,
        _ => return None,
    };
    Some(r.as_str())
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// Options written before `_connect` (and mirrored after it).
#[derive(Debug, Clone)]
struct Pending {
    settings: Settings,
    receive_timeout: Option<f64>,
    stale: StalePolicy,
    attribution: AttributionMode,
    single_frame_fresh: bool,
    tcp_keepalive: bool,
    rcvbuf: i32,
}

impl Default for Pending {
    fn default() -> Self {
        Pending {
            settings: Settings::default(),
            receive_timeout: None,
            stale: StalePolicy::Drop,
            attribution: AttributionMode::SequenceId,
            single_frame_fresh: true,
            tcp_keepalive: false,
            rcvbuf: DEFAULT_RCVBUF,
        }
    }
}

/// Shared by `DectrisCore` and its iterators.
#[derive(Debug, Default)]
pub struct CoreState {
    driver: OnceLock<Arc<Driver>>,
    pending: Mutex<Pending>,
    default_binning: AtomicU32,
    binning_set: AtomicBool,
    decode_start_arrays: AtomicBool,
    last_stop: AtomicU8,
}

impl CoreState {
    pub fn driver(&self) -> Option<&Arc<Driver>> {
        self.driver.get()
    }

    /// The driver, or `DectrisError('driver not connected')`.
    pub fn drv(&self, py: Python<'_>) -> PyResult<Arc<Driver>> {
        self.driver
            .get()
            .cloned()
            .ok_or_else(|| to_pyerr(py, DriverError::NotConnected))
    }

    fn pending<'a>(&'a self, py: Python<'_>) -> MutexGuard<'a, Pending> {
        self.pending
            .lock_py_attached(py)
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_stop(&self, r: Option<StopReason>) {
        self.last_stop.store(stop_code(r), Ordering::Relaxed);
    }

    pub fn binning(&self) -> u32 {
        self.default_binning.load(Ordering::Relaxed).max(1)
    }

    pub fn decode_start_arrays(&self) -> bool {
        self.decode_start_arrays.load(Ordering::Relaxed)
    }

    /// Current settings: the driver's when connected, else the pending copy.
    fn settings(&self, py: Python<'_>) -> Settings {
        match self.driver.get() {
            Some(d) => py.detach(|| d.detector().settings()),
            None => self.pending(py).settings.clone(),
        }
    }

    /// Change a setting in the pending copy and, when connected, in the driver.
    fn update(&self, py: Python<'_>, f: impl Fn(&mut Settings) + Send + Sync) {
        f(&mut self.pending(py).settings);
        if let Some(d) = self.driver.get() {
            py.detach(|| d.detector().update_settings(|s| f(s)));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Conversions used by several methods
// ---------------------------------------------------------------------------------------------

/// The decoded start message (`_start_config`, `_stream_start_config`); the large arrays are
/// `None` unless `stream_decode_start_arrays` (DESIGN §4.4).
pub fn start_dict<'py>(
    py: Python<'py>,
    sc: &StartCache,
    decode_arrays: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let opts = DecodeOptions {
        skip_start_arrays: !decode_arrays,
    };
    let entries = py.detach(|| decode_message_with(&sc.raw, opts).map(|m| m.entries));
    message_to_dict(py, &py_result(py, entries)?)
}

/// An image delivery → binned ndarray.
pub fn image_array<'py>(
    py: Python<'py>,
    x: &Delivery,
    binning: u32,
) -> PyResult<Bound<'py, PyAny>> {
    bin_mean(py, delivery_to_array(py, x)?, binning)
}

pub(crate) fn monitor_array<'py>(
    py: Python<'py>,
    rows: usize,
    cols: usize,
    p: &Pixels,
) -> PyResult<Bound<'py, PyAny>> {
    let shape = [rows, cols];
    Ok(match p {
        Pixels::U8(v) => PyArray1::from_slice(py, v).reshape(shape)?.into_any(),
        Pixels::U16(v) => PyArray1::from_slice(py, v).reshape(shape)?.into_any(),
        Pixels::U32(v) => PyArray1::from_slice(py, v).reshape(shape)?.into_any(),
        Pixels::F32(v) => PyArray1::from_slice(py, v).reshape(shape)?.into_any(),
    })
}

// ---------------------------------------------------------------------------------------------
// DectrisCore
// ---------------------------------------------------------------------------------------------

#[pyclass(
    subclass,
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "DectrisCore"
)]
pub struct DectrisCore {
    pub st: Arc<CoreState>,
}

impl DectrisCore {
    pub(crate) fn unconnected() -> Self {
        let st = CoreState::default();
        st.default_binning.store(1, Ordering::Relaxed);
        DectrisCore { st: Arc::new(st) }
    }

    /// `exposure=None` → the instance's `default_exposure` (yaml), else 0.1 s [G1-M].
    fn exposure(slf: &Bound<'_, Self>, exposure: Option<f64>) -> f64 {
        exposure.unwrap_or_else(|| {
            slf.getattr("default_exposure")
                .ok()
                .and_then(|v| coerce_f64(&v))
                .filter(|e| *e > 0.0)
                .unwrap_or(DEFAULT_EXPOSURE)
        })
    }

    fn binning(&self, b: Option<&Bound<'_, PyAny>>) -> u32 {
        binning_arg(b).unwrap_or_else(|| self.st.binning())
    }

    fn frame_iter<'py>(
        &self,
        py: Python<'py>,
        n_frames: i64,
        mode: FrameMode,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, FrameIter>> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let b = self.binning(binning);
            let n = n_frames.max(0) as u64;
            Bound::new(py, FrameIter::new(self.st.clone(), drv, n, mode, b))
        })
    }

    fn camera_name(slf: &Bound<'_, Self>) -> String {
        slf.getattr("name")
            .ok()
            .and_then(|n| n.extract::<String>().ok())
            .unwrap_or_else(|| "dectris".into())
    }

    /// `release_connection()` body; never raises.
    pub(crate) fn release(slf: &Bound<'_, Self>) {
        let py = slf.py();
        let Some(drv) = slf.get().st.driver().cloned() else {
            return;
        };
        let name = Self::camera_name(slf);
        let _ = call_ok(py, move |d| {
            let s = drv.release_connection(d);
            if s.released {
                d.info(format!("Connection to camera '{name}' released"));
            }
        });
    }
}

// The public members are the list of DESIGN §2.3 [R40].
#[pymethods]
impl DectrisCore {
    /// Unconnected, no I/O; the arguments are ignored (`cls()` and `cls(name=…)`, DESIGN §2.2).
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self::unconnected()
    }

    // ----- construction -------------------------------------------------------------------------

    /// `_connect(dcu, zmq, rcvhwm, default_binsize, dimensions, *, user_agent, header_appendix)`:
    /// startup checks + PULL connect, all best-effort (DESIGN §2.2).
    #[pyo3(signature = (dcu_endpoint, zmq_endpoint, rcvhwm=500, default_binsize=None, dimensions=None, *, user_agent=None, header_appendix=None))]
    #[allow(clippy::too_many_arguments)]
    fn _connect(
        slf: &Bound<'_, Self>,
        dcu_endpoint: String,
        zmq_endpoint: String,
        rcvhwm: i32,
        default_binsize: Option<&Bound<'_, PyAny>>,
        dimensions: Option<&Bound<'_, PyAny>>,
        user_agent: Option<String>,
        header_appendix: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let py = slf.py();
        guard(py, || {
            let st = &slf.get().st;
            if st.driver().is_some() {
                return Err(dectris_error(py, "already connected"));
            }
            if !st.binning_set.load(Ordering::Relaxed) {
                if let Some(b) = binning_arg(default_binsize) {
                    st.default_binning.store(b, Ordering::Relaxed);
                }
            }
            // yaml `dimensions` as a list or tuple of two numbers (the adapter passes a list)
            let dimensions = dimensions.filter(|d| !d.is_none());
            let yaml_dims = dimensions.and_then(|d| {
                let v = crate::convert::items(d, 2)?;
                Some((coerce_f64(&v[0])? as i64, coerce_f64(&v[1])? as i64))
            });
            match (dimensions, yaml_dims) {
                (Some(d), None) => ignored(d, "dimensions", "no geometry check"),
                (_, Some(dims)) => {
                    let mut d = Deferred::new();
                    d.debug(format!("yaml dimensions {dims:?}"));
                    d.emit();
                }
                (None, None) => {}
            }
            let appendix = match header_appendix.filter(|h| !h.is_none()) {
                Some(h) => {
                    let s: String = py.import("json")?.call_method1("dumps", (h,))?.extract()?;
                    serde_json::from_str(&s).ok()
                }
                None => None,
            };
            let exposure = Self::exposure(slf, None);
            let p = {
                let mut p = st.pending(py);
                p.settings.header_appendix = appendix;
                p.settings.default_exposure = exposure;
                p.clone()
            };
            let mut cfg = DriverConfig::new(dcu_endpoint, zmq_endpoint, rcvhwm);
            cfg.tcp_keepalive = p.tcp_keepalive;
            cfg.rcvbuf = p.rcvbuf;
            cfg.yaml_dims = yaml_dims;
            cfg.user_agent = user_agent.unwrap_or_default();
            cfg.settings = p.settings;
            let drv = py_result(py, Driver::new(cfg))?;
            drv.stream().set_receive_timeout(p.receive_timeout);
            drv.stream().set_stale_policy(p.stale);
            drv.stream().set_attribution_mode(p.attribution);
            drv.set_single_frame_fresh(p.single_frame_fresh);
            if let Some(&thread) = MAIN_THREAD.get() {
                drv.signals().set_interrupt_hook(Some(InterruptHook {
                    thread,
                    check: Arc::new(|| {
                        Python::attach(|py| {
                            py.check_signals().map_err(|e| {
                                stash_interrupt(e);
                                DriverError::Internal(INTERRUPTED.into())
                            })
                        })
                    }),
                }));
            }
            if st.driver.set(drv.clone()).is_err() {
                return Err(dectris_error(py, "already connected"));
            }
            let summary = call_ok(py, move |d| drv.connect(d))?;
            print_stdout(py, summary.zmq.stdout.as_deref());
            Ok(())
        })
    }

    // ----- connection -------------------------------------------------------------------------

    /// `CameraBase` ABC: `_connect` already connected; after `release_connection()` (e.g. a second
    /// `with cam:`) re-open the ZMQ stream (review #7). Never raises.
    pub(crate) fn establish_connection(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || {
            let Some(drv) = self.st.driver().cloned() else {
                return Ok(());
            };
            if drv.stream().is_connected() {
                return Ok(());
            }
            let r = call_ok(py, move |d| drv.establish_zmq_stream_connection(d))?;
            print_stdout(py, r.stdout.as_deref());
            Ok(())
        })
    }

    /// Guarded disarm, close the PULL, release the guard, INFO (DESIGN §3.7). Idempotent, never raises.
    fn release_connection(slf: &Bound<'_, Self>) {
        Self::release(slf);
    }

    /// = `release_connection()`.
    fn close(slf: &Bound<'_, Self>) {
        Self::release(slf);
    }

    /// atexit only: `release_connection()`, then close Python logging for good [G1-C].
    fn _final_release(slf: &Bound<'_, Self>) {
        Self::release(slf);
        close_logging();
    }

    /// `threading._register_atexit` hook: set `shutting_down` + notify. No HTTP, no logging.
    fn _begin_shutdown(&self, py: Python<'_>) {
        if let Some(d) = self.st.driver() {
            py.detach(|| d.begin_shutdown());
        }
    }

    /// Public, idempotent `_begin_shutdown` (GUI close path, [G1-S]); permanent for the instance.
    fn request_stop(&self, py: Python<'_>) {
        self._begin_shutdown(py);
    }

    // ----- modes and stream config -------------------------------------------------------------

    /// Validate `{ints,inte,exts,exte}` (`ValueError`), store, invalidate the ensure cache,
    /// best-effort PUT.
    fn set_trigger_mode(&self, py: Python<'_>, mode: String) -> PyResult<()> {
        guard(py, || {
            match self.st.driver().cloned() {
                Some(drv) => call(py, move |_| drv.detector().set_trigger_mode(&mode))?,
                None => {
                    if !["ints", "inte", "exts", "exte"].contains(&mode.as_str()) {
                        return py_result(
                            py,
                            Err(DriverError::InvalidArg(format!(
                                "invalid trigger mode {mode:?}; expected one of ints, inte, exts, exte"
                            ))),
                        );
                    }
                    self.st.pending(py).settings.trigger_mode = mode;
                }
            }
            Ok(())
        })
    }

    /// [#12 keep] Validate `{bslz4, lz4, none}`, store, invalidate the cache, best-effort PUT.
    fn set_compression(&self, py: Python<'_>, algorithm: String) -> PyResult<()> {
        guard(py, || {
            match self.st.driver().cloned() {
                Some(drv) => call(py, move |_| drv.detector().set_compression(&algorithm))?,
                None => {
                    if !["bslz4", "lz4", "none"].contains(&algorithm.as_str()) {
                        return py_result(
                            py,
                            Err(DriverError::InvalidArg(format!(
                                "invalid compression {algorithm:?}; expected one of bslz4, lz4, none"
                            ))),
                        );
                    }
                    self.st.pending(py).settings.compression = algorithm;
                }
            }
            Ok(())
        })
    }

    /// PUT stream mode/format(/header_detail); never raises.
    fn enable_stream_v2(&self, py: Python<'_>) -> PyResult<bool> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().enable_stream_v2()),
            None => Ok(false),
        })
    }

    fn disable_stream_v2(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().disable_stream_v2()),
            None => Ok(()),
        })
    }

    /// `'enabled'`, `'disabled'` or `'unknown'`; never raises.
    fn get_stream_mode(&self, py: Python<'_>) -> PyResult<String> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_stream_mode()),
            None => Ok("unknown".into()),
        })
    }

    // ----- status and HV ------------------------------------------------------------------------

    fn get_countrate_correction(&self, py: Python<'_>) -> PyResult<Option<bool>> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_countrate_correction()),
            None => Ok(None),
        })
    }

    fn set_countrate_correction(&self, py: Python<'_>, enabled: &Bound<'_, PyAny>) -> PyResult<()> {
        guard(py, || {
            let on = enabled.is_truthy()?;
            match self.st.driver().cloned() {
                Some(drv) => call_ok(py, move |_| drv.detector().set_countrate_correction(on)),
                None => Ok(()),
            }
        })
    }

    fn get_detector_state(&self, py: Python<'_>) -> PyResult<String> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_detector_state()),
            None => Ok("unknown".into()),
        })
    }

    fn get_hv_state(&self, py: Python<'_>) -> PyResult<String> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_hv_state()),
            None => Ok("unknown".into()),
        })
    }

    /// PUT `command/hv_enabled`; raises on failure (the GUI reverts its checkbox).
    fn hv_enabled(&self, py: Python<'_>, state: &Bound<'_, PyAny>) -> PyResult<()> {
        guard(py, || {
            let on = state.is_truthy()?;
            let drv = self.st.drv(py)?;
            call(py, move |_| drv.detector().hv_enabled(on))
        })
    }

    /// WARNING once; no HTTP (exte needs the electrical EXT IN signal).
    #[pyo3(signature = (enable=None))]
    fn gate(&self, py: Python<'_>, enable: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        guard(py, || {
            let on = match enable {
                Some(e) => e.is_truthy()?,
                None => true,
            };
            match self.st.driver().cloned() {
                Some(drv) => call_ok(py, move |_| drv.detector().gate(on)),
                None => Ok(()),
            }
        })
    }

    // ----- commands -----------------------------------------------------------------------------

    fn arm(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            call(py, move |_| drv.detector().arm().map(|_| ()))
        })
    }

    fn disarm(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().disarm()),
            None => Ok(()),
        })
    }

    /// Blocking software trigger (DESIGN §3.4); exts/exte → `DetectorStateError`, no HTTP.
    #[pyo3(signature = (timeout=None, count_time=None))]
    fn trigger(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
        count_time: Option<f64>,
    ) -> PyResult<()> {
        guard(py, || {
            if let Some(t) = timeout {
                if !(t.is_finite() && t >= 0.0) {
                    return py_result(
                        py,
                        Err(DriverError::InvalidArg(format!(
                            "timeout must be >= 0, got {t}"
                        ))),
                    );
                }
            }
            let drv = self.st.drv(py)?;
            let req = TriggerRequest::public(timeout.map(Duration::from_secs_f64), count_time);
            call(py, move |_| drv.detector().trigger(req).map(|_| ()))
        })
    }

    fn initialize(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            call(py, move |_| drv.detector().initialize())
        })
    }

    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().abort()),
            None => Ok(()),
        })
    }

    fn cancel(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().cancel()),
            None => Ok(()),
        })
    }

    // ----- configuration ------------------------------------------------------------------------

    #[pyo3(signature = (nimages, exposure, ntrigger=1, frame_time=None))]
    fn set_config(
        &self,
        py: Python<'_>,
        nimages: i64,
        exposure: f64,
        ntrigger: i64,
        frame_time: Option<f64>,
    ) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let (n, t) = (nimages.max(0) as u64, ntrigger.max(0) as u64);
            call(py, move |_| {
                drv.detector().set_config(n, exposure, t, frame_time)
            })
        })
    }

    #[pyo3(signature = (nimages, exposure, ntrigger=1, frame_time=None))]
    fn ensure_config(
        &self,
        py: Python<'_>,
        nimages: i64,
        exposure: f64,
        ntrigger: i64,
        frame_time: Option<f64>,
    ) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let (n, t) = (nimages.max(0) as u64, ntrigger.max(0) as u64);
            call(py, move |_| {
                drv.detector().ensure_config(n, exposure, t, frame_time)
            })
        })
    }

    // ----- FileWriter ---------------------------------------------------------------------------

    #[pyo3(signature = (mode="enabled".to_owned(), name_pattern="series_$id".to_owned(), nimages_per_file=1000, compression_enabled=true, image_nr_start=1))]
    fn configure_filewriter(
        &self,
        py: Python<'_>,
        mode: String,
        name_pattern: String,
        nimages_per_file: u64,
        compression_enabled: bool,
        image_nr_start: u64,
    ) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let cfg = FilewriterConfig {
                mode,
                name_pattern,
                nimages_per_file,
                compression_enabled,
                image_nr_start,
            };
            call(py, move |_| drv.detector().configure_filewriter(&cfg))
        })
    }

    fn disable_filewriter(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().disable_filewriter()),
            None => Ok(()),
        })
    }

    /// Download every listed file into `destination` → `list[pathlib.Path]`.
    fn download_filewriter_files<'py>(
        &self,
        py: Python<'py>,
        destination: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyList>> {
        guard(py, || {
            let dest: PathBuf = py
                .import("os")?
                .call_method1("fspath", (destination,))?
                .extract::<String>()?
                .into();
            let drv = self.st.drv(py)?;
            let files = call(py, move |_| drv.detector().download_filewriter_files(&dest))?;
            let path = py.import("pathlib")?.getattr("Path")?;
            let out = PyList::empty(py);
            for f in files {
                out.append(path.call1((f.to_string_lossy().into_owned(),))?)?;
            }
            Ok(out)
        })
    }

    fn clear_filewriter(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().clear_filewriter()),
            None => Ok(()),
        })
    }

    /// [#12 keep] `list[str]`; `[]` + WARNING on error.
    fn get_filewriter_files(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_filewriter_files()),
            None => Ok(Vec::new()),
        })
    }

    // ----- [#12 keep] SIMPLON helpers ---------------------------------------------------------------

    /// `bit_depth_image`; 0 on any error.
    fn get_bit_depth(&self, py: Python<'_>) -> PyResult<i64> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().get_bit_depth()),
            None => Ok(0),
        })
    }

    fn enable_monitor(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().enable_monitor()),
            None => Ok(()),
        })
    }

    /// Poll `status/state` until idle; returns the last state; never raises.
    #[pyo3(signature = (timeout=10.0, poll_interval=0.2))]
    fn wait_for_idle(&self, py: Python<'_>, timeout: f64, poll_interval: f64) -> PyResult<String> {
        guard(py, || {
            let Some(drv) = self.st.driver().cloned() else {
                return Ok("unknown".into());
            };
            let t = Duration::from_secs_f64(if timeout.is_finite() {
                timeout.max(0.0)
            } else {
                0.0
            });
            let p = Duration::from_secs_f64(if poll_interval.is_finite() {
                poll_interval.clamp(0.01, 60.0)
            } else {
                0.2
            });
            call_ok(py, move |_| drv.detector().wait_for_idle(t, p))
        })
    }

    /// `ensure_config(99999, default_exposure)` → arm → one internal trigger (R7-checked).
    fn arm_for_preview(slf: &Bound<'_, Self>) -> PyResult<()> {
        let py = slf.py();
        guard(py, || {
            let st = &slf.get().st;
            let drv = st.drv(py)?;
            let e = Self::exposure(slf, None);
            st.update(py, move |s| s.default_exposure = e);
            call(py, move |_| drv.arm_for_preview())
        })
    }

    fn disarm_preview(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |_| drv.detector().disarm_preview()),
            None => Ok(()),
        })
    }

    // ----- ZMQ ----------------------------------------------------------------------------------

    /// Fresh PULL; `False` when a receiver of another thread is busy [R32].
    fn reconnect_zmq(&self, py: Python<'_>) -> PyResult<bool> {
        guard(py, || {
            let Some(drv) = self.st.driver().cloned() else {
                return Ok(false);
            };
            let r = call_ok(py, move |d| drv.reconnect_zmq(d))?;
            print_stdout(py, r.stdout.as_deref());
            Ok(r.ok())
        })
    }

    /// Discard queued messages; returns the count (0 when busy) [R22].
    fn flush_zmq_buffer(&self, py: Python<'_>) -> PyResult<u64> {
        guard(py, || match self.st.driver().cloned() {
            Some(drv) => call_ok(py, move |d| drv.flush_zmq_buffer(d)),
            None => Ok(0),
        })
    }

    /// [#12 keep] Connect the PULL if this instance holds none.
    fn establish_zmq_stream_connection(&self, py: Python<'_>) -> PyResult<()> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let r = call_ok(py, move |d| drv.establish_zmq_stream_connection(d))?;
            print_stdout(py, r.stdout.as_deref());
            Ok(())
        })
    }

    // ----- receiving ----------------------------------------------------------------------------

    /// An image dict with `'_start_config'`, `None` (timeout, or the armed start was cached), or
    /// `_STREAM_END` (DESIGN §2.6).
    #[pyo3(signature = (timeout=2000.0))]
    fn message_receiver<'py>(&self, py: Python<'py>, timeout: f64) -> PyResult<Bound<'py, PyAny>> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let ms = if timeout.is_finite() {
                timeout.max(0.0) as u64
            } else {
                0
            };
            let ev = call(py, move |d| drv.message_receiver(ms, d))?;
            match ev {
                MessageEvent::Image(x) => {
                    let raw = x.bytes();
                    let entries =
                        py.detach(|| dectris_core::codec::decode_message(raw).map(|m| m.entries));
                    let msg = message_to_dict(py, &py_result(py, entries)?)?;
                    let start = match &x.start {
                        Some(sc) => start_dict(py, sc, self.st.decode_start_arrays())?.into_any(),
                        None => py.None().into_bound(py),
                    };
                    msg.set_item("_start_config", start)?;
                    Ok(msg.into_any())
                }
                MessageEvent::End => Ok(crate::stream_end(py)?.into_bound(py)),
                MessageEvent::Start(_) | MessageEvent::Nothing => Ok(py.None().into_bound(py)),
            }
        })
    }

    /// = the module-level `decompress_image(message)`.
    fn decompress_image<'py>(
        &self,
        py: Python<'py>,
        message: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        crate::decompress_image_any(py, message)
    }

    /// One frame of the armed series (autocred), non-blocking internal trigger; never disarms.
    #[pyo3(signature = (binning=None))]
    fn acquire_single_frame<'py>(
        &self,
        py: Python<'py>,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        guard(py, || {
            let drv = self.st.drv(py)?;
            let b = self.binning(binning);
            let x = call(py, move |d| drv.acquire_single_frame(d))?;
            image_array(py, &x, b)
        })
    }

    /// `FrameIter` yielding ndarrays (silent stop, `last_stop_reason`).
    #[pyo3(signature = (n_frames, binning=None))]
    fn receive_frames<'py>(
        &self,
        py: Python<'py>,
        n_frames: i64,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, FrameIter>> {
        self.frame_iter(py, n_frames, FrameMode::Frames, binning)
    }

    /// `FrameIter` yielding `(ndarray, meta)` (DESIGN §2.7).
    #[pyo3(signature = (n_frames, binning=None))]
    fn receive_frames_with_metadata<'py>(
        &self,
        py: Python<'py>,
        n_frames: i64,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, FrameIter>> {
        self.frame_iter(py, n_frames, FrameMode::WithMeta, binning)
    }

    /// `FrameIter` yielding `(bytes, meta)`: the complete wire message, not decompressed.
    fn receive_raw_frames<'py>(
        &self,
        py: Python<'py>,
        n_frames: i64,
    ) -> PyResult<Bound<'py, FrameIter>> {
        self.frame_iter(py, n_frames, FrameMode::Raw, None)
    }

    /// `get_movie`/`stream` core: a `MovieIter` (arms on the first `__next__`).
    #[pyo3(signature = (n_frames, exposure=None, binning=None))]
    pub(crate) fn _movie<'py>(
        slf: &Bound<'py, Self>,
        n_frames: i64,
        exposure: Option<f64>,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, MovieIter>> {
        let py = slf.py();
        guard(py, || {
            let this = slf.get();
            let drv = this.st.drv(py)?;
            let e = Self::exposure(slf, exposure);
            let b = this.binning(binning);
            let n = n_frames.max(0) as u64;
            Bound::new(py, MovieIter::new(this.st.clone(), drv, n, e, b))
        })
    }

    pub(crate) fn get_binning(&self) -> u32 {
        self.st.binning()
    }

    /// `get_image` core [R13]: R7 check → ensure_config(1) → arm → trigger → one frame.
    #[pyo3(signature = (exposure=None, binning=None))]
    pub(crate) fn _acquire_image<'py>(
        slf: &Bound<'py, Self>,
        exposure: Option<f64>,
        binning: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        guard(py, || {
            let this = slf.get();
            let drv = this.st.drv(py)?;
            let e = Self::exposure(slf, exposure);
            let b = this.binning(binning);
            let x = call(py, move |d| drv.acquire_image(e, d))?;
            image_array(py, &x, b)
        })
    }

    /// The oriented Monitor frame, `None` while offline with no cached frame (DESIGN §3.5).
    fn _monitor_frame<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        guard(py, || {
            let Some(drv) = self.st.driver().cloned() else {
                return Ok(None);
            };
            match call(py, move |_| drv.detector().monitor_frame())? {
                Some(f) => Ok(Some(monitor_array(py, f.rows, f.cols, &f.pixels)?)),
                None => Ok(None),
            }
        })
    }

    /// Test hook (DESIGN §8 [R2]): hold a named mutex for `seconds` on this thread (detached).
    #[pyo3(signature = (name, seconds, log=false))]
    fn _test_hold_lock(&self, py: Python<'_>, name: &str, seconds: f64, log: bool) -> PyResult<()> {
        guard(py, || {
            let dur = Duration::from_secs_f64(seconds.clamp(0.0, 60.0));
            if name == "pending" {
                let st = self.st.clone();
                py.detach(move || {
                    let _g = st.pending.lock().unwrap_or_else(PoisonError::into_inner);
                    if log {
                        // a record emitted under the lock: exactly what R6 must survive
                        dectris_core::dlog!(Debug, "_test_hold_lock: holding 'pending'");
                    }
                    std::thread::sleep(dur);
                });
                return Ok(());
            }
            let drv = self.st.drv(py)?;
            let name = name.to_owned();
            let held = py.detach(move || {
                if log {
                    dectris_core::dlog!(Debug, "_test_hold_lock: holding {name:?}");
                }
                drv.detector().test_hold_lock(&name, dur) || drv.stream().test_hold_lock(&name, dur)
            });
            if held {
                return Ok(());
            }
            let mut known = vec!["pending"];
            known.extend_from_slice(dectris_core::simplon::detector::TEST_LOCKS);
            known.extend_from_slice(dectris_core::stream::receiver::TEST_LOCKS);
            py_result(
                py,
                Err(DriverError::InvalidArg(format!(
                    "unknown lock; known: {}",
                    known.join(", ")
                ))),
            )
        })
    }

    // ----- read-only ----------------------------------------------------------------------------

    #[getter]
    fn last_stop_reason(&self) -> Option<&'static str> {
        shielded("last_stop_reason", || {
            stop_str(self.st.last_stop.load(Ordering::Relaxed))
        })
    }

    #[getter]
    fn stats(&self, py: Python<'_>) -> PyResult<StreamStats> {
        guard(py, || {
            guard(py, || snapshot(py, self.st.driver().map(|d| &**d)))
        })
    }

    #[getter]
    fn dcu_reachable(&self) -> bool {
        shielded("dcu_reachable", || {
            self.st.driver().is_some_and(|d| d.dcu_reachable())
        })
    }

    // ----- yaml-backed properties (lenient setters [R33]) ---------------------------------------

    #[getter]
    fn trigger_mode(&self, py: Python<'_>) -> String {
        shielded("trigger_mode", || self.st.settings(py).trigger_mode)
    }

    #[setter(trigger_mode)]
    fn set_trigger_mode_prop(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_trigger_mode_prop", || {
            let Some(m) = coerce_str(v)
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| TRIGGER_MODES.contains(&m.as_str()))
            else {
                return ignored(v, "trigger_mode", self.trigger_mode(py));
            };
            self.st.pending(py).settings.trigger_mode = m.clone();
            if let Some(d) = self.st.driver() {
                py.detach(|| d.detector().set_trigger_mode_local(&m));
            }
        })
    }

    #[getter]
    fn compression(&self, py: Python<'_>) -> String {
        shielded("compression", || self.st.settings(py).compression)
    }

    #[setter(compression)]
    fn set_compression_prop(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_compression_prop", || {
            let Some(c) = coerce_str(v)
                .map(|c| c.trim().to_ascii_lowercase())
                .filter(|c| COMPRESSIONS.contains(&c.as_str()))
            else {
                return ignored(v, "compression", self.compression(py));
            };
            self.st.pending(py).settings.compression = c.clone();
            if let Some(d) = self.st.driver() {
                py.detach(|| d.detector().set_compression_local(&c));
            }
        })
    }

    #[getter(stream_mode)]
    fn prop_stream_mode(&self, py: Python<'_>) -> String {
        shielded("prop_stream_mode", || self.st.settings(py).stream_mode)
    }

    #[setter]
    fn set_stream_mode(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_stream_mode", || match coerce_str(v) {
            Some(s) => self.st.update(py, move |x| x.stream_mode = s.clone()),
            None => ignored(v, "stream_mode", self.prop_stream_mode(py)),
        })
    }

    #[getter]
    fn stream_format(&self, py: Python<'_>) -> String {
        shielded("stream_format", || self.st.settings(py).stream_format)
    }

    #[setter]
    fn set_stream_format(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_stream_format", || match coerce_str(v) {
            Some(s) => self.st.update(py, move |x| x.stream_format = s.clone()),
            None => ignored(v, "stream_format", self.stream_format(py)),
        })
    }

    #[getter]
    fn default_binning(&self) -> u32 {
        shielded("default_binning", || self.st.binning())
    }

    /// Records that yaml set it, so `_connect`'s `default_binsize` does not override it.
    #[setter]
    fn set_default_binning(&self, v: &Bound<'_, PyAny>) {
        shielded("set_default_binning", || match binning_arg(Some(v)) {
            Some(b) => {
                self.st.default_binning.store(b, Ordering::Relaxed);
                self.st.binning_set.store(true, Ordering::Relaxed);
            }
            None => ignored(v, "default_binning", self.st.binning()),
        })
    }

    /// The armed series' start (decoded), or None. Setting None clears the cache.
    #[getter]
    fn _stream_start_config<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        guard(py, || {
            guard(py, || {
                let Some(drv) = self.st.driver().cloned() else {
                    return Ok(None);
                };
                match py.detach(|| drv.stream().start_cache()) {
                    Some(sc) => Ok(Some(start_dict(py, &sc, self.st.decode_start_arrays())?)),
                    None => Ok(None),
                }
            })
        })
    }

    #[setter(_stream_start_config)]
    fn set_stream_start_config(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_stream_start_config", || {
            if v.is_none() {
                if let Some(d) = self.st.driver() {
                    py.detach(|| d.stream().clear_start_cache());
                }
            } else {
                ignored(
                    v,
                    "_stream_start_config",
                    "the received start (only None clears it)",
                );
            }
        })
    }

    // ----- OPTION_KEYS (DESIGN §2.2) ------------------------------------------------------------------

    #[getter]
    fn auto_initialize(&self, py: Python<'_>) -> bool {
        shielded("auto_initialize", || self.st.settings(py).auto_initialize)
    }

    #[setter]
    fn set_auto_initialize(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_auto_initialize", || match coerce_bool(v) {
            Some(b) => self.st.update(py, move |s| s.auto_initialize = b),
            None => ignored(v, "auto_initialize", py_bool(self.auto_initialize(py))),
        })
    }

    #[getter]
    fn receive_timeout(&self, py: Python<'_>) -> Option<f64> {
        shielded("receive_timeout", || match self.st.driver() {
            Some(d) => py.detach(|| d.stream().receive_timeout()),
            None => self.st.pending(py).receive_timeout,
        })
    }

    /// Seconds (> 0) or None (= auto window).
    #[setter]
    fn set_receive_timeout(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_receive_timeout", || {
            let t = if v.is_none() {
                None
            } else {
                match coerce_f64(v).filter(|t| *t > 0.0) {
                    Some(t) => Some(t),
                    None => {
                        let keep = self.receive_timeout(py);
                        return ignored(v, "receive_timeout", py_opt(keep, false));
                    }
                }
            };
            self.st.pending(py).receive_timeout = t;
            if let Some(d) = self.st.driver() {
                py.detach(|| d.stream().set_receive_timeout(t));
            }
        })
    }

    #[getter]
    fn trigger_blocking(&self, py: Python<'_>) -> bool {
        shielded("trigger_blocking", || self.st.settings(py).trigger_blocking)
    }

    #[setter]
    fn set_trigger_blocking(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_trigger_blocking", || match coerce_bool(v) {
            Some(b) => self.st.update(py, move |s| s.trigger_blocking = b),
            None => ignored(v, "trigger_blocking", py_bool(self.trigger_blocking(py))),
        })
    }

    #[getter]
    fn frame_time_policy(&self, py: Python<'_>) -> &'static str {
        shielded("frame_time_policy", || {
            self.st.settings(py).frame_time_policy.as_str()
        })
    }

    #[setter]
    fn set_frame_time_policy(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_frame_time_policy", || {
            match coerce_str(v).and_then(|s| FrameTimePolicy::parse(&s)) {
                Some(p) => self.st.update(py, move |s| s.frame_time_policy = p),
                None => ignored(v, "frame_time_policy", self.frame_time_policy(py)),
            }
        })
    }

    #[getter]
    fn stale_series_policy(&self, py: Python<'_>) -> &'static str {
        shielded("stale_series_policy", || match self.st.driver() {
            Some(d) => py.detach(|| d.stream().stale_policy()).as_str(),
            None => self.st.pending(py).stale.as_str(),
        })
    }

    #[setter]
    fn set_stale_series_policy(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_stale_series_policy", || {
            let Some(p) = coerce_str(v).and_then(|s| StalePolicy::parse(&s)) else {
                return ignored(v, "stale_series_policy", self.stale_series_policy(py));
            };
            self.st.pending(py).stale = p;
            if let Some(d) = self.st.driver() {
                py.detach(|| d.stream().set_stale_policy(p));
            }
        })
    }

    #[getter]
    fn series_attribution(&self, py: Python<'_>) -> &'static str {
        shielded("series_attribution", || match self.st.driver() {
            Some(d) => py.detach(|| d.stream().attribution_mode()).as_str(),
            None => self.st.pending(py).attribution.as_str(),
        })
    }

    #[setter]
    fn set_series_attribution(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_series_attribution", || {
            let Some(m) = coerce_str(v).and_then(|s| AttributionMode::parse(&s)) else {
                return ignored(v, "series_attribution", self.series_attribution(py));
            };
            self.st.pending(py).attribution = m;
            if let Some(d) = self.st.driver() {
                py.detach(|| d.stream().set_attribution_mode(m));
            }
        })
    }

    #[getter]
    fn single_frame_fresh(&self, py: Python<'_>) -> bool {
        shielded("single_frame_fresh", || match self.st.driver() {
            Some(d) => d.single_frame_fresh(),
            None => self.st.pending(py).single_frame_fresh,
        })
    }

    #[setter]
    fn set_single_frame_fresh(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_single_frame_fresh", || {
            let Some(b) = coerce_bool(v) else {
                return ignored(
                    v,
                    "single_frame_fresh",
                    py_bool(self.single_frame_fresh(py)),
                );
            };
            self.st.pending(py).single_frame_fresh = b;
            if let Some(d) = self.st.driver() {
                d.set_single_frame_fresh(b);
            }
        })
    }

    /// Process-wide (per-frame DEBUG records and Drop records).
    #[getter]
    fn log_frames(&self) -> bool {
        shielded("log_frames", log_frames)
    }

    #[setter]
    fn set_log_frames(&self, v: &Bound<'_, PyAny>) {
        shielded("set_log_frames", || match coerce_bool(v) {
            Some(b) => set_log_frames(b),
            None => ignored(v, "log_frames", py_bool(log_frames())),
        })
    }

    #[getter]
    fn monitor_max_hz(&self, py: Python<'_>) -> f64 {
        shielded("monitor_max_hz", || self.st.settings(py).monitor_max_hz)
    }

    #[setter]
    fn set_monitor_max_hz(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_monitor_max_hz", || match coerce_f64(v) {
            Some(hz) => self.st.update(py, move |s| s.monitor_max_hz = hz),
            None => ignored(v, "monitor_max_hz", self.monitor_max_hz(py)),
        })
    }

    /// Applies at `_connect` (and every later reconnect of this driver).
    #[getter]
    fn zmq_tcp_keepalive(&self, py: Python<'_>) -> bool {
        shielded("zmq_tcp_keepalive", || self.st.pending(py).tcp_keepalive)
    }

    #[setter]
    fn set_zmq_tcp_keepalive(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_zmq_tcp_keepalive", || match coerce_bool(v) {
            Some(b) => self.st.pending(py).tcp_keepalive = b,
            None => ignored(v, "zmq_tcp_keepalive", py_bool(self.zmq_tcp_keepalive(py))),
        })
    }

    /// TCP receive buffer in bytes, 0 = the OS decides. Applies at `_connect` (and every later
    /// reconnect of this driver).
    #[getter]
    fn zmq_rcvbuf(&self, py: Python<'_>) -> i32 {
        shielded("zmq_rcvbuf", || self.st.pending(py).rcvbuf)
    }

    #[setter]
    fn set_zmq_rcvbuf(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_zmq_rcvbuf", || {
            match coerce_f64(v).filter(|x| x.fract() == 0.0 && (0.0..=i32::MAX as f64).contains(x)) {
                Some(x) => self.st.pending(py).rcvbuf = x as i32,
                None => ignored(v, "zmq_rcvbuf", self.zmq_rcvbuf(py)),
            }
        })
    }

    /// None = never PUT `header_detail`.
    #[getter]
    fn stream_header_detail(&self, py: Python<'_>) -> Option<String> {
        shielded("stream_header_detail", || {
            self.st.settings(py).stream_header_detail
        })
    }

    #[setter]
    fn set_stream_header_detail(&self, py: Python<'_>, v: &Bound<'_, PyAny>) {
        shielded("set_stream_header_detail", || {
            if v.is_none() {
                return self.st.update(py, |s| s.stream_header_detail = None);
            }
            match coerce_str(v) {
                Some(h) => self
                    .st
                    .update(py, move |s| s.stream_header_detail = Some(h.clone())),
                None => {
                    let keep = self.stream_header_detail(py);
                    ignored(v, "stream_header_detail", py_opt(keep, true));
                }
            }
        })
    }

    #[getter]
    fn stream_decode_start_arrays(&self) -> bool {
        shielded("stream_decode_start_arrays", || {
            self.st.decode_start_arrays()
        })
    }

    #[setter]
    fn set_stream_decode_start_arrays(&self, v: &Bound<'_, PyAny>) {
        shielded("set_stream_decode_start_arrays", || match coerce_bool(v) {
            Some(b) => self.st.decode_start_arrays.store(b, Ordering::Relaxed),
            None => ignored(
                v,
                "stream_decode_start_arrays",
                py_bool(self.st.decode_start_arrays()),
            ),
        })
    }
}
