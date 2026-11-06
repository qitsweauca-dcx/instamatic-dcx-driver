//! Receive loop: poll slices, series attribution, timeouts, series tail, drain, flush, reconnect,
//! stats (docs/DESIGN.md §4.3, §2.6, §3.2 R1–R6, §3.7, §4.2 series tail / single-frame paths).
//!
//! [`Stream`] is the stream half of the driver's `Inner` (DESIGN §3.2): `zmq`, `start_cache`,
//! `armed_series_id` / `own_start_seen` / `driver_armed` (+ the [G2-H] pin and the per-epoch
//! bookkeeping, all in one µs [`SeriesState`] mutex), the `receivers` registry, the single-PULL
//! guard ticket and the stream counters. It is pure Rust: every method is meant to run **detached**
//! (inside `py.detach`) and never logs: records go into a caller-provided [`Deferred`], which the
//! caller emits after every guard is dropped (R6); `[Dectris] …` stdout lines are returned in the
//! reports.
//!
//! Lock rules: the socket mutex is held for one poll slice (≤ 250 ms) + recv, a flush drain, a
//! reconnect or a close — and nothing else happens under it (no classification, no start-cache
//! write, no other lock) [R2][G1-I]. The series mutex and the registry mutex are µs and never
//! nested with anything.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use dectris_codec::Header;
use dectris_common::log::{Deferred, Level};
use dectris_common::sync::lock_unpoisoned;
use dectris_common::{DriverError, Result};

use crate::attribution::{classify, Adopt, AttributionMode, Class, SeriesState, StalePolicy};
use crate::guard::{self, GuardTicket};
use crate::message::{Attribution, Delivery, Incoming, StartCache};
use crate::registry::{BusyOwner, Registry};
use crate::stats::{StreamStats, StreamStatsSnapshot};
use crate::zmq_link::{LinkConfig, RawMsg, ZmqLink};

// ---------------------------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------------------------

/// What the receiver needs from the driver's flags / `trig_cv` (DESIGN §3.2). The driver implements it
/// over `dectris_simplon::Signals`: `shutting_down()` → `Signals::shutting_down()`,
/// `on_armed_end()` → `Signals::set_end_seen()` + `Detector::clear_series_owner_for_epoch()`.
pub trait StreamSignals: Send + Sync {
    /// `_begin_shutdown()` / `request_stop()` ran: every receive slice returns `Shutdown`.
    fn shutting_down(&self) -> bool;
    /// The armed series' `end` (or, unattributed, any terminal end) was consumed under series
    /// `epoch`: set `end_seen` and notify the trigger waiters (§3.4 release condition) and, if the
    /// end was `attributed` to the armed series, clear the R7 series owner. The driver ignores an
    /// epoch that `arm()` has moved past (review #5/#6).
    fn on_armed_end(&self, epoch: u64, attributed: bool);
    /// Between receive slices (no mutex held): the driver's main-thread interrupt hook, so Ctrl+C
    /// ends a receive within one slice. `Err` = the interrupt.
    fn check_interrupt(&self) -> Result<()> {
        Ok(())
    }
}

/// Stand-alone [`StreamSignals`] (tests, tools without a detector).
#[derive(Debug, Default)]
pub struct LocalSignals {
    shutting_down: AtomicBool,
    armed_ends: AtomicU64,
}

impl LocalSignals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }
    /// How many times `on_armed_end` ran.
    pub fn armed_ends(&self) -> u64 {
        self.armed_ends.load(Ordering::SeqCst)
    }
}

impl StreamSignals for LocalSignals {
    fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
    fn on_armed_end(&self, _epoch: u64, _attributed: bool) {
        self.armed_ends.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------------------------
// Window and tuning
// ---------------------------------------------------------------------------------------------

/// Budgets of the receive paths. [`StreamTuning::spec`] is 03; tests shrink the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTuning {
    /// Window floor (Q2: 2000 ms; also the window when no frame time is known).
    pub window_floor: Duration,
    /// Window cap (Q2: 60 000 ms).
    pub window_cap: Duration,
    /// Poll slice (250 ms): the socket mutex is held at most this long; `shutting_down` is checked
    /// between slices.
    pub slice: Duration,
    /// `_acquire_image` post-success drain (DESIGN §4.2 step 4: ≤ 50 ms).
    pub drain_after_single: Duration,
    /// Flush safety stop: messages (a running series can refill the queue forever).
    pub flush_max_messages: u64,
    /// Flush safety stop: wall time.
    pub flush_max_time: Duration,
    /// After draining an image, a flush ends only when the stream has been quiet this long: the
    /// PULL queue holds a few messages and the rest of a backlog is still behind it.
    pub flush_quiet: Duration,
}

impl StreamTuning {
    pub const fn spec() -> Self {
        StreamTuning {
            window_floor: Duration::from_millis(2000),
            window_cap: Duration::from_millis(60_000),
            slice: Duration::from_millis(250),
            drain_after_single: Duration::from_millis(50),
            flush_max_messages: 10_000,
            flush_max_time: Duration::from_secs(2),
            flush_quiet: Duration::from_millis(50),
        }
    }
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self::spec()
    }
}

fn valid_secs(x: Option<f64>) -> Option<f64> {
    x.filter(|v| v.is_finite() && *v >= 0.0)
}

/// Receive window (DESIGN §4.3 "Window", Q2): `receive_timeout` (s) overrides; else
/// `clamp(2·frame_time·1000 + 1000, floor, cap)` ms with `frame_time` from the attributed start,
/// else from `last_applied`; with no frame time the window is the floor (2000 ms).
pub fn window_for(
    start_frame_time: Option<f64>,
    applied_frame_time: Option<f64>,
    receive_timeout: Option<f64>,
    floor: Duration,
    cap: Duration,
) -> Duration {
    if let Some(t) = receive_timeout.filter(|v| v.is_finite() && *v > 0.0) {
        return Duration::from_secs_f64(t.min(86_400.0));
    }
    let Some(ft) = valid_secs(start_frame_time).or(valid_secs(applied_frame_time)) else {
        return floor;
    };
    let ms = (2.0 * ft * 1000.0 + 1000.0).min(cap.as_millis() as f64);
    let w = Duration::from_secs_f64(ms / 1000.0);
    w.clamp(floor, cap.max(floor))
}

// ---------------------------------------------------------------------------------------------
// Sessions and events
// ---------------------------------------------------------------------------------------------

/// Per-receive-path context (one iterator, one `message_receiver` call, one `_acquire_image`).
#[derive(Debug, Clone)]
pub struct RecvSession {
    /// The receive window of this path (deadline = now + window per call).
    pub window: Duration,
    pub received: u64,
    pub delivered: u64,
    pub stale: u64,
    pub timeouts: u64,
    pub started: Instant,
    /// The series tail (disarm + one more window) was used [R14].
    pub tail_used: bool,
    /// Set by the iterator's `close()` from any thread: the receive returns `Cancelled` at the
    /// next slice (≤ 250 ms) instead of waiting out the window.
    pub cancel: Option<Arc<AtomicBool>>,
    warned_unknown: bool,
    warned_undecodable: bool,
}

