//! LOGGING_CLOSED is process-global and irreversible, so it is tested in its own test binary
//! (own process) to keep it from leaking into the unit tests.

use dectris_common::log::{close_logging, logging_closed, no_python, python_logging_blocked};

#[test]
fn close_logging_is_global_and_sticky() {
    assert!(!logging_closed());
    assert!(!python_logging_blocked());
    close_logging();
    assert!(logging_closed());
    assert!(python_logging_blocked());
    // visible from every thread, including ones without NO_PYTHON
    let other = std::thread::spawn(|| (logging_closed(), no_python(), python_logging_blocked()))
        .join()
        .unwrap();
    assert_eq!(other, (true, false, true));
    close_logging(); // idempotent
    assert!(logging_closed());
}
