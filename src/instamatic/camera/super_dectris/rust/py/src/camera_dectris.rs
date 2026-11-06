//! `CameraDectris`: the instamatic camera class (interface `dectris_simplon`, docs/DESIGN.md
//! §2.2).
//!
//! `DectrisCore` does every HTTP/ZMQ/decode step and knows nothing of instamatic. This class adds
//! what instamatic asks of a camera (`CameraBase`, which it cannot inherit: a `#[pyclass]` has no
//! Python base; `super_dectris/__init__.py` registers it as a virtual subclass): yaml loading,
//! `get_image`/`get_movie`, the Monitor preview, the Windows timer and the exit hooks. This file is
//! the only place in the extension that imports `instamatic` (its config and version).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dectris_core::preview;
use numpy::PyArray2;
use pyo3::exceptions::{PyException, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyTuple};
use pyo3::PyClassInitializer;

use crate::core::{call, monitor_array, warn, DectrisCore};
use crate::errors::guard;
use crate::session::Session;

// Empty, and still sent: a detector keeps the last value, which other software may have set.
const HEADER_APPENDIX: &str = "";
/// `get_preview_image` scales to fit this, and returns zeros of this size when it has no frame.
const PREVIEW_SIZE: usize = 640;
/// cRED reads `getattr(cam, 'ZMQ_RCVHWM', 1000)`.
const DEFAULT_RCVHWM: i32 = 500;

// ---------------------------------------------------------------------------------------------
// The Windows timer
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

/// `timeBeginPeriod(1)` / `timeEndPeriod(1)`: the process-wide Windows timer tick. `false` when
/// Windows refused.
#[cfg(windows)]
fn set_time_period(on: bool) -> bool {
    // SAFETY: plain calls with a value argument; Windows counts the requests.
    let code = unsafe {
        if on {
            timeBeginPeriod(1)
        } else {
            timeEndPeriod(1)
        }
    };
    code == 0
}

#[cfg(not(windows))]
fn set_time_period(_on: bool) -> bool {
    true
}

static TIMER_BEGINS: AtomicU64 = AtomicU64::new(0);
static TIMER_ENDS: AtomicU64 = AtomicU64::new(0);

/// Test hook: how many `begin`/`end` reached Windows in this process.
#[pyfunction]
pub fn _fine_timer_counts() -> (u64, u64) {
    (
        TIMER_BEGINS.load(Ordering::Relaxed),
        TIMER_ENDS.load(Ordering::Relaxed),
    )
}

/// Holds the Windows timer at 1 ms while a camera is connected.
///
/// A frame's return to the GIL from a busy Python thread waits for a timer tick, 15.6 ms by default:
/// at 100 fps that loses frames (DESIGN §2.2 [R9]). Windows counts the requests,
/// so each holder only has to end what it began.
#[derive(Default)]
struct FineTimer {
    enabled: AtomicBool,
    held: AtomicBool,
}

impl FineTimer {
    fn begin(&self) {
        if !self.enabled.load(Ordering::Relaxed) || self.held.swap(true, Ordering::Relaxed) {
            return;
        }
        if set_time_period(true) {
            TIMER_BEGINS.fetch_add(1, Ordering::Relaxed);
        } else {
            self.held.store(false, Ordering::Relaxed);
            warn("Windows timer left at its default".into());
        }
    }

    fn end(&self) {
        if self.held.swap(false, Ordering::Relaxed) {
            set_time_period(false);
            TIMER_ENDS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        self.end();
    }
}

// ---------------------------------------------------------------------------------------------
// Exit hooks
// ---------------------------------------------------------------------------------------------

/// Live cameras for the exit hooks: a `weakref.WeakSet`, so the hooks keep no instance alive
/// (review #8).
static LIVE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static HOOKS_REGISTERED: AtomicBool = AtomicBool::new(false);

fn live(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    LIVE.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py.import("weakref")?.getattr("WeakSet")?.call0()?.unbind())
    })
    .map(|set| set.bind(py))
}

fn each_live(py: Python<'_>, method: &str) -> PyResult<()> {
    let cameras: Vec<_> = live(py)?.try_iter()?.collect::<PyResult<_>>()?;
    for cam in cameras {
        cam.call_method0(method)?;
    }
    Ok(())
}