impl RecvSession {
    pub fn new(window: Duration) -> Self {
        RecvSession {
            window,
            received: 0,
            delivered: 0,
            stale: 0,
            timeouts: 0,
            started: Instant::now(),
            tail_used: false,
            cancel: None,
            warned_unknown: false,
            warned_undecodable: false,
        }
    }

    /// The iterator owning this session was closed.
    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
    }

    /// The deadline of a call that starts now.
    pub fn deadline(&self) -> Instant {
        Instant::now() + self.window
    }

    /// `received N frames (M stale, K timeouts) in X s` (DESIGN §2.9 Volume: one INFO per series).
    pub fn summary(&self) -> String {
        format!(
            "received {} frames ({} stale, {} timeouts) in {:.2} s",
            self.delivered,
            self.stale,
            self.timeouts,
            self.started.elapsed().as_secs_f64()
        )
    }
}

/// Per-call options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecvOptions {
    /// `message_receiver` (Q3): return [`RecvEvent::Start`] as soon as the armed start is cached.
    /// Iterators leave it off (a start only updates the cache).
    pub return_on_start: bool,
}

/// Result of a receive call.
#[derive(Debug)]
pub enum RecvEvent {
    /// An image to hand out (decode / copy it, then build the metadata).
    Image(Delivery),
    /// The armed start was cached (only with [`RecvOptions::return_on_start`]).
    Start(Arc<StartCache>),
    /// The armed series' end (or a terminal unattributed end): `'end'` / `_STREAM_END`.
    End,
    /// The deadline passed: `'timeout'`.
    Timeout,
    /// `shutting_down` was set: `'shutdown'`.
    Shutdown,
    /// The session's `cancel` flag was set (the iterator was closed): `'cancelled'`.
    Cancelled,
}

enum Step {
    Deliver(Delivery),
    Start(Arc<StartCache>),
    End,
    Continue,
}

// ---------------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------------

/// Result of [`Stream::connect`] / [`Stream::establish`].
#[derive(Debug)]
pub enum ConnectOutcome {
    Connected,
    /// This instance already holds a connected PULL (no-op).
    AlreadyConnected,
    /// Another live instance holds the endpoint's PULL (§3.7): stays disconnected.
    HeldByOther,
    /// `establish` without a previous `connect` (no endpoint known).
    NotConfigured,
    /// Socket creation / connect failed (the guard was released again).
    Failed(DriverError),
}

#[derive(Debug)]
pub struct ConnectReport {
    pub outcome: ConnectOutcome,
    /// `[Dectris] ZMQ connected: <addr> (buffer HWM=<n> messages)` for `sys.stdout`.
    pub stdout: Option<String>,
}

/// Result of [`Stream::flush`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Number of messages discarded (`flush_zmq_buffer()` return value).
    Flushed(u64),
    /// A receiver of another thread is live, or the socket is in use: nothing drained (returns 0).
    Busy(String),
    /// No PULL connected (returns 0).
    NotConnected,
}

impl FlushOutcome {
    /// The `int` `flush_zmq_buffer()` returns.
    pub fn count(&self) -> u64 {
        match self {
            FlushOutcome::Flushed(n) => *n,
            _ => 0,
        }
    }
}

/// Result of [`Stream::reconnect`].
#[derive(Debug)]
pub enum ReconnectOutcome {
    Reconnected,
    Busy(String),
    HeldByOther,
    NotConfigured,
    Failed(DriverError),
}

#[derive(Debug)]
pub struct ReconnectReport {
    pub outcome: ReconnectOutcome,
    /// `[Dectris] ZMQ reconnected: fresh buffer` / `[Dectris] ZMQ reconnect skipped: receiver busy`.
    pub stdout: Option<String>,
}

impl ReconnectReport {
    /// The `bool` `reconnect_zmq()` returns.
    pub fn ok(&self) -> bool {
        matches!(self.outcome, ReconnectOutcome::Reconnected)
    }
}

/// Result of [`Stream::release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseReport {
    pub was_connected: bool,
    /// The `zmq_close` panic text, if one was caught [R8].
    pub close_panic: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

const POLICY_DROP: u8 = 0;
const POLICY_DELIVER: u8 = 1;
const NO_TIMEOUT: u64 = u64::MAX;

/// The stream half of the driver state (see the module docs). Create with [`Stream::new`]; it is
/// always used through an `Arc` (the single-PULL guard holds a `Weak` to it).
pub struct Stream {
    signals: Arc<dyn StreamSignals>,
    tuning: StreamTuning,
    link_cfg: Mutex<Option<LinkConfig>>,
    zmq: Mutex<Option<ZmqLink>>,
    ticket: Mutex<Option<GuardTicket>>,
    series: Mutex<SeriesState>,
    registry: Registry,
    stats: StreamStats,
    mode: AtomicU8,
    stale_policy: AtomicU8,
    /// `receive_timeout` in µs; `NO_TIMEOUT` = None.
    receive_timeout_us: AtomicU64,
    not_connected_warned: AtomicBool,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("tuning", &self.tuning)
            .field("mode", &self.attribution_mode())
            .field("stale_policy", &self.stale_policy())
            .finish_non_exhaustive()
    }
}

/// Mutexes [`Stream::test_hold_lock`] can hold (DESIGN §8 [R2]).
pub const TEST_LOCKS: &[&str] = &["zmq", "link_cfg", "ticket", "series", "receivers", "guard"];

fn hold_for<T>(m: &Mutex<T>, dur: Duration) {
    let _g = lock_unpoisoned(m);
    std::thread::sleep(dur);
}

impl Stream {
    /// Test hook (DESIGN §8 [R2]): hold the named mutex for `dur` on the calling (detached) thread.
    /// `false` for a name not in [`TEST_LOCKS`].
    #[doc(hidden)]
    pub fn test_hold_lock(&self, name: &str, dur: Duration) -> bool {
        match name {
            "zmq" => hold_for(&self.zmq, dur),
            "link_cfg" => hold_for(&self.link_cfg, dur),
            "ticket" => hold_for(&self.ticket, dur),
            "series" => hold_for(&self.series, dur),
            "receivers" => self.registry.test_hold_lock(dur),
            "guard" => guard::test_hold_lock(dur),
            _ => return false,
        }
        true
    }
}

fn try_lock_or_busy<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(PoisonError::into_inner(p)),
        Err(TryLockError::WouldBlock) => None,
    }
}

