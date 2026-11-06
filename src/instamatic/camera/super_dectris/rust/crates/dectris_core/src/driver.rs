//! Driver state: Arc<Inner>, epochs, trig_cv release protocol, series ownership (docs/DESIGN.md §3.2 [R3a][R7] [G1-B][G1-E], §3.4).
//!
//! [`Driver`] is DESIGN's `Inner`, assembled from the two halves the member crates own (DESIGN §3.2): the SIMPLON half ([`Detector`]: settings, `last_applied`, epochs, TriggerBook,
//! `armed_by`, …) and the stream half ([`Stream`]: socket, start cache, attribution, registry,
//! PULL guard). Both share ONE [`Signals`] (`trig_cv` + `cancel_requested`/`end_seen`/
//! `shutting_down`):
//!
//! ```text
//! Driver ─┬─ Arc<Detector> ── hooks: StreamHooks ──► Arc<Stream>   (arm reset / armed id)
//!         ├─ Arc<Stream>  ── signals: DriverSignals ─► Arc<Signals> + Weak<Detector>
//!         └─ Arc<Signals>                                           (no Arc cycle)
//! ```
//!
//! On top of that the driver owns the series-level procedures the py layer wraps: the R7
//! ownership check, `_acquire_image` (with the series tail and the 50 ms drain), the
//! `MovieIter` and `FrameIter` cores, `acquire_single_frame`, `message_receiver`, the ZMQ
//! maintenance calls and `release_connection` / `_begin_shutdown` (DESIGN §2.6, §3.7, §4.2).
//!
//! Every method here is meant to run **detached** (inside `py.detach`). SIMPLON calls log through
//! `dlog!` with no lock held; stream records go into the caller's [`Deferred`] (R6). Nothing in
//! this module attaches to Python, and no `Drop` here logs or does I/O [R8].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::thread::ThreadId;
use std::time::Duration;

use dectris_common::log::{Deferred, Level};
use dectris_common::sync::lock_unpoisoned;
use dectris_common::{DriverError, Result};
use dectris_simplon::detector::{mode_kind, ModeKind};
use dectris_simplon::{
    Budgets, Detector, DriverHooks, Settings, Signals, StartupReport, TriggerCaller,
    TriggerRequest, WorkerPool,
};
use dectris_stream::{
    ConnectReport, Delivery, LinkConfig, ReceiverReg, ReconnectReport, RecvEvent, RecvOptions,
    RecvSession, ReleaseReport, StartCache, Stream, StreamSignals, StreamTuning, DEFAULT_RCVBUF,
};

// ---------------------------------------------------------------------------------------------
// Glue between the halves
// ---------------------------------------------------------------------------------------------

/// [`StreamSignals`] over the shared [`Signals`]: the armed series' end sets `end_seen` (which
/// notifies the trigger waiters, R3a) and clears the R7 series owner (DESIGN §3.2 R7),
/// both only while that series is still the current epoch.
struct DriverSignals {
    signals: Arc<Signals>,
    detector: OnceLock<Weak<Detector>>,
}

impl StreamSignals for DriverSignals {
    fn shutting_down(&self) -> bool {
        self.signals.shutting_down()
    }

    fn check_interrupt(&self) -> Result<()> {
        self.signals.check_interrupt()
    }

    fn on_armed_end(&self, epoch: u64, attributed: bool) {
        let det = self.detector.get().and_then(Weak::upgrade);
        // an end classified before arm() moved to a new epoch belongs to the previous series: it
        // must not release the new series' triggers or clear its owner (review #6)
        if det.as_ref().is_some_and(|d| d.epoch() != epoch) {
            return;
        }
        self.signals.set_end_seen();
        // an unattributed end (series_attribution: off) may be a stale one: ownership stays with
        // the arming thread until its own procedure finishes or disarms (review #5)
        if attributed {
            if let Some(det) = det {
                det.clear_series_owner_for_epoch(epoch);
            }
        }
    }
}

/// [`DriverHooks`] forwarding `arm()`'s reset/armed steps to the stream half. Must not call back
/// into the [`Detector`] (DESIGN §3.2).
struct StreamHooks {
    stream: Arc<Stream>,
}

