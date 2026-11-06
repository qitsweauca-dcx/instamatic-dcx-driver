//! Release flags and the `trig_cv` generation protocol (docs/DESIGN.md §3.2 R3/R3a [G1-B],
//! §3.4 step 4, §3.7).
//!
//! [`Signals`] is the reference implementation of the `trig_cv` + flags that DESIGN places in
//! `dectris_core::driver::Inner`. The driver crate owns the instance (`Arc<Signals>`) and hands the
//! same `Arc` to the [`Detector`](crate::detector::Detector) and to the stream receiver, so every
//! releaser and every waiter share one condvar (DESIGN §3.2).
//!
//! **R3a release protocol.** Every releaser — `cancel()`, `disarm()`, `abort()`,
//! `_begin_shutdown()`/`request_stop()`, a worker storing its outcome, the armed-series end — first
//! stores its flag/outcome, then takes the mutex, increments `gen`, drops the guard and calls
//! `notify_all()`. Every waiter reads `seen = gen` under the mutex, then waits with
//! `wait_timeout_while(guard, slice, |s| s.gen == seen && !released())`. A release that lands
//! between the waiter's check and its wait always bumps `gen`, so no wake-up is lost.
//!
//! **Interrupt hook (R4).** On the thread registered with [`Signals::set_interrupt_hook`] (the
//! Python main thread) waits are cut into 250 ms slices and the hook (the py layer's
//! `check_signals`) runs between slices with **no mutex held**. A hook error ends the wait with
//! [`WaitEnd::Interrupted`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use dectris_common::sync::lock_unpoisoned;
use dectris_common::{DriverError, Result};

/// Slice length on the interrupt-hook thread (DESIGN §3.4 step 4).
pub const MAIN_THREAD_SLICE: Duration = Duration::from_millis(250);
/// Longest single condvar wait when no deadline is given (re-armed in a loop).
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// State guarded by the `trig_cv` mutex: only the generation counter (DESIGN §3.2 [G1-B]).
#[derive(Debug, Default)]
pub struct TrigState {
    pub gen: u64,
}

type InterruptFn = dyn Fn() -> Result<()> + Send + Sync;

/// Called between wait slices on one thread (the Python main thread).
#[derive(Clone)]
pub struct InterruptHook {
    pub thread: ThreadId,
    pub check: Arc<InterruptFn>,
}

impl std::fmt::Debug for InterruptHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptHook")
            .field("thread", &self.thread)
            .finish_non_exhaustive()
    }
}

/// How a wait ended.
#[derive(Debug)]
pub enum WaitEnd {
    /// The release predicate became true.
    Released,
    /// The deadline passed first.
    Deadline,
    /// The interrupt hook returned an error (e.g. KeyboardInterrupt on the main thread).
    Interrupted(DriverError),
}

/// Flags + `trig_cv` (DESIGN §3.2: `flags` × {cancel_requested, end_seen, shutting_down}, `trig_cv`).
#[derive(Debug, Default)]
pub struct Signals {
    cancel_requested: AtomicBool,
    end_seen: AtomicBool,
    shutting_down: AtomicBool,
    trig: Mutex<TrigState>,
    cv: Condvar,
    hook: RwLock<Option<InterruptHook>>,
}

