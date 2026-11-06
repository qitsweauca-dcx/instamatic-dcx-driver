//! Rust → Python conversions (docs/DESIGN.md §2.7, §2.8, §4.4 [G2-P]).
//!
//! * CBOR [`Value`]s → Python objects (`decode_message`, `_start_config`, `tag_decoders`).
//! * Image deliveries → numpy arrays allocated by numpy (C order, `owndata`, writable), filled by
//!   decompressing straight into the array's buffer with the GIL released.
//! * The four-key metadata dict, the raw-header dict, `bin_mean`.

use std::borrow::Cow;
use std::sync::{Mutex, PoisonError};

use dectris_core::codec::cbor::{Elements, MultiDimArray};
use dectris_core::codec::{
    parse_image_message, scan_header, Algorithm, Encoding, Header, ImagePayload, MessageKind,
    Order, Scratch, TypedArray, Value, DEFAULT_CHANNEL,
};
use dectris_core::stream::{Delivery, FrameMeta};
use dectris_core::{DriverError, Result};
use numpy::{PyArray2, PyArrayMethods};
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::types::{PyBytes, PyDict, PyList};

use crate::errors::py_result;

/// A tag outside the data model, surfaced as `.tag` / `.value` (DESIGN §4.4 [G2-P]). The same duck
/// type `decompress_image` accepts.
#[pyclass(
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "CBORTag"
)]
pub struct CborTag {
    #[pyo3(get)]
    pub tag: u64,
    #[pyo3(get)]
    pub value: Py<PyAny>,
}

#[pymethods]
impl CborTag {
    #[new]
    fn new(tag: u64, value: Py<PyAny>) -> Self {
        CborTag { tag, value }
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "CBORTag({}, {})",
            self.tag,
            self.value.bind(py).repr()?
        ))
    }
}

fn numpy(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    py.import("numpy")
}

/// A 1-D typed array → a fresh ndarray (copy; `owndata`).
pub fn typed_to_py<'py>(py: Python<'py>, t: &TypedArray<'_>) -> PyResult<Bound<'py, PyAny>> {
    typed_view(py, t)?.call_method0("copy")
}

/// A 1-D typed array → a read-only ndarray over a `bytes` copy of the data (no `owndata`).
fn typed_view<'py>(py: Python<'py>, t: &TypedArray<'_>) -> PyResult<Bound<'py, PyAny>> {
    let dtype = t.elem.native_dtype();
    if dtype.is_empty() {
        return py_result(
            py,
            Err(DriverError::decode(
                0,
                format!("typed array tag {} has no numpy dtype", t.elem.tag),
            )),
        );
    }
    let b = PyBytes::new(py, &t.data);
    numpy(py)?.call_method1("frombuffer", (b, dtype))
}

/// Tags 40 / 1040 → an ndarray of shape `dims` (elements are already in C order).
pub fn multidim_to_py<'py>(py: Python<'py>, m: &MultiDimArray<'_>) -> PyResult<Bound<'py, PyAny>> {
    let dims: Vec<usize> = m.dims.clone();
    let flat = match &m.elements {
        Elements::Typed(t) => typed_view(py, t)?,
        Elements::Values(vs) => {
            let list = PyList::empty(py);
            for v in vs {
                list.append(value_to_py(py, v)?)?;
            }
            let np = numpy(py)?;
            let kw = PyDict::new(py);
            kw.set_item("dtype", py.get_type::<PyAny>())?;
            np.call_method("array", (list,), Some(&kw))?
        }
    };
    // copy after the reshape: the result owns its data, C order, writable (DESIGN §2.8)
    flat.call_method1("reshape", (dims,))?.call_method0("copy")
}

/// One CBOR value → Python (DESIGN §2.7, §4.4).
pub fn value_to_py<'py>(py: Python<'py>, v: &Value<'_>) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::Integer(i) => i.into_pyobject(py)?.into_any(),
        Value::Bytes(b) => PyBytes::new(py, b).into_any(),
        Value::Text(t) | Value::DateTime(t) => t.as_ref().into_pyobject(py)?.into_any(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for x in items {
                list.append(value_to_py(py, x)?)?;
            }
            list.into_any()
        }
        Value::Map(entries) => {
            let d = PyDict::new(py);
            for (k, x) in entries {
                d.set_item(value_to_py(py, k)?, value_to_py(py, x)?)?;
            }
            d.into_any()
        }
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any(),
        Value::Null => py.None().into_bound(py),
        // an unassigned simple value: its number (DESIGN §4.4)
        Value::Simple(n) => n.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::TypedArray(t) => typed_to_py(py, t)?,
        Value::MultiDim(m) => multidim_to_py(py, m)?,
        Value::Tag(tag, inner) => {
            let value = value_to_py(py, inner)?.unbind();
            Bound::new(py, CborTag { tag: *tag, value })?.into_any()
        }
        // a start array skipped on the receive path (DESIGN §4.4): None
        Value::Skipped { .. } => py.None().into_bound(py),
    })
}

