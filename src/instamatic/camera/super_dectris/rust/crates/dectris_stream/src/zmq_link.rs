//! ZeroMQ PULL socket wrapper: RCVHWM, linger 0, keepalive, poll+recv, flush drain, close
//! (docs/DESIGN.md §4.3 "Socket", §3.7 shutdown step 5.2; PROTOCOL §7.1–§7.3).
//!
//! **This is the only file of the crate that names a `zmq` type** (the libzmq 4.3.5 git pin will be
//! swapped in through the workspace manifest; keeping every call here makes that swap trivial).
//! Everything else sees [`RawMsg`] (one Stream V2 message = one ZeroMQ message, zero copy) and
//! [`ZmqLink`].
//!
//! * Socket: `PULL`, `RCVHWM` set **before** `connect`, `LINGER 0`, default `RECONNECT_IVL`,
//!   `TCP_KEEPALIVE 1 / IDLE 60 s` when requested. `connect` is asynchronous in libzmq: a wrong
//!   host/port only shows up as receive timeouts (PROTOCOL §7.1).
//! * Receive: `poll(POLLIN, slice)` + `recv(DONTWAIT)`. `EINTR` / `EAGAIN` are "nothing yet".
//! * Close: `zmq::Socket::drop` panics when `zmq_close` fails (zmq-0.10.0/src/lib.rs:494-500), so
//!   [`ZmqLink::close`] and `Drop` run it inside `catch_unwind` [R8].
//!
//! [`PushServer`] is a small PUSH publisher for tests (this crate's and the driver's): it binds an
//! ephemeral `tcp://127.0.0.1:*` port.

use std::fmt;
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use dectris_common::{DriverError, Result};

/// `TCP_KEEPALIVE_IDLE` used when `zmq_tcp_keepalive` is on (DESIGN §4.3).
pub const KEEPALIVE_IDLE_S: i32 = 60;

/// `ZMQ_RCVBUF` (bytes) unless the yaml says otherwise. Left to itself, Windows stalls a fast
/// transfer for about 30 ms at a time, and the sender's queue overflows.
pub const DEFAULT_RCVBUF: i32 = 4 * 1024 * 1024;

/// Socket parameters of one PULL connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    /// `tcp://<ZMQ_host>:<ZMQ_port>`.
    pub endpoint: String,
    /// `ZMQ_RCVHWM` (messages), set before `connect`.
    pub rcvhwm: i32,
    /// `ZMQ_RCVBUF` (bytes), set before `connect`; 0 leaves the size to the OS.
    pub rcvbuf: i32,
    /// `zmq_tcp_keepalive` option: `TCP_KEEPALIVE 1 / IDLE 60 s`.
    pub tcp_keepalive: bool,
}

impl LinkConfig {
    pub fn new(endpoint: impl Into<String>, rcvhwm: i32) -> Self {
        LinkConfig {
            endpoint: endpoint.into(),
            rcvhwm,
            rcvbuf: DEFAULT_RCVBUF,
            tcp_keepalive: false,
        }
    }
}

/// One received ZeroMQ message (= one Stream V2 message), not copied. Derefs to `[u8]`.
pub struct RawMsg(zmq::Message);

impl RawMsg {
    /// A message holding a copy of `bytes` (tests, fakes).
    pub fn from_slice(bytes: &[u8]) -> Self {
        RawMsg(zmq::Message::from(bytes))
    }

    /// The bytes as an owned `Vec` (one copy).
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl Deref for RawMsg {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for RawMsg {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RawMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawMsg({} bytes)", self.0.len())
    }
}

// RawMsg crosses the `py.detach` boundary (DESIGN §3.3): it must be Send.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<RawMsg>();
    assert_send::<ZmqLink>();
};

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-string payload".to_owned()
    }
}

/// A connected PULL socket and its private context.
///
/// `Send + !Sync` (like `zmq::Socket`): the driver keeps it in `Mutex<Option<ZmqLink>>` and locks
/// it only for one poll slice + recv, a flush drain, reconnect or close (DESIGN §3.2).
pub struct ZmqLink {
    sock: Option<zmq::Socket>,
    ctx: Option<zmq::Context>,
    cfg: LinkConfig,
}

impl fmt::Debug for ZmqLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZmqLink").field("cfg", &self.cfg).finish()
    }
}