impl Stream {
    /// Unconnected stream with the DESIGN budgets.
    pub fn new(signals: Arc<dyn StreamSignals>) -> Arc<Stream> {
        Self::with_tuning(signals, StreamTuning::spec())
    }

    /// Unconnected stream with explicit budgets (tests shrink the window floor).
    pub fn with_tuning(signals: Arc<dyn StreamSignals>, tuning: StreamTuning) -> Arc<Stream> {
        Arc::new(Stream {
            signals,
            tuning,
            link_cfg: Mutex::new(None),
            zmq: Mutex::new(None),
            ticket: Mutex::new(None),
            series: Mutex::new(SeriesState::default()),
            registry: Registry::new(),
            stats: StreamStats::default(),
            mode: AtomicU8::new(AttributionMode::SequenceId.to_u8()),
            stale_policy: AtomicU8::new(POLICY_DROP),
            receive_timeout_us: AtomicU64::new(NO_TIMEOUT),
            not_connected_warned: AtomicBool::new(false),
        })
    }

    pub fn tuning(&self) -> StreamTuning {
        self.tuning
    }

    // ----- options ----------------------------------------------------------------------------

    pub fn attribution_mode(&self) -> AttributionMode {
        AttributionMode::from_u8(self.mode.load(Ordering::Relaxed))
    }
    pub fn set_attribution_mode(&self, m: AttributionMode) {
        self.mode.store(m.to_u8(), Ordering::Relaxed);
    }
    pub fn stale_policy(&self) -> StalePolicy {
        if self.stale_policy.load(Ordering::Relaxed) == POLICY_DELIVER {
            StalePolicy::Deliver
        } else {
            StalePolicy::Drop
        }
    }
    pub fn set_stale_policy(&self, p: StalePolicy) {
        let v = match p {
            StalePolicy::Drop => POLICY_DROP,
            StalePolicy::Deliver => POLICY_DELIVER,
        };
        self.stale_policy.store(v, Ordering::Relaxed);
    }
    /// `receive_timeout` (s); `None` = automatic window.
    pub fn receive_timeout(&self) -> Option<f64> {
        let v = self.receive_timeout_us.load(Ordering::Relaxed);
        (v != NO_TIMEOUT).then(|| v as f64 / 1e6)
    }
    /// Values that are not finite and > 0 mean `None`.
    pub fn set_receive_timeout(&self, t: Option<f64>) {
        let v = match t.filter(|v| v.is_finite() && *v > 0.0) {
            Some(s) => (s.min(86_400.0) * 1e6) as u64,
            None => NO_TIMEOUT,
        };
        self.receive_timeout_us.store(v, Ordering::Relaxed);
    }

    // ----- window -----------------------------------------------------------------------------

    /// The receive window (DESIGN §4.3): `receive_timeout`, else from the attributed start's
    /// `frame_time`, else `applied_frame_time` (`last_applied.frame_time`), else the floor.
    pub fn window(&self, applied_frame_time: Option<f64>) -> Duration {
        let mode = self.attribution_mode();
        let start_ft = {
            let st = lock_unpoisoned(&self.series);
            let attributed = st.own_start_seen || !st.driver_armed || mode == AttributionMode::Off;
            if attributed {
                st.start_cache.as_ref().and_then(|s| s.frame_time())
            } else {
                None
            }
        };
        window_for(
            start_ft,
            applied_frame_time,
            self.receive_timeout(),
            self.tuning.window_floor,
            self.tuning.window_cap,
        )
    }

    // ----- registry / stats / state ------------------------------------------------------------

    /// The `receivers` registry (DESIGN §3.2 R2/R7).
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// A live receiver owned by another thread (R2 busy, R7 ownership).
    pub fn busy_for(&self, caller: ThreadId) -> Option<BusyOwner> {
        self.registry.busy_for(caller)
    }

    pub fn stats(&self) -> StreamStatsSnapshot {
        self.stats.snapshot()
    }

    /// `armed_series_id` (`None` = unknown).
    pub fn armed_series_id(&self) -> Option<u64> {
        lock_unpoisoned(&self.series).armed_id
    }
    pub fn own_start_seen(&self) -> bool {
        lock_unpoisoned(&self.series).own_start_seen
    }
    /// The driver armed at least once (sticky; row 5 when false).
    pub fn driver_armed(&self) -> bool {
        lock_unpoisoned(&self.series).driver_armed
    }
    /// The armed series' end was consumed (receive path or flush) since the armed start.
    pub fn armed_end_seen(&self) -> bool {
        lock_unpoisoned(&self.series).armed_end_seen
    }
    /// `_stream_start_config` source: the cached start.
    pub fn start_cache(&self) -> Option<Arc<StartCache>> {
        lock_unpoisoned(&self.series).start_cache.clone()
    }
    /// `_stream_start_config = None`: clears the cache only (`own_start_seen` is kept, so armed
    /// images keep flowing, with `count_time = None`).
    pub fn clear_start_cache(&self) {
        lock_unpoisoned(&self.series).start_cache = None;
    }
    /// [R18] `true` the first time per series that a modifier ≠ itemsize is reported: the caller
    /// logs the one WARNING.
    pub fn note_modifier_mismatch(&self) -> bool {
        let mut st = lock_unpoisoned(&self.series);
        !std::mem::replace(&mut st.modifier_warned, true)
    }

    // ----- arm hooks (dectris_simplon::DriverHooks) --------------------------------------------

    /// `arm()` step 3 (before the arm PUT): new epoch; clear start cache / `own_start_seen` / pin;
    /// armed id unknown; `driver_armed = true`; reset the per-epoch self-checks.
    pub fn on_arm_reset(&self, epoch: u64) {
        lock_unpoisoned(&self.series).on_arm_reset(epoch, Instant::now());
    }

    /// `arm()` steps 5–6, after a successful arm PUT: store the parsed sequence id (ignored in
    /// `fifo` mode) and run the PUT→parse defence. Returns `true` if a mismatching cached start was
    /// dropped. A hook for an older epoch is ignored.
    pub fn on_armed(&self, epoch: u64, sequence_id: Option<u64>) -> bool {
        let mode = self.attribution_mode();
        let mut st = lock_unpoisoned(&self.series);
        if st.epoch != epoch {
            return false;
        }
        st.on_armed(sequence_id, mode)
    }

    /// R7 ownership cleared (disarm/abort/initialize/failed arm). Attribution is unchanged: frames
    /// of the armed series that arrive after a disarm (PROTOCOL §2.4 Information #4) stay attributed.
    pub fn on_ownership_cleared(&self) {}

    // ----- connection --------------------------------------------------------------------------

    /// Endpoint of the last `connect` (connected or not).
    pub fn endpoint(&self) -> Option<String> {
        lock_unpoisoned(&self.link_cfg)
            .as_ref()
            .map(|c| c.endpoint.clone())
    }

    pub fn is_connected(&self) -> bool {
        lock_unpoisoned(&self.zmq).is_some()
    }

