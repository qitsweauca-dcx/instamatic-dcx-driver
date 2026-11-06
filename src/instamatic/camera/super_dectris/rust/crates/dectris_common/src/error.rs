//! The driver's error type (docs/DESIGN.md §6).
//!
//! Every crate returns [`DriverError`]. The py crate maps each variant to a Python exception class
//! through [`DriverError::kind`]:
//!
//! | variant | Python exception |
//! |---|---|
//! | `Http` | `DectrisHTTPError` (`.status_code`, `.url`) |
//! | `Transport` | `DectrisConnectionError` (OSError-derived) |
//! | `Timeout` | `DectrisTimeout` (OSError-derived) |
//! | `State` | `DetectorStateError` |
//! | `Busy` | `DectrisBusy` |
//! | `Decode` | `StreamDecodeError` |
//! | `InvalidArg` | `ValueError` |
//! | `Zmq`, `NotConnected`, `Shutdown`, `Internal` | `DectrisError` |
//!
//! Panics never cross FFI: `ffi_guard!` (py) and the workers' `catch_unwind` turn them into
//! `Internal` / `DectrisError("internal error: …")`.

/// Typed driver error (DESIGN §6).
#[derive(thiserror::Error, Debug)]
pub enum DriverError {
    #[error("HTTP {status} for {url}: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },
    #[error("connection error: {0}")]
    Transport(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("detector state {state}: {needed}")]
    State { state: String, needed: String },
    #[error("receiver busy: {0}")]
    Busy(String),
    #[error("zmq error: {0}")]
    Zmq(#[from] zmq::Error),
    #[error("stream decode error at {offset}: {msg}")]
    Decode { offset: usize, msg: String },
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("driver not connected")]
    NotConnected,
    #[error("driver shutting down")]
    Shutdown,
    #[error("{0}")]
    Internal(String),
}

/// `Result` alias used across the driver crates.
pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// Which Python exception class a [`DriverError`] becomes (DESIGN §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExceptionKind {
    /// `DectrisHTTPError`
    Http,
    /// `DectrisConnectionError`
    Connection,
    /// `DectrisTimeout`
    Timeout,
    /// `DetectorStateError`
    DetectorState,
    /// `DectrisBusy`
    Busy,
    /// `StreamDecodeError`
    StreamDecode,
    /// builtin `ValueError`
    ValueError,
    /// `DectrisError` (the base class)
    Generic,
}

impl ExceptionKind {
    /// The Python class name (`__qualname__`) of this kind.
    pub const fn python_name(self) -> &'static str {
        match self {
            ExceptionKind::Http => "DectrisHTTPError",
            ExceptionKind::Connection => "DectrisConnectionError",
            ExceptionKind::Timeout => "DectrisTimeout",
            ExceptionKind::DetectorState => "DetectorStateError",
            ExceptionKind::Busy => "DectrisBusy",
            ExceptionKind::StreamDecode => "StreamDecodeError",
            ExceptionKind::ValueError => "ValueError",
            ExceptionKind::Generic => "DectrisError",
        }
    }
}

impl DriverError {
    /// The Python exception class this error maps to (DESIGN §6).
    pub const fn kind(&self) -> ExceptionKind {
        match self {
            DriverError::Http { .. } => ExceptionKind::Http,
            DriverError::Transport(_) => ExceptionKind::Connection,
            DriverError::Timeout(_) => ExceptionKind::Timeout,
            DriverError::State { .. } => ExceptionKind::DetectorState,
            DriverError::Busy(_) => ExceptionKind::Busy,
            DriverError::Decode { .. } => ExceptionKind::StreamDecode,
            DriverError::InvalidArg(_) => ExceptionKind::ValueError,
            DriverError::Zmq(_)
            | DriverError::NotConnected
            | DriverError::Shutdown
            | DriverError::Internal(_) => ExceptionKind::Generic,
        }
    }