impl ZmqLink {
    /// New context + `PULL`, `RCVHWM` and `RCVBUF` before `connect`, `LINGER 0`, optional keepalive,
    /// `connect`.
    pub fn connect(cfg: &LinkConfig) -> Result<ZmqLink> {
        if cfg.rcvhwm < 0 {
            return Err(DriverError::InvalidArg(format!(
                "ZMQ_RCVHWM must be >= 0, got {}",
                cfg.rcvhwm
            )));
        }
        let ctx = zmq::Context::new();
        let sock = ctx.socket(zmq::PULL)?;
        // From here on `link` owns the socket: an error below closes it through Drop.
        let link = ZmqLink {
            sock: Some(sock),
            ctx: Some(ctx),
            cfg: cfg.clone(),
        };
        let s = link.sock()?;
        s.set_rcvhwm(cfg.rcvhwm)?;
        if cfg.rcvbuf > 0 {
            s.set_rcvbuf(cfg.rcvbuf)?;
        }
        s.set_linger(0)?;
        if cfg.tcp_keepalive {
            s.set_tcp_keepalive(1)?;
            s.set_tcp_keepalive_idle(KEEPALIVE_IDLE_S)?;
        }
        s.connect(&cfg.endpoint)?;
        Ok(link)
    }

    fn sock(&self) -> Result<&zmq::Socket> {
        self.sock
            .as_ref()
            .ok_or_else(|| DriverError::Internal("zmq socket already closed".into()))
    }

    /// The configuration this link was opened with.
    pub fn config(&self) -> &LinkConfig {
        &self.cfg
    }

    /// `ZMQ_RCVHWM` read back from the socket.
    pub fn rcvhwm(&self) -> Result<i32> {
        Ok(self.sock()?.get_rcvhwm()?)
    }

    /// `ZMQ_RCVBUF` read back from the socket.
    pub fn rcvbuf(&self) -> Result<i32> {
        Ok(self.sock()?.get_rcvbuf()?)
    }

    /// `ZMQ_LINGER` read back from the socket.
    pub fn linger(&self) -> Result<i32> {
        Ok(self.sock()?.get_linger()?)
    }

    /// `(ZMQ_TCP_KEEPALIVE, ZMQ_TCP_KEEPALIVE_IDLE)` read back from the socket.
    pub fn tcp_keepalive(&self) -> Result<(i32, i32)> {
        let s = self.sock()?;
        Ok((s.get_tcp_keepalive()?, s.get_tcp_keepalive_idle()?))
    }

    /// Wait up to `slice` for a message, then receive it without blocking.
    /// `Ok(None)` = nothing arrived in the slice (or an `EINTR`/`EAGAIN`).
    pub fn poll_recv(&self, slice: Duration) -> Result<Option<RawMsg>> {
        let s = self.sock()?;
        let ms = i64::try_from(slice.as_millis()).unwrap_or(i64::MAX);
        match s.poll(zmq::POLLIN, ms) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(zmq::Error::EINTR) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        self.try_recv()
    }

    /// Non-blocking receive (`DONTWAIT`): `Ok(None)` on `EAGAIN`.
    pub fn try_recv(&self) -> Result<Option<RawMsg>> {
        let s = self.sock()?;
        let mut m = zmq::Message::new();
        match s.recv(&mut m, zmq::DONTWAIT) {
            Ok(()) => Ok(Some(RawMsg(m))),
            Err(zmq::Error::EAGAIN) | Err(zmq::Error::EINTR) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Close the socket (LINGER 0) and drop the context, catching a `zmq_close` panic [R8].
    /// `Err(text)` = the panic message (log it, never raise).
    pub fn close(mut self) -> std::result::Result<(), String> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> std::result::Result<(), String> {
        let sock = self.sock.take();
        let ctx = self.ctx.take();
        let r = catch_unwind(AssertUnwindSafe(move || {
            if let Some(s) = sock {
                let _ = s.set_linger(0);
                drop(s);
            }
            drop(ctx);
        }));
        r.map_err(|p| panic_text(p.as_ref()))
    }
}

impl Drop for ZmqLink {
    fn drop(&mut self) {
        // Errors are swallowed here: an explicit close() reports them.
        let _ = self.close_inner();
    }
}

/// Test publisher: a ZeroMQ `PUSH` bound on `tcp://127.0.0.1:<ephemeral>` (DESIGN §8 `fake_push`).
pub struct PushServer {
    sock: Option<zmq::Socket>,
    ctx: Option<zmq::Context>,
    endpoint: String,
}

impl fmt::Debug for PushServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushServer")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl PushServer {
    /// Bind `tcp://127.0.0.1:*`. Sends time out after 5 s (a PUSH with no peer blocks).
    pub fn bind_ephemeral() -> Result<PushServer> {
        Self::bind("tcp://127.0.0.1:*")
    }

    /// Bind an explicit endpoint (e.g. to re-bind a port after a restart).
    pub fn bind(endpoint: &str) -> Result<PushServer> {
        let ctx = zmq::Context::new();
        let sock = ctx.socket(zmq::PUSH)?;
        sock.set_linger(0)?;
        sock.set_sndtimeo(5000)?;
        sock.bind(endpoint)?;
        let endpoint = match sock.get_last_endpoint()? {
            Ok(s) => s,
            Err(_) => return Err(DriverError::Internal("non-UTF-8 endpoint".into())),
        };
        Ok(PushServer {
            sock: Some(sock),
            ctx: Some(ctx),
            endpoint,
        })
    }

    /// The bound endpoint, e.g. `tcp://127.0.0.1:53211`.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Send one message (blocks while no PULL is connected or the pipe is full; 5 s timeout).
    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let s = self
            .sock
            .as_ref()
            .ok_or_else(|| DriverError::Internal("push socket closed".into()))?;
        s.send(bytes, 0)?;
        Ok(())
    }

