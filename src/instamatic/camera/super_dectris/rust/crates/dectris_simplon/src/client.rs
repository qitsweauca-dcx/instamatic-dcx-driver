//! SIMPLON 1.8 REST client: one ureq Agent, URL building, HTTP-200-only policy, timeouts
//! (docs/DESIGN.md §4.1; PROTOCOL §1, §6).
//!
//! * One [`ureq::Agent`]: connect timeout 1 s, `http_status_as_error(false)`, 4 idle connections per
//!   host, `User-Agent` + `Accept: application/json`; every request sets its own
//!   `timeout_global` ([`Budgets`]).
//! * Wire conventions [G2-L]: config PUT `{"value": v}`; commands PUT without a body (or
//!   `{"value": v}` when the command takes one); GET responses `{"value": …}`; only HTTP 200 is
//!   success. A config PUT's body (a JSON list of changed keys, possibly empty or absent) is parsed
//!   only for a DEBUG record and is never required.
//! * Error mapping: non-200 → [`DriverError::Http`]; ureq timeouts and `io::ErrorKind::TimedOut`
//!   → [`DriverError::Timeout`]; every other transport failure → [`DriverError::Transport`].
//! * Offline back-off [R10]: a transport error or timeout on **any** request opens a back-off
//!   window ([`Budgets::backoff`], 5 s); any HTTP response (whatever its status) closes it. Only the
//!   public Tk-polled getters and the monitor consult it ([`Client::in_backoff`]).
//! * Initialize gate [G1-H][G2-A]: while the driver's own initialize PUT is outstanding the gate is
//!   closed and every request except that PUT fails fast with `DriverError::State` and no I/O
//!   (vendor: "Blocking operation, no other API operation may be performed", PROTOCOL §2.4).
//!
//! No logging happens while a lock of this module is held (DESIGN §3.2 R6).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dectris_common::sync::lock_unpoisoned;
use dectris_common::{dlog, DriverError, Result};
use serde_json::Value;

/// SIMPLON API version segment (PROTOCOL §1.1).
pub const API_VERSION: &str = "1.8.0";

/// Default response-body limit for JSON GETs (a `pixel_mask` darray is a few MB).
pub const JSON_LIMIT: u64 = 32 * 1024 * 1024;
/// Response-body limit for the monitor TIFF (1028×1062 u32 ≈ 4.4 MB).
pub const TIFF_LIMIT: u64 = 64 * 1024 * 1024;
/// Chunk size of `/data/<file>` downloads (DESIGN §4.1).
pub const DOWNLOAD_CHUNK: usize = 64 * 1024;
/// How many bytes of a non-200 body are kept in `DriverError::Http::body`.
const ERROR_BODY_MAX: usize = 512;

/// SIMPLON module (URL segment, PROTOCOL §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    Detector,
    Stream,
    Monitor,
    Filewriter,
    System,
}

impl Module {
    pub const fn as_str(self) -> &'static str {
        match self {
            Module::Detector => "detector",
            Module::Stream => "stream",
            Module::Monitor => "monitor",
            Module::Filewriter => "filewriter",
            Module::System => "system",
        }
    }
}

/// SIMPLON task (URL segment). `Images` is the monitor's `images/…` resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Config,
    Status,
    Command,
    Images,
}

impl Task {
    pub const fn as_str(self) -> &'static str {
        match self {
            Task::Config => "config",
            Task::Status => "status",
            Task::Command => "command",
            Task::Images => "images",
        }
    }
}