impl DriverHooks for StreamHooks {
    fn on_arm_reset(&self, epoch: u64) {
        self.stream.on_arm_reset(epoch);
    }
    fn on_armed(&self, epoch: u64, sequence_id: Option<u64>) {
        self.stream.on_armed(epoch, sequence_id);
    }
    fn on_ownership_cleared(&self) {
        self.stream.on_ownership_cleared();
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration and reports
// ---------------------------------------------------------------------------------------------

/// Everything `_connect` passes down (DESIGN §2.2), plus the budgets tests shrink.
#[derive(Clone)]
pub struct DriverConfig {
    /// `http://<DCU_host>:<DCU_port>`.
    pub dcu_endpoint: String,
    /// `tcp://<ZMQ_host>:<ZMQ_port>`.
    pub zmq_endpoint: String,
    /// `ZMQ_RCVHWM`.
    pub rcvhwm: i32,
    /// `zmq_rcvbuf`.
    pub rcvbuf: i32,
    /// `zmq_tcp_keepalive`.
    pub tcp_keepalive: bool,
    /// yaml `dimensions` (compared with `x/y_pixels_in_detector`).
    pub yaml_dims: Option<(i64, i64)>,
    pub user_agent: String,
    pub settings: Settings,
    pub budgets: Budgets,
    pub tuning: StreamTuning,
    /// Trigger worker pool; `None` = the process-wide pool (ceiling 8). Tests inject a private one.
    pub pool: Option<Arc<WorkerPool>>,
}

impl DriverConfig {
    /// Spec budgets and tuning; default settings.
    pub fn new(
        dcu_endpoint: impl Into<String>,
        zmq_endpoint: impl Into<String>,
        rcvhwm: i32,
    ) -> Self {
        DriverConfig {
            dcu_endpoint: dcu_endpoint.into(),
            zmq_endpoint: zmq_endpoint.into(),
            rcvhwm,
            rcvbuf: DEFAULT_RCVBUF,
            tcp_keepalive: false,
            yaml_dims: None,
            user_agent: String::new(),
            settings: Settings::default(),
            budgets: Budgets::spec(),
            tuning: StreamTuning::spec(),
            pool: None,
        }
    }
}

/// Result of [`Driver::connect`] (`_connect` steps 2–4).
#[derive(Debug)]
pub struct ConnectSummary {
    pub startup: StartupReport,
    pub zmq: ConnectReport,
}

/// Why a receive path stopped (`last_stop_reason`, DESIGN §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    NFrames,
    End,
    Timeout,
    Cancelled,
    Shutdown,
}

impl StopReason {
    /// The Python string.
    pub const fn as_str(self) -> &'static str {
        match self {
            StopReason::NFrames => "n_frames",
            StopReason::End => "end",
            StopReason::Timeout => "timeout",
            StopReason::Cancelled => "cancelled",
            StopReason::Shutdown => "shutdown",
        }
    }
}

/// One step of a frame iterator.
#[derive(Debug)]
pub enum FrameStep {
    Frame(Delivery),
    Stopped(StopReason),
}

/// Result of [`Driver::message_receiver`] (DESIGN §2.6).
#[derive(Debug)]
pub enum MessageEvent {
    /// An image of the armed series (the py layer builds the dict with `'_start_config'`).
    Image(Delivery),
    /// The armed start was cached (Q3): the py layer returns `None` at once.
    Start(Arc<StartCache>),
    /// `_STREAM_END`.
    End,
    /// `None` after `timeout` ms (or at shutdown).
    Nothing,
}

/// Result of [`Driver::release_connection`].
#[derive(Debug)]
pub struct ReleaseSummary {
    /// `false` if this call was a no-op (already released).
    pub released: bool,
    pub disarmed: bool,
    pub zmq: Option<ReleaseReport>,
}

// ---------------------------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------------------------

/// DESIGN's `Inner`. Always used through an `Arc` (iterators hold a clone).
pub struct Driver {
    det: Arc<Detector>,
    stream: Arc<Stream>,
    signals: Arc<Signals>,
    zmq_endpoint: String,
    rcvhwm: i32,
    rcvbuf: i32,
    tcp_keepalive: bool,
    yaml_dims: Option<(i64, i64)>,
    dcu_reachable: AtomicBool,
    single_frame_fresh: AtomicBool,
    released: AtomicBool,
    /// Held from the R7 check through `arm()` and the first trigger (review #1).
    setup: Mutex<()>,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("zmq_endpoint", &self.zmq_endpoint)
            .field("rcvhwm", &self.rcvhwm)
            .field("dcu_reachable", &self.dcu_reachable())
            .finish_non_exhaustive()
    }
}

impl Driver {
    /// Build both halves (no I/O). [`Driver::connect`] does the startup checks and opens the PULL.
    pub fn new(cfg: DriverConfig) -> Result<Arc<Driver>> {
        let signals = Signals::new();
        let dsig = Arc::new(DriverSignals {
            signals: signals.clone(),
            detector: OnceLock::new(),
        });
        let stream = Stream::with_tuning(dsig.clone(), cfg.tuning);
        let mut b = Detector::builder(cfg.dcu_endpoint)
            .budgets(cfg.budgets)
            .signals(signals.clone())
            .hooks(Arc::new(StreamHooks {
                stream: stream.clone(),
            }))
            .settings(cfg.settings);
        if !cfg.user_agent.is_empty() {
            b = b.user_agent(cfg.user_agent);
        }
        if let Some(p) = cfg.pool {
            b = b.pool(p);
        }
        let det = Arc::new(b.build()?);
        // Set exactly once, right after the build: nothing can have filled it before.
        let _ = dsig.detector.set(Arc::downgrade(&det));
        Ok(Arc::new(Driver {
            det,
            stream,
            signals,
            zmq_endpoint: cfg.zmq_endpoint,
            rcvhwm: cfg.rcvhwm,
            rcvbuf: cfg.rcvbuf,
            tcp_keepalive: cfg.tcp_keepalive,
            yaml_dims: cfg.yaml_dims,
            dcu_reachable: AtomicBool::new(false),
            single_frame_fresh: AtomicBool::new(true),
            released: AtomicBool::new(false),
            setup: Mutex::new(()),
        }))
    }

