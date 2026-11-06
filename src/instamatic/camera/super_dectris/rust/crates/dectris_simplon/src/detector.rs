//! Detector state-machine policy (docs/DESIGN.md §4.2; PROTOCOL §2-§4).
//!
//! [`Detector`] owns the SIMPLON side of the driver state that DESIGN §3.2 lists in `Inner`
//! (`last_applied`, `ensure_cache`, `modes`/`options`, `epoch`, `TriggerBook`, the initialize slot,
//! the monitor cache, the last good getter values, `armed_by`) and the policy around it:
//! `ensure_idle` [G2-J], `disarm`/`arm`/`trigger`/`cancel`/`abort`/`initialize` [G2-A],
//! `set_config` steps 0-8 ([G1-D] cache invalidation, [G2-B] enable-mode mapping, [G2-C]
//! best-effort first `frame_time`, [G2-M] header_appendix retry, `frame_time_policy`), the re-apply
//! of the last configuration after an implicit initialize (DESIGN §12 #7), `ensure_config`, and the
//! `[#12 keep]` wrappers.
//!
//! The state that only the driver/stream crates can touch (`start_cache`, `armed_series_id`,
//! `own_start_seen`, `driver_armed`, the receivers registry) is reached through [`DriverHooks`].
//! Releases and waits go through the shared [`Signals`] (`trig_cv` + flags), DESIGN §3.2.
//!
//! All methods take `&self`; `Detector` is `Send + Sync`. Every method may block on HTTP: the py
//! layer calls them inside `Python::detach`. No method holds a lock across I/O, a wait, a log
//! record or another lock (R3, R6).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use dectris_common::sync::lock_unpoisoned;
use dectris_common::{dlog, DriverError, Result};
use serde_json::{json, Value};

use crate::client::{gate_error, is_status, is_transport, Budgets, Client, Module, Request, Task};
use crate::monitor::MonitorCache;
use crate::signals::{Signals, WaitEnd};
use crate::tiff::{MonitorDecoder, TiffCrateDecoder};
use crate::worker::{spawn_worker, Outcome, WorkerKind, WorkerPool};

/// Valid trigger modes (PROTOCOL §4.4-3).
pub const TRIGGER_MODES: [&str; 4] = ["ints", "inte", "exts", "exte"];
/// Valid `compression` values for `set_compression` (`none` is local only, PROTOCOL §15 #5).
pub const COMPRESSIONS: [&str; 3] = ["bslz4", "lz4", "none"];
/// Fallback `detector_readout_time` when the GET fails (PROTOCOL §5.3).
pub const FALLBACK_READOUT: f64 = 0.004;
/// Tolerance of the step-7 readback comparison.
const READBACK_TOL: f64 = 1e-6;

/// Trigger-mode class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    Ints,
    Inte,
    Exts,
    Exte,
    /// Any other string (the lenient property accepts anything); treated like `ints` for triggers.
    Other,
}

pub fn mode_kind(mode: &str) -> ModeKind {
    match mode {
        "ints" => ModeKind::Ints,
        "inte" => ModeKind::Inte,
        "exts" => ModeKind::Exts,
        "exte" => ModeKind::Exte,
        _ => ModeKind::Other,
    }
}

/// `frame_time_policy` (DESIGN §12 #4): what `set_config` does when `frame_time` is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameTimePolicy {
    /// `frame_time = exposure + detector_readout_time` (default).
    Readout,
    /// Leave the DCU value.
    Detector,
}

impl FrameTimePolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "readout" => Some(FrameTimePolicy::Readout),
            "detector" => Some(FrameTimePolicy::Detector),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            FrameTimePolicy::Readout => "readout",
            FrameTimePolicy::Detector => "detector",
        }
    }
}

/// The yaml-backed modes and options the SIMPLON layer reads (DESIGN §2.2 OPTION_KEYS, properties).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub trigger_mode: String,
    pub compression: String,
    pub stream_mode: String,
    pub stream_format: String,
    pub auto_initialize: bool,
    pub trigger_blocking: bool,
    pub frame_time_policy: FrameTimePolicy,
    /// `None` = never PUT `header_detail`.
    pub stream_header_detail: Option<String>,
    /// The `_HEADER_APPENDIX` object (or `None` = never PUT).
    pub header_appendix: Option<Value>,
    /// Monitor rate limit (DESIGN §12 #18, default 10 Hz). Values ≤ 0 or non-finite mean 10.
    pub monitor_max_hz: f64,
    /// `default_exposure` (yaml) for `arm_for_preview`.
    pub default_exposure: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            trigger_mode: "ints".into(),
            compression: "bslz4".into(),
            stream_mode: "enabled".into(),
            stream_format: "cbor".into(),
            auto_initialize: true,
            trigger_blocking: true,
            frame_time_policy: FrameTimePolicy::Readout,
            stream_header_detail: None,
            header_appendix: None,
            monitor_max_hz: 10.0,
            default_exposure: 0.1,
        }
    }
}

/// The last successful `set_config` (DESIGN §3.2 `last_applied` [R16]); mapped values [G2-B] and the
/// **effective** `frame_time`, never `None` [G1-M].
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedConfig {
    pub nimages: u64,
    pub ntrigger: u64,
    pub exposure: f64,
    pub frame_time: f64,
    /// The `frame_time` the caller passed (`None`/0 = omitted).
    pub requested_frame_time: Option<f64>,
    pub trigger_mode: String,
    pub compression: String,
}

/// `ensure_config` cache key (PROTOCOL §3.7; mapped values [G2-B]).
#[derive(Debug, Clone, PartialEq)]
pub struct EnsureKey {
    pub nimages: u64,
    pub ntrigger: u64,
    pub exposure: f64,
    pub frame_time: Option<f64>,
    pub trigger_mode: String,
    pub compression: String,
}

/// Hooks into state owned by `dectris_core::driver` / `dectris_stream`. Called on the caller's
/// (detached) thread with no lock of this crate held; implementations must not attach to Python,
/// must not log through Python, and must not call back into the [`Detector`].
/// `on_ownership_cleared` also runs on the shutdown path (`release_connection` → `disarm()`).
pub trait DriverHooks: Send + Sync {
    /// `arm()` step 3, after the epoch bump and the flag/TriggerBook reset, before the arm PUT:
    /// clear `start_cache`, `own_start_seen`; `armed_series_id = unknown`; `driver_armed = true`.
    fn on_arm_reset(&self, _epoch: u64) {}
    /// `arm()` step 5/6, after a successful arm PUT, with the parsed sequence id (`None` = absent).
    fn on_armed(&self, _epoch: u64, _sequence_id: Option<u64>) {}
    /// Series ownership cleared (R7): `disarm()`, `abort()`, `initialize()`, failed arm.
    fn on_ownership_cleared(&self) {}
}

/// No-op hooks (tests, standalone use).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;
impl DriverHooks for NoHooks {}

/// Who called `trigger()` (DESIGN §3.4 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerCaller {
    /// Public `trigger()` (cRED, GUI button). Blocking unless `trigger_blocking` is false.
    Public,
    /// `MovieIter`, `acquire_single_frame`, `_acquire_image`, `arm_for_preview`: non-blocking.
    Internal,
    /// `_acquire_image`: an internal trigger that blocks like a public one (DESIGN §4.2).
    InternalBlocking,
}

/// Arguments of `trigger()`.
#[derive(Debug, Clone, Copy)]
pub struct TriggerRequest {
    pub caller: TriggerCaller,
    /// Global timeout of the PUT; `None` = the §3.4 budget.
    pub timeout: Option<Duration>,
    /// `{"value": count_time}` body (inte); `None` = no body.
    pub count_time: Option<f64>,
}

impl TriggerRequest {
    pub fn public(timeout: Option<Duration>, count_time: Option<f64>) -> Self {
        TriggerRequest {
            caller: TriggerCaller::Public,
            timeout,
            count_time,
        }
    }

    pub fn internal(count_time: Option<f64>) -> Self {
        TriggerRequest {
            caller: TriggerCaller::Internal,
            timeout: None,
            count_time,
        }
    }
}

/// What `trigger()` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerResult {
    /// The PUT completed with HTTP 200 (blocking callers).
    Completed,
    /// A worker was spawned and the caller returned at once (non-blocking callers).
    Spawned,
    /// No PUT: the series was already triggered (ints rules, step 2).
    Skipped,
    /// No PUT: `cancel_requested`/`shutting_down` was set (step 1, `triggers_skipped`).
    Suppressed,
    /// The caller was released early by cancel/disarm/abort/shutdown/end (`trigger_unblocked`).
    Released,
}

/// Result of `arm()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmOutcome {
    pub epoch: u64,
    pub sequence_id: Option<u64>,
    /// An implicit initialize ran in `ensure_idle` [R16].
    pub reinitialized: bool,
}

/// Result of the constructor's startup checks (DESIGN §2.2 `_connect` steps 2-3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartupReport {
    /// `dcu_reachable`.
    pub reachable: bool,
    pub state: Option<String>,
    pub trigger_mode: Option<String>,
    pub pixels: Option<(i64, i64)>,
    pub stream_enabled: bool,
}

/// Counters for `StreamStats` (DESIGN §2.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimplonStats {
    pub trigger_unblocked: u64,
    pub triggers_skipped: u64,
}