#[pyfunction]
fn _begin_shutdown_all(py: Python<'_>) -> PyResult<()> {
    each_live(py, "_begin_shutdown")
}

#[pyfunction]
fn _final_release_all(py: Python<'_>) -> PyResult<()> {
    each_live(py, "_final_release")
}

/// Once per process: `_final_release` = release_connection() + close logging (exit only) [G1-C];
/// `_begin_shutdown` runs in threading._shutdown, before atexit [R6].
fn register_exit_hooks(py: Python<'_>) -> PyResult<()> {
    if HOOKS_REGISTERED.swap(true, Ordering::Relaxed) {
        return Ok(());
    }
    py.import("atexit")?
        .call_method1("register", (wrap_pyfunction!(_final_release_all, py)?,))?;
    if let Some(reg) = py.import("threading")?.getattr_opt("_register_atexit")? {
        // "can't register atexit after shutdown"
        if let Err(e) = reg.call1((wrap_pyfunction!(_begin_shutdown_all, py)?,)) {
            if !e.is_instance_of::<PyRuntimeError>(py) {
                return Err(e);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// CameraDectris
// ---------------------------------------------------------------------------------------------

/// `int(v)`.
fn py_int(v: &Bound<'_, PyAny>) -> PyResult<i64> {
    v.py().import("builtins")?.getattr("int")?.call1((v,))?.extract()
}

/// `binning or kwargs.get('binsize')`.
fn binning_or_binsize<'py>(
    binning: Option<&Bound<'py, PyAny>>,
    kwargs: Option<&Bound<'py, PyDict>>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    if let Some(b) = binning {
        if b.is_truthy()? {
            return Ok(Some(b.clone()));
        }
    }
    match kwargs {
        Some(k) => k.get_item("binsize"),
        None => Ok(None),
    }
}

/// Dectris SINGLA via SIMPLON REST + Stream V2.
#[pyclass(
    extends = DectrisCore,
    subclass,
    frozen,
    dict,
    weakref,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "CameraDectris"
)]
pub struct CameraDectris {
    timer: FineTimer,
    preview_failing: AtomicBool,
}

impl CameraDectris {
    /// The Monitor frame scaled for the live view; `None` while offline with no cached frame.
    fn preview<'py>(slf: &Bound<'py, Self>) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = slf.py();
        let Some(drv) = slf.as_super().get().st.driver().cloned() else {
            return Ok(None);
        };
        let scaled = call(py, move |_| {
            match drv.detector().monitor_frame()? {
                Some(f) => preview::fit(f.rows, f.cols, &f.pixels, PREVIEW_SIZE).map(Some),
                None => Ok(None),
            }
        })?;
        match scaled {
            Some((rows, cols, pixels)) => Ok(Some(monitor_array(py, rows, cols, &pixels)?)),
            None => Ok(None),
        }
    }

    fn connect(slf: &Bound<'_, Self>) -> PyResult<()> {
        let py = slf.py();
        let app = py.import("instamatic")?;
        let ua = format!(
            "{}/{}",
            app.getattr("__app_title__")?.str()?,
            app.getattr("__app_version__")?.str()?
        );
        let endpoint = |scheme: &str, host: &str, port: &str| -> PyResult<String> {
            Ok(format!(
                "{scheme}://{}:{}",
                slf.getattr(host)?.str()?,
                slf.getattr(port)?.str()?
            ))
        };
        slf.setattr("DCU_endpoint", endpoint("http", "DCU_host", "DCU_port")?)?;
        slf.setattr("ZMQ_stream", endpoint("tcp", "ZMQ_host", "ZMQ_port")?)?;
        slf.setattr("convert_uint16", false)?;
        slf.setattr("session", Bound::new(py, Session::new(&ua))?)?;

        // used only if yaml did not set default_binning
        let binsize = match slf.getattr_opt("default_binsize")? {
            Some(b) => py_int(&b)?,
            None => 1,
        };
        let dimensions = py
            .import("builtins")?
            .getattr("list")?
            .call1((slf.getattr("dimensions")?,))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("user_agent", &ua)?;
        kwargs.set_item("header_appendix", HEADER_APPENDIX)?;
        slf.call_method(
            "_connect",
            (
                slf.getattr("DCU_endpoint")?,
                slf.getattr("ZMQ_stream")?,
                slf.getattr("ZMQ_RCVHWM")?,
                binsize,
                dimensions,
            ),
            Some(&kwargs),
        )?;
        Ok(())
    }
}