/// The decoded top-level map of a message (every key, DESIGN §2.7).
pub fn message_to_dict<'py>(
    py: Python<'py>,
    entries: &[(Value<'_>, Value<'_>)],
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    for (k, v) in entries {
        d.set_item(value_to_py(py, k)?, value_to_py(py, v)?)?;
    }
    Ok(d)
}

pub fn kind_str(k: &MessageKind) -> Option<String> {
    match k {
        MessageKind::Start => Some("start".into()),
        MessageKind::Image => Some("image".into()),
        MessageKind::End => Some("end".into()),
        MessageKind::Other(s) => Some(s.clone()),
        MessageKind::Missing => None,
    }
}

/// `decode_raw_header(raw)`: `{'type', 'series_id', 'series_unique_id', 'image_id', 'real_time'}`.
pub fn header_to_dict<'py>(py: Python<'py>, h: &Header) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("type", kind_str(&h.kind))?;
    d.set_item("series_id", h.series_id)?;
    d.set_item("series_unique_id", h.series_unique_id.as_deref())?;
    d.set_item("image_id", h.image_id)?;
    d.set_item("real_time", h.real_time.map(|r| r.to_vec()))?;
    Ok(d)
}

/// The four-key metadata dict (DESIGN §2.7): `real_time` as a list (cRED divides it).
pub fn meta_to_dict<'py>(py: Python<'py>, m: &FrameMeta) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("series_id", m.series_id)?;
    d.set_item("image_id", m.image_id)?;
    d.set_item("count_time", m.count_time)?;
    d.set_item("real_time", m.real_time.map(|r| r.to_vec()))?;
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------------------------

/// The destination buffer of a decompress that runs detached.
struct OutBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the buffer belongs to a numpy array created by `alloc_array` that has not been handed
// to Python yet: no other thread can see it, and the owning `Bound` is kept alive by the caller
// for the whole detach (see `decode_payload_into_new_array`, `iters::next_frame`).
unsafe impl Send for OutBuf {}

/// Allocate an uninitialised C-order 2-D array for `p` (numpy owns the memory); returns the array
/// and its data pointer.
fn alloc_image<'py>(py: Python<'py>, p: &ImagePayload<'_>) -> Result<(Bound<'py, PyAny>, *mut u8)> {
    alloc_array(py, payload_shape(p))
}

fn alloc_array<'py>(
    py: Python<'py>,
    (rows, cols, elem_size): ImageShape,
) -> Result<(Bound<'py, PyAny>, *mut u8)> {
    let dims = [rows, cols];
    // SAFETY (all three): `PyArray::new` returns an uninitialised array; every byte is written by
    // `decode_into` before the array is returned to Python (a failed decode drops the array).
    Ok(match elem_size {
        1 => {
            let a = unsafe { PyArray2::<u8>::new(py, dims, false) };
            let ptr = a.data();
            (a.into_any(), ptr)
        }
        2 => {
            let a = unsafe { PyArray2::<u16>::new(py, dims, false) };
            let ptr = a.data().cast::<u8>();
            (a.into_any(), ptr)
        }
        4 => {
            let a = unsafe { PyArray2::<u32>::new(py, dims, false) };
            let ptr = a.data().cast::<u8>();
            (a.into_any(), ptr)
        }
        n => {
            return Err(DriverError::decode(
                0,
                format!("image element size {n} is not supported"),
            ))
        }
    })
}

