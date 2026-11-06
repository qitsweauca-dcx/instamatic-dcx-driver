//! Lock helpers shared by all crates (DESIGN §3.2).
//!
//! **(R5)** Poisoned mutexes are recovered with `into_inner()`: a panic in one caller (caught by
//! `ffi_guard!` or a worker's `catch_unwind`) must not turn every later call into a panic.
//! This helper is for **detached** threads only; attached threads use
//! `pyo3::sync::MutexExt::lock_py_attached` in the py crate (R6).

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `m.lock()`, recovering the guard if the mutex is poisoned (DESIGN §3.2 R5).
#[inline]
pub fn lock_unpoisoned<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recovers_poisoned_mutex() {
        let m = Arc::new(Mutex::new(41_u32));
        let m2 = Arc::clone(&m);
        let r = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(r.is_err());
        assert!(m.is_poisoned());
        *lock_unpoisoned(&m) += 1;
        assert_eq!(*lock_unpoisoned(&m), 42);
    }
}