#[derive(Debug, Default)]
struct LastValues {
    hv: Option<String>,
    stream_mode: Option<String>,
    countrate_correction: Option<bool>,
}

#[derive(Debug, Clone)]
struct ArmedBy {
    thread: ThreadId,
    name: String,
}

#[derive(Debug, Default)]
struct TriggerBook {
    epoch: u64,
    issued: u64,
    inflight: Option<Arc<Outcome>>,
}

const HA_UNKNOWN: u8 = 0;

/// Builder for [`Detector`].
pub struct DetectorBuilder {
    endpoint: String,
    user_agent: String,
    budgets: Budgets,
    signals: Option<Arc<Signals>>,
    pool: Option<Arc<WorkerPool>>,
    hooks: Option<Arc<dyn DriverHooks>>,
    decoder: Option<Arc<dyn MonitorDecoder>>,
    settings: Settings,
}

impl DetectorBuilder {
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn budgets(mut self, b: Budgets) -> Self {
        self.budgets = b;
        self
    }

    /// The shared `trig_cv` + flags (default: a fresh one).
    pub fn signals(mut self, s: Arc<Signals>) -> Self {
        self.signals = Some(s);
        self
    }

    /// The trigger worker pool (default: the process-wide pool, ceiling 8).
    pub fn pool(mut self, p: Arc<WorkerPool>) -> Self {
        self.pool = Some(p);
        self
    }

    pub fn hooks(mut self, h: Arc<dyn DriverHooks>) -> Self {
        self.hooks = Some(h);
        self
    }

    /// The monitor TIFF decoder (default: [`TiffCrateDecoder`]).
    pub fn decoder(mut self, d: Arc<dyn MonitorDecoder>) -> Self {
        self.decoder = Some(d);
        self
    }

    pub fn settings(mut self, s: Settings) -> Self {
        self.settings = s;
        self
    }

    /// Build (no I/O).
    pub fn build(self) -> Result<Detector> {
        let client = Arc::new(Client::new(&self.endpoint, &self.user_agent, self.budgets)?);
        Ok(Detector {
            client,
            signals: self.signals.unwrap_or_default(),
            pool: self.pool.unwrap_or_else(WorkerPool::global),
            hooks: self
                .hooks
                .unwrap_or_else(|| Arc::new(NoHooks) as Arc<dyn DriverHooks>),
            decoder: self.decoder.unwrap_or_else(|| Arc::new(TiffCrateDecoder)),
            settings: Mutex::new(self.settings),
            last_applied: Mutex::new(None),
            ensure_cache: Mutex::new(None),
            epoch: AtomicU64::new(0),
            triggers: Mutex::new(TriggerBook::default()),
            orphans: Mutex::new(Vec::new()),
            init_slot: Mutex::new(None),
            last_values: Mutex::new(LastValues::default()),
            header_appendix_form: AtomicU8::new(HA_UNKNOWN),
            armed_by: Mutex::new(None),
            preview_armed: AtomicBool::new(false),
            filewriter_on: AtomicBool::new(false),
            filewriter_expect_data: AtomicBool::new(false),
            gate_warned: AtomicBool::new(false),
            monitor: Mutex::new(MonitorCache::default()),
            trigger_unblocked: AtomicU64::new(0),
            triggers_skipped: AtomicU64::new(0),
        })
    }
}

/// The SIMPLON state-machine policy object. Share it through `Arc`.
pub struct Detector {
    pub(crate) client: Arc<Client>,
    pub(crate) signals: Arc<Signals>,
    pool: Arc<WorkerPool>,
    hooks: Arc<dyn DriverHooks>,
    pub(crate) decoder: Arc<dyn MonitorDecoder>,
    settings: Mutex<Settings>,
    last_applied: Mutex<Option<AppliedConfig>>,
    ensure_cache: Mutex<Option<EnsureKey>>,
    epoch: AtomicU64,
    triggers: Mutex<TriggerBook>,
    orphans: Mutex<Vec<Arc<Outcome>>>,
    init_slot: Mutex<Option<Arc<Outcome>>>,
    last_values: Mutex<LastValues>,
    pub(crate) header_appendix_form: AtomicU8,
    armed_by: Mutex<Option<ArmedBy>>,
    preview_armed: AtomicBool,
    /// `configure_filewriter(mode='enabled')` until `disable_filewriter`: `disarm` closes the files.
    pub(crate) filewriter_on: AtomicBool,
    /// This series should leave data files (`nimages_per_file > 0`), until `clear_filewriter`.
    pub(crate) filewriter_expect_data: AtomicBool,
    gate_warned: AtomicBool,
    pub(crate) monitor: Mutex<MonitorCache>,
    trigger_unblocked: AtomicU64,
    triggers_skipped: AtomicU64,
}

impl std::fmt::Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Detector")
            .field("endpoint", &self.client.base())
            .field("epoch", &self.epoch())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// construction, accessors, small helpers
// ---------------------------------------------------------------------------------------------

/// Mutexes [`Detector::test_hold_lock`] can hold (DESIGN §8 [R2]).
pub const TEST_LOCKS: &[&str] = &[
    "settings",
    "last_applied",
    "ensure_cache",
    "triggers",
    "orphans",
    "init_slot",
    "last_values",
    "armed_by",
    "monitor",
];

fn hold_for<T>(m: &Mutex<T>, dur: Duration) {
    let _g = lock_unpoisoned(m);
    std::thread::sleep(dur);
}

impl Detector {
    /// Test hook (DESIGN §8 [R2]): hold the named mutex for `dur` on the calling (detached) thread.
    /// `false` for a name not in [`TEST_LOCKS`].
    #[doc(hidden)]
    pub fn test_hold_lock(&self, name: &str, dur: Duration) -> bool {
        match name {
            "settings" => hold_for(&self.settings, dur),
            "last_applied" => hold_for(&self.last_applied, dur),
            "ensure_cache" => hold_for(&self.ensure_cache, dur),
            "triggers" => hold_for(&self.triggers, dur),
            "orphans" => hold_for(&self.orphans, dur),
            "init_slot" => hold_for(&self.init_slot, dur),
            "last_values" => hold_for(&self.last_values, dur),
            "armed_by" => hold_for(&self.armed_by, dur),
            "monitor" => hold_for(&self.monitor, dur),
            _ => return false,
        }
        true
    }

    /// Start a builder for `DCU_endpoint` (`http://host:port`).
    pub fn builder(endpoint: impl Into<String>) -> DetectorBuilder {
        DetectorBuilder {
            endpoint: endpoint.into(),
            user_agent: "instamatic-dcx7".into(),
            budgets: Budgets::spec(),
            signals: None,
            pool: None,
            hooks: None,
            decoder: None,
            settings: Settings::default(),
        }
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    pub fn signals(&self) -> &Arc<Signals> {
        &self.signals
    }

    pub fn budgets(&self) -> &Budgets {
        self.client.budgets()
    }

    pub fn pool(&self) -> &Arc<WorkerPool> {
        &self.pool
    }

    /// Current epoch (bumped by `arm()`).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Snapshot of the settings.
    pub fn settings(&self) -> Settings {
        lock_unpoisoned(&self.settings).clone()
    }

    /// Mutate settings (local only). Use the dedicated setters for `trigger_mode`/`compression`,
    /// which also invalidate the ensure cache.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        f(&mut lock_unpoisoned(&self.settings));
    }

    /// The `trigger_mode` property setter: local only, any string; invalidates the ensure cache.
    pub fn set_trigger_mode_local(&self, mode: &str) {
        lock_unpoisoned(&self.settings).trigger_mode = mode.to_owned();
        self.invalidate_ensure_cache();
    }

    /// The `compression` property setter: local only, any string; invalidates the ensure cache.
    pub fn set_compression_local(&self, alg: &str) {
        lock_unpoisoned(&self.settings).compression = alg.to_owned();
        self.invalidate_ensure_cache();
    }

    /// `last_applied` (DESIGN §3.2) — used by §4.3's window and §3.4's trigger budget.
    pub fn last_applied(&self) -> Option<AppliedConfig> {
        lock_unpoisoned(&self.last_applied).clone()
    }

    pub fn invalidate_ensure_cache(&self) {
        *lock_unpoisoned(&self.ensure_cache) = None;
    }

    /// The cached `ensure_config` key (`_last_config`, private).
    pub fn ensure_cache_key(&self) -> Option<EnsureKey> {
        lock_unpoisoned(&self.ensure_cache).clone()
    }

    pub fn stats(&self) -> SimplonStats {
        SimplonStats {
            trigger_unblocked: self.trigger_unblocked.load(Ordering::Relaxed),
            triggers_skipped: self.triggers_skipped.load(Ordering::Relaxed),
        }
    }

    /// True while the driver's own initialize PUT is outstanding [G1-H].
    pub fn init_in_flight(&self) -> bool {
        self.client.init_gate_closed()
    }

    /// `request_stop()` / `_begin_shutdown()`: set `shutting_down` and notify; no HTTP, no logging.
    pub fn request_stop(&self) {
        self.signals.begin_shutdown();
    }

    pub(crate) fn det(&self, task: Task, key: &str) -> String {
        self.client.url(Module::Detector, task, key)
    }