    // ----- accessors ---------------------------------------------------------------------------

    pub fn detector(&self) -> &Arc<Detector> {
        &self.det
    }
    pub fn stream(&self) -> &Arc<Stream> {
        &self.stream
    }
    pub fn signals(&self) -> &Arc<Signals> {
        &self.signals
    }
    pub fn dcu_reachable(&self) -> bool {
        self.dcu_reachable.load(Ordering::SeqCst)
    }
    pub fn single_frame_fresh(&self) -> bool {
        self.single_frame_fresh.load(Ordering::Relaxed)
    }
    pub fn set_single_frame_fresh(&self, v: bool) {
        self.single_frame_fresh.store(v, Ordering::Relaxed);
    }

    fn link_config(&self) -> LinkConfig {
        let mut c = LinkConfig::new(self.zmq_endpoint.clone(), self.rcvhwm);
        c.rcvbuf = self.rcvbuf;
        c.tcp_keepalive = self.tcp_keepalive;
        c
    }

    /// The current receive window (DESIGN §4.3 Q2): `receive_timeout`, else the attributed start's
    /// `frame_time`, else `last_applied.frame_time`, else the floor.
    pub fn window(&self) -> Duration {
        self.stream
            .window(self.det.last_applied().map(|a| a.frame_time))
    }

    // ----- construction ------------------------------------------------------------------------

    /// `_connect` steps 2–4 (DESIGN §2.2), all best-effort: SIMPLON startup checks, then the
    /// single-PULL guard + eager connect. The py layer prints `zmq.stdout` after every guard is
    /// dropped.
    pub fn connect(&self, d: &mut Deferred) -> ConnectSummary {
        let startup = self.det.connect_checks(self.yaml_dims);
        self.dcu_reachable
            .store(startup.reachable, Ordering::SeqCst);
        let zmq = self.stream.connect(self.link_config(), d);
        self.released.store(false, Ordering::SeqCst);
        ConnectSummary { startup, zmq }
    }

    // ----- R7 series ownership -----------------------------------------------------------------

    /// DESIGN §3.2 R7 [G1-E]: `DectrisBusy` (no HTTP) if a series armed by the driver is owned by
    /// another thread: a live receiver of another thread, or `armed_by` another thread with no
    /// disarm/abort/initialize/armed end since. Used by `_acquire_image`, `MovieIter`'s first
    /// `__next__` and `arm_for_preview`; public `arm`/`set_config`/`disarm` are not checked.
    pub fn check_ownership(&self) -> Result<()> {
        if !self.stream.driver_armed() {
            return Ok(());
        }
        let me = std::thread::current().id();
        let owner = self
            .stream
            .busy_for(me)
            .map(|b| b.name)
            .or_else(|| self.det.armed_by_other_thread());
        match owner {
            Some(name) => Err(DriverError::Busy(format!(
                "detector owned by a running series on thread {name}"
            ))),
            None => Ok(()),
        }
    }