#[pymethods]
impl CameraDectris {
    /// Unconnected, no I/O; `__init__` does the rest, so a subclass's `load_defaults` is called.
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(
        _args: &Bound<'_, PyTuple>,
        _kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyClassInitializer<Self> {
        PyClassInitializer::from(DectrisCore::unconnected()).add_subclass(CameraDectris {
            timer: FineTimer::default(),
            preview_failing: AtomicBool::new(false),
        })
    }

    #[pyo3(signature = (name=None))]
    fn __init__(slf: &Bound<'_, Self>, name: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let py = slf.py();
        guard(py, || {
            let name = match name.filter(|n| !n.is_none()) {
                Some(n) => n,
                None => {
                    // reuse the live Dectris config without reloading it [G1-K]
                    let camera = py.import("instamatic.config")?.getattr("camera")?;
                    let interface = camera.getattr_opt("interface")?;
                    match interface {
                        Some(i) if i.eq("dectris_simplon")? => camera.getattr("name")?,
                        _ => "dectris-simplon".into_pyobject(py)?.into_any(),
                    }
                }
            };
            slf.setattr("name", name)?;
            slf.call_method0("load_defaults")?; // every yaml key -> setattr; the Rust setters are lenient [R33]
            if let Some(v) = slf.getattr_opt("zmq_rcvhwm")?.filter(|v| !v.is_none()) {
                match py_int(&v) {
                    Ok(hwm) => slf.setattr("ZMQ_RCVHWM", hwm)?,
                    Err(e) if e.is_instance_of::<PyTypeError>(py) || e.is_instance_of::<PyValueError>(py) => {
                        warn(format!("zmq_rcvhwm={} ignored; using {DEFAULT_RCVHWM}", v.repr()?));
                    }
                    Err(e) => return Err(e),
                }
            }
            Self::connect(slf)?;
            let this = slf.get();
            let enabled = match slf.getattr_opt("windows_fine_timer")? {
                Some(v) => v.is_truthy()?,
                None => true,
            };
            this.timer
                .enabled
                .store(enabled && cfg!(windows), Ordering::Relaxed);
            this.timer.begin();
            live(py)?.call_method1("add", (slf,))?;
            register_exit_hooks(py)
        })
    }

    #[classattr]
    fn streamable() -> bool {
        true
    }

    #[classattr]
    #[pyo3(name = "ZMQ_RCVHWM")]
    fn zmq_rcvhwm_default() -> i32 {
        DEFAULT_RCVHWM
    }

    // ----- what `CameraBase` gave ----------------------------------------------------------------

    fn load_defaults(slf: &Bound<'_, Self>) -> PyResult<()> {
        let py = slf.py();
        let config = py.import("instamatic.config")?;
        let name = slf.getattr("name")?;
        if name.ne(config.getattr("settings")?.getattr("camera")?)? {
            let kwargs = PyDict::new(py);
            kwargs.set_item("camera_name", &name)?;
            config.call_method("load_camera_config", (), Some(&kwargs))?;
        }
        let mapping = config.getattr("camera")?.getattr("mapping")?;
        for item in mapping.call_method0("items")?.try_iter()? {
            let (key, value): (String, Bound<'_, PyAny>) = item?.extract()?;
            slf.setattr(key.as_str(), value)?;
        }
        Ok(())
    }

    fn get_name<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        slf.getattr("name")
    }

    fn get_camera_dimensions<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        slf.getattr("dimensions")
    }

    fn __enter__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, Self>> {
        slf.call_method0("establish_connection")?;
        Ok(slf.clone())
    }

    fn __exit__(
        slf: &Bound<'_, Self>,
        _kind: &Bound<'_, PyAny>,
        _value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        slf.call_method0("release_connection")?;
        Ok(())
    }

    // ----- connection ----------------------------------------------------------------------------

