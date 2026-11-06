//! Test support (feature `test-util`): a scriptable fake SIMPLON 1.8 HTTP server for tests (std `TcpListener`, one thread per
//! connection, HTTP/1.1 keep-alive). Written from PROTOCOL §1-§6 only.
//!
//! * State machine: `na`/`idle`/`ready`/`acquire`/`initialize`/`configure`/`error`; arm idle→ready
//!   (sequence id +1), trigger ready→acquire (held per [`TriggerHold`]) →idle, disarm/abort/cancel
//!   →idle (and release held triggers), initialize →`initialize` for `init_delay` →idle (sequence
//!   id reset, stream mode reset to `disabled`).
//! * Request log with timestamps; per-route injection of status codes, latency, hangs, closed
//!   connections; frame_time rewrite-vs-reject model; header_appendix object rejection; monitor
//!   TIFF or 408; filewriter files + `/data/<file>`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::Budgets;

#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
    pub headers: Vec<(String, String)>,
    pub at: Instant,
}

impl Logged {
    /// `"PUT detector/command/arm"` style key (module/task/key without the api prefix).
    pub fn key(&self) -> String {
        format!("{} {}", self.method, short_path(&self.path))
    }
}

/// `/detector/api/1.8.0/command/arm` → `detector/command/arm`; `/data/x` → `data/x`.
pub fn short_path(path: &str) -> String {
    let p = path.trim_start_matches('/');
    let parts: Vec<&str> = p.splitn(4, '/').collect();
    if parts.len() == 4 && parts[1] == "api" {
        format!("{}/{}", parts[0], parts[3])
    } else {
        p.to_owned()
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Respond with this status and body.
    Status(u16, String),
    /// Sleep, then handle normally.
    Delay(Duration),
    /// Hold the request until the fake is dropped or `release_hangs()` is called; then close the
    /// connection without answering.
    Hang,
    /// Close the connection without answering.
    Close,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub method: Option<String>,
    pub path_contains: String,
    pub action: Action,
    /// `None` = forever.
    pub remaining: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmBody {
    SequenceId,
    SequenceIdSpace,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerHold {
    /// Return at once; the series completes (state → idle).
    Immediate,
    /// Hold until cancel/disarm/abort (state stays `acquire`).
    UntilRelease,
    /// Hold for this long or until released.
    For(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtModel {
    /// User Manual §7.4.2: the DCU rewrites the other parameter.
    Rewrite,
    /// PROTOCOL §15 #31 reject model: a conflicting frame_time PUT gets HTTP 400.
    Reject,
}

#[derive(Debug)]
pub struct FakeState {
    pub state: String,
    pub hv: String,
    pub detector: HashMap<String, Value>,
    pub stream: HashMap<String, Value>,
    pub monitor: HashMap<String, Value>,
    pub filewriter: HashMap<String, Value>,
    pub filewriter_state: String,
    pub files: Vec<(String, Vec<u8>)>,
    pub seq: u64,
    pub arm_body: ArmBody,
    pub log: Vec<Logged>,
    pub rules: Vec<Rule>,
    pub release_gen: u64,
    pub hang_gen: u64,
    pub init_delay: Duration,
    pub ft_model: FtModel,
    pub reject_object_header_appendix: bool,
    pub monitor_tiff: Option<Vec<u8>>,
    pub trigger_hold: TriggerHold,
    pub triggers_in_flight: usize,
    pub max_triggers_in_flight: usize,
    pub triggers_done: u64,
}

impl Default for FakeState {
    fn default() -> Self {
        let mut detector = HashMap::new();
        detector.insert("count_time".into(), json!(0.5));
        detector.insert("frame_time".into(), json!(0.504));
        detector.insert("detector_readout_time".into(), json!(0.004));
        detector.insert("nimages".into(), json!(1));
        detector.insert("ntrigger".into(), json!(1));
        detector.insert("trigger_mode".into(), json!("ints"));
        detector.insert("compression".into(), json!("bslz4"));
        detector.insert("x_pixels_in_detector".into(), json!(1028));
        detector.insert("y_pixels_in_detector".into(), json!(1062));
        detector.insert("bit_depth_image".into(), json!(32));
        detector.insert("countrate_correction_applied".into(), json!(true));
        let mut stream = HashMap::new();
        stream.insert("mode".into(), json!("enabled"));
        stream.insert("format".into(), json!("cbor"));
        stream.insert("header_detail".into(), json!("basic"));
        let mut monitor = HashMap::new();
        monitor.insert("mode".into(), json!("disabled"));
        let mut filewriter = HashMap::new();
        filewriter.insert("mode".into(), json!("disabled"));
        FakeState {
            state: "idle".into(),
            hv: "READY".into(),
            detector,
            stream,
            monitor,
            filewriter,
            filewriter_state: "ready".into(),
            files: Vec::new(),
            seq: 0,
            arm_body: ArmBody::SequenceId,
            log: Vec::new(),
            rules: Vec::new(),
            release_gen: 0,
            hang_gen: 0,
            init_delay: Duration::from_millis(50),
            ft_model: FtModel::Rewrite,
            reject_object_header_appendix: false,
            monitor_tiff: None,
            trigger_hold: TriggerHold::Immediate,
            triggers_in_flight: 0,
            max_triggers_in_flight: 0,
            triggers_done: 0,
        }
    }
}

struct Shared {
    st: Mutex<FakeState>,
    cv: Condvar,
    stop: AtomicBool,
    connections: AtomicUsize,
}

pub struct FakeSimplon {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<std::thread::JoinHandle<()>>,
}

fn lock(m: &Mutex<FakeState>) -> MutexGuard<'_, FakeState> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeSimplon {
    pub fn start() -> Self {
        Self::start_with(FakeState::default())
    }

    pub fn start_with(state: FakeState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            st: Mutex::new(state),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
        });
        let sh = Arc::clone(&shared);
        let accept = std::thread::Builder::new()
            .name("fake-simplon-accept".into())
            .spawn(move || accept_loop(listener, sh))
            .unwrap();
        FakeSimplon {
            addr,
            shared,
            accept: Some(accept),
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Lock and mutate the state.
    pub fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        let r = f(&mut lock(&self.shared.st));
        self.shared.cv.notify_all();
        r
    }

    pub fn set_state(&self, s: &str) {
        self.with(|st| st.state = s.into());
    }

    pub fn state(&self) -> String {
        self.with(|st| st.state.clone())
    }

    pub fn rule(
        &self,
        method: Option<&str>,
        path_contains: &str,
        action: Action,
        times: Option<u32>,
    ) {
        self.with(|st| {
            st.rules.push(Rule {
                method: method.map(str::to_owned),
                path_contains: path_contains.into(),
                action,
                remaining: times,
            })
        });
    }

    pub fn clear_rules(&self) {
        self.with(|st| st.rules.clear());
    }

    /// Let every `Action::Hang` request go (their connections close without an answer).
    pub fn release_hangs(&self) {
        self.with(|st| st.hang_gen += 1);
    }

    pub fn log(&self) -> Vec<Logged> {
        self.with(|st| st.log.clone())
    }

    /// Short keys (`"PUT detector/command/arm"`) of every logged request.
    pub fn keys(&self) -> Vec<String> {
        self.log().iter().map(Logged::key).collect()
    }

    pub fn clear_log(&self) {
        self.with(|st| st.log.clear());
    }

    pub fn count(&self, key: &str) -> usize {
        self.keys().iter().filter(|k| k.as_str() == key).count()
    }

    pub fn detector_value(&self, key: &str) -> Option<Value> {
        self.with(|st| st.detector.get(key).cloned())
    }

    pub fn stream_value(&self, key: &str) -> Option<Value> {
        self.with(|st| st.stream.get(key).cloned())
    }

    /// Wait until `pred` holds (≤ `timeout`).
    pub fn wait_for(&self, timeout: Duration, pred: impl Fn(&FakeState) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = lock(&self.shared.st);
        loop {
            if pred(&g) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .shared
                .cv
                .wait_timeout(g, (deadline - now).min(Duration::from_millis(20)))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for FakeSimplon {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.with(|st| {
            st.hang_gen += 1;
            st.release_gen += 1;
        });
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn accept_loop(listener: TcpListener, sh: Arc<Shared>) {
    while !sh.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let sh2 = Arc::clone(&sh);
                sh.connections.fetch_add(1, Ordering::SeqCst);
                let _ = std::thread::Builder::new()
                    .name("fake-simplon-conn".into())
                    .spawn(move || {
                        connection(stream, &sh2);
                        sh2.connections.fetch_sub(1, Ordering::SeqCst);
                    });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(r: &mut BufReader<TcpStream>, sh: &Shared) -> Option<Req> {
    let mut line = String::new();
    loop {
        match r.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if sh.stop.load(Ordering::SeqCst) {
                    return None;
                }
                if !line.is_empty() {
                    continue;
                }
            }
            Err(_) => return None,
        }
    }
    let mut it = line.split_whitespace();
    let method = it.next()?.to_owned();
    let path = it.next()?.to_owned();
    let mut len = 0_usize;
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        match r.read_line(&mut h) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if sh.stop.load(Ordering::SeqCst) {
                    return None;
                }
                continue;
            }
            Err(_) => return None,
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0_u8; len];
    let mut got = 0;
    while got < len {
        match r.read(&mut body[got..]) {
            Ok(0) => return None,
            Ok(n) => got += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if sh.stop.load(Ordering::SeqCst) {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    Some(Req {
        method,
        path,
        headers,
        body,
    })
}

struct Resp {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn json_resp(status: u16, v: Value) -> Resp {
    Resp {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&v).unwrap(),
    }
}

fn empty(status: u16) -> Resp {
    Resp {
        status,
        content_type: "application/json",
        body: Vec::new(),
    }
}

fn text(status: u16, s: &str) -> Resp {
    Resp {
        status,
        content_type: "text/plain",
        body: s.as_bytes().to_vec(),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn write_resp(w: &mut TcpStream, r: &Resp) -> bool {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    w.write_all(head.as_bytes()).is_ok() && w.write_all(&r.body).is_ok() && w.flush().is_ok()
}

fn connection(stream: TcpStream, sh: &Shared) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    while let Some(req) = read_request(&mut reader, sh) {
        match handle(&req, sh) {
            Some(resp) => {
                if !write_resp(&mut writer, &resp) {
                    return;
                }
            }
            None => {
                let _ = writer.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    }
}

/// Apply injection rules. `Some(resp)` short-circuits; `None` continues; `Err(())` = close.
fn apply_rules(req: &Req, sh: &Shared) -> Result<Option<Resp>, ()> {
    let action = {
        let mut st = lock(&sh.st);
        let mut found = None;
        for r in st.rules.iter_mut() {
            if r.method.as_deref().is_some_and(|m| m != req.method) {
                continue;
            }
            if !req.path.contains(&r.path_contains) {
                continue;
            }
            if let Some(n) = r.remaining.as_mut() {
                if *n == 0 {
                    continue;
                }
                *n -= 1;
            }
            found = Some(r.action.clone());
            break;
        }
        found
    };
    match action {
        None => Ok(None),
        Some(Action::Status(code, body)) => Ok(Some(text(code, &body))),
        Some(Action::Delay(d)) => {
            std::thread::sleep(d);
            Ok(None)
        }
        Some(Action::Close) => Err(()),
        Some(Action::Hang) => {
            let mut st = lock(&sh.st);
            let g0 = st.hang_gen;
            while st.hang_gen == g0 && !sh.stop.load(Ordering::SeqCst) {
                st = sh
                    .cv
                    .wait_timeout(st, Duration::from_millis(50))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            Err(())
        }
    }
}

fn handle(req: &Req, sh: &Shared) -> Option<Resp> {
    let body: Option<Value> = if req.body.is_empty() {
        None
    } else {
        serde_json::from_slice(&req.body).ok()
    };
    {
        let mut st = lock(&sh.st);
        st.log.push(Logged {
            method: req.method.clone(),
            path: req.path.clone(),
            body: body.clone(),
            headers: req.headers.clone(),
            at: Instant::now(),
        });
    }
    sh.cv.notify_all();
    match apply_rules(req, sh) {
        Err(()) => return None,
        Ok(Some(r)) => return Some(r),
        Ok(None) => {}
    }
    let sp = short_path(&req.path);
    let parts: Vec<&str> = sp.splitn(3, '/').collect();
    let value = body.as_ref().and_then(|b| b.get("value").cloned());
    let r = match (req.method.as_str(), parts.as_slice()) {
        ("GET", ["data", name]) => {
            let st = lock(&sh.st);
            match st.files.iter().find(|(n, _)| n == name) {
                Some((_, data)) => Resp {
                    status: 200,
                    content_type: "application/octet-stream",
                    body: data.clone(),
                },
                None => text(404, "no such file"),
            }
        }
        ("GET", ["detector", "status", key]) => {
            let st = lock(&sh.st);
            match *key {
                "state" => json_resp(200, json!({"value": st.state, "value_type": "string"})),
                "high_voltage/state" => json_resp(200, json!({"value": st.hv})),
                _ => text(404, "unknown status key"),
            }
        }
        ("GET", ["detector", "config", key]) => {
            let st = lock(&sh.st);
            if st.state == "na" {
                text(400, "Parameter does not exist")
            } else {
                match st.detector.get(*key) {
                    Some(v) => json_resp(200, json!({"value": v})),
                    None => text(404, "unknown key"),
                }
            }
        }
        ("PUT", ["detector", "config", key]) => detector_put(sh, key, value),
        ("PUT", ["detector", "command", cmd]) => return command(sh, cmd, value),
        ("GET", ["stream", "config", key]) => {
            let st = lock(&sh.st);
            match st.stream.get(*key) {
                Some(v) => json_resp(200, json!({"value": v})),
                None => text(404, "unknown key"),
            }
        }
        ("PUT", ["stream", "config", key]) => {
            let mut st = lock(&sh.st);
            if *key == "header_appendix"
                && st.reject_object_header_appendix
                && value.as_ref().is_some_and(Value::is_object)
            {
                text(400, "value_type string expected")
            } else if let Some(v) = value {
                st.stream.insert((*key).to_owned(), v);
                json_resp(200, json!([key]))
            } else {
                text(400, "missing value")
            }
        }
        ("GET", ["monitor", "images", "monitor"]) => {
            let st = lock(&sh.st);
            match &st.monitor_tiff {
                Some(t) => Resp {
                    status: 200,
                    content_type: "image/tiff",
                    body: t.clone(),
                },
                None => text(408, "no image"),
            }
        }
        ("PUT", ["monitor", "config", key]) => {
            let mut st = lock(&sh.st);
            if let Some(v) = value {
                st.monitor.insert((*key).to_owned(), v);
            }
            json_resp(200, json!([key]))
        }
        ("PUT", ["filewriter", "config", key]) => {
            let mut st = lock(&sh.st);
            if let Some(v) = value {
                st.filewriter.insert((*key).to_owned(), v);
            }
            json_resp(200, json!([key]))
        }
        ("GET", ["filewriter", "status", "state"]) => {
            let st = lock(&sh.st);
            json_resp(200, json!({"value": st.filewriter_state}))
        }
        ("GET", ["filewriter", "status", "files"]) => {
            let st = lock(&sh.st);
            let names: Vec<&str> = st.files.iter().map(|(n, _)| n.as_str()).collect();
            json_resp(200, json!({"value": names}))
        }
        ("PUT", ["filewriter", "command", "clear"]) => {
            lock(&sh.st).files.clear();
            empty(200)
        }
        _ => text(404, "unknown resource"),
    };
    sh.cv.notify_all();
    Some(r)
}

fn f(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

fn detector_put(sh: &Shared, key: &str, value: Option<Value>) -> Resp {
    let mut st = lock(&sh.st);
    if st.state == "na" {
        return text(400, "Parameter does not exist");
    }
    let Some(v) = value else {
        return text(400, "missing value");
    };
    let readout = f(st.detector.get("detector_readout_time"));
    let mut changed = vec![key.to_owned()];
    match key {
        "count_time" => {
            let ct = v.as_f64().unwrap_or(0.0);
            let ft = f(st.detector.get("frame_time"));
            if ft - ct < readout - 1e-12 {
                st.detector.insert("frame_time".into(), json!(ct + readout));
                changed.push("frame_time".into());
            }
            st.detector.insert("count_time".into(), v);
        }
        "frame_time" => {
            let ft = v.as_f64().unwrap_or(0.0);
            let ct = f(st.detector.get("count_time"));
            if ft - ct < readout - 1e-12 {
                match st.ft_model {
                    FtModel::Reject => return text(400, "frame_time conflicts with count_time"),
                    FtModel::Rewrite => {
                        st.detector.insert("count_time".into(), json!(ft - readout));
                        changed.push("count_time".into());
                    }
                }
            }
            st.detector.insert("frame_time".into(), v);
        }
        "nimages" | "ntrigger" => {
            if !v.is_u64() {
                return text(400, "value_type int expected");
            }
            st.detector.insert(key.into(), v);
        }
        _ => {
            st.detector.insert(key.into(), v);
        }
    }
    changed.sort();
    json_resp(200, json!(changed))
}

fn seq_body(st: &FakeState) -> Resp {
    match st.arm_body {
        ArmBody::SequenceId => json_resp(200, json!({"sequence_id": st.seq})),
        ArmBody::SequenceIdSpace => json_resp(200, json!({"sequence id": st.seq})),
        ArmBody::Empty => empty(200),
    }
}

fn command(sh: &Shared, cmd: &str, value: Option<Value>) -> Option<Resp> {
    let mut st = lock(&sh.st);
    let r = match cmd {
        "arm" => {
            if st.state != "idle" {
                text(400, "not idle")
            } else {
                st.seq += 1;
                st.triggers_done = 0;
                st.state = "ready".into();
                seq_body(&st)
            }
        }
        "disarm" | "abort" => {
            st.state = "idle".into();
            st.release_gen += 1;
            seq_body(&st)
        }
        "cancel" => {
            if st.state == "acquire" {
                st.state = "idle".into();
            }
            st.release_gen += 1;
            seq_body(&st)
        }
        "hv_enabled" => {
            match value.as_ref().and_then(Value::as_bool) {
                Some(true) => st.hv = "READY".into(),
                Some(false) => st.hv = "OFF".into(),
                None => return Some(text(400, "bool expected")),
            }
            empty(200)
        }
        "initialize" => {
            st.state = "initialize".into();
            let delay = st.init_delay;
            sh.cv.notify_all();
            drop(st);
            std::thread::sleep(delay);
            let mut st = lock(&sh.st);
            st.state = "idle".into();
            st.seq = 0;
            st.stream.insert("mode".into(), json!("disabled"));
            st.monitor.insert("mode".into(), json!("disabled"));
            sh.cv.notify_all();
            return Some(empty(200));
        }
        "trigger" => {
            if st.state != "ready" && st.state != "acquire" {
                return Some(text(400, "not armed"));
            }
            st.state = "acquire".into();
            st.triggers_in_flight += 1;
            st.max_triggers_in_flight = st.max_triggers_in_flight.max(st.triggers_in_flight);
            let hold = st.trigger_hold;
            let g0 = st.release_gen;
            sh.cv.notify_all();
            let deadline = match hold {
                TriggerHold::Immediate => Some(Instant::now()),
                TriggerHold::UntilRelease => None,
                TriggerHold::For(d) => Some(Instant::now() + d),
            };
            let mut released = false;
            loop {
                if st.release_gen != g0 || sh.stop.load(Ordering::SeqCst) {
                    released = true;
                    break;
                }
                let now = Instant::now();
                if let Some(d) = deadline {
                    if now >= d {
                        break;
                    }
                }
                let wait = deadline
                    .map(|d| d - now)
                    .unwrap_or(Duration::from_millis(50))
                    .min(Duration::from_millis(50));
                st = sh
                    .cv
                    .wait_timeout(st, wait)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            st.triggers_in_flight -= 1;
            if !released && st.state == "acquire" && st.triggers_in_flight == 0 {
                // one trigger's series completed; after ntrigger of them the detector
                // auto-disarms (SIMPLON footnote 1)
                st.triggers_done += 1;
                let nt = st
                    .detector
                    .get("ntrigger")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                st.state = if st.triggers_done >= nt {
                    "idle".into()
                } else {
                    "ready".into()
                };
            }
            empty(200)
        }
        _ => text(404, "unknown command"),
    };
    drop(st);
    sh.cv.notify_all();
    Some(r)
}

/// Budgets shrunk so policy tests run in milliseconds.
pub fn fast_budgets() -> Budgets {
    let ms = Duration::from_millis;
    let mut b = Budgets::spec();
    b.connect = ms(500);
    b.poll_get = ms(500);
    b.get = ms(1000);
    b.probe = ms(1000);
    b.get_slow = ms(1000);
    b.put = ms(1000);
    b.put_monitor = ms(1000);
    b.arm = ms(2000);
    b.abort = ms(1000);
    b.init_put = ms(5000);
    b.init_idle_wait = ms(2000);
    b.transient_wait = ms(600);
    b.idle_poll = ms(20);
    b.trigger_connect = ms(1000);
    b.trigger_min = ms(2000);
    b.trigger_max = ms(10_000);
    b.trigger_margin = ms(2000);
    b.monitor_get = ms(1000);
    b.download = ms(5000);
    b.backoff = ms(300);
    b
}