    fn check_shutdown(&self) -> Result<()> {
        if self.signals.shutting_down() {
            Err(DriverError::Shutdown)
        } else {
            Ok(())
        }
    }

    /// Log late outcomes of orphan trigger workers (Ok → DEBUG, Err → WARNING) (DESIGN §3.4 step 4).
    pub fn drain_late_outcomes(&self) {
        let done: Vec<Arc<Outcome>> = {
            let mut g = lock_unpoisoned(&self.orphans);
            let (done, keep): (Vec<_>, Vec<_>) = g.drain(..).partition(|o| o.is_done());
            *g = keep;
            done
        };
        for o in done {
            match o.take() {
                Some(Ok(_)) => dlog!(Debug, "late trigger outcome (epoch {}): ok", o.epoch()),
                Some(Err(e)) => dlog!(Warn, "late trigger outcome (epoch {}): {e}", o.epoch()),
                None => {}
            }
        }
    }

    /// Number of trigger outcomes not yet drained (tests).
    pub fn orphan_count(&self) -> usize {
        lock_unpoisoned(&self.orphans).len()
    }

    // ---- series ownership (R7) ----

    fn set_series_owner_current(&self) {
        let t = std::thread::current();
        let a = ArmedBy {
            thread: t.id(),
            name: t.name().unwrap_or("<unnamed>").to_owned(),
        };
        *lock_unpoisoned(&self.armed_by) = Some(a);
    }

    /// Clear `armed_by` and tell the driver (disarm/abort/initialize/armed-series end, R7).
    pub fn clear_series_owner(&self) {
        *lock_unpoisoned(&self.armed_by) = None;
        self.hooks.on_ownership_cleared();
    }

    /// Clear `armed_by` only while the series of `epoch` is still the current one. The epoch is
    /// read under the `armed_by` lock: an `arm()` that already bumped the epoch is never cleared
    /// by a late end of the previous series (review #6). Returns whether it cleared.
    pub fn clear_series_owner_for_epoch(&self, epoch: u64) -> bool {
        {
            let mut g = lock_unpoisoned(&self.armed_by);
            if self.epoch() != epoch {
                return false;
            }
            *g = None;
        }
        self.hooks.on_ownership_cleared();
        true
    }

    /// Clear `armed_by` only if `thread` armed the current series (a dropped `MovieIter`, no I/O).
    pub fn clear_series_owner_of(&self, thread: ThreadId) -> bool {
        {
            let mut g = lock_unpoisoned(&self.armed_by);
            if g.as_ref().is_none_or(|a| a.thread != thread) {
                return false;
            }
            *g = None;
        }
        self.hooks.on_ownership_cleared();
        true
    }

    /// `(thread id, thread name)` of the thread that armed the current series, if not cleared.
    pub fn series_owner(&self) -> Option<(ThreadId, String)> {
        lock_unpoisoned(&self.armed_by)
            .as_ref()
            .map(|a| (a.thread, a.name.clone()))
    }

