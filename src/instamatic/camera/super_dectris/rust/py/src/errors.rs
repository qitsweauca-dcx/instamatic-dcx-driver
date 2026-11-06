//! Exception classes, `DriverError` → `PyErr`, and the panic guard (docs/DESIGN.md §6 [R30]).
//!
//! The classes are built with builtins `type(name, bases, dict)` at module init, because
//! `create_exception!` allows a single base only and `DectrisConnectionError`/`DectrisTimeout`/
//! `StreamDecodeError` need two. Each dict carries `__module__` and `__qualname__`, so the classes
//! report `instamatic.camera.super_dectris._super_dectris.<Name>`.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};

use dectris_core::{DriverError, ExceptionKind};
use pyo3::exceptions::{PyConnectionError, PyException, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyTuple, PyType};

pub const MODULE: &str = "instamatic.camera.super_dectris._super_dectris";

/// The message of the sentinel error a wait returns when the main-thread interrupt hook caught a
/// pending signal; [`to_pyerr`] swaps it back for the stashed `KeyboardInterrupt` (DESIGN §6).
pub const INTERRUPTED: &str = "interrupted";

pub struct Exceptions {
    pub base: Py<PyType>,
    pub http: Py<PyType>,
    pub connection: Py<PyType>,
    pub timeout: Py<PyType>,
    pub state: Py<PyType>,
    pub busy: Py<PyType>,
    pub decode: Py<PyType>,
}

static EXC: PyOnceLock<Exceptions> = PyOnceLock::new();

thread_local! {
    /// The `PyErr` raised by `check_signals()` inside a sliced wait on the main thread.
    static STASHED: RefCell<Option<PyErr>> = const { RefCell::new(None) };
}

/// Keep the interrupt for [`to_pyerr`] (called by the interrupt hook, attached).
pub fn stash_interrupt(e: PyErr) {
    STASHED.with(|s| *s.borrow_mut() = Some(e));
}

/// The stashed interrupt that no error carried out (a never-raising wait swallowed it).
pub fn take_stashed() -> Option<PyErr> {
    STASHED.with(|s| s.borrow_mut().take())
}

fn make(
    py: Python<'_>,
    name: &str,
    bases: Vec<Bound<'_, PyAny>>,
    doc: &str,
) -> PyResult<Py<PyType>> {
    let type_ = py.import("builtins")?.getattr("type")?;
    let d = PyDict::new(py);
    d.set_item("__module__", MODULE)?;
    d.set_item("__qualname__", name)?;
    d.set_item("__doc__", doc)?;
    let t = type_.call1((name, PyTuple::new(py, bases)?, d))?;
    Ok(t.cast_into::<PyType>()?.unbind())
}

fn build(py: Python<'_>) -> PyResult<Exceptions> {
    let exc = py.get_type::<PyException>().into_any();
    let base = make(
        py,
        "DectrisError",
        vec![exc],
        "Base class of the Dectris driver errors.",
    )?;
    let b = || base.bind(py).clone().into_any();
    let http = make(
        py,
        "DectrisHTTPError",
        vec![b()],
        "A SIMPLON request answered with a status other than 200 (.status_code, .url).",
    )?;
    let connection = make(
        py,
        "DectrisConnectionError",
        vec![b(), py.get_type::<PyConnectionError>().into_any()],
        "The DCU could not be reached.",
    )?;
    let timeout = make(
        py,
        "DectrisTimeout",
        vec![b(), py.get_type::<PyTimeoutError>().into_any()],
        "A request or a receive window timed out.",
    )?;
    let state = make(
        py,
        "DetectorStateError",
        vec![b()],
        "The detector is in a state that does not allow the operation.",
    )?;
    let busy = make(
        py,
        "DectrisBusy",
        vec![b()],
        "The detector or the receiver is owned by another thread or series.",
    )?;
    let decode = make(
        py,
        "StreamDecodeError",
        vec![b(), py.get_type::<PyValueError>().into_any()],
        "A Stream V2 message could not be decoded.",
    )?;
    Ok(Exceptions {
        base,
        http,
        connection,
        timeout,
        state,
        busy,
        decode,
    })
}