    /// Start of a series setup (`_acquire_image`, `MovieIter`, `arm_for_preview`): takes the setup
    /// lock, then runs the R7 check. Another thread inside its own setup → `DectrisBusy` at once
    /// (never a wait), so two threads can no longer both pass R7 and both arm (review #1). Hold
    /// the guard until `arm()` and the first trigger are done, never across the receive.
    fn begin_setup(&self) -> Result<MutexGuard<'_, ()>> {
        let g = match self.setup.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err(DriverError::Busy(
                    "detector series being set up on another thread".into(),
                ))
            }
        };
        self.check_ownership()?;
        Ok(g)
    }

    /// After an error once `arm()` succeeded: guarded disarm, which also clears the R7 owner, so
    /// a failed trigger/receive never leaves the detector armed and owned (review #2). No HTTP on
    /// the shutdown path (release_connection disarms).
    fn disarm_after_error(&self, e: &DriverError) {
        if !matches!(e, DriverError::Shutdown) && !self.signals.shutting_down() {
            self.det.disarm();
        }
    }

    // ----- triggers ------------------------------------------------------------------------------

    fn mode(&self) -> ModeKind {
        mode_kind(&self.det.settings().trigger_mode)
    }

    /// One internal software trigger, if the mode takes software triggers (ints/inte; any other
    /// string is treated like ints). exts/exte → INFO only.
    fn internal_trigger(
        &self,
        caller: TriggerCaller,
        count_time: Option<f64>,
        d: &mut Deferred,
    ) -> Result<()> {
        match self.mode() {
            ModeKind::Exts | ModeKind::Exte => {
                d.info("external trigger mode: waiting for the EXT IN signal");
                Ok(())
            }
            kind => {
                let ct = if kind == ModeKind::Inte {
                    count_time
                } else {
                    None
                };
                self.det
                    .trigger(TriggerRequest {
                        caller,
                        timeout: None,
                        count_time: ct,
                    })
                    .map(|_| ())
            }
        }
    }

    // ----- single-frame paths (DESIGN §4.2) ----------------------------------------------------------

    /// `_acquire_image` [R13]: R7 check → `ensure_config(nimages=1)` → `arm()` → blocking trigger
    /// (ints/inte) → one armed-series image with the series tail [R14] → ≤ 50 ms drain. No disarm
    /// on success (the nimages=1 series auto-disarms); an error after the arm disarms. Timeout →
    /// `DectrisTimeout`.
    pub fn acquire_image(&self, exposure: f64, d: &mut Deferred) -> Result<Delivery> {
        let reg = {
            let _setup = self.begin_setup()?;
            let reg = self.stream.registry().register(self.window());
            self.det.ensure_config(1, exposure, 1, None)?;
            self.det.arm()?;
            if let Err(e) =
                self.internal_trigger(TriggerCaller::InternalBlocking, Some(exposure), d)
            {
                self.disarm_after_error(&e);
                return Err(e);
            }
            reg
        };
        let window = self.window();
        reg.set_window(window);
        let mut sess = RecvSession::new(window);
        let mut deadline = sess.deadline();
        let det = self.det.clone();
        let ev = match self.stream.recv_owned(
            &mut sess,
            &mut deadline,
            RecvOptions::default(),
            &mut || det.disarm(),
            d,
        ) {
            Ok(ev) => ev,
            Err(e) => {
                self.disarm_after_error(&e);
                return Err(e);
            }
        };
        let out = match ev {
            RecvEvent::Image(x) => {
                // The nimages=1 series is complete once its image is here: release R7 ownership
                // even if the end arrives after the 50 ms drain (it is then stale next epoch).
                self.det.clear_series_owner();
                self.stream.drain_after_single(&mut sess, d)?;
                Ok(x)
            }
            RecvEvent::Timeout => Err(DriverError::Timeout(format!(
                "no frame received within {:.1} s (+ series tail)",
                window.as_secs_f64()
            ))),
            RecvEvent::End => Err(DriverError::Timeout(
                "series ended before a frame was received".into(),
            )),
            RecvEvent::Shutdown => Err(DriverError::Shutdown),
            RecvEvent::Start(_) | RecvEvent::Cancelled => {
                Err(DriverError::Internal("unexpected receive event".into()))
            }
        };
        reg.release();
        d.push_stream(Level::Info, sess.summary());
        out
    }

    /// `acquire_single_frame` (DESIGN §4.2): internal non-blocking trigger (once per series in ints),
    /// then one armed-series image or `DectrisTimeout`. Never disarms (autocred owns the series).
    /// `single_frame_fresh` returns the newest queued image instead of the oldest [R24].
    pub fn acquire_single_frame(&self, d: &mut Deferred) -> Result<Delivery> {
        let window = self.window();
        let reg = self.stream.registry().register(window);
        self.internal_trigger(TriggerCaller::Internal, None, d)?;
        let mut sess = RecvSession::new(window);
        let deadline = sess.deadline();
        let ev = if self.single_frame_fresh() {
            self.stream.recv_freshest(&mut sess, deadline, d)?
        } else {
            self.stream
                .recv_next(&mut sess, deadline, RecvOptions::default(), d)?
        };
        reg.release();
        match ev {
            RecvEvent::Image(x) => Ok(x),
            RecvEvent::Timeout => Err(DriverError::Timeout(format!(
                "no frame received within {:.1} s",
                window.as_secs_f64()
            ))),
            RecvEvent::End => Err(DriverError::Timeout(
                "series ended before a frame was received".into(),
            )),
            RecvEvent::Shutdown => Err(DriverError::Shutdown),
            RecvEvent::Start(_) | RecvEvent::Cancelled => {
                Err(DriverError::Internal("unexpected receive event".into()))
            }
        }
    }

    /// `arm_for_preview()` [#12 keep] with the R7 check (DESIGN §2.3) under the setup lock.
    pub fn arm_for_preview(&self) -> Result<()> {
        let _setup = self.begin_setup()?;
        self.det.arm_for_preview()
    }

    /// `message_receiver(timeout=2000)` (DESIGN §2.6): the armed start → [`MessageEvent::Start`] at
    /// once (Q3); an armed image; the armed end; `Nothing` after `timeout_ms` or at shutdown.
    pub fn message_receiver(&self, timeout_ms: u64, d: &mut Deferred) -> Result<MessageEvent> {
        let window = Duration::from_millis(timeout_ms);
        let reg = self.stream.registry().register(window);
        let mut sess = RecvSession::new(window);
        let deadline = sess.deadline();
        let opts = RecvOptions {
            return_on_start: true,
        };
        let ev = self.stream.recv_next(&mut sess, deadline, opts, d);
        reg.release();
        Ok(match ev? {
            RecvEvent::Image(x) => MessageEvent::Image(x),
            RecvEvent::Start(s) => MessageEvent::Start(s),
            RecvEvent::End => MessageEvent::End,
            RecvEvent::Timeout | RecvEvent::Shutdown | RecvEvent::Cancelled => {
                MessageEvent::Nothing
            }
        })
    }

    // ----- ZMQ maintenance -----------------------------------------------------------------------

    /// `flush_zmq_buffer()` [R22]: the number of discarded messages (0 when busy).
    pub fn flush_zmq_buffer(&self, d: &mut Deferred) -> u64 {
        self.stream.flush(std::thread::current().id(), d).count()
    }

    /// `reconnect_zmq()` [R32]; a successful reconnect invalidates the ensure cache (DESIGN §4.2).
    pub fn reconnect_zmq(&self, d: &mut Deferred) -> ReconnectReport {
        let r = self.stream.reconnect(std::thread::current().id(), d);
        if r.ok() {
            self.det.invalidate_ensure_cache();
            self.released.store(false, Ordering::SeqCst);
        }
        r
    }

    /// `establish_zmq_stream_connection()` [#12 keep].
    pub fn establish_zmq_stream_connection(&self, d: &mut Deferred) -> ConnectReport {
        let r = self.stream.establish(std::thread::current().id(), d);
        if self.stream.is_connected() {
            self.released.store(false, Ordering::SeqCst);
        }
        r
    }

    // ----- shutdown (DESIGN §3.7) --------------------------------------------------------------------

    /// `_begin_shutdown()` / `request_stop()`: set `shutting_down` and notify `trig_cv`. No HTTP,
    /// no logging, idempotent, permanent for the instance [G1-S].
    pub fn begin_shutdown(&self) {
        self.det.request_stop();
    }

    /// `release_connection()` steps 1–3 (DESIGN §3.7): guarded `disarm()` (skipped when the DCU was
    /// unreachable at construction, giving a clean offline exit), close the PULL (LINGER 0, the
    /// `zmq_close` panic caught inside), release the guard. Idempotent; never fails. The py layer
    /// logs the INFO (step 4) and, on the atexit path only, sets `LOGGING_CLOSED` (step 5).
    pub fn release_connection(&self, d: &mut Deferred) -> ReleaseSummary {
        if self.released.swap(true, Ordering::SeqCst) {
            return ReleaseSummary {
                released: false,
                disarmed: false,
                zmq: None,
            };
        }
        let disarmed = self.dcu_reachable() && !self.det.init_in_flight();
        if disarmed {
            self.det.disarm();
        }
        let zmq = self.stream.release(d);
        ReleaseSummary {
            released: true,
            disarmed,
            zmq: Some(zmq),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Receive contexts
// ---------------------------------------------------------------------------------------------

/// Per-iterator receive state. Only the iterating thread locks it (for the whole of one
/// `__next__`, detached); `close()` never does, so it cannot block on a running receive.
#[derive(Debug, Default)]
struct RecvCtx {
    sess: Option<RecvSession>,
    reg: Option<ReceiverReg>,
}

impl RecvCtx {
    fn release(&mut self) {
        if let Some(r) = self.reg.take() {
            r.release();
        }
    }
}

// ----- FrameIter core (DESIGN §2.6) ------------------------------------------------------------------

/// Which data a frame iterator hands out (the py layer converts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    Frames,
    WithMeta,
    Raw,
}

/// Core of `receive_frames` / `receive_frames_with_metadata` / `receive_raw_frames`: the
/// silent-stop family (PRESERVE). Does not arm and does not stop on `cancel_requested`
/// (buffered frames are preserved).
pub struct FrameRecv {
    drv: Arc<Driver>,
    pub mode: FrameMode,
    remaining: AtomicU64,
    done: AtomicBool,
    /// `close()` from any thread wakes a blocked `next()` within one slice.
    cancel: Arc<AtomicBool>,
    reason: StopSlot,
    ctx: Mutex<RecvCtx>,
}

impl std::fmt::Debug for FrameRecv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameRecv")
            .field("mode", &self.mode)
            .field("remaining", &self.remaining())
            .field("done", &self.done.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl FrameRecv {
    pub fn new(drv: Arc<Driver>, n_frames: u64, mode: FrameMode) -> Self {
        FrameRecv {
            drv,
            mode,
            remaining: AtomicU64::new(n_frames),
            done: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            reason: StopSlot::default(),
            ctx: Mutex::new(RecvCtx::default()),
        }
    }

    pub fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::SeqCst)
    }

    /// The window of this iterator (fixed at the first `__next__`), if started.
    pub fn window(&self) -> Option<Duration> {
        lock_unpoisoned(&self.ctx).sess.as_ref().map(|s| s.window)
    }

    /// Why it stopped, once it has (the first reason sticks).
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.reason.get()
    }

    /// `__next__`. A stop is final: later calls return the same `Stopped(reason)`.
    pub fn next(&self, d: &mut Deferred) -> Result<FrameStep> {
        if self.done.load(Ordering::SeqCst) || self.remaining() == 0 {
            self.reason.set(StopReason::NFrames);
            return Ok(FrameStep::Stopped(self.reason.or(StopReason::NFrames)));
        }
        let mut ctx = lock_unpoisoned(&self.ctx);
        if ctx.sess.is_none() {
            let w = self.drv.window();
            ctx.reg = Some(self.drv.stream.registry().register(w));
            let mut sess = RecvSession::new(w);
            sess.cancel = Some(Arc::clone(&self.cancel));
            ctx.sess = Some(sess);
        }
        if let Some(r) = &ctx.reg {
            r.touch();
        }
        let RecvCtx { sess, .. } = &mut *ctx;
        let Some(sess) = sess.as_mut() else {
            return Err(DriverError::Internal("receive session missing".into()));
        };
        let deadline = sess.deadline();
        let ev = self
            .drv
            .stream
            .recv_next(sess, deadline, RecvOptions::default(), d);
        let stop = match ev {
            Ok(RecvEvent::Image(x)) => {
                let left = self
                    .remaining
                    .fetch_sub(1, Ordering::SeqCst)
                    .saturating_sub(1);
                if left == 0 {
                    // The registry entry goes with the last frame, not on the next call.
                    self.reason.set(StopReason::NFrames);
                    self.finish(&mut ctx, d);
                }
                return Ok(FrameStep::Frame(x));
            }
            Ok(RecvEvent::Timeout) => StopReason::Timeout,
            Ok(RecvEvent::End) => StopReason::End,
            Ok(RecvEvent::Shutdown) => StopReason::Shutdown,
            Ok(RecvEvent::Cancelled) => StopReason::Cancelled,
            Ok(RecvEvent::Start(_)) => {
                return Err(DriverError::Internal("unexpected start event".into()))
            }
            Err(e) => {
                self.finish(&mut ctx, d);
                return Err(e);
            }
        };
        if stop == StopReason::Timeout {
            d.push_stream(
                Level::Info,
                format!(
                    "receive stopped: no frame within {:.1} s",
                    ctx.sess.as_ref().map_or(0.0, |s| s.window.as_secs_f64())
                ),
            );
        }
        self.reason.set(stop);
        self.finish(&mut ctx, d);
        Ok(FrameStep::Stopped(self.reason.or(stop)))
    }

    fn finish(&self, ctx: &mut RecvCtx, d: &mut Deferred) {
        if !self.done.swap(true, Ordering::SeqCst) {
            if let Some(s) = &ctx.sess {
                d.push_stream(Level::Info, s.summary());
            }
        }
        ctx.release();
    }

    /// `close()`: stop and drop the registry entry (no HTTP); a `next()` blocked on another thread
    /// returns `Stopped(Cancelled)` within one slice. Before the n-th frame the reason is
    /// `cancelled`; after it, `n_frames` stays.
    pub fn close(&self) {
        if self.remaining() > 0 {
            self.reason.set(StopReason::Cancelled);
        }
        self.cancel.store(true, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
        if let Ok(mut ctx) = self.ctx.try_lock() {
            ctx.release();
        }
    }
}