impl Signals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// R3a: bump `gen` under the mutex, drop the guard, `notify_all`. Call **after** storing the
    /// flag or outcome that releases the waiters.
    pub fn notify(&self) {
        {
            let mut g = lock_unpoisoned(&self.trig);
            g.gen = g.gen.wrapping_add(1);
        }
        self.cv.notify_all();
    }

    /// Current generation (diagnostics / tests).
    pub fn generation(&self) -> u64 {
        lock_unpoisoned(&self.trig).gen
    }

    /// `cancel()`, `disarm()`, `abort()`: set `cancel_requested`, then notify — before their own
    /// HTTP call [R4].
    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.notify();
    }

    /// The armed series' `end` was seen (set by the stream receiver, §4.3 [R12]).
    pub fn set_end_seen(&self) {
        self.end_seen.store(true, Ordering::SeqCst);
        self.notify();
    }

    /// `_begin_shutdown()` / `request_stop()`: permanent for the instance [G1-S].
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.notify();
    }

    /// `arm()` step 3: clear `cancel_requested` and `end_seen` for the new epoch.
    pub fn clear_for_arm(&self) {
        self.cancel_requested.store(false, Ordering::SeqCst);
        self.end_seen.store(false, Ordering::SeqCst);
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn end_seen(&self) -> bool {
        self.end_seen.load(Ordering::SeqCst)
    }

    pub fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Register the interrupt hook (the py layer: main thread id + `check_signals`).
    pub fn set_interrupt_hook(&self, hook: Option<InterruptHook>) {
        match self.hook.write() {
            Ok(mut g) => *g = hook,
            Err(p) => *p.into_inner() = hook,
        }
    }

    fn hook_for_current_thread(&self) -> Option<Arc<InterruptFn>> {
        let g = match self.hook.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let h = g.as_ref()?;
        (h.thread == std::thread::current().id()).then(|| Arc::clone(&h.check))
    }

    /// Run the interrupt hook if the calling thread is the hook's thread (receive loops call this
    /// between poll slices, with no mutex held). `Err` = the interrupt (review #2).
    pub fn check_interrupt(&self) -> Result<()> {
        match self.hook_for_current_thread() {
            Some(check) => check(),
            None => Ok(()),
        }
    }

    /// Wait until `released()` is true or `deadline` passes (R3a). `released` must only read
    /// atomics: it runs under the `trig_cv` mutex.
    pub fn wait_until(&self, deadline: Option<Instant>, released: &dyn Fn() -> bool) -> WaitEnd {
        let hook = self.hook_for_current_thread();
        loop {
            let slice_end = {
                let g = lock_unpoisoned(&self.trig);
                if released() {
                    return WaitEnd::Released;
                }
                let now = Instant::now();
                let mut dur = match deadline {
                    Some(d) if d <= now => return WaitEnd::Deadline,
                    Some(d) => d - now,
                    None => MAX_WAIT,
                };
                if hook.is_some() {
                    dur = dur.min(MAIN_THREAD_SLICE);
                }
                let seen = g.gen;
                let (g, _) = self
                    .cv
                    .wait_timeout_while(g, dur, |s| s.gen == seen && !released())
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(g);
                released()
            };
            if slice_end {
                return WaitEnd::Released;
            }
            if let Some(check) = &hook {
                // R4: no mutex held here.
                if let Err(e) = check() {
                    return WaitEnd::Interrupted(e);
                }
            }
        }
    }

    /// Sleep for `d`, returning early on shutdown. Returns `Err(Shutdown)` if shutting down, or the
    /// interrupt error.
    pub fn sleep(&self, d: Duration) -> Result<()> {
        if d.is_zero() {
            return if self.shutting_down() {
                Err(DriverError::Shutdown)
            } else {
                Ok(())
            };
        }
        match self.wait_until(Some(Instant::now() + d), &|| self.shutting_down()) {
            WaitEnd::Released => Err(DriverError::Shutdown),
            WaitEnd::Deadline => Ok(()),
            WaitEnd::Interrupted(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_wakes_waiter() {
        let s = Signals::new();
        let s2 = Arc::clone(&s);
        let t = std::thread::spawn(move || {
            let t0 = Instant::now();
            let end = s2.wait_until(None, &|| s2.cancel_requested());
            (matches!(end, WaitEnd::Released), t0.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        s.request_cancel();
        let (ok, el) = t.join().unwrap();
        assert!(ok);
        assert!(el < Duration::from_secs(2));
    }

    #[test]
    fn deadline() {
        let s = Signals::new();
        let t0 = Instant::now();
        let end = s.wait_until(Some(Instant::now() + Duration::from_millis(60)), &|| false);
        assert!(matches!(end, WaitEnd::Deadline));
        assert!(t0.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn already_released_returns_at_once() {
        let s = Signals::new();
        s.begin_shutdown();
        assert!(matches!(
            s.wait_until(None, &|| s.shutting_down()),
            WaitEnd::Released
        ));
        assert!(matches!(
            s.sleep(Duration::from_secs(5)),
            Err(DriverError::Shutdown)
        ));
    }

    #[test]
    fn clear_for_arm_resets_flags() {
        let s = Signals::new();
        s.request_cancel();
        s.set_end_seen();
        assert!(s.cancel_requested() && s.end_seen());
        s.clear_for_arm();
        assert!(!s.cancel_requested() && !s.end_seen());
        assert!(s.generation() >= 2);
    }

    #[test]
    fn interrupt_hook_runs_between_slices_on_its_thread_only() {
        let s = Signals::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = Arc::clone(&calls);
        s.set_interrupt_hook(Some(InterruptHook {
            thread: std::thread::current().id(),
            check: Arc::new(move || {
                if c2.fetch_add(1, Ordering::SeqCst) >= 1 {
                    Err(DriverError::Internal("interrupted".into()))
                } else {
                    Ok(())
                }
            }),
        }));
        let t0 = Instant::now();
        let end = s.wait_until(None, &|| false);
        assert!(matches!(end, WaitEnd::Interrupted(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(t0.elapsed() >= MAIN_THREAD_SLICE * 2);
        // another thread: no slicing, no hook
        let s2 = Arc::clone(&s);
        let r = std::thread::spawn(move || {
            matches!(
                s2.wait_until(Some(Instant::now() + Duration::from_millis(30)), &|| false),
                WaitEnd::Deadline
            )
        })
        .join()
        .unwrap();
        assert!(r);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// [G1-B] at this layer: 10 000 waiter/releaser races, no lost wake-up.
    #[test]
    fn no_lost_wakeup_race() {
        for i in 0..10_000_u32 {
            let s = Signals::new();
            let s2 = Arc::clone(&s);
            let waiter = std::thread::spawn(move || {
                s2.wait_until(Some(Instant::now() + Duration::from_secs(5)), &|| {
                    s2.cancel_requested()
                })
            });
            if i % 3 == 0 {
                std::thread::yield_now();
            }
            s.request_cancel();
            assert!(matches!(waiter.join().unwrap(), WaitEnd::Released));
        }
    }
}