    /// `Internal` from a caught panic payload: `"internal error: <msg>"` (DESIGN §6 Panics).
    pub fn from_panic(payload: &(dyn std::any::Any + Send)) -> Self {
        let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with a non-string payload".to_owned()
        };
        DriverError::Internal(format!("internal error: {msg}"))
    }

    /// Shorthand for `Decode { offset, msg }`.
    pub fn decode(offset: usize, msg: impl Into<String>) -> Self {
        DriverError::Decode {
            offset,
            msg: msg.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    #[test]
    fn is_send_sync_static() {
        assert_send_sync_static::<DriverError>();
    }

    #[test]
    fn display_strings() {
        let cases: Vec<(DriverError, &str)> = vec![
            (
                DriverError::Http {
                    status: 400,
                    url: "http://10.42.41.10/detector/api/1.8.0/command/arm".into(),
                    body: "bad".into(),
                },
                "HTTP 400 for http://10.42.41.10/detector/api/1.8.0/command/arm: bad",
            ),
            (
                DriverError::Transport("refused".into()),
                "connection error: refused",
            ),
            (DriverError::Timeout("5 s".into()), "timeout: 5 s"),
            (
                DriverError::State {
                    state: "error".into(),
                    needed: "idle".into(),
                },
                "detector state error: idle",
            ),
            (DriverError::Busy("x".into()), "receiver busy: x"),
            (
                DriverError::decode(17, "short"),
                "stream decode error at 17: short",
            ),
            (
                DriverError::InvalidArg("mode".into()),
                "invalid argument: mode",
            ),
            (DriverError::NotConnected, "driver not connected"),
            (DriverError::Shutdown, "driver shutting down"),
            (DriverError::Internal("boom".into()), "boom"),
        ];
        for (e, want) in cases {
            assert_eq!(e.to_string(), want);
        }
    }

    #[test]
    fn zmq_error_converts_with_question_mark() {
        fn f() -> Result<()> {
            Err(zmq::Error::EAGAIN)?;
            Ok(())
        }
        let e = f().unwrap_err();
        assert!(matches!(e, DriverError::Zmq(zmq::Error::EAGAIN)));
        assert!(e.to_string().starts_with("zmq error: "));
        assert_eq!(e.kind(), ExceptionKind::Generic);
    }

    #[test]
    fn exception_mapping() {
        use ExceptionKind as K;
        let cases = [
            (
                DriverError::Http {
                    status: 500,
                    url: String::new(),
                    body: String::new(),
                },
                K::Http,
                "DectrisHTTPError",
            ),
            (
                DriverError::Transport(String::new()),
                K::Connection,
                "DectrisConnectionError",
            ),
            (
                DriverError::Timeout(String::new()),
                K::Timeout,
                "DectrisTimeout",
            ),
            (
                DriverError::State {
                    state: String::new(),
                    needed: String::new(),
                },
                K::DetectorState,
                "DetectorStateError",
            ),
            (DriverError::Busy(String::new()), K::Busy, "DectrisBusy"),
            (
                DriverError::decode(0, ""),
                K::StreamDecode,
                "StreamDecodeError",
            ),
            (
                DriverError::InvalidArg(String::new()),
                K::ValueError,
                "ValueError",
            ),
            (
                DriverError::Zmq(zmq::Error::ETERM),
                K::Generic,
                "DectrisError",
            ),
            (DriverError::NotConnected, K::Generic, "DectrisError"),
            (DriverError::Shutdown, K::Generic, "DectrisError"),
            (
                DriverError::Internal(String::new()),
                K::Generic,
                "DectrisError",
            ),
        ];
        for (e, k, name) in cases {
            assert_eq!(e.kind(), k, "{e:?}");
            assert_eq!(k.python_name(), name);
        }
    }

    #[test]
    fn from_panic_payloads() {
        let p = std::panic::catch_unwind(|| panic!("static str")).unwrap_err();
        assert_eq!(
            DriverError::from_panic(&*p).to_string(),
            "internal error: static str"
        );
        let p = std::panic::catch_unwind(|| panic!("fmt {}", 3)).unwrap_err();
        assert_eq!(
            DriverError::from_panic(&*p).to_string(),
            "internal error: fmt 3"
        );
        let p = std::panic::catch_unwind(|| std::panic::panic_any(5_u8)).unwrap_err();
        assert!(matches!(
            DriverError::from_panic(&*p),
            DriverError::Internal(_)
        ));
    }
}