/// The first stop reason of an iterator (sticky: later calls report the same one).
#[derive(Debug, Default)]
struct StopSlot(Mutex<Option<StopReason>>);

impl StopSlot {
    /// Record `r` unless a reason is already set.
    fn set(&self, r: StopReason) {
        lock_unpoisoned(&self.0).get_or_insert(r);
    }
    fn get(&self) -> Option<StopReason> {
        *lock_unpoisoned(&self.0)
    }
    fn or(&self, r: StopReason) -> StopReason {
        self.get().unwrap_or(r)
    }
}

// ----- MovieIter core (DESIGN §2.6 [R7][R14][G1-F]) ----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MovieState {
    Idle,
    Arming,
    Armed,
    Done,
}

/// Core of `get_movie` / `stream`: arms on the first `__next__`, disarms on exhaustion, error or
/// `close()`, and never stops short silently: a short movie raises `DectrisTimeout` [G1-F].
pub struct Movie {
    drv: Arc<Driver>,
    n: u64,
    exposure: f64,
    state: Mutex<MovieState>,
    close_requested: AtomicBool,
    delivered: AtomicU64,
    /// Set with `close_requested`: wakes a `next()` blocked in the receive within one slice.
    cancel: Arc<AtomicBool>,
    reason: StopSlot,
    /// The thread whose `arm()` started this series (for the ownership release on drop).
    armer: OnceLock<ThreadId>,
    ctx: Mutex<RecvCtx>,
}