    /// This instance holds the endpoint's single-PULL guard.
    pub fn holds_guard(&self) -> bool {
        lock_unpoisoned(&self.ticket).is_some()
    }

    /// `ZMQ_RCVHWM` read back from the live socket.
    pub fn rcvhwm(&self) -> Option<i32> {
        lock_unpoisoned(&self.zmq)
            .as_ref()
            .and_then(|l| l.rcvhwm().ok())
    }

    /// `(RCVHWM, LINGER, (TCP_KEEPALIVE, TCP_KEEPALIVE_IDLE))` read back from the live socket.
    pub fn socket_readback(&self) -> Option<(i32, i32, (i32, i32))> {
        let g = lock_unpoisoned(&self.zmq);
        let l = g.as_ref()?;
        Some((l.rcvhwm().ok()?, l.linger().ok()?, l.tcp_keepalive().ok()?))
    }

    fn weak_self(self: &Arc<Self>) -> Weak<dyn Any + Send + Sync> {
        let a: Arc<dyn Any + Send + Sync> = Arc::clone(self) as Arc<dyn Any + Send + Sync>;
        Arc::downgrade(&a)
    }

    /// Take the guard if this instance does not hold it. `false` = another live instance holds it.
    fn ensure_guard(self: &Arc<Self>, endpoint: &str) -> bool {
        let weak = self.weak_self();
        let mut t = lock_unpoisoned(&self.ticket);
        if t.is_some() {
            return true;
        }
        match guard::acquire(endpoint, weak) {
            Ok(tk) => {
                *t = Some(tk);
                true
            }
            Err(_) => false,
        }
    }

    fn drop_guard(&self) {
        let t = lock_unpoisoned(&self.ticket).take();
        drop(t);
    }

    /// `_connect` step 4 (DESIGN §2.2, §3.7): acquire the single-PULL guard, then open the PULL
    /// (RCVHWM before connect, LINGER 0, keepalive). Remembers `cfg` for `reconnect`/`establish`.
    pub fn connect(self: &Arc<Self>, cfg: LinkConfig, d: &mut Deferred) -> ConnectReport {
        *lock_unpoisoned(&self.link_cfg) = Some(cfg.clone());
        if self.is_connected() {
            d.push_stream(
                Level::Debug,
                format!("ZMQ already connected: {}", cfg.endpoint),
            );
            return ConnectReport {
                outcome: ConnectOutcome::AlreadyConnected,
                stdout: None,
            };
        }
        if !self.ensure_guard(&cfg.endpoint) {
            d.push_stream(
                Level::Warn,
                format!(
                    "another CameraDectris already holds the PULL for {}; this instance stays \
                     disconnected until the holder releases it (reconnect_zmq() retries)",
                    cfg.endpoint
                ),
            );
            return ConnectReport {
                outcome: ConnectOutcome::HeldByOther,
                stdout: None,
            };
        }
        match ZmqLink::connect(&cfg) {
            Ok(link) => {
                let hwm = link.rcvhwm().unwrap_or(cfg.rcvhwm);
                *lock_unpoisoned(&self.zmq) = Some(link);
                self.not_connected_warned.store(false, Ordering::Relaxed);
                d.push_stream(
                    Level::Info,
                    format!(
                        "ZMQ connected: {} (buffer HWM={hwm} messages)",
                        cfg.endpoint
                    ),
                );
                ConnectReport {
                    outcome: ConnectOutcome::Connected,
                    stdout: Some(format!(
                        "[Dectris] ZMQ connected: {} (buffer HWM={hwm} messages)",
                        cfg.endpoint
                    )),
                }
            }
            Err(e) => {
                self.drop_guard();
                d.push_stream(
                    Level::Error,
                    format!("ZMQ connect to {} failed: {e}", cfg.endpoint),
                );
                ConnectReport {
                    outcome: ConnectOutcome::Failed(e),
                    stdout: None,
                }
            }
        }
    }

    /// `establish_zmq_stream_connection()` [#12 keep]: connect with the remembered configuration
    /// if this instance holds no PULL; no-op (DEBUG) if it does. Busy rules as `reconnect`.
    pub fn establish(self: &Arc<Self>, caller: ThreadId, d: &mut Deferred) -> ConnectReport {
        if self.is_connected() {
            d.push_stream(Level::Debug, "ZMQ stream already connected");
            return ConnectReport {
                outcome: ConnectOutcome::AlreadyConnected,
                stdout: None,
            };
        }
        if let Some(b) = self.busy_for(caller) {
            d.push_stream(
                Level::Warn,
                format!("ZMQ connect skipped: receiver busy on thread {}", b.name),
            );
            return ConnectReport {
                outcome: ConnectOutcome::HeldByOther,
                stdout: None,
            };
        }
        let cfg = lock_unpoisoned(&self.link_cfg).clone();
        match cfg {
            Some(cfg) => self.connect(cfg, d),
            None => {
                d.push_stream(Level::Warn, "ZMQ connect skipped: no endpoint configured");
                ConnectReport {
                    outcome: ConnectOutcome::NotConfigured,
                    stdout: None,
                }
            }
        }
    }

    /// `release_connection()` step 5.2–5.3 (DESIGN §3.7): close the socket (LINGER 0) inside
    /// `catch_unwind`, drop the context, release the guard. Idempotent, never fails.
    pub fn release(&self, d: &mut Deferred) -> ReleaseReport {
        let link = lock_unpoisoned(&self.zmq).take();
        let was_connected = link.is_some();
        let close_panic = link.and_then(|l| l.close().err());
        if let Some(p) = &close_panic {
            d.push_stream(Level::Warn, format!("ZMQ socket close failed: {p}"));
        }
        self.drop_guard();
        if was_connected {
            d.push_stream(Level::Debug, "ZMQ socket closed");
        }
        ReleaseReport {
            was_connected,
            close_panic,
        }
    }

