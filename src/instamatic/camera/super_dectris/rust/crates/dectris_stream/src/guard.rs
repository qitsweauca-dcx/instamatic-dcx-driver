//! Process-wide single-PULL guard per (host, port) (docs/DESIGN.md §3.7 [R6][R8] [G1-L]).
//!
//! `static GUARD: Mutex<HashMap<"host:port", (id, Weak<owner>)>>`. The first instance per endpoint
//! gets a [`GuardTicket`] and connects; a second instance gets [`GuardHeld`] and stays disconnected
//! until the holder releases (ticket dropped) or its owner is gone (dead `Weak`).
//!
//! **In-process only [G1-L]:** another process (autoconfig CLI, a second program using super_dectris) still opens its
//! own PULL; the DCU then round-robins messages between the clients (PROTOCOL §7.1).
//!
//! The mutex is held for µs, never across I/O, never nested, nothing logs under it. The py layer may
//! lock it from attached threads with a plain lock: the holder never needs the GIL (DESIGN §3.2 R6).

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, Weak};

use dectris_common::sync::lock_unpoisoned;

type Owner = Weak<dyn Any + Send + Sync>;

struct Holder {
    id: u64,
    owner: Owner,
}

/// Test hook (DESIGN §8 [R2]): hold the process-wide PULL guard table for `dur`.
#[doc(hidden)]
pub fn test_hold_lock(dur: std::time::Duration) {
    let _g = lock_unpoisoned(table());
    std::thread::sleep(dur);
}

fn table() -> &'static Mutex<HashMap<String, Holder>> {
    static GUARD: OnceLock<Mutex<HashMap<String, Holder>>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Guard key for an endpoint: `tcp://Host:31001` → `host:31001`. Endpoints that do not parse as
/// `scheme://host:port` use the whole string (lower-cased). Host names are not resolved, so
/// `localhost` and `127.0.0.1` are different keys.
pub fn endpoint_key(endpoint: &str) -> String {
    let e = endpoint.trim();
    let rest = e.split_once("://").map_or(e, |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    match rest.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            format!(
                "{}:{}",
                host.trim_matches(|c| c == '[' || c == ']')
                    .to_ascii_lowercase(),
                port
            )
        }
        _ => e.to_ascii_lowercase(),
    }
}

/// The endpoint is held by another live owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardHeld {
    pub key: String,
}

/// RAII ownership of an endpoint's PULL. Dropping it releases the endpoint (idempotent; it never
/// removes a newer holder's entry).
#[derive(Debug)]
pub struct GuardTicket {
    key: String,
    id: u64,
}

impl GuardTicket {
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for GuardTicket {
    fn drop(&mut self) {
        let mut g = lock_unpoisoned(table());
        if g.get(&self.key).is_some_and(|h| h.id == self.id) {
            g.remove(&self.key);
        }
    }
}

/// Acquire the endpoint for `owner`. A previous holder whose owner is gone counts as released.
pub fn acquire(endpoint: &str, owner: Owner) -> Result<GuardTicket, GuardHeld> {
    let key = endpoint_key(endpoint);
    let mut g = lock_unpoisoned(table());
    if let Some(h) = g.get(&key) {
        if h.owner.strong_count() > 0 {
            return Err(GuardHeld { key });
        }
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    g.insert(key.clone(), Holder { id, owner });
    Ok(GuardTicket { key, id })
}

/// Whether a live owner holds the endpoint.
pub fn is_held(endpoint: &str) -> bool {
    let key = endpoint_key(endpoint);
    lock_unpoisoned(table())
        .get(&key)
        .is_some_and(|h| h.owner.strong_count() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn owner() -> (Arc<dyn Any + Send + Sync>, Owner) {
        let a: Arc<dyn Any + Send + Sync> = Arc::new(0u8);
        let w = Arc::downgrade(&a);
        (a, w)
    }

    #[test]
    fn keys() {
        assert_eq!(endpoint_key("tcp://DCU.local:31001"), "dcu.local:31001");
        assert_eq!(
            endpoint_key(" tcp://192.0.2.1:31001/ "),
            "192.0.2.1:31001"
        );
        assert_eq!(endpoint_key("tcp://[::1]:5"), "::1:5");
        assert_eq!(endpoint_key("garbage"), "garbage");
        assert_eq!(endpoint_key("tcp://host:notaport"), "tcp://host:notaport");
    }

    #[test]
    fn first_wins_second_waits_release_frees() {
        let ep = "tcp://guard-test-a:1";
        let (a, wa) = owner();
        let (_b, wb) = owner();
        let t = acquire(ep, wa).unwrap();
        assert!(is_held(ep));
        assert_eq!(acquire(ep, wb.clone()).unwrap_err().key, "guard-test-a:1");
        drop(t);
        assert!(!is_held(ep));
        let t2 = acquire(ep, wb).unwrap();
        assert_eq!(t2.key(), "guard-test-a:1");
        drop(a);
    }

    #[test]
    fn dead_owner_counts_as_released() {
        let ep = "tcp://guard-test-b:2";
        let (a, wa) = owner();
        let stale = acquire(ep, wa).unwrap();
        drop(a);
        assert!(!is_held(ep));
        let (_b, wb) = owner();
        let fresh = acquire(ep, wb).unwrap();
        // dropping the stale ticket must not remove the newer holder
        drop(stale);
        assert!(is_held(ep));
        drop(fresh);
        assert!(!is_held(ep));
    }

    #[test]
    fn different_ports_are_independent() {
        let (_a, wa) = owner();
        let (_b, wb) = owner();
        let _t1 = acquire("tcp://guard-test-c:1", wa).unwrap();
        let _t2 = acquire("tcp://guard-test-c:2", wb).unwrap();
    }
}
