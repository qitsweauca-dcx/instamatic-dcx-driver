//! `instamatic.camera.super_dectris._super_dectris` — the PyO3 extension (docs/DESIGN.md §2).
//!
//! Module surface (DESIGN §2.1): the `CameraDectris` camera class on the `DectrisCore` driver type,
//! `DectrisCore`'s iterators, `cam.session`'s classes, the exception
//! classes, `_STREAM_END`, `decode_message`, `decompress_image`, `decode_raw_header`,
//! `decompress`, `reset_log_cache`, `OPTION_KEYS`, `TYPED_ARRAY_DTYPES`, `tag_decoders`,
//! `__version__`.

use std::borrow::Cow;

use dectris_core::codec::TYPED_ARRAY_DTYPES as DTYPES;
use dectris_core::codec::{
    decode_message as decode_cbor, decompress as decompress_named, parse_image_message, TypedArray,
    DEFAULT_CHANNEL,
};
use dectris_core::DriverError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyTuple};

mod camera_dectris;
mod convert;
mod core;
mod errors;
mod iters;
mod logging;
mod session;
mod stats;

use convert::{duck_multidim_to_array, message_to_dict, raw_header, typed_to_py};
use errors::{guard, py_result, to_pyerr};

/// The optional yaml keys (DESIGN §2.2); each is also a writable `DectrisCore` property.
pub const OPTION_KEYS: [&str; 13] = [
    "auto_initialize",
    "receive_timeout",
    "trigger_blocking",
    "frame_time_policy",
    "stale_series_policy",
    "series_attribution",
    "single_frame_fresh",
    "log_frames",
    "monitor_max_hz",
    "zmq_tcp_keepalive",
    "zmq_rcvbuf",
    "stream_header_detail",
    "stream_decode_start_arrays",
];

/// `_STREAM_END`: one `object()` (DESIGN §2.1).
static STREAM_END: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

pub fn stream_end(py: Python<'_>) -> PyResult<Py<PyAny>> {
    STREAM_END
        .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
            Ok(py.import("builtins")?.getattr("object")?.call0()?.unbind())
        })
        .map(|o| o.clone_ref(py))
}

/// `decode_message(raw: bytes) -> dict`: every top-level key; typed arrays as ndarrays (DESIGN §2.7).
#[pyfunction]
fn decode_message<'py>(py: Python<'py>, raw: &Bound<'py, PyBytes>) -> PyResult<Bound<'py, PyDict>> {
    guard(py, || {
        let bytes = raw.as_bytes();
        let msg = py.detach(|| decode_cbor(bytes).map(|m| m.entries));
        let entries = py_result(py, msg)?;
        message_to_dict(py, &entries)
    })
}

/// `decompress_image(message) -> ndarray` (DESIGN §2.7): `bytes` (decoded first), or a dict whose
/// `data['threshold_1']` is an ndarray (returned) or a `.tag/.value` tag 40/1040 object.
#[pyfunction]
fn decompress_image<'py>(
    py: Python<'py>,
    message: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    decompress_image_any(py, message)
}