/// Decompress `p` into a freshly allocated array, with the GIL released for the decode.
pub fn decode_payload_into_new_array<'py>(
    py: Python<'py>,
    p: &ImagePayload<'_>,
) -> PyResult<Bound<'py, PyAny>> {
    let (arr, ptr) = py_result(py, alloc_image(py, p))?;
    // `arr` is C-contiguous with exactly rows·cols·itemsize bytes (codec size validation +
    // `alloc_image`) and stays alive (owned by this frame) until the detach returns.
    let out = OutBuf {
        ptr,
        len: p.byte_len(),
    };
    let r = py.detach(move || {
        let out = out;
        // SAFETY: see `OutBuf`; the slice covers the array's data buffer only.
        let slice = unsafe { std::slice::from_raw_parts_mut(out.ptr, out.len) };
        p.decode_into(slice, &mut Scratch::new()).map(|_| ())
    });
    py_result(py, r)?;
    Ok(arr)
}

/// An image delivery → ndarray (DESIGN §2.6 step 3/4).
pub fn delivery_to_array<'py>(py: Python<'py>, x: &Delivery) -> PyResult<Bound<'py, PyAny>> {
    let parsed = py_result(py, parse_image_message(x.bytes(), DEFAULT_CHANNEL))?;
    decode_payload_into_new_array(py, &parsed.payload)
}

/// What an output array is allocated from: rows, columns, element size.
pub type ImageShape = (usize, usize, usize);

fn payload_shape(p: &ImagePayload<'_>) -> ImageShape {
    (p.rows, p.cols, p.elem.size)
}

/// The shape of the iterator's last frame: the next output can be allocated before the detach.
#[derive(Default)]
pub struct ShapeMemo(Mutex<Option<ImageShape>>);

impl ShapeMemo {
    fn get(&self, py: Python<'_>) -> Option<ImageShape> {
        *self
            .0
            .lock_py_attached(py)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, py: Python<'_>, shape: ImageShape) {
        *self
            .0
            .lock_py_attached(py)
            .unwrap_or_else(PoisonError::into_inner) = Some(shape);
    }
}

/// The buffer of an output array allocated before the detach, on its way into it.
pub struct DecodeTarget {
    buf: OutBuf,
    shape: ImageShape,
}

impl DecodeTarget {
    /// Decompress `x` into the array, if it has the expected shape; `false` leaves it unwritten.
    pub fn decode(self, x: &Delivery) -> Result<bool> {
        let parsed = parse_image_message(x.bytes(), DEFAULT_CHANNEL)?;
        let p = &parsed.payload;
        if payload_shape(p) != self.shape || p.byte_len() != self.buf.len {
            return Ok(false);
        }
        // SAFETY: see `OutBuf`; the slice covers the array's data buffer only.
        let slice = unsafe { std::slice::from_raw_parts_mut(self.buf.ptr, self.buf.len) };
        p.decode_into(slice, &mut Scratch::new()).map(|_| true)
    }
}

/// The output of the next frame when the shape is known (DESIGN §2.6 step 2 [R9]): the array, which
/// the caller keeps alive for the whole detach, and what goes into the detach. An allocation
/// failure is not the frame's: [`finish_image`] then takes the frame the slow way.
pub fn prealloc_image<'py>(
    py: Python<'py>,
    memo: &ShapeMemo,
) -> Option<(Bound<'py, PyAny>, DecodeTarget)> {
    let shape = memo.get(py)?;
    let (array, ptr) = alloc_array(py, shape).ok()?;
    let len = shape.0 * shape.1 * shape.2;
    Some((
        array,
        DecodeTarget {
            buf: OutBuf { ptr, len },
            shape,
        },
    ))
}

/// The array of a delivered frame: `decoded` if the detach filled it, else allocate from the frame
/// (first frame, or a shape change: at most once per series in steady state) and remember the shape.
pub fn finish_image<'py>(
    py: Python<'py>,
    x: &Delivery,
    decoded: Option<Bound<'py, PyAny>>,
    memo: &ShapeMemo,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(a) = decoded {
        return Ok(a);
    }
    let parsed = py_result(py, parse_image_message(x.bytes(), DEFAULT_CHANNEL))?;
    let array = decode_payload_into_new_array(py, &parsed.payload)?;
    memo.set(py, payload_shape(&parsed.payload));
    Ok(array)
}