/// Every time budget of the SIMPLON layer (DESIGN §4.1 timeout table, §3.4, §4.2). [`Budgets::spec`]
/// holds the design values; tests shrink them.
#[derive(Debug, Clone)]
pub struct Budgets {
    /// Agent connect timeout (1 s).
    pub connect: Duration,
    /// The four Tk-polled getters (1 s) [R10].
    pub poll_get: Duration,
    /// `trigger_mode`, `binning_mode`, `bit_depth_image`, `detector_readout_time`,
    /// `x/y_pixels_in_detector`, readback GETs (3 s).
    pub get: Duration,
    /// Startup state probe (3 s total).
    pub probe: Duration,
    /// 405-fallback GETs, filewriter `status/files`, idle-wait / policy state polls (5 s).
    pub get_slow: Duration,
    /// Detector / stream / filewriter config PUTs, `hv_enabled` (5 s).
    pub put: Duration,
    /// Monitor `config/mode` PUT (3 s).
    pub put_monitor: Duration,
    /// `arm`, `disarm`, filewriter `clear` (30 s).
    pub arm: Duration,
    /// `abort`, `cancel` (15 s).
    pub abort: Duration,
    /// `initialize` PUT on a worker (130 s).
    pub init_put: Duration,
    /// Idle wait after an initialize (30 s).
    pub init_idle_wait: Duration,
    /// Idle wait after `configure`/`initialize` transients and in `disarm` (10 s).
    pub transient_wait: Duration,
    /// Idle-wait poll interval (0.2 s).
    pub idle_poll: Duration,
    /// Trigger PUT connect timeout (5 s).
    pub trigger_connect: Duration,
    /// Trigger global timeout clamp (30 s … 24 h) and the margin added to the series length (60 s).
    pub trigger_min: Duration,
    pub trigger_max: Duration,
    pub trigger_margin: Duration,
    /// `images/monitor` GET (2 s).
    pub monitor_get: Duration,
    /// `/data/<file>` download (120 s per file).
    pub download: Duration,
    /// Offline back-off window (5 s) [R10].
    pub backoff: Duration,
}

impl Budgets {
    /// The values of DESIGN §4.1 / §3.4 / §4.2.
    pub fn spec() -> Self {
        Budgets {
            connect: Duration::from_secs(1),
            poll_get: Duration::from_secs(1),
            get: Duration::from_secs(3),
            probe: Duration::from_secs(3),
            get_slow: Duration::from_secs(5),
            put: Duration::from_secs(5),
            put_monitor: Duration::from_secs(3),
            arm: Duration::from_secs(30),
            abort: Duration::from_secs(15),
            init_put: Duration::from_secs(130),
            init_idle_wait: Duration::from_secs(30),
            transient_wait: Duration::from_secs(10),
            idle_poll: Duration::from_millis(200),
            trigger_connect: Duration::from_secs(5),
            trigger_min: Duration::from_secs(30),
            trigger_max: Duration::from_secs(24 * 3600),
            trigger_margin: Duration::from_secs(60),
            monitor_get: Duration::from_secs(2),
            download: Duration::from_secs(120),
            backoff: Duration::from_secs(5),
        }
    }
}

impl Default for Budgets {
    fn default() -> Self {
        Self::spec()
    }
}

/// An HTTP response that was received (any status).
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// Parse the body as JSON; an empty body is `None`.
    pub fn json(&self) -> Option<Value> {
        if self.body.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        serde_json::from_slice(&self.body).ok()
    }
}

/// One HTTP request, as run by [`Client::execute`] (also used by the workers).
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    /// `None` = no body (commands without input, GETs).
    pub body: Option<Value>,
    /// Global timeout of the whole request, body included.
    pub budget: Duration,
    /// Per-request connect timeout override (the trigger uses 5 s).
    pub connect: Option<Duration>,
    /// Response body limit.
    pub limit: u64,
    /// True only for the driver's own initialize PUT (passes the initialize gate).
    pub bypass_gate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Put => "PUT",
        }
    }
}

impl Request {
    pub fn get(url: impl Into<String>, budget: Duration) -> Self {
        Request {
            method: Method::Get,
            url: url.into(),
            body: None,
            budget,
            connect: None,
            limit: JSON_LIMIT,
            bypass_gate: false,
        }
    }

    pub fn put(url: impl Into<String>, body: Option<Value>, budget: Duration) -> Self {
        Request {
            method: Method::Put,
            url: url.into(),
            body,
            budget,
            connect: None,
            limit: JSON_LIMIT,
            bypass_gate: false,
        }
    }
}

/// The SIMPLON REST client. `Send + Sync`; share it through `Arc` (workers hold an `Arc<Client>`).
#[derive(Debug)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    budgets: Budgets,
    offline_until: Mutex<Option<Instant>>,
    /// Outstanding initialize PUTs (a count, so a finishing worker cannot reopen the gate while
    /// the next initialize is already out).
    init_gate: AtomicU32,
}