    /// `reconnect_zmq()` [R32]: busy check (another thread's live receiver, then `try_lock` on the
    /// socket) → close → new PULL with the same HWM → connect. The caller also invalidates the
    /// `ensure_config` cache (DESIGN §4.2). Never fails; [`ReconnectReport::ok`] is the `bool`.
    pub fn reconnect(self: &Arc<Self>, caller: ThreadId, d: &mut Deferred) -> ReconnectReport {
        const SKIPPED: &str = "[Dectris] ZMQ reconnect skipped: receiver busy";
        if let Some(b) = self.busy_for(caller) {
            d.push_stream(
                Level::Warn,
                format!("reconnect_zmq skipped: receiver busy on thread {}", b.name),
            );
            return ReconnectReport {
                outcome: ReconnectOutcome::Busy(b.name),
                stdout: Some(SKIPPED.to_owned()),
            };
        }
        let Some(cfg) = lock_unpoisoned(&self.link_cfg).clone() else {
            d.push_stream(Level::Warn, "reconnect_zmq skipped: no endpoint configured");
            return ReconnectReport {
                outcome: ReconnectOutcome::NotConfigured,
                stdout: None,
            };
        };
        if !self.ensure_guard(&cfg.endpoint) {
            d.push_stream(
                Level::Warn,
                format!(
                    "reconnect_zmq skipped: another CameraDectris holds the PULL for {}",
                    cfg.endpoint
                ),
            );
            return ReconnectReport {
                outcome: ReconnectOutcome::HeldByOther,
                stdout: None,
            };
        }
        let Some(mut g) = try_lock_or_busy(&self.zmq) else {
            d.push_stream(
                Level::Warn,
                "reconnect_zmq skipped: socket in use by a receiver",
            );
            return ReconnectReport {
                outcome: ReconnectOutcome::Busy("socket in use".into()),
                stdout: Some(SKIPPED.to_owned()),
            };
        };
        let close_panic = g.take().and_then(|l| l.close().err());
        let res = ZmqLink::connect(&cfg);
        let outcome = match res {
            Ok(link) => {
                *g = Some(link);
                Ok(())
            }
            Err(e) => Err(e),
        };
        drop(g);
        if let Some(p) = close_panic {
            d.push_stream(Level::Warn, format!("ZMQ socket close failed: {p}"));
        }
        match outcome {
            Ok(()) => {
                StreamStats::add(&self.stats.reconnects, 1);
                self.not_connected_warned.store(false, Ordering::Relaxed);
                d.push_stream(
                    Level::Info,
                    format!(
                        "ZMQ reconnected: fresh buffer ({}, HWM={})",
                        cfg.endpoint, cfg.rcvhwm
                    ),
                );
                ReconnectReport {
                    outcome: ReconnectOutcome::Reconnected,
                    stdout: Some("[Dectris] ZMQ reconnected: fresh buffer".to_owned()),
                }
            }
            Err(e) => {
                d.push_stream(
                    Level::Error,
                    format!("ZMQ reconnect to {} failed: {e}", cfg.endpoint),
                );
                ReconnectReport {
                    outcome: ReconnectOutcome::Failed(e),
                    stdout: None,
                }
            }
        }
    }

    // ----- flush -------------------------------------------------------------------------------

    /// `flush_zmq_buffer()` [R22][G1-I][G2-I] (DESIGN §4.3): busy check, then `recv(DONTWAIT)` until
    /// `EAGAIN` with a header scan per message; once an image was drained, until the stream has been
    /// quiet for `flush_quiet`. An unseen armed start is cached (after the socket guard is dropped)
    /// and not counted; everything else is discarded and counted; stale ones also in `stats.stale`,
    /// armed images also in `stats.flushed_armed` (+ WARNING when the armed end had not been seen
    /// before the flush). Never clears the start cache (Q28).
    pub fn flush(&self, caller: ThreadId, d: &mut Deferred) -> FlushOutcome {
        if let Some(b) = self.busy_for(caller) {
            d.push_stream(
                Level::Warn,
                format!(
                    "flush_zmq_buffer skipped: receiver busy on thread {}",
                    b.name
                ),
            );
            return FlushOutcome::Busy(b.name);
        }
        struct Drained {
            header: Result<Header>,
            start_msg: Option<RawMsg>,
        }
        let started = Instant::now();
        let mut drained: Vec<Drained> = Vec::new();
        let mut zmq_err = None;
        let mut capped = false;
        {
            let Some(g) = try_lock_or_busy(&self.zmq) else {
                d.push_stream(
                    Level::Warn,
                    "flush_zmq_buffer skipped: socket in use by a receiver",
                );
                return FlushOutcome::Busy("socket in use".into());
            };
            let Some(link) = g.as_ref() else {
                drop(g);
                d.push_stream(Level::Debug, "flush_zmq_buffer: ZMQ not connected");
                return FlushOutcome::NotConnected;
            };
            let mut last_image: Option<Instant> = None;
            loop {
                if drained.len() as u64 >= self.tuning.flush_max_messages
                    || started.elapsed() >= self.tuning.flush_max_time
                {
                    capped = true;
                    break;
                }
                let next = match last_image {
                    None => link.try_recv(),
                    Some(t) => {
                        let quiet = self.tuning.flush_quiet.saturating_sub(t.elapsed());
                        if quiet.is_zero() {
                            break;
                        }
                        link.poll_recv(quiet)
                    }
                };
                match next {
                    Ok(Some(m)) => {
                        let header = dectris_codec::scan_header(&m);
                        let is_start =
                            matches!(&header, Ok(h) if h.kind == dectris_codec::MessageKind::Start);
                        if matches!(&header, Ok(h) if h.kind == dectris_codec::MessageKind::Image) {
                            last_image = Some(Instant::now());
                        }
                        drained.push(Drained {
                            header,
                            start_msg: is_start.then_some(m),
                        });
                    }
                    Ok(None) if last_image.is_none() => break,
                    Ok(None) => {}
                    Err(e) => {
                        zmq_err = Some(e);
                        break;
                    }
                }
            }
        } // socket guard dropped [G1-I]
        if !drained.is_empty() {
            self.stats.mark_recv(Instant::now());
        }
        // Start scans (and their copies) outside every lock.
        let prepared: Vec<(Result<Header>, Option<StartCache>)> = drained
            .into_iter()
            .map(|x| {
                let sc = x
                    .start_msg
                    .and_then(|m| StartCache::from_raw(&m).ok().map(|(_, sc)| sc));
                (x.header, sc)
            })
            .collect();

        let mode = self.attribution_mode();
        let policy = self.stale_policy();
        let window = self.tuning.window_floor;
        let mut count = 0u64;
        let mut stale = 0u64;
        let mut armed_images = 0u64;
        let mut terminal_ends = 0u64;
        let mut attributed_end = false;
        let end_before;
        let end_epoch;
        {
            let mut st = lock_unpoisoned(&self.series);
            end_epoch = st.epoch;
            end_before = st.armed_end_seen;
            let now = Instant::now();
            for (header, sc) in prepared {
                let Ok(h) = header else {
                    count += 1;
                    continue;
                };
                let class = classify(&h, &st.view(), mode, policy, now, window);
                st.note(&h, class, now);
                match class {
                    Class::OwnStart(adopt) => match sc {
                        Some(sc) => {
                            if adopt == Adopt::Newer {
                                d.push_stream(
                                    Level::Warn,
                                    format!(
                                        "series armed outside the driver: adopting series {:?} (was {:?})",
                                        h.series_id, st.armed_id
                                    ),
                                );
                            }
                            st.accept_own_start(&h, sc);
                        }
                        None => count += 1,
                    },
                    Class::UnattributedStart => match sc {
                        Some(sc) => {
                            st.accept_unattributed_start(sc);
                        }
                        None => count += 1,
                    },
                    Class::Deliver | Class::DeliverStartMissing => {
                        count += 1;
                        armed_images += 1;
                    }
                    Class::ArmedEnd | Class::UnattributedEnd => {
                        count += 1;
                        terminal_ends += 1;
                        if class == Class::ArmedEnd {
                            st.armed_end_seen = true;
                            attributed_end = true;
                        }
                    }
                    c if c.is_stale() => {
                        count += 1;
                        stale += 1;
                    }
                    _ => count += 1,
                }
            }
        } // series guard dropped
        StreamStats::add(&self.stats.flushed, count);
        StreamStats::add(&self.stats.flushed_armed, armed_images);
        StreamStats::add(&self.stats.stale, stale);
        if terminal_ends > 0 {
            self.signals.on_armed_end(end_epoch, attributed_end);
        }
        if count > 0 {
            d.push_stream(
                Level::Info,
                format!("Flushed {count} stale messages from ZMQ buffer"),
            );
        }
        if armed_images > 0 && !end_before {
            d.push_stream(
                Level::Warn,
                format!("flush discarded {armed_images} image(s) of the current series"),
            );
        }
        if capped {
            d.push_stream(
                Level::Warn,
                format!(
                    "flush_zmq_buffer stopped after {count} message(s): the stream keeps producing"
                ),
            );
        }
        if let Some(e) = zmq_err {
            d.push_stream(Level::Warn, format!("flush_zmq_buffer stopped: {e}"));
        }
        FlushOutcome::Flushed(count)
    }