    fn establish_connection(slf: &Bound<'_, Self>) -> PyResult<()> {
        slf.as_super().get().establish_connection(slf.py())?;
        slf.get().timer.begin();
        Ok(())
    }

    fn release_connection(slf: &Bound<'_, Self>) {
        DectrisCore::release(slf.as_super());
        slf.get().timer.end();
    }

    fn close(slf: &Bound<'_, Self>) {
        Self::release_connection(slf);
    }

    // ----- images --------------------------------------------------------------------------------

    fn get_image_dimensions(slf: &Bound<'_, Self>) -> PyResult<(i64, i64)> {
        let b = f64::from(slf.as_super().get().get_binning());
        let dims = slf.getattr("dimensions")?;
        let scaled = |i: usize| -> PyResult<i64> {
            Ok((dims.get_item(i)?.extract::<f64>()? / b) as i64)
        };
        Ok((scaled(0)?, scaled(1)?))
    }

    /// The live view; never raises for a failed frame: zeros and one WARNING per failure episode
    /// [G1-J], silent zeros during the offline back-off with no cached frame.
    #[pyo3(signature = (binning=None, **_kwargs))]
    fn get_preview_image<'py>(
        slf: &Bound<'py, Self>,
        binning: Option<&Bound<'py, PyAny>>,
        _kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let _ = binning;
        let py = slf.py();
        guard(py, || {
            let this = slf.get();
            let frame = match Self::preview(slf) {
                Ok(frame) => {
                    if frame.is_some() {
                        this.preview_failing.store(false, Ordering::Relaxed);
                    }
                    frame
                }
                Err(e) if e.is_instance_of::<PyException>(py) => {
                    if !this.preview_failing.swap(true, Ordering::Relaxed) {
                        warn(format!("Monitor preview failed: {e}"));
                    }
                    None
                }
                Err(e) => return Err(e), // KeyboardInterrupt
            };
            Ok(frame.unwrap_or_else(|| {
                PyArray2::<u16>::zeros(py, [PREVIEW_SIZE, PREVIEW_SIZE], false).into_any()
            }))
        })
    }

    #[pyo3(signature = (exposure=None, binning=None, **kwargs))]
    fn get_image<'py>(
        slf: &Bound<'py, Self>,
        exposure: Option<f64>,
        binning: Option<&Bound<'py, PyAny>>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let b = binning_or_binsize(binning, kwargs)?;
        DectrisCore::_acquire_image(slf.as_super(), exposure, b.as_ref())
    }

    #[pyo3(signature = (n_frames, exposure=None, binning=None, **kwargs))]
    fn get_movie<'py>(
        slf: &Bound<'py, Self>,
        n_frames: i64,
        exposure: Option<f64>,
        binning: Option<&Bound<'py, PyAny>>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, crate::iters::MovieIter>> {
        let b = binning_or_binsize(binning, kwargs)?;
        DectrisCore::_movie(slf.as_super(), n_frames, exposure, b.as_ref())
    }

    /// Alias of `get_movie` (DESIGN §2.3).
    #[pyo3(signature = (n_frames, exposure=None, binning=None, **kwargs))]
    fn stream<'py>(
        slf: &Bound<'py, Self>,
        n_frames: i64,
        exposure: Option<f64>,
        binning: Option<&Bound<'py, PyAny>>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, crate::iters::MovieIter>> {
        Self::get_movie(slf, n_frames, exposure, binning, kwargs)
    }
}

/// `CamClient._init_dict` enumerates `get_camera_class(interface).__dict__` (camera_client.py): the
/// `DectrisCore` members live on the base type, so copy their descriptors into this class [R39].
pub fn copy_core_members(py: Python<'_>) -> PyResult<()> {
    let (core, cam) = (py.get_type::<DectrisCore>(), py.get_type::<CameraDectris>());
    let (core_dict, cam_dict) = (core.getattr("__dict__")?, cam.getattr("__dict__")?);
    for key in core.dir()? {
        let key: String = key.extract()?;
        if !key.starts_with('_') && !cam_dict.contains(key.as_str())? {
            cam.setattr(key.as_str(), core_dict.get_item(key.as_str())?)?;
        }
    }
    Ok(())
}