/// Body of `decompress_image` (also `DectrisCore.decompress_image`).
pub(crate) fn decompress_image_any<'py>(
    py: Python<'py>,
    message: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    guard(py, || {
        let msg = if let Ok(b) = message.cast::<PyBytes>() {
            // fast path: validate + decode `threshold_1` straight into a new array
            if let Ok(p) = parse_image_message(b.as_bytes(), DEFAULT_CHANNEL) {
                return convert::decode_payload_into_new_array(py, &p.payload);
            }
            decode_message(py, b)?.into_any()
        } else {
            message.clone()
        };
        let channel = "threshold_1";
        let data = msg.get_item("data").ok();
        let ch = data.as_ref().and_then(|d| d.get_item(channel).ok());
        let Some(ch) = ch else {
            let keys: Vec<String> = match &data {
                Some(d) => d
                    .call_method0("keys")
                    .and_then(|k| {
                        k.try_iter()?
                            .map(|x| x?.str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                None => msg
                    .call_method0("keys")
                    .and_then(|k| {
                        k.try_iter()?
                            .map(|x| x?.str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            let where_ = if data.is_some() {
                "message['data']"
            } else {
                "message"
            };
            return Err(to_pyerr(
                py,
                DriverError::decode(
                    0,
                    format!("no '{channel}' channel in {where_}; keys: {keys:?}"),
                ),
            ));
        };
        let ndarray = py.import("numpy")?.getattr("ndarray")?;
        if ch.is_instance(&ndarray)? {
            return Ok(ch);
        }
        duck_multidim_to_array(py, &ch)
    })
}

/// `decode_raw_header(raw: bytes) -> dict` (turbo classify; no decompression).
#[pyfunction]
fn decode_raw_header<'py>(
    py: Python<'py>,
    raw: &Bound<'py, PyBytes>,
) -> PyResult<Bound<'py, PyDict>> {
    guard(py, || raw_header(py, raw.as_bytes()))
}

/// `decompress(algorithm: str, elem_size: int, data: bytes) -> bytes` (oracle hook).
#[pyfunction]
fn decompress<'py>(
    py: Python<'py>,
    algorithm: &str,
    elem_size: u64,
    data: &Bound<'py, PyBytes>,
) -> PyResult<Bound<'py, PyBytes>> {
    guard(py, || {
        let bytes = data.as_bytes();
        let alg = algorithm.as_bytes();
        let out = py.detach(|| decompress_named(alg, elem_size, bytes));
        Ok(PyBytes::new(py, &py_result(py, out)?))
    })
}

/// `tag_decoders[tag](value)`: the per-tag decoders `decode_message` applies, for values decoded
/// by another CBOR library (cbor2 `CBORTag.value`).
#[pyfunction]
fn _decode_tag<'py>(
    py: Python<'py>,
    tag: u64,
    value: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    guard(py, || match tag {
        0 => Ok(value.str()?.into_any()),
        56500 => {
            let (alg, modifier, data) = convert::tag56500_parts(value).ok_or_else(|| {
                to_pyerr(
                    py,
                    DriverError::decode(0, "tag 56500 content must be [str, uint, bytes]"),
                )
            })?;
            let out = py.detach(|| decompress_named(alg.as_bytes(), modifier, &data));
            Ok(PyBytes::new(py, &py_result(py, out)?).into_any())
        }
        64..=87 => {
            let bytes: Vec<u8> = if value.is_instance_of::<PyBytes>() {
                value.extract()?
            } else {
                // a nested tag 56500 (cbor2 CBORTag or our CBORTag)
                let t: u64 = value.getattr("tag")?.extract()?;
                let inner = value.getattr("value")?;
                _decode_tag(py, t, &inner)?.extract()?
            };
            let t = py_result(py, TypedArray::from_wire(tag, Cow::Owned(bytes), None))?;
            typed_to_py(py, &t)
        }
        40 | 1040 => {
            let (dims, arr) = convert::items(value, 2)
                .and_then(|v| Some((v[0].extract::<Vec<usize>>().ok()?, v[1].clone())))
                .ok_or_else(|| {
                    to_pyerr(
                        py,
                        DriverError::decode(0, "tag 40/1040 content must be [dims, array]"),
                    )
                })?;
            let ndarray = py.import("numpy")?.getattr("ndarray")?;
            if arr.is_instance(&ndarray)? {
                let kw = PyDict::new(py);
                kw.set_item("order", if tag == 1040 { "F" } else { "C" })?;
                let shaped = arr.call_method("reshape", (dims,), Some(&kw))?;
                let kw = PyDict::new(py);
                kw.set_item("order", "C")?;
                return shaped.call_method("copy", (), Some(&kw));
            }
            let obj = Bound::new(
                py,
                convert::CborTag {
                    tag,
                    value: value.clone().unbind(),
                },
            )?;
            duck_multidim_to_array(py, obj.as_any())
        }
        t => Err(to_pyerr(
            py,
            DriverError::decode(0, format!("tag {t} has no decoder")),
        )),
    })
}

#[pymodule]
fn _super_dectris(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    logging::install(py)?;
    errors::register(m)?;
    m.add("_STREAM_END", stream_end(py)?)?;
    m.add("OPTION_KEYS", PyTuple::new(py, OPTION_KEYS)?)?;

    let dtypes = PyDict::new(py);
    for (tag, dt) in DTYPES {
        dtypes.set_item(tag, dt)?;
    }
    m.add("TYPED_ARRAY_DTYPES", dtypes)?;

    // The Python main thread, for the interrupt hook (DESIGN §3.2 R4); only if imported on it.
    let threading = py.import("threading")?;
    if threading
        .call_method0("current_thread")?
        .is(&threading.call_method0("main_thread")?)
    {
        let _ = core::MAIN_THREAD.set(std::thread::current().id());
    }

    m.add_class::<core::DectrisCore>()?;
    m.add_class::<camera_dectris::CameraDectris>()?;
    camera_dectris::copy_core_members(py)?;
    m.add_class::<session::Session>()?;
    m.add_class::<session::Response>()?;
    m.add_class::<stats::StreamStats>()?;
    m.add_class::<iters::FrameIter>()?;
    m.add_class::<iters::MovieIter>()?;
    m.add_class::<convert::CborTag>()?;
    m.add_function(wrap_pyfunction!(decode_message, m)?)?;
    m.add_function(wrap_pyfunction!(decompress_image, m)?)?;
    m.add_function(wrap_pyfunction!(decode_raw_header, m)?)?;
    m.add_function(wrap_pyfunction!(decompress, m)?)?;
    m.add_function(wrap_pyfunction!(_decode_tag, m)?)?;
    m.add_function(wrap_pyfunction!(camera_dectris::_fine_timer_counts, m)?)?;
    m.add_function(wrap_pyfunction!(logging::reset_log_cache, m)?)?;
    m.add_function(wrap_pyfunction!(logging::_test_attach_counts, m)?)?;

    // tag_decoders: {tag: functools.partial(_decode_tag, tag)} for 0, 40, 1040, 56500, 64–87
    let partial = py.import("functools")?.getattr("partial")?;
    let decode_tag = m.getattr("_decode_tag")?;
    let decoders = PyDict::new(py);
    let mut tags: Vec<u64> = vec![0, 40, 1040, 56500];
    tags.extend(DTYPES.iter().map(|(t, _)| *t));
    for t in tags {
        decoders.set_item(t, partial.call1((&decode_tag, t))?)?;
    }
    m.add("tag_decoders", decoders)?;
    Ok(())
}
