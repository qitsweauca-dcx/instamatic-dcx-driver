//! FrameIter / MovieIter pyclasses and message_receiver (docs/DESIGN.md §2.6 [R7][R9][G1-T]).
//!
//! Thin wrappers over `dectris_core::driver::{FrameRecv, Movie}`. Each image `__next__` is one
//! detach: the output array is allocated first, from the shape of the last frame, and the receive
//! and the decompress run in the same detach, because every return to the GIL costs a frame's
//! time when a CPU-bound Python thread competes for it [R9]. The first frame of an iterator and a
//! shape change take a second detach.

use std::sync::Arc;

use dectris_core::driver::{Driver, FrameMode, FrameRecv, FrameStep, Movie, StopReason};
use dectris_core::log::Deferred;
use dectris_core::stream::Delivery;
use dectris_core::Result as DriverResult;
use pyo3::exceptions::PyStopIteration;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};

use crate::convert::{bin_mean, finish_image, meta_to_dict, prealloc_image, ShapeMemo};
use crate::core::{call, call_ok, CoreState};
use crate::errors::guard;

/// Record the stop reason on the core and end the iteration.
fn stop(st: &CoreState, reason: StopReason) -> PyErr {
    st.set_stop(Some(reason));
    PyStopIteration::new_err(())
}

/// One image through one detach (see the module docs): `next` is the core's receive.
fn next_image<'py>(
    py: Python<'py>,
    memo: &ShapeMemo,
    binning: u32,
    next: impl FnOnce(&mut Deferred) -> DriverResult<FrameStep> + Send,
) -> PyResult<Result<(Bound<'py, PyAny>, Delivery), StopReason>> {
    // `array` owns the buffer of `target` and outlives the detach (`OutBuf`).
    let (array, target) = prealloc_image(py, memo).unzip();
    let (step, decoded) = call(py, move |d| {
        let step = next(d)?;
        let decoded = match (&step, target) {
            (FrameStep::Frame(x), Some(t)) => t.decode(x)?,
            _ => false,
        };
        Ok((step, decoded))
    })?;
    let x = match step {
        FrameStep::Frame(x) => x,
        FrameStep::Stopped(r) => return Ok(Err(r)),
    };
    let img = finish_image(py, &x, array.filter(|_| decoded), memo)?;
    Ok(Ok((bin_mean(py, img, binning)?, x)))
}

/// Backs `receive_frames` / `receive_frames_with_metadata` / `receive_raw_frames`: the
/// silent-stop family (StopIteration + `last_stop_reason`), never arms, does not stop on cancel.
#[pyclass(
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "FrameIter"
)]
pub struct FrameIter {
    st: Arc<CoreState>,
    recv: FrameRecv,
    binning: u32,
    shape: ShapeMemo,
}

impl FrameIter {
    pub fn new(
        st: Arc<CoreState>,
        drv: Arc<Driver>,
        n: u64,
        mode: FrameMode,
        binning: u32,
    ) -> Self {
        FrameIter {
            st,
            recv: FrameRecv::new(drv, n, mode),
            binning,
            shape: ShapeMemo::default(),
        }
    }
}

#[pymethods]
impl FrameIter {
    fn __iter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        guard(py, || {
            let recv = &self.recv;
            let failed = |e: PyErr| {
                if let Some(r) = recv.stop_reason() {
                    self.st.set_stop(Some(r));
                }
                e
            };
            if matches!(recv.mode, FrameMode::Raw) {
                let x = match call(py, move |d| recv.next(d)).map_err(failed)? {
                    FrameStep::Frame(x) => x,
                    FrameStep::Stopped(r) => return Err(stop(&self.st, r)),
                };
                let raw = PyBytes::new(py, x.bytes()).into_any();
                let meta = meta_to_dict(py, &x.meta())?;
                return Ok(PyTuple::new(py, [raw, meta.into_any()])?.into_any());
            }
            let (img, x) = next_image(py, &self.shape, self.binning, move |d| recv.next(d))
                .map_err(failed)?
                .map_err(|r| stop(&self.st, r))?;
            if matches!(recv.mode, FrameMode::WithMeta) {
                let meta = meta_to_dict(py, &x.meta())?;
                return Ok(PyTuple::new(py, [img, meta.into_any()])?.into_any());
            }
            Ok(img)
        })
    }

    /// Stop and release the receiver registration (no HTTP).
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let recv = &self.recv;
        let before = recv.stop_reason();
        guard(py, || call_ok(py, move |_| recv.close()))?;
        // closed before the n-th frame: last_stop_reason says 'cancelled' at once
        if let Some(r) = self.recv.stop_reason().filter(|r| Some(*r) != before) {
            self.st.set_stop(Some(r));
        }
        Ok(())
    }

    /// Frames still to be delivered.
    #[getter]
    fn remaining(&self) -> u64 {
        self.recv.remaining()
    }
}

/// Backs `get_movie` / `stream` (via `_movie`): arms on the first `__next__`, disarms on
/// exhaustion or `close()`; a short movie raises `DectrisTimeout` [G1-F].
#[pyclass(
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "MovieIter"
)]
pub struct MovieIter {
    st: Arc<CoreState>,
    movie: Movie,
    binning: u32,
    shape: ShapeMemo,
}

impl MovieIter {
    pub fn new(st: Arc<CoreState>, drv: Arc<Driver>, n: u64, exposure: f64, binning: u32) -> Self {
        MovieIter {
            st,
            movie: Movie::new(drv, n, exposure),
            binning,
            shape: ShapeMemo::default(),
        }
    }
}

#[pymethods]
impl MovieIter {
    fn __iter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        guard(py, || {
            let movie = &self.movie;
            match next_image(py, &self.shape, self.binning, move |d| movie.next(d)) {
                Ok(Ok((img, _))) => Ok(img),
                Ok(Err(r)) => Err(stop(&self.st, r)),
                Err(e) => {
                    // a short movie raises DectrisTimeout; last_stop_reason still says why
                    if let Some(r) = self.movie.stop_reason() {
                        self.st.set_stop(Some(r));
                    }
                    Err(e)
                }
            }
        })
    }

    /// Armed → guarded disarm; idempotent; never blocks on a running `__next__`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let movie = &self.movie;
        let before = movie.stop_reason();
        guard(py, || call_ok(py, move |_| movie.close()))?;
        // closed before the n-th frame: last_stop_reason says 'cancelled' at once
        if let Some(r) = self.movie.stop_reason().filter(|r| Some(*r) != before) {
            self.st.set_stop(Some(r));
        }
        Ok(())
    }

    #[getter]
    fn delivered(&self) -> u64 {
        self.movie.delivered()
    }

    #[getter]
    fn n_frames(&self) -> u64 {
        self.movie.n()
    }
}