    /// Change the send timeout (ms; -1 = block forever).
    pub fn set_send_timeout_ms(&self, ms: i32) -> Result<()> {
        if let Some(s) = self.sock.as_ref() {
            s.set_sndtimeo(ms)?;
        }
        Ok(())
    }
}

impl Drop for PushServer {
    fn drop(&mut self) {
        let sock = self.sock.take();
        let ctx = self.ctx.take();
        let _ = catch_unwind(AssertUnwindSafe(move || {
            drop(sock);
            drop(ctx);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn recv_within(link: &ZmqLink, total: Duration) -> Option<RawMsg> {
        let end = Instant::now() + total;
        while Instant::now() < end {
            if let Some(m) = link.poll_recv(Duration::from_millis(50)).unwrap() {
                return Some(m);
            }
        }
        None
    }

    #[test]
    fn libzmq_is_the_mpl_4_3_5_build() {
        // DESIGN §12 #1: libzmq 4.3.5 (MPL-2.0). A drift back to 4.3.4 (LGPL) must fail here.
        assert_eq!(zmq::version(), (4, 3, 5));
    }

    #[test]
    fn options_read_back() {
        let push = PushServer::bind_ephemeral().unwrap();
        let mut cfg = LinkConfig::new(push.endpoint(), 4);
        cfg.tcp_keepalive = true;
        let link = ZmqLink::connect(&cfg).unwrap();
        assert_eq!(link.rcvhwm().unwrap(), 4);
        assert_eq!(link.rcvbuf().unwrap(), DEFAULT_RCVBUF);
        assert_eq!(link.linger().unwrap(), 0);
        assert_eq!(link.tcp_keepalive().unwrap(), (1, KEEPALIVE_IDLE_S));
        let mut cfg2 = LinkConfig::new(push.endpoint(), 1000);
        cfg2.rcvbuf = 0;
        let link2 = ZmqLink::connect(&cfg2).unwrap();
        assert_eq!(link2.rcvhwm().unwrap(), 1000);
        assert_eq!(link2.rcvbuf().unwrap(), -1); // OS default
        assert_eq!(link2.tcp_keepalive().unwrap().0, -1); // OS default
        assert!(link.close().is_ok());
        assert!(link2.close().is_ok());
    }

    #[test]
    fn negative_hwm_rejected() {
        let e = ZmqLink::connect(&LinkConfig::new("tcp://127.0.0.1:1", -1)).unwrap_err();
        assert!(matches!(e, DriverError::InvalidArg(_)));
    }

    #[test]
    fn bad_endpoint_is_a_zmq_error() {
        let e = ZmqLink::connect(&LinkConfig::new("nonsense", 4)).unwrap_err();
        assert!(matches!(e, DriverError::Zmq(_)), "{e:?}");
    }

    #[test]
    fn roundtrip_bytes_identical() {
        let push = PushServer::bind_ephemeral().unwrap();
        let link = ZmqLink::connect(&LinkConfig::new(push.endpoint(), 4)).unwrap();
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 251) as u8).collect();
        push.send(&payload).unwrap();
        let m = recv_within(&link, Duration::from_secs(5)).expect("message");
        assert_eq!(&*m, payload.as_slice());
        assert_eq!(m.to_vec(), payload);
        assert!(link.try_recv().unwrap().is_none());
    }

    #[test]
    fn poll_times_out_without_messages() {
        let push = PushServer::bind_ephemeral().unwrap();
        let link = ZmqLink::connect(&LinkConfig::new(push.endpoint(), 4)).unwrap();
        let t = Instant::now();
        assert!(link.poll_recv(Duration::from_millis(80)).unwrap().is_none());
        assert!(t.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn raw_msg_from_slice() {
        let m = RawMsg::from_slice(b"abc");
        assert_eq!(&*m, b"abc");
        assert_eq!(format!("{m:?}"), "RawMsg(3 bytes)");
    }
}