    /// Name of the arming thread if it is **another** thread and ownership was not cleared since
    /// (the `armed_by` half of the R7 check; the driver adds `driver_armed` and the registry).
    pub fn armed_by_other_thread(&self) -> Option<String> {
        let me = std::thread::current().id();
        lock_unpoisoned(&self.armed_by)
            .as_ref()
            .filter(|a| a.thread != me)
            .map(|a| a.name.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// state reads, getters, idle wait
// ---------------------------------------------------------------------------------------------

impl Detector {
    /// Policy GET of `status/state` (real I/O, no back-off): `'unknown'` on any failure.
    pub(crate) fn state_internal(&self) -> String {
        match self
            .client
            .get_string(&self.det(Task::Status, "state"), self.budgets().get_slow)
        {
            Ok(s) => s,
            Err(e) => {
                dlog!(Debug, "state read failed: {e}");
                "unknown".into()
            }
        }
    }

    /// `get_detector_state()`: one GET (1 s); back-off → `'unknown'` without I/O; initialize in
    /// flight → `'initialize'` without I/O [G1-H]; failure → DEBUG + `'unknown'`.
    pub fn get_detector_state(&self) -> String {
        if self.init_in_flight() {
            return "initialize".into();
        }
        if self.client.in_backoff() {
            return "unknown".into();
        }
        match self
            .client
            .get_string(&self.det(Task::Status, "state"), self.budgets().poll_get)
        {
            Ok(s) => s,
            Err(e) => {
                dlog!(Debug, "get_detector_state failed: {e}");
                "unknown".into()
            }
        }
    }

    /// `get_hv_state()`: `NA/OFF/RAMPING/READY` or `'unknown'`; same rules as the state getter
    /// (initialize in flight → last good value).
    pub fn get_hv_state(&self) -> String {
        if self.init_in_flight() {
            return lock_unpoisoned(&self.last_values)
                .hv
                .clone()
                .unwrap_or_else(|| "unknown".into());
        }
        if self.client.in_backoff() {
            return "unknown".into();
        }
        match self.client.get_string(
            &self.det(Task::Status, "high_voltage/state"),
            self.budgets().poll_get,
        ) {
            Ok(s) => {
                lock_unpoisoned(&self.last_values).hv = Some(s.clone());
                s
            }
            Err(e) => {
                dlog!(Debug, "get_hv_state failed: {e}");
                "unknown".into()
            }
        }
    }

    /// `get_countrate_correction()`: `Some(bool)` or `None`; same rules as the state getter.
    pub fn get_countrate_correction(&self) -> Option<bool> {
        if self.init_in_flight() {
            return lock_unpoisoned(&self.last_values).countrate_correction;
        }
        if self.client.in_backoff() {
            return None;
        }
        match self.client.get_bool(
            &self.det(Task::Config, "countrate_correction_applied"),
            self.budgets().poll_get,
        ) {
            Ok(b) => {
                lock_unpoisoned(&self.last_values).countrate_correction = Some(b);
                Some(b)
            }
            Err(e) => {
                dlog!(Debug, "get_countrate_correction failed: {e}");
                None
            }
        }
    }

    pub(crate) fn remember_stream_mode(&self, mode: Option<String>) -> Option<String> {
        let mut g = lock_unpoisoned(&self.last_values);
        match mode {
            Some(m) => {
                g.stream_mode = Some(m);
                None
            }
            None => g.stream_mode.clone(),
        }
    }

    /// `set_countrate_correction(enabled)`: best-effort PUT (5 s) then GET `bit_depth_image`;
    /// never raises (PROTOCOL §6.1).
    pub fn set_countrate_correction(&self, enabled: bool) {
        dlog!(Info, "Setting countrate_correction_applied to {enabled}");
        match self.client.put_config(
            &self.det(Task::Config, "countrate_correction_applied"),
            json!(enabled),
            self.budgets().put,
        ) {
            Ok(_) => {
                let bd = self.get_bit_depth();
                dlog!(
                    Info,
                    "countrate_correction_applied={enabled}; bit_depth_image={bd}"
                );
            }
            Err(e) => dlog!(
                Warn,
                "Failed to set countrate_correction_applied={enabled}: {e}"
            ),
        }
    }

    /// `get_bit_depth()` [#12 keep]: GET `bit_depth_image` (3 s); `0` on any error (DEBUG).
    pub fn get_bit_depth(&self) -> i64 {
        match self.client.get_i64(
            &self.det(Task::Config, "bit_depth_image"),
            self.budgets().get,
        ) {
            Ok(v) => v,
            Err(e) => {
                dlog!(Debug, "get_bit_depth failed: {e}");
                0
            }
        }
    }

    /// `hv_enabled(state)`: PUT `command/hv_enabled {"value": bool}` (5 s); raises (the GUI reverts
    /// its checkbox).
    pub fn hv_enabled(&self, state: bool) -> Result<()> {
        dlog!(
            Info,
            "Setting high voltage {}",
            if state { "on" } else { "off" }
        );
        self.client
            .put_command(
                &self.det(Task::Command, "hv_enabled"),
                Some(json!({ "value": state })),
                self.budgets().put,
            )
            .map(|_| ())
            .inspect_err(|e| dlog!(Error, "Failed to set hv_enabled={state}: {e}"))
    }

    /// `gate(enable)`: WARNING once, no HTTP (DESIGN §4.2, PROTOCOL §4.2 exte).
    pub fn gate(&self, _enable: bool) {
        if !self.gate_warned.swap(true, Ordering::SeqCst) {
            dlog!(
                Warn,
                "exte requires an electrical enable signal on EXT IN; gate() has no hardware effect"
            );
        }
    }

    /// Idle wait: poll `status/state` every `poll` until `idle`, `timeout` or shutdown. WARNING on
    /// timeout; returns the last observed state; `Err(Shutdown)` on shutdown.
    pub(crate) fn idle_wait(&self, timeout: Duration, poll: Duration) -> Result<String> {
        let poll = poll.max(Duration::from_millis(10));
        let deadline = Instant::now() + timeout;
        loop {
            self.check_shutdown()?;
            let st = self.state_internal();
            if st == "idle" {
                return Ok(st);
            }
            dlog!(Debug, "Waiting for idle, current state: {st}");
            let now = Instant::now();
            if now >= deadline {
                dlog!(
                    Warn,
                    "wait for idle timed out after {:.1} s (state {st})",
                    timeout.as_secs_f64()
                );
                return Ok(st);
            }
            self.signals.sleep(poll.min(deadline - now))?;
        }
    }

    /// `wait_for_idle(timeout=10.0, poll_interval=0.2)` [#12 keep]: never raises; returns the last
    /// observed state (`'unknown'` if interrupted by shutdown).
    pub fn wait_for_idle(&self, timeout: Duration, poll_interval: Duration) -> String {
        match self.idle_wait(timeout, poll_interval) {
            Ok(s) => s,
            Err(_) => "unknown".into(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// ensure_idle, initialize
// ---------------------------------------------------------------------------------------------

impl Detector {
    /// `ensure_idle()` (DESIGN §4.2 [G2-J]): returns `reinitialized` [R16]. Errors: `Shutdown`,
    /// `DetectorStateError` (auto_initialize off, or an initialize that never finishes).
    pub fn ensure_idle(&self) -> Result<bool> {
        self.check_shutdown()?;
        if self.init_in_flight() {
            dlog!(Info, "ensure_idle: initialize in flight; waiting for it");
            return self.initialized_or_state_error();
        }
        let st = self.state_internal();
        self.ensure_idle_from(st, true, 0)
    }

    fn ensure_idle_from(&self, state: String, allow_disarm: bool, depth: u8) -> Result<bool> {
        self.check_shutdown()?;
        let b = self.budgets().clone();
        match state.as_str() {
            "idle" => {
                if depth == 0 {
                    dlog!(Debug, "Detector already idle");
                }
                Ok(false)
            }
            "ready" | "acquire" if allow_disarm => {
                dlog!(Info, "ensure_idle: state {state}; disarming");
                self.signals.request_cancel();
                self.cancel_before_disarm(&state);
                let r = self
                    .client
                    .put_command(&self.det(Task::Command, "disarm"), None, b.arm);
                self.clear_series_owner();
                match r {
                    Err(e) => {
                        dlog!(Warn, "ensure_idle: disarm failed ({e}); initializing");
                        self.recover_by_initialize(&state)
                    }
                    Ok(_) => {
                        let st2 = self.state_internal();
                        if st2 == "idle" {
                            Ok(false)
                        } else if depth < 3 && matches!(st2.as_str(), "configure" | "initialize") {
                            self.ensure_idle_from(st2, false, depth + 1)
                        } else {
                            dlog!(Warn, "ensure_idle: state {st2} after disarm; initializing");
                            self.recover_by_initialize(&st2)
                        }
                    }
                }
            }
            "initialize" => {
                // An initialize not issued by this driver is running: wait for it, never issue a
                // second one (vendor: blocking operation, PROTOCOL §2.4) [G2-J].
                dlog!(Info, "ensure_idle: detector initializing; waiting for idle");
                let st2 = self.idle_wait(b.init_put, b.idle_poll)?;
                match st2.as_str() {
                    "idle" => {
                        // the detector was re-initialized: stream/monitor reset, config lost
                        self.after_initialize();
                        Ok(true)
                    }
                    "initialize" => Err(DriverError::State {
                        state: st2,
                        needed: "idle (initialize did not finish within its budget)".into(),
                    }),
                    _ if depth < 3 => self.ensure_idle_from(st2, allow_disarm, depth + 1),
                    _ => self.recover_by_initialize(&st2),
                }
            }
            "configure" => {
                let st2 = self.idle_wait(b.transient_wait, b.idle_poll)?;
                if st2 == "idle" {
                    Ok(false)
                } else {
                    self.recover_by_initialize(&st2)
                }
            }
            _ => self.recover_by_initialize(&state),
        }
    }

    /// From `acquire`, PUT `cancel` (15 s): a DCU may finish a running series before answering
    /// `disarm`. A failure is only logged: the `disarm` that follows decides.
    fn cancel_before_disarm(&self, state: &str) {
        if state == "acquire" {
            if let Err(e) = self.client.put_command(
                &self.det(Task::Command, "cancel"),
                None,
                self.budgets().abort,
            ) {
                dlog!(Error, "Failed to cancel before disarm: {e}");
            }
        }
    }

    fn recover_by_initialize(&self, state: &str) -> Result<bool> {
        if !self.settings().auto_initialize {
            return Err(DriverError::State {
                state: state.to_owned(),
                needed: "idle (auto_initialize is off)".into(),
            });
        }
        dlog!(Info, "ensure_idle: state {state}; initializing");
        self.initialized_or_state_error()
    }

    /// `initialize_inner()` for `ensure_idle`: a failed initialize PUT is a `DetectorStateError`,
    /// not "reinitialized" (review #8), so the caller never re-applies config to a broken DCU.
    fn initialized_or_state_error(&self) -> Result<bool> {
        if self.initialize_inner()? {
            return Ok(true);
        }
        Err(DriverError::State {
            state: self.state_internal(),
            needed: "idle (initialize failed)".into(),
        })
    }

    /// Post-initialize steps (DESIGN §4.2 initialize row): `enable_stream_v2()` + internal monitor
    /// enable (initialize may reset both, PROTOCOL §15 #32) → invalidate the ensure cache (`last_applied` is
    /// kept) → clear series ownership.
    fn after_initialize(&self) {
        self.enable_stream_v2();
        self.enable_monitor();
        self.invalidate_ensure_cache();
        self.clear_series_owner();
    }

    /// The initialize procedure (DESIGN §4.2 [G2-A]): one PUT (130 s) on a Python-free worker; the
    /// caller waits only for the outcome or `shutting_down`. While the PUT is outstanding no other
    /// request leaves this driver (init gate). A second caller joins the running initialize.
    /// Returns `Ok(true)` after a successful initialize + post steps, `Ok(false)` if the PUT
    /// failed (ERROR logged); `Err` only for shutdown, an interrupt, or a spawn failure.
    pub(crate) fn initialize_inner(&self) -> Result<bool> {
        self.check_shutdown()?;
        let b = self.budgets().clone();
        let (outcome, owner) = {
            let mut g = lock_unpoisoned(&self.init_slot);
            match g.as_ref() {
                Some(o) if !o.is_done() => (Arc::clone(o), false),
                _ => {
                    let o = Outcome::new(WorkerKind::Initialize, self.epoch());
                    *g = Some(Arc::clone(&o));
                    (o, true)
                }
            }
        };
        if owner {
            dlog!(
                Info,
                "Initializing detector (PUT command/initialize, up to {:.0} s)...",
                b.init_put.as_secs_f64()
            );
            self.client.set_init_gate(true);
            let mut req = Request::put(self.det(Task::Command, "initialize"), None, b.init_put);
            req.bypass_gate = true;
            let gate_client = Arc::clone(&self.client);
            if let Err(e) = spawn_worker(
                Arc::clone(&self.client),
                req,
                Arc::clone(&outcome),
                Arc::clone(&self.signals),
                None,
                Some(Box::new(move || gate_client.set_init_gate(false))),
            ) {
                self.client.set_init_gate(false);
                outcome.store(Err(DriverError::Internal(e.to_string())));
                self.signals.notify();
                dlog!(Error, "Failed to initialize: {e}");
                return Err(e);
            }
        } else {
            dlog!(Info, "initialize already in flight; waiting for it");
        }
        let sig = Arc::clone(&self.signals);
        let o2 = Arc::clone(&outcome);
        match self
            .signals
            .wait_until(None, &move || o2.is_done() || sig.shutting_down())
        {
            WaitEnd::Interrupted(e) => return Err(e),
            WaitEnd::Released | WaitEnd::Deadline => {}
        }
        if !outcome.is_done() {
            return Err(DriverError::Shutdown);
        }
        if !outcome.succeeded() {
            match (owner, outcome.take()) {
                (true, Some(Err(e))) => dlog!(Error, "Failed to initialize: {e}"),
                _ => dlog!(
                    Warn,
                    "initialize failed (see the initializing thread's log)"
                ),
            }
            return Ok(false);
        }
        if owner {
            let _ = outcome.take();
        }
        let st = self.idle_wait(b.init_idle_wait, b.idle_poll)?;
        self.after_initialize();
        dlog!(Info, "Detector initialized (state: {st})");
        Ok(true)
    }

    /// Public `initialize()`: never raises for DCU failures (logged); raises only `Shutdown`, an
    /// interrupt or a worker spawn failure. Clears series ownership.
    pub fn initialize(&self) -> Result<()> {
        self.drain_late_outcomes();
        self.initialize_inner().map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------------
// disarm, abort, cancel, arm
// ---------------------------------------------------------------------------------------------

impl Detector {
    fn wait_init_outcome(&self, timeout: Duration) {
        let o = lock_unpoisoned(&self.init_slot).clone();
        if let Some(o) = o {
            let sig = Arc::clone(&self.signals);
            let _ = self
                .signals
                .wait_until(Some(Instant::now() + timeout), &move || {
                    o.is_done() || sig.shutting_down()
                });
        }
    }

    /// `disarm()` (DESIGN §4.2): sets `cancel_requested` + notifies **first** [R4], clears series
    /// ownership, then acts on the state: `idle/na` → DEBUG; `error` → WARNING;
    /// `initialize/configure` → wait ≤ 10 s, return without sending; other non-`ready/acquire` →
    /// WARNING; `ready/acquire` → PUT disarm (30 s). Failure → ERROR. Never raises.
    ///
    /// `acquire` → PUT cancel first: a DCU may finish the running series before it answers `disarm`
    /// (hours for a 99999-image series). With the FileWriter on, `idle` also
    /// sends the PUT (spec Table 5.7: `disarm` "writes all data to file"; cRED stops with `cancel`,
    /// so it arrives here idle) and the budget is the download budget, for a writer backlog.
    pub fn disarm(&self) {
        self.signals.request_cancel();
        self.preview_armed.store(false, Ordering::SeqCst);
        self.clear_series_owner();
        let b = self.budgets().clone();
        if self.init_in_flight() {
            dlog!(
                Debug,
                "disarm: initialize in flight; waiting ≤ {:.0} s, no request",
                b.transient_wait.as_secs_f64()
            );
            self.wait_init_outcome(b.transient_wait);
            return;
        }
        let filewriter_on = self.filewriter_on.load(Ordering::SeqCst);
        let budget = if filewriter_on { b.download } else { b.arm };
        let st = self.state_internal();
        match st.as_str() {
            "idle" if filewriter_on => {
                dlog!(Info, "Disarming idle detector to close the FileWriter files...");
                if let Err(e) =
                    self.client
                        .put_command(&self.det(Task::Command, "disarm"), None, budget)
                {
                    dlog!(Warn, "disarm to close the FileWriter files failed: {e}");
                }
            }
            "idle" | "na" => dlog!(Debug, "Detector already {st}, skipping disarm"),
            "error" => dlog!(Warn, "disarm skipped: detector in error state"),
            "initialize" | "configure" => {
                let _ = self.idle_wait(b.transient_wait, b.idle_poll);
            }
            "ready" | "acquire" => {
                self.cancel_before_disarm(&st);
                dlog!(Info, "Disarming detector (state: {st})...");
                if let Err(e) =
                    self.client
                        .put_command(&self.det(Task::Command, "disarm"), None, budget)
                {
                    dlog!(Error, "Failed to disarm: {e}");
                }
            }
            _ => dlog!(Warn, "disarm skipped: detector state {st}"),
        }
    }

    /// `abort()`: `cancel_requested` + notify first; `idle/na/error` → skip, else PUT abort (15 s).
    /// Clears series ownership. Never raises, never re-initialises.
    pub fn abort(&self) {
        self.signals.request_cancel();
        self.clear_series_owner();
        if self.init_in_flight() {
            dlog!(
                Warn,
                "abort skipped: initialize in flight (SIMPLON blocking operation)"
            );
            return;
        }
        let st = self.state_internal();
        match st.as_str() {
            "idle" | "na" | "error" => dlog!(Debug, "abort skipped (state {st})"),
            _ => {
                dlog!(Info, "Aborting detector (state: {st})...");
                if let Err(e) = self.client.put_command(
                    &self.det(Task::Command, "abort"),
                    None,
                    self.budgets().abort,
                ) {
                    dlog!(Error, "Failed to abort: {e}");
                }
            }
        }
    }

    /// `cancel()`: `cancel_requested` + notify first; PUT cancel (15 s) only from `acquire` (RED
    /// calls it unarmed). Never raises, never re-initialises.
    pub fn cancel(&self) {
        self.signals.request_cancel();
        if self.init_in_flight() {
            dlog!(Debug, "cancel skipped: initialize in flight");
            return;
        }
        let st = self.state_internal();
        if st == "acquire" {
            dlog!(Info, "Cancelling acquisition...");
            if let Err(e) = self.client.put_command(
                &self.det(Task::Command, "cancel"),
                None,
                self.budgets().abort,
            ) {
                dlog!(Error, "Failed to cancel: {e}");
            }
        } else {
            dlog!(Debug, "cancel skipped (state {st})");
        }
    }

    /// `arm()` (DESIGN §4.2): 1 `ensure_idle`; 2 re-apply `last_applied` after an implicit initialize
    /// (DESIGN §12 #7); 3 epoch bump + flag/TriggerBook reset + `on_arm_reset`; 4 PUT arm (30 s, raise);
    /// 5 parse `"sequence_id"`/`"sequence id"`; `on_armed`.
    pub fn arm(&self) -> Result<ArmOutcome> {
        self.drain_late_outcomes();
        self.check_shutdown()?;
        dlog!(Info, "Arming detector...");
        let reinit = self.ensure_idle()?;
        if reinit {
            match self.last_applied() {
                Some(cfg) => {
                    let s = self.settings();
                    self.put_core(
                        cfg.nimages,
                        cfg.ntrigger,
                        cfg.exposure,
                        Some(cfg.frame_time),
                    )?;
                    self.put_modes(&s)?;
                    self.enable_stream_v2();
                    dlog!(Info, "detector re-initialized; configuration re-applied");
                }
                None => dlog!(
                    Warn,
                    "detector re-initialized; DCU default configuration in effect"
                ),
            }
            self.invalidate_ensure_cache();
        }
        // step 3
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.signals.clear_for_arm();
        *lock_unpoisoned(&self.triggers) = TriggerBook {
            epoch,
            issued: 0,
            inflight: None,
        };
        self.hooks.on_arm_reset(epoch);
        self.set_series_owner_current();
        // step 4
        let body =
            match self
                .client
                .put_command(&self.det(Task::Command, "arm"), None, self.budgets().arm)
            {
                Ok(b) => b,
                Err(e) => {
                    dlog!(Error, "Failed to arm detector: {e}");
                    self.clear_series_owner();
                    return Err(e);
                }
            };
        // step 5
        let seq = parse_sequence_id(body.as_ref());
        match seq {
            Some(id) => dlog!(Info, "Detector armed (sequence id {id})"),
            None => dlog!(
                Debug,
                "arm response carried no sequence id; attribution falls back to the first start"
            ),
        }
        self.hooks.on_armed(epoch, seq);
        Ok(ArmOutcome {
            epoch,
            sequence_id: seq,
            reinitialized: reinit,
        })
    }
}

/// Parse the arm/disarm body's sequence id: `"sequence_id"` or `"sequence id"`
/// (DESIGN §4.2 arm step 5); a bare number is accepted too.
pub fn parse_sequence_id(body: Option<&Value>) -> Option<u64> {
    let v = body?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let obj = v.as_object()?;
    for key in ["sequence_id", "sequence id"] {
        if let Some(x) = obj.get(key) {
            if let Some(n) = x.as_u64() {
                return Some(n);
            }
            if let Some(n) = x.get("value").and_then(Value::as_u64) {
                return Some(n);
            }
        }
    }
    None
}

/// [G2-B] inte/exte: `nimages > 1 && ntrigger == 1` → `(1, nimages)`; otherwise `(1, ntrigger)`.
/// Other modes unchanged. Returns `(nimages, ntrigger, swapped)`.
pub fn map_enable_mode(kind: ModeKind, nimages: u64, ntrigger: u64) -> (u64, u64, bool) {
    match kind {
        ModeKind::Inte | ModeKind::Exte => {
            if nimages > 1 && ntrigger == 1 {
                (1, nimages, true)
            } else {
                (1, ntrigger, false)
            }
        }
        _ => (nimages, ntrigger, false),
    }
}

// ---------------------------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------------------------

fn fmt_ft(ft: Option<f64>) -> String {
    ft.map_or_else(|| "None".into(), |f| format!("{f}"))
}

impl Detector {
    fn put_cfg_strict(&self, key: &str, value: Value) -> Result<()> {
        self.client
            .put_config(
                &self.det(Task::Config, key),
                value.clone(),
                self.budgets().put,
            )
            .map(|_| ())
            .inspect_err(|e| dlog!(Error, "Failed to set {key}={value}: {e}"))
    }

    /// Step 3: `frame_time` (best-effort [G2-C]) → `count_time` → `frame_time` → `nimages` →
    /// `ntrigger` (strict).
    pub(crate) fn put_core(
        &self,
        nimages: u64,
        ntrigger: u64,
        exposure: f64,
        frame_time: Option<f64>,
    ) -> Result<()> {
        if let Some(ft) = frame_time {
            match self.client.put_config(
                &self.det(Task::Config, "frame_time"),
                json!(ft),
                self.budgets().put,
            ) {
                Ok(_) => {}
                Err(e @ DriverError::Http { .. }) => {
                    dlog!(
                        Warn,
                        "pre-set frame_time rejected (reject model); continuing ({e})"
                    )
                }
                Err(e) => {
                    dlog!(Error, "Failed to set frame_time={ft}: {e}");
                    return Err(e);
                }
            }
        }
        self.put_cfg_strict("count_time", json!(exposure))?;
        if let Some(ft) = frame_time {
            self.put_cfg_strict("frame_time", json!(ft))?;
        }
        self.put_cfg_strict("nimages", json!(nimages))?;
        self.put_cfg_strict("ntrigger", json!(ntrigger))?;
        Ok(())
    }

    /// Steps 4-6: `trigger_mode` (405 → GET + compare), `compression` (bslz4/lz4 only), stream
    /// `header_appendix` (WARNING on failure, [G2-M] retry).
    pub(crate) fn put_modes(&self, s: &Settings) -> Result<()> {
        self.put_405_strict("trigger_mode", &s.trigger_mode)?;
        match s.compression.as_str() {
            "bslz4" | "lz4" => self.put_405_strict("compression", &s.compression)?,
            "none" => dlog!(
                Warn,
                "compression 'none' is not a DCU value; not sent (PROTOCOL §15 #5)"
            ),
            other => dlog!(Warn, "compression {other:?} is not a DCU value; not sent"),
        }
        if let Some(ha) = &s.header_appendix {
            self.put_header_appendix(ha);
        }
        Ok(())
    }

    /// PUT a detector config string; 405 → GET (5 s, raises on failure) + compare (WARNING on
    /// mismatch); other HTTP errors raise (PROTOCOL §6.1).
    fn put_405_strict(&self, key: &str, value: &str) -> Result<()> {
        let url = self.det(Task::Config, key);
        match self
            .client
            .put_config(&url, json!(value), self.budgets().put)
        {
            Ok(_) => Ok(()),
            Err(e) if is_status(&e, 405) => {
                let got = self
                    .client
                    .get_string(&url, self.budgets().get_slow)
                    .inspect_err(|e2| {
                        dlog!(
                            Error,
                            "{key}: PUT returned 405 and the readback failed: {e2}"
                        )
                    })?;
                if got != value {
                    dlog!(
                        Warn,
                        "{key}: PUT returned 405; DCU holds {got:?}, wanted {value:?}"
                    );
                }
                Ok(())
            }
            Err(e) => {
                dlog!(Error, "Failed to set {key}={value}: {e}");
                Err(e)
            }
        }
    }

    /// Best-effort variant (set_trigger_mode / set_compression): 405 → GET + compare, every error
    /// is a WARNING.
    fn put_405_best_effort(&self, key: &str, value: &str) {
        let url = self.det(Task::Config, key);
        match self
            .client
            .put_config(&url, json!(value), self.budgets().put)
        {
            Ok(_) => {}
            Err(e) if is_status(&e, 405) => {
                match self.client.get_string(&url, self.budgets().get_slow) {
                    Ok(got) if got == value => {}
                    Ok(got) => dlog!(
                        Warn,
                        "{key}: PUT returned 405; DCU holds {got:?}, wanted {value:?}"
                    ),
                    Err(e2) => dlog!(
                        Warn,
                        "{key}: PUT returned 405 and the readback failed: {e2}"
                    ),
                }
            }
            Err(e) => dlog!(Warn, "Failed to set {key}={value}: {e}"),
        }
    }

    /// Step 7: GET-back `count_time`/`frame_time` (3 s), WARNING if |Δ| > 1e-6. Returns the
    /// effective `frame_time` (readback, else the PUT value, else `exposure + 0.004`) [G1-M].
    fn readback(&self, exposure: f64, frame_time: Option<f64>) -> f64 {
        let g = self.budgets().get;
        match self
            .client
            .get_f64(&self.det(Task::Config, "count_time"), g)
        {
            Ok(ct) if (ct - exposure).abs() > READBACK_TOL => dlog!(
                Warn,
                "count_time readback {ct} differs from the requested {exposure}"
            ),
            Ok(_) => {}
            Err(e) => dlog!(Warn, "count_time readback failed: {e}"),
        }
        let ft_rb = match self
            .client
            .get_f64(&self.det(Task::Config, "frame_time"), g)
        {
            Ok(ft) => {
                if let Some(want) = frame_time {
                    if (ft - want).abs() > READBACK_TOL {
                        dlog!(
                            Warn,
                            "frame_time readback {ft} differs from the requested {want}"
                        );
                    }
                }
                (ft.is_finite() && ft > 0.0).then_some(ft)
            }
            Err(e) => {
                dlog!(Warn, "frame_time readback failed: {e}");
                None
            }
        };
        ft_rb.or(frame_time).unwrap_or(exposure + FALLBACK_READOUT)
    }

    /// `set_config(nimages, exposure, ntrigger=1, frame_time=None)` (DESIGN §4.2 steps 0-8).
    pub fn set_config(
        &self,
        nimages: u64,
        exposure: f64,
        ntrigger: u64,
        frame_time: Option<f64>,
    ) -> Result<()> {
        self.drain_late_outcomes();
        if !(exposure.is_finite() && exposure > 0.0) {
            return Err(DriverError::InvalidArg(format!(
                "exposure must be a positive number, got {exposure}"
            )));
        }
        if nimages == 0 || ntrigger == 0 {
            return Err(DriverError::InvalidArg(format!(
                "nimages and ntrigger must be >= 1 (got {nimages}, {ntrigger})"
            )));
        }
        if let Some(ft) = frame_time {
            if !ft.is_finite() || ft < 0.0 {
                return Err(DriverError::InvalidArg(format!(
                    "frame_time must be a non-negative number, got {ft}"
                )));
            }
        }
        // step 0 [G1-D]
        self.invalidate_ensure_cache();
        self.check_shutdown()?;
        // step 1
        self.ensure_idle()?;
        let s = self.settings();
        // step 2
        let requested = frame_time.filter(|f| *f > 0.0);
        let ft = match (requested, s.frame_time_policy) {
            (Some(f), _) => Some(f),
            (None, FrameTimePolicy::Readout) => {
                let ro = match self.client.get_f64(
                    &self.det(Task::Config, "detector_readout_time"),
                    self.budgets().get,
                ) {
                    Ok(r) if r.is_finite() && r >= 0.0 => r,
                    Ok(r) => {
                        dlog!(
                            Warn,
                            "detector_readout_time {r} invalid; using {FALLBACK_READOUT} s"
                        );
                        FALLBACK_READOUT
                    }
                    Err(e) => {
                        dlog!(
                            Warn,
                            "detector_readout_time read failed ({e}); using {FALLBACK_READOUT} s"
                        );
                        FALLBACK_READOUT
                    }
                };
                Some(exposure + ro)
            }
            (None, FrameTimePolicy::Detector) => None,
        };
        // step 2a [G2-B]
        let (n, t, swapped) = map_enable_mode(mode_kind(&s.trigger_mode), nimages, ntrigger);
        if swapped {
            dlog!(
                Info,
                "enable mode: nimages→ntrigger (nimages=1, ntrigger={t})"
            );
        }
        dlog!(
            Info,
            "Configuring: nimages={n}, ntrigger={t}, exposure={exposure}, frame_time={}",
            fmt_ft(ft)
        );
        // steps 3-6
        self.put_core(n, t, exposure, ft)?;
        self.put_modes(&s)?;
        // step 7
        let effective = self.readback(exposure, ft);
        // step 8
        *lock_unpoisoned(&self.last_applied) = Some(AppliedConfig {
            nimages: n,
            ntrigger: t,
            exposure,
            frame_time: effective,
            requested_frame_time: requested,
            trigger_mode: s.trigger_mode,
            compression: s.compression,
        });
        Ok(())
    }

    /// `ensure_config(...)`: key `(nimages, ntrigger, exposure, frame_time or None, trigger_mode,
    /// compression)` with mapped values; equal → DEBUG skip; else `set_config` +
    /// `enable_stream_v2()`, cached only if both succeed.
    pub fn ensure_config(
        &self,
        nimages: u64,
        exposure: f64,
        ntrigger: u64,
        frame_time: Option<f64>,
    ) -> Result<()> {
        let s = self.settings();
        let (n, t, _) = map_enable_mode(mode_kind(&s.trigger_mode), nimages, ntrigger);
        let key = EnsureKey {
            nimages: n,
            ntrigger: t,
            exposure,
            frame_time: frame_time.filter(|f| *f != 0.0),
            trigger_mode: s.trigger_mode,
            compression: s.compression,
        };
        let hit = lock_unpoisoned(&self.ensure_cache).as_ref() == Some(&key);
        if hit {
            dlog!(
                Debug,
                "ensure_config: configuration unchanged; skipping write"
            );
            return Ok(());
        }
        self.set_config(nimages, exposure, ntrigger, frame_time)?;
        if self.enable_stream_v2() {
            *lock_unpoisoned(&self.ensure_cache) = Some(key);
        } else {
            dlog!(
                Warn,
                "ensure_config: stream enable failed; configuration not cached"
            );
        }
        Ok(())
    }

    /// `set_trigger_mode(mode)`: validate (`ValueError`), store, invalidate the ensure cache,
    /// best-effort PUT (PROTOCOL §4.4-3).
    pub fn set_trigger_mode(&self, mode: &str) -> Result<()> {
        if !TRIGGER_MODES.contains(&mode) {
            return Err(DriverError::InvalidArg(format!(
                "invalid trigger mode {mode:?}; expected one of ints, inte, exts, exte"
            )));
        }
        self.set_trigger_mode_local(mode);
        dlog!(Info, "Setting trigger_mode to {mode}");
        self.put_405_best_effort("trigger_mode", mode);
        Ok(())
    }

    /// `set_compression(algorithm)` [#12 keep]: validate `{bslz4, lz4, none}` (`ValueError`), store,
    /// invalidate the ensure cache; best-effort PUT only for bslz4/lz4 (`none` → WARNING, local).
    pub fn set_compression(&self, algorithm: &str) -> Result<()> {
        if !COMPRESSIONS.contains(&algorithm) {
            return Err(DriverError::InvalidArg(format!(
                "invalid compression {algorithm:?}; expected one of bslz4, lz4, none"
            )));
        }
        self.set_compression_local(algorithm);
        dlog!(Info, "Setting compression to {algorithm}");
        if algorithm == "none" {
            dlog!(
                Warn,
                "compression 'none' is not a DCU value; kept locally only (PROTOCOL §15 #5)"
            );
        } else {
            self.put_405_best_effort("compression", algorithm);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// trigger (DESIGN §3.4)
// ---------------------------------------------------------------------------------------------

enum Plan {
    Spawn(Arc<Outcome>),
    WaitFor(Arc<Outcome>),
    Skip { warn: bool },
}

impl Detector {
    /// Global timeout of a trigger PUT (DESIGN §3.4 step 3): `timeout` if given, else ints
    /// `nimages×frame_time + 60 s`, inte `frame_time + 60 s`, clamped to [30 s, 24 h]. Unknown
    /// configuration (no `set_config` in this process): ints → 24 h, inte → 60 s.
    pub fn trigger_budget(&self, kind: ModeKind, timeout: Option<Duration>) -> Duration {
        if let Some(t) = timeout {
            return t;
        }
        let b = self.budgets();
        let secs = match (kind, self.last_applied()) {
            (ModeKind::Inte, Some(c)) => c.frame_time + b.trigger_margin.as_secs_f64(),
            (ModeKind::Inte, None) => b.trigger_margin.as_secs_f64(),
            (_, Some(c)) => c.nimages as f64 * c.frame_time + b.trigger_margin.as_secs_f64(),
            (_, None) => b.trigger_max.as_secs_f64(),
        };
        let secs = if secs.is_finite() {
            secs
        } else {
            b.trigger_max.as_secs_f64()
        };
        Duration::from_secs_f64(
            secs.clamp(b.trigger_min.as_secs_f64(), b.trigger_max.as_secs_f64()),
        )
    }

    fn undo_reservation(&self, epoch: u64, o: &Arc<Outcome>) {
        let mut g = lock_unpoisoned(&self.triggers);
        if g.epoch == epoch {
            g.issued = g.issued.saturating_sub(1);
            if g.inflight.as_ref().is_some_and(|x| Arc::ptr_eq(x, o)) {
                g.inflight = None;
            }
        }
    }

    fn trigger_released(&self) -> bool {
        self.signals.cancel_requested() || self.signals.shutting_down() || self.signals.end_seen()
    }

    /// `trigger(timeout=None, count_time=None)` (DESIGN §3.4; DESIGN §12 #6).
    ///
    /// 0. exts/exte → `DetectorStateError`, no HTTP [R23].
    /// 1. `cancel_requested`/`shutting_down` → suppressed (INFO, `triggers_skipped`) [R4].
    /// 2. Epoch book: ints internal/non-blocking → a PUT only if `issued == 0` (or
    ///    `nimages == 1 && issued < ntrigger`); ints public blocking → skip (WARNING) once
    ///    `issued >= ntrigger`; inte → serialised, ≤ 1 in flight per epoch.
    /// 3. Ceiling of 8 live workers → `DectrisBusy`; spawn a Python-free worker.
    /// 4. Blocking callers wait for the outcome, cancel/disarm/abort, shutdown, or the armed
    ///    series' end.
    pub fn trigger(&self, req: TriggerRequest) -> Result<TriggerResult> {
        self.drain_late_outcomes();
        let s = self.settings();
        let kind = mode_kind(&s.trigger_mode);
        // step 0
        if matches!(kind, ModeKind::Exts | ModeKind::Exte) {
            return Err(DriverError::State {
                state: s.trigger_mode.clone(),
                needed: format!("software trigger not allowed in {}", s.trigger_mode),
            });
        }
        if self.init_in_flight() {
            return Err(gate_error());
        }
        let blocking = match req.caller {
            TriggerCaller::Public => s.trigger_blocking,
            TriggerCaller::Internal => false,
            TriggerCaller::InternalBlocking => true,
        };
        let epoch = self.epoch();
        let last = self.last_applied();
        let outcome = loop {
            // step 1 (re-checked after every inte serialisation wait, DESIGN §12 #6)
            if self.signals.cancel_requested() || self.signals.shutting_down() {
                self.triggers_skipped.fetch_add(1, Ordering::Relaxed);
                dlog!(
                    Info,
                    "trigger suppressed: series cancelled/disarmed — re-arm first"
                );
                return Ok(TriggerResult::Suppressed);
            }
            // step 2
            let plan = {
                let mut g = lock_unpoisoned(&self.triggers);
                if epoch < g.epoch {
                    // armed again since this trigger began: it belongs to a finished series
                    drop(g);
                    self.triggers_skipped.fetch_add(1, Ordering::Relaxed);
                    dlog!(
                        Info,
                        "trigger suppressed: the detector was re-armed meanwhile"
                    );
                    return Ok(TriggerResult::Suppressed);
                }
                if g.epoch != epoch {
                    *g = TriggerBook {
                        epoch,
                        issued: 0,
                        inflight: None,
                    };
                }
                let wait_for = match (kind, g.inflight.as_ref()) {
                    (ModeKind::Inte, Some(o)) if !o.is_done() => Some(Arc::clone(o)),
                    _ => None,
                };
                if let Some(o) = wait_for {
                    Plan::WaitFor(o)
                } else {
                    let go = match kind {
                        ModeKind::Inte => true,
                        _ if blocking && req.caller == TriggerCaller::Public => {
                            last.as_ref().is_none_or(|c| g.issued < c.ntrigger)
                        }
                        _ => {
                            g.issued == 0
                                || last
                                    .as_ref()
                                    .is_some_and(|c| c.nimages == 1 && g.issued < c.ntrigger)
                        }
                    };
                    if go {
                        let o = Outcome::new(WorkerKind::Trigger, epoch);
                        g.issued += 1;
                        g.inflight = Some(Arc::clone(&o));
                        Plan::Spawn(o)
                    } else {
                        Plan::Skip {
                            warn: blocking && req.caller == TriggerCaller::Public,
                        }
                    }
                }
            };
            match plan {
                Plan::Spawn(o) => break o,
                Plan::Skip { warn } => {
                    if warn {
                        dlog!(Warn, "trigger skipped: all ntrigger triggers of this series were already issued");
                    } else {
                        dlog!(Debug, "series already triggered");
                    }
                    return Ok(TriggerResult::Skipped);
                }
                Plan::WaitFor(prev) => {
                    let p2 = Arc::clone(&prev);
                    let end = self
                        .signals
                        .wait_until(None, &|| p2.is_done() || self.trigger_released());
                    match end {
                        WaitEnd::Interrupted(e) => return Err(e),
                        _ if prev.is_done() => continue,
                        _ => {
                            self.trigger_unblocked.fetch_add(1, Ordering::Relaxed);
                            dlog!(
                                Info,
                                "trigger released before it was sent (cancel/disarm/end/shutdown)"
                            );
                            return Ok(TriggerResult::Released);
                        }
                    }
                }
            }
        };
        // step 3
        let Some(slot) = self.pool.try_acquire() else {
            self.undo_reservation(epoch, &outcome);
            return Err(DriverError::Busy(
                "too many outstanding trigger requests".into(),
            ));
        };
        let budget = self.trigger_budget(kind, req.timeout);
        let body = req.count_time.map(|ct| json!({ "value": ct }));
        let mut put = Request::put(self.det(Task::Command, "trigger"), body, budget);
        put.connect = Some(self.budgets().trigger_connect);
        if kind == ModeKind::Inte {
            dlog!(
                Debug,
                "Triggering detector (inte, count_time={:?})",
                req.count_time
            );
        } else {
            dlog!(Info, "Triggering detector...");
        }
        if let Err(e) = spawn_worker(
            Arc::clone(&self.client),
            put,
            Arc::clone(&outcome),
            Arc::clone(&self.signals),
            Some(slot),
            None,
        ) {
            self.undo_reservation(epoch, &outcome);
            return Err(e);
        }
        if !blocking {
            lock_unpoisoned(&self.orphans).push(outcome);
            return Ok(TriggerResult::Spawned);
        }
        // step 4
        let deadline =
            Instant::now() + budget + self.budgets().trigger_connect + Duration::from_secs(5);
        let o2 = Arc::clone(&outcome);
        let end = self
            .signals
            .wait_until(Some(deadline), &|| o2.is_done() || self.trigger_released());
        if outcome.is_done() {
            return match outcome.take() {
                Some(Err(e)) => {
                    dlog!(Error, "Trigger failed: {e}");
                    Err(e)
                }
                _ => Ok(TriggerResult::Completed),
            };
        }
        lock_unpoisoned(&self.orphans).push(outcome);
        match end {
            WaitEnd::Interrupted(e) => Err(e),
            WaitEnd::Deadline => Err(DriverError::Timeout(format!(
                "trigger did not complete within {:.0} s",
                budget.as_secs_f64()
            ))),
            WaitEnd::Released => {
                self.trigger_unblocked.fetch_add(1, Ordering::Relaxed);
                dlog!(
                    Info,
                    "trigger released (cancel/disarm/end/shutdown); the PUT finishes on its own"
                );
                Ok(TriggerResult::Released)
            }
        }
    }

    // ---- [#12 keep] preview arming ----

    /// `arm_for_preview()` [#12 keep]: `ensure_config(nimages=99999, exposure=default_exposure)` →
    /// `arm()` → one internal non-blocking trigger in ints/inte; sets `preview_armed`. The R7
    /// ownership check is the driver's (it wraps this call).
    pub fn arm_for_preview(&self) -> Result<()> {
        let s = self.settings();
        self.ensure_config(99_999, s.default_exposure, 1, None)?;
        self.arm()?;
        if matches!(
            mode_kind(&s.trigger_mode),
            ModeKind::Ints | ModeKind::Inte | ModeKind::Other
        ) {
            if let Err(e) = self.trigger(TriggerRequest::internal(None)) {
                // armed but not triggered: disarm (also clears the R7 owner, review #2)
                self.disarm();
                return Err(e);
            }
        }
        self.preview_armed.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// `disarm_preview()` [#12 keep]: guarded `disarm()` only if `preview_armed`, then clear it.
    pub fn disarm_preview(&self) {
        if self.preview_armed.swap(false, Ordering::SeqCst) {
            self.disarm();
        }
    }

    pub fn preview_armed(&self) -> bool {
        self.preview_armed.load(Ordering::SeqCst)
    }

    // ---- constructor checks ----

    /// `_connect` steps 2-3 (DESIGN §2.2), all best-effort: startup probe (3 s); on a transport failure
    /// WARNING + `reachable=false` and nothing else. Otherwise `trigger_mode` VERIFIED/MISMATCH,
    /// `enable_stream_v2()`, `x/y_pixels_in_detector` vs yaml, guarded disarm, monitor enable.
    pub fn connect_checks(&self, yaml_dims: Option<(i64, i64)>) -> StartupReport {
        let base = self.client.base().to_owned();
        dlog!(Info, "DCU_endpoint: {base}");
        let b = self.budgets().clone();
        let mut rep = StartupReport::default();
        let state = match self
            .client
            .get_string(&self.det(Task::Status, "state"), b.probe)
        {
            Ok(s) => s,
            Err(e) if is_transport(&e) => {
                dlog!(
                    Warn,
                    "DCU unreachable at {base} ({e}); skipping startup checks"
                );
                return rep;
            }
            Err(e) => {
                dlog!(Warn, "startup state probe failed: {e}");
                "unknown".into()
            }
        };
        rep.reachable = true;
        rep.state = Some(state.clone());
        let local = self.settings().trigger_mode;
        match self
            .client
            .get_string(&self.det(Task::Config, "trigger_mode"), b.get)
        {
            Ok(m) => {
                if m == local {
                    dlog!(Info, "Trigger mode VERIFIED: {m}");
                } else {
                    dlog!(Warn, "Trigger mode MISMATCH: camera={m}, yaml={local}");
                }
                rep.trigger_mode = Some(m);
            }
            Err(e) => dlog!(Warn, "could not read trigger_mode: {e}"),
        }
        rep.stream_enabled = self.enable_stream_v2();
        let x = self
            .client
            .get_i64(&self.det(Task::Config, "x_pixels_in_detector"), b.get);
        let y = self
            .client
            .get_i64(&self.det(Task::Config, "y_pixels_in_detector"), b.get);
        match (x, y) {
            (Ok(x), Ok(y)) => {
                rep.pixels = Some((x, y));
                if let Some((dx, dy)) = yaml_dims {
                    if (x, y) != (dx, dy) {
                        dlog!(
                            Warn,
                            "detector geometry {x}x{y} differs from yaml dimensions [{dx}, {dy}]"
                        );
                    }
                }
            }
            (Err(e), _) | (_, Err(e)) => dlog!(Debug, "geometry readback failed: {e}"),
        }
        if matches!(state.as_str(), "ready" | "acquire") {
            self.disarm();
        }
        self.enable_monitor();
        rep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_id_parsing() {
        assert_eq!(parse_sequence_id(Some(&json!({"sequence_id": 5}))), Some(5));
        assert_eq!(parse_sequence_id(Some(&json!({"sequence id": 6}))), Some(6));
        assert_eq!(
            parse_sequence_id(Some(&json!({"sequence_id": {"value": 7}}))),
            Some(7)
        );
        assert_eq!(parse_sequence_id(Some(&json!(8))), Some(8));
        assert_eq!(parse_sequence_id(Some(&json!({}))), None);
        assert_eq!(parse_sequence_id(Some(&json!({"sequence_id": -1}))), None);
        assert_eq!(parse_sequence_id(None), None);
    }

    #[test]
    fn enable_mode_mapping() {
        use ModeKind::*;
        assert_eq!(map_enable_mode(Inte, 5, 1), (1, 5, true));
        assert_eq!(map_enable_mode(Exte, 5, 1), (1, 5, true));
        assert_eq!(map_enable_mode(Inte, 1, 99_999), (1, 99_999, false));
        assert_eq!(map_enable_mode(Inte, 3, 4), (1, 4, false));
        assert_eq!(map_enable_mode(Ints, 5, 1), (5, 1, false));
        assert_eq!(map_enable_mode(Exts, 5, 2), (5, 2, false));
    }

    #[test]
    fn mode_kinds_and_policy() {
        assert_eq!(mode_kind("ints"), ModeKind::Ints);
        assert_eq!(mode_kind("exte"), ModeKind::Exte);
        assert_eq!(mode_kind("weird"), ModeKind::Other);
        assert_eq!(
            FrameTimePolicy::parse("readout"),
            Some(FrameTimePolicy::Readout)
        );
        assert_eq!(
            FrameTimePolicy::parse("detector").map(FrameTimePolicy::as_str),
            Some("detector")
        );
        assert_eq!(FrameTimePolicy::parse("x"), None);
    }

    fn det() -> Detector {
        Detector::builder("http://127.0.0.1:9").build().unwrap()
    }

    #[test]
    fn trigger_budget_rules() {
        let d = det();
        let b = Budgets::spec();
        assert_eq!(d.trigger_budget(ModeKind::Ints, None), b.trigger_max);
        assert_eq!(
            d.trigger_budget(ModeKind::Inte, None),
            Duration::from_secs(60)
        );
        assert_eq!(
            d.trigger_budget(ModeKind::Ints, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        *lock_unpoisoned(&d.last_applied) = Some(AppliedConfig {
            nimages: 10,
            ntrigger: 1,
            exposure: 0.5,
            frame_time: 1.0,
            requested_frame_time: None,
            trigger_mode: "ints".into(),
            compression: "bslz4".into(),
        });
        assert_eq!(
            d.trigger_budget(ModeKind::Ints, None),
            Duration::from_secs(70)
        );
        assert_eq!(
            d.trigger_budget(ModeKind::Inte, None),
            Duration::from_secs(61)
        );
        *lock_unpoisoned(&d.last_applied) = Some(AppliedConfig {
            nimages: 1,
            ntrigger: 1,
            exposure: 0.001,
            frame_time: 0.001,
            requested_frame_time: None,
            trigger_mode: "ints".into(),
            compression: "bslz4".into(),
        });
        // 0.001 + 60 → clamped up? no: 60.001 > 30
        assert!(d.trigger_budget(ModeKind::Ints, None) >= Duration::from_secs(60));
        let mut small = Budgets::spec();
        small.trigger_margin = Duration::ZERO;
        let d2 = Detector::builder("http://127.0.0.1:9")
            .budgets(small)
            .build()
            .unwrap();
        assert_eq!(
            d2.trigger_budget(ModeKind::Ints, None),
            Budgets::spec().trigger_max
        );
        *lock_unpoisoned(&d2.last_applied) = d.last_applied();
        assert_eq!(
            d2.trigger_budget(ModeKind::Ints, None),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn local_setters_invalidate_cache() {
        let d = det();
        let key = EnsureKey {
            nimages: 1,
            ntrigger: 1,
            exposure: 0.1,
            frame_time: None,
            trigger_mode: "ints".into(),
            compression: "bslz4".into(),
        };
        *lock_unpoisoned(&d.ensure_cache) = Some(key.clone());
        d.set_trigger_mode_local("whatever");
        assert!(d.ensure_cache_key().is_none());
        assert_eq!(d.settings().trigger_mode, "whatever");
        *lock_unpoisoned(&d.ensure_cache) = Some(key);
        d.set_compression_local("lz4");
        assert!(d.ensure_cache_key().is_none());
    }

    #[test]
    fn invalid_arguments() {
        let d = det();
        assert!(matches!(
            d.set_trigger_mode("bad"),
            Err(DriverError::InvalidArg(_))
        ));
        assert!(matches!(
            d.set_compression("zstd"),
            Err(DriverError::InvalidArg(_))
        ));
        assert!(matches!(
            d.set_config(1, 0.0, 1, None),
            Err(DriverError::InvalidArg(_))
        ));
        assert!(matches!(
            d.set_config(0, 0.1, 1, None),
            Err(DriverError::InvalidArg(_))
        ));
        assert!(matches!(
            d.set_config(1, 0.1, 1, Some(f64::NAN)),
            Err(DriverError::InvalidArg(_))
        ));
    }

    #[test]
    fn detector_is_send_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<Detector>();
    }
}