    // ----- receive -----------------------------------------------------------------------------

    /// One poll slice under the socket mutex. `None` = not connected.
    fn poll_slice(&self, slice: Duration) -> Option<Result<Option<RawMsg>>> {
        let g = lock_unpoisoned(&self.zmq);
        g.as_ref().map(|l| l.poll_recv(slice))
    }

    fn try_recv_now(&self) -> Option<Result<Option<RawMsg>>> {
        let g = lock_unpoisoned(&self.zmq);
        g.as_ref().map(|l| l.try_recv())
    }

    /// Receive until an event or `deadline` (DESIGN §2.6 step 3, §4.3). Non-image messages do not
    /// extend the deadline; every slice (≤ 250 ms) checks `shutting_down`. Records go to `d`.
    pub fn recv_next(
        &self,
        sess: &mut RecvSession,
        deadline: Instant,
        opts: RecvOptions,
        d: &mut Deferred,
    ) -> Result<RecvEvent> {
        loop {
            if self.signals.shutting_down() {
                return Ok(RecvEvent::Shutdown);
            }
            if sess.cancelled() {
                return Ok(RecvEvent::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                self.on_timeout(sess, now, d);
                return Ok(RecvEvent::Timeout);
            }
            let slice = deadline
                .saturating_duration_since(now)
                .min(self.tuning.slice);
            match self.poll_slice(slice) {
                None => {
                    if !self.not_connected_warned.swap(true, Ordering::Relaxed) {
                        d.push_stream(
                            Level::Warn,
                            "ZMQ stream not connected (another instance holds the PULL, or it was \
                             released): receive paths time out until reconnect_zmq()",
                        );
                    }
                    std::thread::sleep(slice);
                    self.signals.check_interrupt()?;
                }
                Some(Err(e)) => return Err(e),
                // only after an idle slice: never a GIL round trip per message
                Some(Ok(None)) => self.signals.check_interrupt()?,
                Some(Ok(Some(msg))) => match self.process(msg, sess, opts, d) {
                    Step::Deliver(x) => {
                        StreamStats::add(&self.stats.delivered, 1);
                        sess.delivered += 1;
                        return Ok(RecvEvent::Image(x));
                    }
                    Step::Start(s) => return Ok(RecvEvent::Start(s)),
                    Step::End => return Ok(RecvEvent::End),
                    Step::Continue => {}
                },
            }
        }
    }

    /// Series tail rule [R14] for paths that own their series (`_acquire_image`, `MovieIter`):
    /// like [`Self::recv_next`], but the first time the window expires the guarded `disarm`
    /// closure runs and the receive continues for one more window (SIMPLON Information #4: the
    /// last image is available only after a disarm). A second expiry returns `Timeout`: the
    /// caller raises `DectrisTimeout`. `deadline` is updated in place.
    pub fn recv_owned(
        &self,
        sess: &mut RecvSession,
        deadline: &mut Instant,
        opts: RecvOptions,
        disarm: &mut dyn FnMut(),
        d: &mut Deferred,
    ) -> Result<RecvEvent> {
        loop {
            match self.recv_next(sess, *deadline, opts, d)? {
                RecvEvent::Timeout if !sess.tail_used => {
                    sess.tail_used = true;
                    d.push_stream(
                        Level::Info,
                        "series tail: window expired with frames outstanding; disarming and \
                         waiting one more window",
                    );
                    disarm();
                    *deadline = Instant::now() + sess.window;
                }
                ev => return Ok(ev),
            }
        }
    }

    /// `_acquire_image` step 4 (DESIGN §4.2 [R13]): a ≤ 50 ms drain that reads at most one further
    /// message. The armed end is consumed (returns `true`); anything else is classified and
    /// counted (an armed image read here counts as stale). Never extends past 50 ms.
    pub fn drain_after_single(&self, sess: &mut RecvSession, d: &mut Deferred) -> Result<bool> {
        let deadline = Instant::now() + self.tuning.drain_after_single;
        loop {
            if self.signals.shutting_down() {
                return Ok(false);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let slice = deadline.saturating_duration_since(now);
            match self.poll_slice(slice) {
                None => return Ok(false),
                Some(Err(e)) => return Err(e),
                Some(Ok(None)) => {}
                Some(Ok(Some(msg))) => {
                    return Ok(match self.process(msg, sess, RecvOptions::default(), d) {
                        Step::End => true,
                        Step::Deliver(_) => {
                            StreamStats::add(&self.stats.stale, 1);
                            sess.stale += 1;
                            false
                        }
                        _ => false,
                    });
                }
            }
        }
    }

    /// `acquire_single_frame` with `single_frame_fresh: true` [R24]: wait for one image like
    /// [`Self::recv_next`], then drain the queue without blocking and return the **newest**
    /// attributed image; the older ones count in `stats.stale`. An armed end in the queue is
    /// consumed (trigger waiters are notified) and ends the drain. The drain stops at the flush
    /// caps (`flush_max_messages` / `flush_max_time`) so a fast series cannot keep it spinning.
    pub fn recv_freshest(
        &self,
        sess: &mut RecvSession,
        deadline: Instant,
        d: &mut Deferred,
    ) -> Result<RecvEvent> {
        let ev = self.recv_next(sess, deadline, RecvOptions::default(), d)?;
        let RecvEvent::Image(mut best) = ev else {
            return Ok(ev);
        };
        let started = Instant::now();
        let mut drained = 0u64;
        loop {
            if drained >= self.tuning.flush_max_messages
                || started.elapsed() >= self.tuning.flush_max_time
            {
                break;
            }
            drained += 1;
            let msg = match self.try_recv_now() {
                Some(Ok(Some(m))) => m,
                Some(Err(e)) => {
                    d.push_stream(Level::Debug, format!("fresh-frame drain stopped: {e}"));
                    break;
                }
                _ => break,
            };
            match self.process(msg, sess, RecvOptions::default(), d) {
                Step::Deliver(newer) if newer.attribution != Attribution::Stale => {
                    StreamStats::add(&self.stats.stale, 1);
                    sess.stale += 1;
                    best = newer;
                }
                Step::End => break,
                _ => {}
            }
        }
        Ok(RecvEvent::Image(best))
    }

    fn on_timeout(&self, sess: &mut RecvSession, now: Instant, d: &mut Deferred) {
        StreamStats::add(&self.stats.timeouts, 1);
        sess.timeouts += 1;
        let mode = self.attribution_mode();
        let mut mismatch = None;
        let mut no_start = false;
        {
            let mut st = lock_unpoisoned(&self.series);
            // [G2-D] self-check: an epoch with no armed-id message but a stale START after arm().
            if mode == AttributionMode::SequenceId
                && st.driver_armed
                && st.arm_returned
                && !st.armed_msg_seen
                && !st.mismatch_reported
            {
                if let Some(seen) = st.stale_start_after_arm {
                    st.mismatch_reported = true;
                    mismatch = Some((st.armed_id, seen));
                }
            }
            // DESIGN §10 ZMQ endpoint errors: no armed start within one window of arm.
            if mode != AttributionMode::Off
                && st.driver_armed
                && st.arm_returned
                && !st.own_start_seen
                && !st.start_missing_fallback
                && !st.no_start_warned
                && st
                    .armed_at
                    .is_some_and(|t| now.saturating_duration_since(t) >= sess.window)
            {
                st.no_start_warned = true;
                no_start = true;
            }
        }
        if let Some((armed, seen)) = mismatch {
            StreamStats::add(&self.stats.attribution_mismatch, 1);
            let armed = armed.map_or_else(|| "unknown".to_owned(), |a| a.to_string());
            d.push_stream(
                Level::Error,
                format!(
                    "series_id never matched arm sequence_id (armed={armed}, seen={seen}) — set \
                     series_attribution: fifo"
                ),
            );
        }
        if no_start {
            // [G1-R]
            let ep = self.endpoint().unwrap_or_else(|| "<not configured>".into());
            d.push_stream(
                Level::Warn,
                format!(
                    "no Stream V2 start within {:.1} s of arm — check ZMQ_host/ZMQ_port ({ep}) and \
                     stream mode",
                    sess.window.as_secs_f64()
                ),
            );
        }
        if dectris_common::log::log_frames() {
            d.push_stream(
                Level::Debug,
                format!("receive: poll timeout ({} ms)", sess.window.as_millis()),
            );
        }
    }

    /// Scan, classify, update the series state, decide (DESIGN §4.3). No socket lock is held.
    fn process(
        &self,
        msg: RawMsg,
        sess: &mut RecvSession,
        opts: RecvOptions,
        d: &mut Deferred,
    ) -> Step {
        let now = Instant::now();
        StreamStats::add(&self.stats.received, 1);
        self.stats.mark_recv(now);
        sess.received += 1;
        let inc = match Incoming::scan(msg) {
            Ok(i) => i,
            Err(e) => {
                if !std::mem::replace(&mut sess.warned_undecodable, true) {
                    d.push_stream(
                        Level::Warn,
                        format!("undecodable Stream V2 message skipped: {e}"),
                    );
                }
                return Step::Continue;
            }
        };
        let Incoming { msg, header, start } = inc;
        let mode = self.attribution_mode();
        let policy = self.stale_policy();
        let log_frames = dectris_common::log::log_frames();
        let mut terminal = false;
        let mut attributed_end = false;
        let mut stale = false;
        let end_epoch;
        let step = {
            let mut st = lock_unpoisoned(&self.series);
            end_epoch = st.epoch;
            let class = classify(&header, &st.view(), mode, policy, now, sess.window);
            st.note(&header, class, now);
            match class {
                Class::OwnStart(adopt) => match start {
                    Some(sc) => {
                        match adopt {
                            Adopt::Newer => d.push_stream(
                                Level::Warn,
                                format!(
                                    "series armed outside the driver: adopting series {:?} (was {:?})",
                                    header.series_id, st.armed_id
                                ),
                            ),
                            Adopt::FirstStart => d.push_stream(
                                Level::Debug,
                                format!(
                                    "series {:?} adopted (first start after arm)",
                                    header.series_id
                                ),
                            ),
                            Adopt::No => {}
                        }
                        let a = st.accept_own_start(&header, sc);
                        d.push_stream(
                            Level::Debug,
                            format!(
                                "Stream started: series_id={:?} count_time={:?} frame_time={:?}",
                                a.series_id(),
                                a.count_time(),
                                a.frame_time()
                            ),
                        );
                        if opts.return_on_start {
                            Step::Start(a)
                        } else {
                            Step::Continue
                        }
                    }
                    None => Step::Continue,
                },
                Class::DuplicateStart => Step::Continue,
                Class::UnattributedStart => match start {
                    Some(sc) => {
                        if mode != AttributionMode::Off
                            && !std::mem::replace(&mut st.unattributed_warned, true)
                        {
                            d.push_stream(
                                Level::Warn,
                                "series attribution unavailable: the detector was not armed by \
                                 this driver; every frame is delivered",
                            );
                        }
                        let a = st.accept_unattributed_start(sc);
                        if opts.return_on_start {
                            Step::Start(a)
                        } else {
                            Step::Continue
                        }
                    }
                    None => Step::Continue,
                },
                Class::StaleStart => {
                    stale = true;
                    if st.first_stale_warning(header.series_id) {
                        d.push_stream(
                            Level::Warn,
                            format!(
                                "stale start of series {:?} ignored (armed series {:?})",
                                header.series_id, st.armed_id
                            ),
                        );
                    }
                    Step::Continue
                }
                Class::Deliver | Class::DeliverUnattributed => {
                    let start = st.start_cache.clone();
                    if class == Class::DeliverUnattributed
                        && mode != AttributionMode::Off
                        && !std::mem::replace(&mut st.unattributed_warned, true)
                    {
                        d.push_stream(
                            Level::Warn,
                            "series attribution unavailable: the detector was not armed by this \
                             driver; every frame is delivered",
                        );
                    }
                    if start.is_none() && !std::mem::replace(&mut st.no_count_time_warned, true) {
                        d.push_stream(
                            Level::Warn,
                            "No start config cached — count_time metadata will be None",
                        );
                    }
                    if log_frames {
                        d.push_stream(
                            Level::Debug,
                            format!(
                                "Received frame: series={:?}, image={:?}",
                                header.series_id, header.image_id
                            ),
                        );
                    }
                    let attribution = if class == Class::Deliver {
                        Attribution::Armed
                    } else {
                        Attribution::Unattributed
                    };
                    Step::Deliver(Delivery {
                        msg,
                        header,
                        start,
                        attribution,
                    })
                }
                Class::DeliverStartMissing => {
                    if !std::mem::replace(&mut st.start_missing_fallback, true) {
                        d.push_stream(
                            Level::Warn,
                            format!(
                                "armed start missing (dropped by DCU?): delivering series {:?} \
                                 images with count_time=None",
                                header.series_id
                            ),
                        );
                    }
                    Step::Deliver(Delivery {
                        msg,
                        header,
                        start: None,
                        attribution: Attribution::StartMissing,
                    })
                }
                Class::StaleImage { deliver, armed_id } => {
                    stale = true;
                    if st.first_stale_warning(header.series_id) {
                        d.push_stream(
                            Level::Warn,
                            format!(
                                "stale image(s) of series {:?} {} (armed series {:?}{})",
                                header.series_id,
                                if deliver {
                                    "delivered with count_time=None"
                                } else {
                                    "dropped"
                                },
                                st.armed_id,
                                if armed_id {
                                    ", its start not received yet"
                                } else {
                                    ""
                                }
                            ),
                        );
                    }
                    if deliver {
                        Step::Deliver(Delivery {
                            msg,
                            header,
                            start: None,
                            attribution: Attribution::Stale,
                        })
                    } else {
                        Step::Continue
                    }
                }
                Class::ArmedEnd | Class::UnattributedEnd => {
                    if class == Class::ArmedEnd {
                        st.armed_end_seen = true;
                        attributed_end = true;
                    }
                    terminal = true;
                    d.push_stream(
                        Level::Debug,
                        format!("Stream ended: series_id={:?}", header.series_id),
                    );
                    Step::End
                }
                Class::StaleEnd => {
                    stale = true;
                    // Routine after a finished movie, whose end nobody read; it carries no data.
                    d.push_stream(
                        Level::Debug,
                        format!(
                            "stale end of series {:?} ignored (armed series {:?})",
                            header.series_id, st.armed_id
                        ),
                    );
                    Step::Continue
                }
                Class::Unknown => {
                    if !std::mem::replace(&mut sess.warned_unknown, true) {
                        d.push_stream(
                            Level::Warn,
                            format!(
                                "unexpected Stream V2 message type '{}' ignored",
                                header.kind.as_str()
                            ),
                        );
                    }
                    Step::Continue
                }
            }
        }; // series guard dropped
        if stale {
            StreamStats::add(&self.stats.stale, 1);
            sess.stale += 1;
        }
        if terminal {
            self.signals.on_armed_end(end_epoch, attributed_end);
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: Duration = Duration::from_millis(2000);
    const C: Duration = Duration::from_millis(60_000);

    #[test]
    fn window_table() {
        // no frame time → floor
        assert_eq!(window_for(None, None, None, F, C), F);
        // short frame times → floor (Q2: none for ft ≤ 0.5 s)
        assert_eq!(window_for(Some(0.1), None, None, F, C), F);
        assert_eq!(window_for(Some(0.5), None, None, F, C), F);
        // 2·0.9 s + 1 s = 2.8 s
        assert_eq!(
            window_for(Some(0.9), None, None, F, C),
            Duration::from_millis(2800)
        );
        // start beats last_applied; last_applied used when the start has none
        assert_eq!(
            window_for(Some(2.0), Some(10.0), None, F, C),
            Duration::from_millis(5000)
        );
        assert_eq!(
            window_for(None, Some(10.0), None, F, C),
            Duration::from_millis(21_000)
        );
        // cap
        assert_eq!(window_for(Some(100.0), None, None, F, C), C);
        // invalid values ignored
        assert_eq!(window_for(Some(f64::NAN), Some(-1.0), None, F, C), F);
        // receive_timeout overrides (s)
        assert_eq!(
            window_for(Some(100.0), None, Some(0.3), F, C),
            Duration::from_millis(300)
        );
        assert_eq!(window_for(None, None, Some(0.0), F, C), F);
        // floor override (tests)
        let f = Duration::from_millis(100);
        assert_eq!(window_for(None, None, None, f, C), f);
        assert_eq!(
            window_for(Some(0.1), None, None, f, C),
            Duration::from_millis(1200)
        );
    }

    #[test]
    fn receive_timeout_option() {
        let s = Stream::new(LocalSignals::new());
        assert_eq!(s.receive_timeout(), None);
        s.set_receive_timeout(Some(5.0));
        assert_eq!(s.receive_timeout(), Some(5.0));
        assert_eq!(s.window(None), Duration::from_secs(5));
        s.set_receive_timeout(Some(f64::INFINITY));
        assert_eq!(s.receive_timeout(), None);
        s.set_receive_timeout(Some(-1.0));
        assert_eq!(s.receive_timeout(), None);
        assert_eq!(s.window(Some(0.9)), Duration::from_millis(2800));
    }

    #[test]
    fn options_roundtrip() {
        let s = Stream::new(LocalSignals::new());
        assert_eq!(s.attribution_mode(), AttributionMode::SequenceId);
        assert_eq!(s.stale_policy(), StalePolicy::Drop);
        s.set_attribution_mode(AttributionMode::Fifo);
        s.set_stale_policy(StalePolicy::Deliver);
        assert_eq!(s.attribution_mode(), AttributionMode::Fifo);
        assert_eq!(s.stale_policy(), StalePolicy::Deliver);
    }

    #[test]
    fn session_summary() {
        let mut s = RecvSession::new(F);
        s.delivered = 3;
        s.stale = 1;
        s.timeouts = 2;
        let t = s.summary();
        assert!(
            t.starts_with("received 3 frames (1 stale, 2 timeouts) in "),
            "{t}"
        );
    }

    #[test]
    fn unconnected_stream_is_harmless() {
        let s = Stream::new(LocalSignals::new());
        let mut d = Deferred::new();
        assert_eq!(
            s.flush(std::thread::current().id(), &mut d),
            FlushOutcome::NotConnected
        );
        let r = s.release(&mut d);
        assert!(!r.was_connected);
        let rr = s.reconnect(std::thread::current().id(), &mut d);
        assert!(matches!(rr.outcome, ReconnectOutcome::NotConfigured));
        assert!(!rr.ok());
        assert!(s.rcvhwm().is_none());
        d.discard();
    }
}