impl std::fmt::Debug for Movie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Movie")
            .field("n", &self.n)
            .field("exposure", &self.exposure)
            .field("state", &*lock_unpoisoned(&self.state))
            .field("delivered", &self.delivered())
            .finish_non_exhaustive()
    }
}

impl Movie {
    pub fn new(drv: Arc<Driver>, n_frames: u64, exposure: f64) -> Self {
        Movie {
            drv,
            n: n_frames,
            exposure,
            state: Mutex::new(MovieState::Idle),
            close_requested: AtomicBool::new(false),
            delivered: AtomicU64::new(0),
            cancel: Arc::new(AtomicBool::new(false)),
            reason: StopSlot::default(),
            armer: OnceLock::new(),
            ctx: Mutex::new(RecvCtx::default()),
        }
    }

    /// Why the movie stopped, once it has (the first reason sticks; set before a `DectrisTimeout`
    /// is raised too).
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.reason.get()
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::SeqCst)
    }

    pub fn n(&self) -> u64 {
        self.n
    }

    fn set_state(&self, s: MovieState) {
        *lock_unpoisoned(&self.state) = s;
    }

    /// `__next__`: `Frame`, or `Stopped(NFrames)` after the n-th frame (disarmed when that frame
    /// arrived), or `Stopped(Cancelled)` after `close()`. Every other stop with frames missing is
    /// an error (`DectrisTimeout`, or the arming error).
    pub fn next(&self, d: &mut Deferred) -> Result<FrameStep> {
        if self.n == 0 {
            self.reason.set(StopReason::NFrames);
            self.set_state(MovieState::Done);
            return Ok(FrameStep::Stopped(self.reason.or(StopReason::NFrames)));
        }
        let first = {
            let mut st = lock_unpoisoned(&self.state);
            match *st {
                MovieState::Done => {
                    drop(st);
                    if let Ok(mut ctx) = self.ctx.try_lock() {
                        ctx.release();
                    }
                    return Ok(FrameStep::Stopped(self.reason.or(StopReason::Cancelled)));
                }
                MovieState::Idle => {
                    *st = MovieState::Arming;
                    true
                }
                MovieState::Arming | MovieState::Armed => false,
            }
        };
        let mut ctx = lock_unpoisoned(&self.ctx);
        if first {
            if let Err(e) = self.arm(&mut ctx, d) {
                self.set_state(MovieState::Done);
                ctx.release();
                return Err(e);
            }
            // step 3: a close() that landed while arming
            let closed = {
                let mut st = lock_unpoisoned(&self.state);
                if self.close_requested.load(Ordering::SeqCst) {
                    *st = MovieState::Done;
                    true
                } else {
                    *st = MovieState::Armed;
                    false
                }
            };
            if closed {
                ctx.release();
                self.drv.det.disarm();
                self.reason.set(StopReason::Cancelled);
                return Ok(FrameStep::Stopped(StopReason::Cancelled));
            }
        }
        if self.close_requested.load(Ordering::SeqCst) {
            // close() already disarmed (or will, if it saw Arming)
            ctx.release();
            return Ok(FrameStep::Stopped(self.reason.or(StopReason::Cancelled)));
        }
        if self.drv.mode() == ModeKind::Inte && !first {
            // one serialised trigger before each frame; the first went out in arm()
            if let Err(e) =
                self.drv
                    .internal_trigger(TriggerCaller::Internal, Some(self.exposure), d)
            {
                return Err(self.fail(&mut ctx, e, d));
            }
        }
        if let Some(r) = &ctx.reg {
            r.touch();
        }
        let det = self.drv.det.clone();
        let RecvCtx { sess, .. } = &mut *ctx;
        let Some(sess) = sess.as_mut() else {
            return Err(DriverError::Internal("receive session missing".into()));
        };
        let mut deadline = sess.deadline();
        let ev = self.drv.stream.recv_owned(
            sess,
            &mut deadline,
            RecvOptions::default(),
            &mut || det.disarm(),
            d,
        );
        let reason = match ev {
            Ok(RecvEvent::Image(x)) => {
                let k = self.delivered.fetch_add(1, Ordering::SeqCst) + 1;
                if k >= self.n {
                    if let Some(s) = &ctx.sess {
                        d.push_stream(Level::Info, s.summary());
                    }
                    ctx.release();
                    self.reason.set(StopReason::NFrames);
                    self.set_state(MovieState::Done);
                    // Exhausted: disarm now (which clears the R7 owner), not on a later call, by
                    // when another thread may own a newer series (review #3).
                    self.drv.det.disarm();
                }
                return Ok(FrameStep::Frame(x));
            }
            Ok(RecvEvent::Cancelled) => {
                // close() from another thread: it already disarmed (state was Armed)
                if let Some(s) = &ctx.sess {
                    d.push_stream(Level::Info, s.summary());
                }
                ctx.release();
                self.reason.set(StopReason::Cancelled);
                return Ok(FrameStep::Stopped(self.reason.or(StopReason::Cancelled)));
            }
            Ok(RecvEvent::Timeout) => StopReason::Timeout,
            Ok(RecvEvent::End) => StopReason::End,
            Ok(RecvEvent::Shutdown) => StopReason::Shutdown,
            Ok(RecvEvent::Start(_)) => {
                return Err(self.fail(
                    &mut ctx,
                    DriverError::Internal("unexpected start event".into()),
                    d,
                ))
            }
            Err(e) => return Err(self.fail(&mut ctx, e, d)),
        };
        // our own series-tail disarm also sets cancel_requested: only an outside cancel counts
        let tail_used = ctx.sess.as_ref().is_some_and(|s| s.tail_used);
        let reason =
            if reason == StopReason::End && !tail_used && self.drv.signals.cancel_requested() {
                StopReason::Cancelled
            } else {
                reason
            };
        self.reason.set(reason);
        let e = DriverError::Timeout(format!(
            "movie ended ({}) after {} of {} frames",
            reason.as_str(),
            self.delivered(),
            self.n
        ));
        if reason == StopReason::Shutdown {
            // no HTTP on the shutdown path: release_connection disarms
            self.set_state(MovieState::Done);
            ctx.release();
            return Err(e);
        }
        Err(self.fail(&mut ctx, e, d))
    }

    /// First `__next__` steps 0 and 2 (DESIGN §2.6): R7 check, `ensure_config` (ints/exts: nimages=n;
    /// inte/exte: nimages=1, ntrigger=n [G2-B]), `arm()`, the first trigger, all under the setup
    /// lock. A failed first trigger disarms (review #2).
    fn arm(&self, ctx: &mut RecvCtx, d: &mut Deferred) -> Result<()> {
        let _setup = self.drv.begin_setup()?;
        ctx.reg = Some(self.drv.stream.registry().register(self.drv.window()));
        let kind = self.drv.mode();
        let (nimages, ntrigger) = match kind {
            ModeKind::Inte | ModeKind::Exte => (1, self.n.max(1)),
            _ => (self.n.max(1), 1),
        };
        self.drv
            .det
            .ensure_config(nimages, self.exposure, ntrigger, None)?;
        self.drv.det.arm()?;
        let _ = self.armer.set(std::thread::current().id());
        let w = self.drv.window();
        if let Some(r) = &ctx.reg {
            r.set_window(w);
        }
        let mut sess = RecvSession::new(w);
        sess.cancel = Some(Arc::clone(&self.cancel));
        ctx.sess = Some(sess);
        if let Err(e) = self
            .drv
            .internal_trigger(TriggerCaller::Internal, Some(self.exposure), d)
        {
            self.drv.disarm_after_error(&e);
            return Err(e);
        }
        Ok(())
    }

    /// Terminal error: Done, release the registry entry, guarded disarm, summary record.
    fn fail(&self, ctx: &mut RecvCtx, e: DriverError, d: &mut Deferred) -> DriverError {
        self.set_state(MovieState::Done);
        if let Some(s) = &ctx.sess {
            d.push_stream(Level::Info, s.summary());
        }
        ctx.release();
        self.drv.det.disarm();
        e
    }

    /// `close()` (DESIGN §2.6): Armed → Done + guarded disarm; Arming → the arming thread disarms;
    /// Idle → Done. Idempotent; never blocks on a running `__next__`.
    pub fn close(&self) {
        if self.delivered() < self.n {
            self.reason.set(StopReason::Cancelled);
        }
        self.close_requested.store(true, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
        let disarm = {
            let mut st = lock_unpoisoned(&self.state);
            match *st {
                MovieState::Armed => {
                    *st = MovieState::Done;
                    true
                }
                MovieState::Idle => {
                    *st = MovieState::Done;
                    false
                }
                MovieState::Arming | MovieState::Done => false,
            }
        };
        if let Ok(mut ctx) = self.ctx.try_lock() {
            ctx.release();
        }
        if disarm {
            self.drv.det.disarm();
        }
    }
}

impl Drop for Movie {
    /// A movie dropped before it finished (no exhaustion, no `close()`, no error) gives up its R7
    /// ownership locally, so other threads are not `DectrisBusy` for good (review A4). No HTTP
    /// here [R8]: the detector stays armed until the next owner's `ensure_idle` disarms it.
    fn drop(&mut self) {
        let state = *self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if state != MovieState::Done {
            if let Some(t) = self.armer.get() {
                self.drv.det.clear_series_owner_of(*t);
            }
        }
    }
}
