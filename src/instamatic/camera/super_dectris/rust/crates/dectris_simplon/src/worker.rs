//! Python-free transient workers `dectris-trigger-<n>` / `dectris-init-<n>`
//! (docs/DESIGN.md §3.1, §3.4 [R3][R4][R23], §4.2 [G2-A]).
//!
//! Each worker:
//! * calls [`dectris_common::log::set_no_python`] as its first statement (it never attaches to
//!   Python, not even through ureq's own `debug!` records, §2.9 [R1]);
//! * executes exactly one `ureq` PUT;
//! * holds only `Arc<Client>` + `Arc<Outcome>` (+ the `Arc<Signals>` it notifies);
//! * is `catch_unwind`-wrapped and stores `DriverError::Internal` on a panic;
//! * stores its outcome, releases its slot, then notifies `trig_cv` (R3a).
//!
//! Trigger workers count against a ceiling of 8 live workers ([`WorkerPool`], process-wide by
//! default, orphans of earlier epochs included) [R3]. The initialize worker is exempt (its own
//! ceiling of 1 is enforced by the detector's init slot) [G2-A].

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dectris_common::sync::lock_unpoisoned;
use dectris_common::{DriverError, Result};
use serde_json::Value;

use crate::client::{Client, Request};
use crate::signals::Signals;

/// Ceiling of live trigger workers (DESIGN §3.4 step 3).
pub const TRIGGER_WORKER_CEILING: usize = 8;

/// What a worker produced: the parsed 200 body (or `None`), or the error.
pub type WorkerResult = Result<Option<Value>>;

/// Worker kind (thread name prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerKind {
    Trigger,
    Initialize,
}

impl WorkerKind {
    pub const fn prefix(self) -> &'static str {
        match self {
            WorkerKind::Trigger => "dectris-trigger",
            WorkerKind::Initialize => "dectris-init",
        }
    }
}

/// The one-shot result cell a worker fills (DESIGN §3.2 `TriggerBook.inflight`).
#[derive(Debug)]
pub struct Outcome {
    epoch: u64,
    kind: WorkerKind,
    done: AtomicBool,
    ok: AtomicBool,
    slot: Mutex<Option<WorkerResult>>,
}

impl Outcome {
    pub fn new(kind: WorkerKind, epoch: u64) -> Arc<Self> {
        Arc::new(Outcome {
            epoch,
            kind,
            done: AtomicBool::new(false),
            ok: AtomicBool::new(false),
            slot: Mutex::new(None),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn kind(&self) -> WorkerKind {
        self.kind
    }

    /// True once the worker stored its result (an atomic: safe inside a `trig_cv` predicate).
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// True if the stored result is `Ok` (meaningful once [`is_done`](Self::is_done)).
    pub fn succeeded(&self) -> bool {
        self.ok.load(Ordering::SeqCst)
    }

    /// Store the result (first store wins).
    pub fn store(&self, r: WorkerResult) {
        let ok = r.is_ok();
        {
            let mut g = lock_unpoisoned(&self.slot);
            if g.is_none() && !self.done.load(Ordering::SeqCst) {
                *g = Some(r);
            } else {
                return;
            }
        }
        self.ok.store(ok, Ordering::SeqCst);
        self.done.store(true, Ordering::SeqCst);
    }

    /// Take the stored result (once). `None` if not done or already taken.
    pub fn take(&self) -> Option<WorkerResult> {
        if !self.is_done() {
            return None;
        }
        lock_unpoisoned(&self.slot).take()
    }
}

/// Counter of live trigger workers with a ceiling [R3].
#[derive(Debug)]
pub struct WorkerPool {
    live: AtomicUsize,
    ceiling: usize,
}

impl WorkerPool {
    pub fn new(ceiling: usize) -> Arc<Self> {
        Arc::new(WorkerPool {
            live: AtomicUsize::new(0),
            ceiling,
        })
    }

    /// The process-wide pool (ceiling 8) shared by every detector instance (DESIGN §3.1).
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<WorkerPool>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| WorkerPool::new(TRIGGER_WORKER_CEILING)))
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    /// Reserve a slot, or `None` at the ceiling.
    pub fn try_acquire(self: &Arc<Self>) -> Option<WorkerSlot> {
        let ceiling = self.ceiling;
        self.live
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < ceiling).then_some(n + 1)
            })
            .ok()
            .map(|_| WorkerSlot {
                pool: Arc::clone(self),
            })
    }
}

/// RAII slot: decrements the pool on drop (also on a panic path).
#[derive(Debug)]
pub struct WorkerSlot {
    pool: Arc<WorkerPool>,
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.pool.live.fetch_sub(1, Ordering::SeqCst);
    }
}

static WORKER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Spawn one Python-free worker that runs `req` and stores the result in `outcome`.
///
/// Order at the end of the worker: store outcome → `on_done` (e.g. re-open the init gate) → drop
/// the slot → notify `trig_cv`. A thread-spawn failure is returned as `DectrisError` and nothing is
/// stored.
pub fn spawn_worker(
    client: Arc<Client>,
    req: Request,
    outcome: Arc<Outcome>,
    signals: Arc<Signals>,
    slot: Option<WorkerSlot>,
    on_done: Option<Box<dyn FnOnce() + Send>>,
) -> Result<()> {
    let n = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}-{n}", outcome.kind().prefix());
    let builder = std::thread::Builder::new().name(name.clone());
    builder
        .spawn(move || {
            dectris_common::log::set_no_python();
            let r = catch_unwind(AssertUnwindSafe(|| {
                client.execute(&req).and_then(|reply| {
                    let reply = crate::client::require_200(reply, &req.url)?;
                    Ok(reply.json())
                })
            }))
            .unwrap_or_else(|p| Err(DriverError::from_panic(&*p)));
            outcome.store(r);
            if let Some(f) = on_done {
                let _ = catch_unwind(AssertUnwindSafe(f));
            }
            drop(slot);
            signals.notify();
        })
        .map(|_| ())
        .map_err(|e| DriverError::Internal(format!("cannot spawn worker thread {name}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_ceiling_and_raii() {
        let p = WorkerPool::new(2);
        let a = p.try_acquire().unwrap();
        let b = p.try_acquire().unwrap();
        assert!(p.try_acquire().is_none());
        assert_eq!(p.live(), 2);
        drop(a);
        assert_eq!(p.live(), 1);
        let c = p.try_acquire().unwrap();
        drop((b, c));
        assert_eq!(p.live(), 0);
    }

    #[test]
    fn outcome_first_store_wins_and_take_once() {
        let o = Outcome::new(WorkerKind::Trigger, 3);
        assert!(!o.is_done());
        assert!(o.take().is_none());
        o.store(Ok(None));
        o.store(Err(DriverError::Shutdown));
        assert!(o.is_done() && o.succeeded());
        assert_eq!(o.epoch(), 3);
        assert!(matches!(o.take(), Some(Ok(None))));
        assert!(o.take().is_none());
    }

    #[test]
    fn global_pool_is_shared() {
        let a = WorkerPool::global();
        let b = WorkerPool::global();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.ceiling(), TRIGGER_WORKER_CEILING);
    }
}
