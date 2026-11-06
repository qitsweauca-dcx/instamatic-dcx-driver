//! Receiver registry for busy detection, owner-thread scoped (docs/DESIGN.md §3.2 R2 [R5], R7).
//!
//! An entry lives from an iterator's first `__next__` (or from entry into `message_receiver` /
//! `_acquire_image` / `acquire_single_frame`) until StopIteration, the last frame, `close()`, Drop,
//! or **2 × window without a `__next__`**. [`ReceiverReg`] is the RAII handle: dropping it (or
//! [`ReceiverReg::release`]) removes the entry.
//!
//! `flush_zmq_buffer` / `reconnect_zmq` are busy when a live entry is owned by **another** thread
//! ([`Registry::busy_for`]); the owner thread itself may flush (cRED's post-disarm flush while
//! `frame_iter` is still referenced, DESIGN §3.2 R2 [R5]). The R7 series-ownership check uses the
//! same query.
//!
//! The mutex is held for µs, never across I/O, never nested, and nothing logs under it (R3/R6).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use dectris_common::sync::lock_unpoisoned;

#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    owner: ThreadId,
    owner_name: String,
    last_next: Instant,
    window: Duration,
}

impl Entry {
    fn live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_next) <= self.window.saturating_mul(2)
    }
}

#[derive(Debug, Default)]
struct RegInner {
    entries: Mutex<Vec<Entry>>,
    next_id: AtomicU64,
}

/// The `receivers` registry (DESIGN §3.2). Cheap to clone (shared).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<RegInner>,
}

/// A live owner reported by [`Registry::busy_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyOwner {
    pub thread: ThreadId,
    /// Thread name (`<unnamed>` if none).
    pub name: String,
}

fn current_thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_owned()
}

impl Registry {
    /// Test hook (DESIGN §8 [R2]): hold the entries mutex for `dur`.
    #[doc(hidden)]
    pub fn test_hold_lock(&self, dur: Duration) {
        let _g = lock_unpoisoned(&self.inner.entries);
        std::thread::sleep(dur);
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Register a receiver owned by the **current** thread.
    pub fn register(&self, window: Duration) -> ReceiverReg {
        self.register_for(std::thread::current().id(), current_thread_name(), window)
    }

    /// Register a receiver for an explicit owner thread.
    pub fn register_for(
        &self,
        owner: ThreadId,
        owner_name: String,
        window: Duration,
    ) -> ReceiverReg {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        {
            // Expired entries are kept (a later `touch` revives them); they are removed only by
            // their handle's release/Drop, so the list is bounded by the live handles.
            let mut g = lock_unpoisoned(&self.inner.entries);
            g.push(Entry {
                id,
                owner,
                owner_name,
                last_next: now,
                window,
            });
        }
        ReceiverReg {
            inner: Arc::clone(&self.inner),
            id,
            released: AtomicBool::new(false),
        }
    }

    /// A live entry owned by a thread other than `caller` (R2 busy / R7 ownership).
    pub fn busy_for(&self, caller: ThreadId) -> Option<BusyOwner> {
        let now = Instant::now();
        let g = lock_unpoisoned(&self.inner.entries);
        g.iter()
            .find(|e| e.owner != caller && e.live(now))
            .map(|e| BusyOwner {
                thread: e.owner,
                name: e.owner_name.clone(),
            })
    }

    /// Number of live entries (diagnostics, tests).
    pub fn live_count(&self) -> usize {
        let now = Instant::now();
        lock_unpoisoned(&self.inner.entries)
            .iter()
            .filter(|e| e.live(now))
            .count()
    }
}

/// RAII registry entry held by an iterator / receive call (DESIGN §2.1 `FrameIter.reg`). `Send + Sync`.
#[derive(Debug)]
pub struct ReceiverReg {
    inner: Arc<RegInner>,
    id: u64,
    released: AtomicBool,
}

impl ReceiverReg {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Record a `__next__` (keeps the entry live for another 2 × window).
    pub fn touch(&self) {
        self.update(|e| e.last_next = Instant::now());
    }

    /// Change the window (e.g. once the attributed start gives the frame time).
    pub fn set_window(&self, window: Duration) {
        self.update(|e| {
            e.window = window;
            e.last_next = Instant::now();
        });
    }

    fn update(&self, f: impl FnOnce(&mut Entry)) {
        if self.released.load(Ordering::Acquire) {
            return;
        }
        let mut g = lock_unpoisoned(&self.inner.entries);
        if let Some(e) = g.iter_mut().find(|e| e.id == self.id) {
            f(e);
        }
    }

    /// Remove the entry (idempotent): StopIteration, last frame, `close()`.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        lock_unpoisoned(&self.inner.entries).retain(|e| e.id != self.id);
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
}

impl Drop for ReceiverReg {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ReceiverReg>();
        assert_send_sync::<Registry>();
    };

    #[test]
    fn owner_thread_scoping() {
        let r = Registry::new();
        let me = std::thread::current().id();
        let reg = r.register(Duration::from_secs(2));
        assert!(
            r.busy_for(me).is_none(),
            "own entry never makes the owner busy"
        );
        let r2 = r.clone();
        let other = std::thread::Builder::new()
            .name("tk-main".into())
            .spawn(move || r2.busy_for(std::thread::current().id()))
            .unwrap()
            .join()
            .unwrap();
        let b = other.expect("busy from another thread");
        assert_eq!(b.thread, me);
        assert_eq!(r.live_count(), 1);
        drop(reg);
        assert_eq!(r.live_count(), 0);
    }

    #[test]
    fn release_is_idempotent_and_touch_after_release_is_noop() {
        let r = Registry::new();
        let reg = r.register(Duration::from_secs(1));
        reg.release();
        reg.release();
        reg.touch();
        assert!(reg.is_released());
        assert_eq!(r.live_count(), 0);
    }

    #[test]
    fn entries_expire_after_two_windows() {
        let r = Registry::new();
        let reg = r.register_for(
            std::thread::current().id(),
            "x".into(),
            Duration::from_millis(30),
        );
        let r2 = r.clone();
        let check = move || {
            let r3 = r2.clone();
            std::thread::spawn(move || r3.busy_for(std::thread::current().id()).is_some())
                .join()
                .unwrap()
        };
        assert!(check());
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !check(),
            "stale entry (> 2 × window without __next__) is not busy"
        );
        reg.touch();
        assert!(check(), "touch revives it");
        reg.set_window(Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(80));
        assert!(check());
    }
}