impl Client {
    /// Build the agent (no I/O). `endpoint` is `DCU_endpoint` (`http://host:port`), with or
    /// without a trailing slash.
    pub fn new(endpoint: &str, user_agent: &str, budgets: Budgets) -> Result<Self> {
        let base = endpoint.trim().trim_end_matches('/').to_owned();
        if base.is_empty() {
            return Err(DriverError::InvalidArg("empty DCU endpoint".into()));
        }
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(budgets.connect))
            .max_idle_connections_per_host(4)
            .user_agent(header_safe_user_agent(user_agent))
            .accept("application/json")
            .build();
        Ok(Client {
            agent: ureq::Agent::new_with_config(config),
            base,
            budgets,
            offline_until: Mutex::new(None),
            init_gate: AtomicU32::new(0),
        })
    }

    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    /// The endpoint without trailing slash (`DCU_endpoint`).
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `http://<dcu>/<module>/api/1.8.0/<task>/<key>` (PROTOCOL §1.1).
    pub fn url(&self, module: Module, task: Task, key: &str) -> String {
        format!(
            "{}/{}/api/{}/{}/{}",
            self.base,
            module.as_str(),
            API_VERSION,
            task.as_str(),
            key
        )
    }

    /// `http://<dcu>/data/<file>` (no API prefix, PROTOCOL §6.6), percent-encoded.
    pub fn data_url(&self, file: &str) -> String {
        format!("{}/data/{}", self.base, percent_encode_path(file))
    }

    /// True while the offline back-off window is open [R10].
    pub fn in_backoff(&self) -> bool {
        let until = *lock_unpoisoned(&self.offline_until);
        matches!(until, Some(t) if Instant::now() < t)
    }

    fn note_transport_failure(&self) {
        *lock_unpoisoned(&self.offline_until) = Some(Instant::now() + self.budgets.backoff);
    }

    fn note_response(&self) {
        *lock_unpoisoned(&self.offline_until) = None;
    }

    /// Close (`true`) or open (`false`) the initialize gate for one initialize PUT. Calls pair up:
    /// the gate stays closed until every close has been matched by an open.
    pub fn set_init_gate(&self, closed: bool) {
        if closed {
            self.init_gate.fetch_add(1, Ordering::SeqCst);
        } else {
            let _ = self
                .init_gate
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        }
    }

    /// True while one of the driver's own initialize PUTs is outstanding.
    pub fn init_gate_closed(&self) -> bool {
        self.init_gate.load(Ordering::SeqCst) > 0
    }

    /// Run one request. Any HTTP response is `Ok(Reply)` whatever its status; transport errors and
    /// timeouts are `Err` and open the back-off window.
    pub fn execute(&self, req: &Request) -> Result<Reply> {
        if !req.bypass_gate && self.init_gate_closed() {
            return Err(gate_error());
        }
        let result = self.execute_raw(req);
        match &result {
            Ok(_) => self.note_response(),
            Err(DriverError::Transport(_) | DriverError::Timeout(_)) => {
                self.note_transport_failure()
            }
            Err(_) => {}
        }
        result
    }

    fn execute_raw(&self, req: &Request) -> Result<Reply> {
        let url = req.url.as_str();
        let map = |e: ureq::Error| map_ureq_error(e, req.method, url, req.budget);
        let mut resp = match req.method {
            Method::Get => {
                let mut cfg = self
                    .agent
                    .get(url)
                    .config()
                    .timeout_global(Some(req.budget));
                if let Some(c) = req.connect {
                    cfg = cfg.timeout_connect(Some(c));
                }
                cfg.build().call().map_err(map)?
            }
            Method::Put => {
                let mut cfg = self
                    .agent
                    .put(url)
                    .config()
                    .timeout_global(Some(req.budget));
                if let Some(c) = req.connect {
                    cfg = cfg.timeout_connect(Some(c));
                }
                let b = cfg.build();
                match &req.body {
                    Some(v) => b.send_json(v).map_err(map)?,
                    None => b.send_empty().map_err(map)?,
                }
            }
        };
        let status = resp.status().as_u16();
        let limit = if status == 200 {
            req.limit
        } else {
            ERROR_BODY_MAX as u64 * 8
        };
        let body = match resp.body_mut().with_config().limit(limit).read_to_vec() {
            Ok(b) => b,
            // A non-200 whose body cannot be read is still that status.
            Err(_) if status != 200 => Vec::new(),
            Err(e) => return Err(map(e)),
        };
        Ok(Reply { status, body })
    }

    /// GET and return the JSON `value` (the whole document if it has no `value` key).
    /// Non-200 → `Http`.
    pub fn get_value(&self, url: &str, budget: Duration) -> Result<Value> {
        let reply = self.execute(&Request::get(url, budget))?;
        let reply = require_200(reply, url)?;
        let doc: Value = serde_json::from_slice(&reply.body).map_err(|e| {
            DriverError::Transport(format!("invalid JSON in response from {url}: {e}"))
        })?;
        Ok(unwrap_value(doc))
    }

    /// GET a string value.
    pub fn get_string(&self, url: &str, budget: Duration) -> Result<String> {
        match self.get_value(url, budget)? {
            Value::String(s) => Ok(s),
            other => Err(type_error(url, "string", &other)),
        }
    }

    /// GET a float value (ints are accepted).
    pub fn get_f64(&self, url: &str, budget: Duration) -> Result<f64> {
        let v = self.get_value(url, budget)?;
        v.as_f64().ok_or_else(|| type_error(url, "number", &v))
    }

    /// GET an integer value (an integral float is accepted).
    pub fn get_i64(&self, url: &str, budget: Duration) -> Result<i64> {
        let v = self.get_value(url, budget)?;
        if let Some(i) = v.as_i64() {
            return Ok(i);
        }
        match v.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(f as i64),
            _ => Err(type_error(url, "integer", &v)),
        }
    }

    /// GET a bool value.
    pub fn get_bool(&self, url: &str, budget: Duration) -> Result<bool> {
        let v = self.get_value(url, budget)?;
        v.as_bool().ok_or_else(|| type_error(url, "bool", &v))
    }

    /// Config PUT `{"value": v}`. Success = HTTP 200; the changed-keys list is returned for a
    /// DEBUG record and may be empty [G2-L].
    pub fn put_config(&self, url: &str, value: Value, budget: Duration) -> Result<Vec<String>> {
        let mut body = serde_json::Map::new();
        body.insert("value".into(), value);
        let reply = self.execute(&Request::put(url, Some(Value::Object(body)), budget))?;
        let reply = require_200(reply, url)?;
        let changed = match reply.json() {
            Some(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        if !changed.is_empty() {
            dlog!(Debug, "PUT {url}: changed {changed:?}");
        }
        Ok(changed)
    }

    /// Command PUT (no body, or `body`). Success = HTTP 200; the parsed body (absent, `{}` or an
    /// object) is returned.
    pub fn put_command(
        &self,
        url: &str,
        body: Option<Value>,
        budget: Duration,
    ) -> Result<Option<Value>> {
        let reply = self.execute(&Request::put(url, body, budget))?;
        let reply = require_200(reply, url)?;
        Ok(reply.json())
    }

    /// GET raw bytes (any status is returned; the caller decides). Used by the monitor (408).
    pub fn get_bytes(&self, url: &str, budget: Duration, limit: u64) -> Result<Reply> {
        let mut req = Request::get(url, budget);
        req.limit = limit;
        self.execute(&req)
    }

    /// Stream `GET url` into `dest` in [`DOWNLOAD_CHUNK`] chunks; returns the byte count. On any
    /// error the partial file is removed.
    pub fn download_to(&self, url: &str, budget: Duration, dest: &Path) -> Result<u64> {
        if self.init_gate_closed() {
            return Err(gate_error());
        }
        let result = self.download_raw(url, budget, dest);
        match &result {
            Ok(_) | Err(DriverError::Http { .. }) => self.note_response(),
            Err(DriverError::Transport(_) | DriverError::Timeout(_)) => {
                self.note_transport_failure()
            }
            Err(_) => {}
        }
        if result.is_err() {
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    fn download_raw(&self, url: &str, budget: Duration, dest: &Path) -> Result<u64> {
        let map = |e: ureq::Error| map_ureq_error(e, Method::Get, url, budget);
        let resp = self
            .agent
            .get(url)
            .config()
            .timeout_global(Some(budget))
            .build()
            .call()
            .map_err(map)?;
        let status = resp.status().as_u16();
        let mut reader = resp.into_body().into_reader();
        if status != 200 {
            let mut buf = Vec::new();
            let _ = reader.take(ERROR_BODY_MAX as u64).read_to_end(&mut buf);
            return Err(http_error(status, url, &buf));
        }
        let mut file = std::fs::File::create(dest)
            .map_err(|e| DriverError::Internal(format!("cannot create {}: {e}", dest.display())))?;
        let mut chunk = vec![0_u8; DOWNLOAD_CHUNK];
        let mut total = 0_u64;
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(map_io_error(e, Method::Get, url, budget)),
            };
            let part = chunk.get(..n).unwrap_or(&[]);
            file.write_all(part).map_err(|e| {
                DriverError::Internal(format!("cannot write {}: {e}", dest.display()))
            })?;
            total += n as u64;
        }
        file.flush()
            .map_err(|e| DriverError::Internal(format!("cannot write {}: {e}", dest.display())))?;
        Ok(total)
    }
}

/// The User-Agent as the header carries it. ureq builds the header at every request and panics
/// on a byte that `http::HeaderValue` refuses: below 0x20 except tab, and 0x7F. Those characters
/// (from `__app_title__` or `__app_version__`) become spaces, with one WARNING.
pub fn header_safe_user_agent(user_agent: &str) -> String {
    let bad = |c: char| (c < ' ' && c != '\t') || c == '\u{7f}';
    if !user_agent.chars().any(bad) {
        return user_agent.to_owned();
    }
    let clean: String = user_agent.chars().map(|c| if bad(c) { ' ' } else { c }).collect();
    dlog!(Warn, "User-Agent {user_agent:?} has control characters; sent as {clean:?}");
    clean
}

/// `{"value": x, …}` → `x`; anything else unchanged (PROTOCOL §1.2).
pub fn unwrap_value(doc: Value) -> Value {
    match doc {
        Value::Object(mut m) => match m.remove("value") {
            Some(v) => v,
            None => Value::Object(m),
        },
        other => other,
    }
}

/// `Http` unless the status is 200.
pub fn require_200(reply: Reply, url: &str) -> Result<Reply> {
    if reply.status == 200 {
        Ok(reply)
    } else {
        Err(http_error(reply.status, url, &reply.body))
    }
}

/// True if `e` is an HTTP error with this status.
pub fn is_status(e: &DriverError, status: u16) -> bool {
    matches!(e, DriverError::Http { status: s, .. } if *s == status)
}

/// True if `e` is a transport failure or a timeout (the DCU did not answer).
pub fn is_transport(e: &DriverError) -> bool {
    matches!(e, DriverError::Transport(_) | DriverError::Timeout(_))
}

fn http_error(status: u16, url: &str, body: &[u8]) -> DriverError {
    let mut text = String::from_utf8_lossy(body).trim().to_owned();
    if text.len() > ERROR_BODY_MAX {
        let mut cut = ERROR_BODY_MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    DriverError::Http {
        status,
        url: url.to_owned(),
        body: text,
    }
}

fn type_error(url: &str, want: &str, got: &Value) -> DriverError {
    let mut s = got.to_string();
    if s.len() > 80 {
        let mut cut = 80;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    DriverError::Transport(format!(
        "unexpected value from {url}: expected {want}, got {s}"
    ))
}

/// The error returned for any request while the driver's own initialize PUT is outstanding.
pub fn gate_error() -> DriverError {
    DriverError::State {
        state: "initialize".into(),
        needed: "no other request while initialize is in flight (SIMPLON blocking operation)"
            .into(),
    }
}

/// Map a ureq error to the driver's error model (DESIGN §6).
pub fn map_ureq_error(e: ureq::Error, method: Method, url: &str, budget: Duration) -> DriverError {
    match e {
        ureq::Error::Timeout(t) => DriverError::Timeout(format!(
            "{} {url}: {t} timeout (budget {:.1} s)",
            method.as_str(),
            budget.as_secs_f64()
        )),
        ureq::Error::Io(io) => map_io_error(io, method, url, budget),
        other => DriverError::Transport(format!("{} {url}: {other}", method.as_str())),
    }
}

fn map_io_error(io: std::io::Error, method: Method, url: &str, budget: Duration) -> DriverError {
    use std::io::ErrorKind as K;
    match io.kind() {
        K::TimedOut | K::WouldBlock => DriverError::Timeout(format!(
            "{} {url}: {io} (budget {:.1} s)",
            method.as_str(),
            budget.as_secs_f64()
        )),
        _ => DriverError::Transport(format!("{} {url}: {io}", method.as_str())),
    }
}

/// Percent-encode a path segment (RFC 3986 unreserved characters pass through).
pub fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::new("http://10.42.41.10/", "ua", Budgets::spec()).unwrap()
    }

    #[test]
    fn urls() {
        let c = client();
        assert_eq!(c.base(), "http://10.42.41.10");
        assert_eq!(
            c.url(Module::Detector, Task::Config, "count_time"),
            "http://10.42.41.10/detector/api/1.8.0/config/count_time"
        );
        assert_eq!(
            c.url(Module::Monitor, Task::Images, "monitor"),
            "http://10.42.41.10/monitor/api/1.8.0/images/monitor"
        );
        assert_eq!(
            c.url(Module::Detector, Task::Status, "high_voltage/state"),
            "http://10.42.41.10/detector/api/1.8.0/status/high_voltage/state"
        );
        assert_eq!(
            c.data_url("a b_master.h5"),
            "http://10.42.41.10/data/a%20b_master.h5"
        );
    }

    #[test]
    fn empty_endpoint_rejected() {
        assert!(matches!(
            Client::new("  ", "ua", Budgets::spec()),
            Err(DriverError::InvalidArg(_))
        ));
    }

    #[test]
    fn unwrap_value_shapes() {
        assert_eq!(
            unwrap_value(serde_json::json!({"value": 3, "unit": "s"})),
            serde_json::json!(3)
        );
        assert_eq!(
            unwrap_value(serde_json::json!(["a"])),
            serde_json::json!(["a"])
        );
        assert_eq!(
            unwrap_value(serde_json::json!({"x": 1})),
            serde_json::json!({"x": 1})
        );
    }

    #[test]
    fn http_error_truncates_body() {
        let body = "é".repeat(600);
        let e = http_error(500, "u", body.as_bytes());
        match e {
            DriverError::Http { status, body, .. } => {
                assert_eq!(status, 500);
                assert!(body.len() <= ERROR_BODY_MAX + 3);
            }
            _ => panic!(),
        }
        assert!(is_status(&http_error(405, "u", b""), 405));
        assert!(!is_status(&http_error(405, "u", b""), 404));
    }

    #[test]
    fn reply_json_empty_is_none() {
        let r = Reply {
            status: 200,
            body: b"  ".to_vec(),
        };
        assert!(r.json().is_none());
        let r = Reply {
            status: 200,
            body: br#"{"sequence_id": 4}"#.to_vec(),
        };
        assert_eq!(r.json().unwrap()["sequence_id"], 4);
    }

    #[test]
    fn gate_blocks_without_io() {
        let c = client();
        c.set_init_gate(true);
        let t0 = Instant::now();
        let e = c
            .get_value("http://10.255.255.1/x", Duration::from_secs(30))
            .unwrap_err();
        assert!(matches!(e, DriverError::State { .. }));
        assert!(t0.elapsed() < Duration::from_millis(100));
        assert!(!c.in_backoff());
    }

    #[test]
    fn gate_counts_overlapping_initializes() {
        // review #7: the old worker's reopen must not open the gate of the next initialize
        let c = client();
        c.set_init_gate(true);
        c.set_init_gate(true);
        c.set_init_gate(false);
        assert!(c.init_gate_closed());
        c.set_init_gate(false);
        assert!(!c.init_gate_closed());
        c.set_init_gate(false); // unmatched open: stays open, never wraps
        assert!(!c.init_gate_closed());
    }
}