/// `binning > 1`: crop to a multiple of `b`, block mean → float64 (`image_utils.apply_binning`).
pub fn bin_mean<'py>(py: Python<'py>, a: Bound<'py, PyAny>, b: u32) -> PyResult<Bound<'py, PyAny>> {
    if b <= 1 {
        return Ok(a);
    }
    let shape: (usize, usize) = a.getattr("shape")?.extract()?;
    let b = b as usize;
    let (r, c) = (shape.0 / b, shape.1 / b);
    let cropped = a.get_item((
        pyo3::types::PySlice::new(py, 0, (r * b) as isize, 1),
        pyo3::types::PySlice::new(py, 0, (c * b) as isize, 1),
    ))?;
    let kw = PyDict::new(py);
    kw.set_item("axis", (1, 3))?;
    cropped
        .call_method1("reshape", ((r, b, c, b),))?
        .call_method("mean", (), Some(&kw))
}

// ---------------------------------------------------------------------------------------------
// decompress_image input shapes (DESIGN §2.7)
// ---------------------------------------------------------------------------------------------

/// The items of a list or tuple of exactly `n` elements (cbor2 decodes CBOR arrays as lists).
pub fn items<'py>(obj: &Bound<'py, PyAny>, n: usize) -> Option<Vec<Bound<'py, PyAny>>> {
    let v: Vec<Bound<'py, PyAny>> = obj.extract().ok()?;
    (v.len() == n).then_some(v)
}

/// `[algorithm: str, modifier: int, data: bytes]` (tag 56500 content, [G2-K]).
pub fn tag56500_parts(obj: &Bound<'_, PyAny>) -> Option<(String, u64, Vec<u8>)> {
    let v = items(obj, 3)?;
    if !v[2].is_instance_of::<PyBytes>() {
        return None;
    }
    Some((
        v[0].extract().ok()?,
        v[1].extract().ok()?,
        v[2].extract().ok()?,
    ))
}

/// `tag 56500 = [algorithm, modifier, bytes]` from Python (cbor2 / our CBORTag / a list).
fn encoding_from_py<'a>(obj: &Bound<'_, PyAny>) -> PyResult<Encoding<'a>> {
    if let Ok(b) = obj.extract::<Vec<u8>>() {
        if obj.is_instance_of::<PyBytes>() {
            return Ok(Encoding::Raw(Cow::Owned(b)));
        }
    }
    let tag: Option<u64> = obj.getattr("tag").ok().and_then(|t| t.extract().ok());
    if tag == Some(56500) {
        let v = obj.getattr("value")?;
        let (alg, modifier, data): (Vec<u8>, u64, Vec<u8>) = match tag56500_parts(&v) {
            Some((a, m, d)) => (a.into_bytes(), m, d),
            None => {
                return Err(crate::errors::to_pyerr(
                    obj.py(),
                    DriverError::decode(0, "tag 56500 content must be [str, uint, bytes]"),
                ))
            }
        };
        let algorithm = py_result(obj.py(), Algorithm::from_wire(&alg))?;
        return Ok(Encoding::Compressed {
            algorithm,
            modifier,
            data: Cow::Owned(data),
        });
    }
    Err(crate::errors::to_pyerr(
        obj.py(),
        DriverError::decode(0, "typed-array payload must be bytes or a tag 56500"),
    ))
}

/// A duck-typed `.tag/.value` multidimensional array (tag 40/1040) → ndarray.
pub fn duck_multidim_to_array<'py>(
    py: Python<'py>,
    obj: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let tag: u64 = obj.getattr("tag")?.extract()?;
    let order = match tag {
        40 => Order::C,
        1040 => Order::F,
        t => {
            return Err(crate::errors::to_pyerr(
                py,
                DriverError::decode(0, format!("expected tag 40 or 1040, got {t}")),
            ))
        }
    };
    let value = obj.getattr("value")?;
    let parts = items(&value, 2).and_then(|v| {
        let dims: Vec<u64> = v[0].extract().ok()?;
        Some((dims, v[1].clone()))
    });
    let Some((dims, typed)) = parts else {
        return Err(crate::errors::to_pyerr(
            py,
            DriverError::decode(0, "tag 40/1040 content must be [dims, array]"),
        ));
    };
    let elem_tag: u64 = typed.getattr("tag")?.extract()?;
    let encoding = encoding_from_py(&typed.getattr("value")?)?;
    let payload = py_result(
        py,
        ImagePayload::from_parts(&dims, order, elem_tag, encoding),
    )?;
    decode_payload_into_new_array(py, &payload)
}

/// `decode_raw_header(raw)`.
pub fn raw_header<'py>(py: Python<'py>, raw: &[u8]) -> PyResult<Bound<'py, PyDict>> {
    let h = py_result(py, scan_header(raw))?;
    header_to_dict(py, &h)
}
