//! `cam.session`: the part of `requests.Session` its callers use (`get`, `put`; on the response
//! `status_code`, `ok`, `url`, `content`, `text`, `json()`, `raise_for_status()`).

use std::time::Duration;

use dectris_core::simplon::client::Reply;
use dectris_core::simplon::PlainSession;
use dectris_core::DriverError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::core::call;
use crate::errors::{guard, to_pyerr};

/// The timeout of a request that names none (`requests` would wait for ever).
const DEFAULT_TIMEOUT: f64 = 30.0;

fn budget(py: Python<'_>, timeout: Option<f64>) -> PyResult<Duration> {
    let t = timeout.unwrap_or(DEFAULT_TIMEOUT);
    if t.is_finite() && t > 0.0 {
        Ok(Duration::from_secs_f64(t))
    } else {
        Err(to_pyerr(
            py,
            DriverError::InvalidArg(format!("timeout must be > 0, got {t}")),
        ))
    }
}

#[pyclass(
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "DectrisResponse"
)]
pub struct Response {
    url: String,
    reply: Reply,
}

#[pymethods]
impl Response {
    #[getter]
    fn status_code(&self) -> u16 {
        self.reply.status
    }

    #[getter]
    fn ok(&self) -> bool {
        self.reply.status < 400
    }

    #[getter]
    fn url(&self) -> &str {
        &self.url
    }

    #[getter]
    fn content<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.reply.body)
    }

    #[getter]
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.reply.body).into_owned()
    }

    /// The body parsed as JSON; `ValueError` when it is not JSON.
    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("json")?.call_method1("loads", (self.text(),))
    }

    /// `DectrisHTTPError` on a 4xx or 5xx status.
    fn raise_for_status(&self, py: Python<'_>) -> PyResult<()> {
        if self.reply.status < 400 {
            return Ok(());
        }
        Err(to_pyerr(
            py,
            DriverError::Http {
                status: self.reply.status,
                url: self.url.clone(),
                body: self.text(),
            },
        ))
    }
}

#[pyclass(
    frozen,
    module = "instamatic.camera.super_dectris._super_dectris",
    name = "DectrisSession"
)]
pub struct Session {
    http: PlainSession,
}

impl Session {
    pub fn new(user_agent: &str) -> Self {
        Session {
            http: PlainSession::new(user_agent),
        }
    }
}

#[pymethods]
impl Session {
    #[pyo3(signature = (url, *, timeout=None))]
    fn get(&self, py: Python<'_>, url: String, timeout: Option<f64>) -> PyResult<Response> {
        guard(py, || {
            let b = budget(py, timeout)?;
            let reply = call(py, |_| self.http.get(&url, b))?;
            Ok(Response { url, reply })
        })
    }

    /// `json`: any object `json.dumps` takes; left out, the request has no body.
    #[pyo3(signature = (url, *, json=None, timeout=None))]
    fn put(
        &self,
        py: Python<'_>,
        url: String,
        json: Option<&Bound<'_, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Response> {
        guard(py, || {
            let b = budget(py, timeout)?;
            let body: Option<String> = match json {
                Some(v) => Some(py.import("json")?.call_method1("dumps", (v,))?.extract()?),
                None => None,
            };
            let reply = call(py, |_| self.http.put(&url, body.as_deref(), b))?;
            Ok(Response { url, reply })
        })
    }
}