/// Create the classes (once) and add them to the module.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let e = EXC.get_or_try_init(py, || build(py))?;
    for (name, t) in [
        ("DectrisError", &e.base),
        ("DectrisHTTPError", &e.http),
        ("DectrisConnectionError", &e.connection),
        ("DectrisTimeout", &e.timeout),
        ("DetectorStateError", &e.state),
        ("DectrisBusy", &e.busy),
        ("StreamDecodeError", &e.decode),
    ] {
        m.add(name, t.clone_ref(py))?;
    }
    Ok(())
}

fn exc(py: Python<'_>) -> PyResult<&Exceptions> {
    EXC.get_or_try_init(py, || build(py))
}

/// Instantiate one of our classes with exactly one positional message (OSError-derived classes
/// never see errno/strerror unpacking, DESIGN §6).
fn instance<'py>(py: Python<'py>, t: &Py<PyType>, msg: String) -> PyResult<Bound<'py, PyAny>> {
    t.bind(py).call1((msg,))
}

/// `DriverError` → the mapped Python exception (DESIGN §6).
pub fn to_pyerr(py: Python<'_>, e: DriverError) -> PyErr {
    if matches!(&e, DriverError::Internal(m) if m == INTERRUPTED) {
        if let Some(stashed) = STASHED.with(|s| s.borrow_mut().take()) {
            return stashed;
        }
    }
    match build_err(py, e) {
        Ok(err) => err,
        Err(err) => err,
    }
}

fn build_err(py: Python<'_>, e: DriverError) -> PyResult<PyErr> {
    let x = exc(py)?;
    let msg = e.to_string();
    let obj = match (e.kind(), &e) {
        (ExceptionKind::ValueError, _) => return Ok(PyValueError::new_err(msg)),
        (ExceptionKind::Http, DriverError::Http { status, url, .. }) => {
            let o = instance(py, &x.http, msg)?;
            o.setattr("status_code", *status)?;
            o.setattr("url", url.as_str())?;
            o
        }
        (ExceptionKind::Http, _) => instance(py, &x.http, msg)?,
        (ExceptionKind::Connection, _) => instance(py, &x.connection, msg)?,
        (ExceptionKind::Timeout, _) => instance(py, &x.timeout, msg)?,
        (ExceptionKind::DetectorState, _) => instance(py, &x.state, msg)?,
        (ExceptionKind::Busy, _) => {
            // `DectrisBusy('detector owned by a running series on thread <name>')` (DESIGN §3.2 R7)
            let m = match &e {
                DriverError::Busy(m) => m.clone(),
                _ => msg,
            };
            instance(py, &x.busy, m)?
        }
        (ExceptionKind::StreamDecode, _) => instance(py, &x.decode, msg)?,
        (ExceptionKind::Generic, _) => instance(py, &x.base, msg)?,
    };
    Ok(PyErr::from_value(obj))
}

/// `Result<T, DriverError>` → `PyResult<T>`.
pub fn py_result<T>(py: Python<'_>, r: dectris_core::Result<T>) -> PyResult<T> {
    r.map_err(|e| to_pyerr(py, e))
}

/// A `DectrisError` with a message (programming errors, "not connected").
pub fn dectris_error(py: Python<'_>, msg: impl Into<String>) -> PyErr {
    to_pyerr(py, DriverError::Internal(msg.into()))
}

/// `ffi_guard!` (DESIGN §6): run a `#[pymethods]`/`#[pyfunction]` body under `catch_unwind`, so a
/// panic surfaces as `DectrisError("internal error: …")`, never as `PanicException`.
pub fn guard<T>(py: Python<'_>, f: impl FnOnce() -> PyResult<T>) -> PyResult<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        // from_panic → Internal("internal error: <payload>")
        Err(payload) => Err(to_pyerr(py, DriverError::from_panic(payload.as_ref()))),
    }
}
