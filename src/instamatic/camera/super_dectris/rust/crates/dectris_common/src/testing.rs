//! Test-only capture logger (feature `test-util`, always on for this crate's own tests).
//!
//! One global `log::Log` per test binary. Tests run in parallel, so each test tags its messages
//! uniquely and collects them with [`take_matching`].
//!
//! ```text
//! dectris_common::testing::install();
//! dectris_common::dlog!(Warn, "my-test-tag {}", 1);
//! let recs = dectris_common::testing::take_matching("my-test-tag");
//! ```

use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;

use crate::sync::lock_unpoisoned;

/// A record seen by the capture logger.
#[derive(Debug, Clone)]
pub struct CapturedRecord {
    pub level: ::log::Level,
    pub target: String,
    pub message: String,
    pub module_path: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub thread: ThreadId,
    /// `log::no_python()` on the emitting thread at log time.
    pub no_python: bool,
}

struct CaptureLogger {
    records: Mutex<Vec<CapturedRecord>>,
}

impl ::log::Log for CaptureLogger {
    fn enabled(&self, _: &::log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &::log::Record<'_>) {
        let rec = CapturedRecord {
            level: record.level(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
            module_path: record.module_path().map(str::to_owned),
            file: record.file().map(str::to_owned),
            line: record.line(),
            thread: std::thread::current().id(),
            no_python: crate::log::no_python(),
        };
        lock_unpoisoned(&self.records).push(rec);
    }

    fn flush(&self) {}
}

static LOGGER: OnceLock<CaptureLogger> = OnceLock::new();
static INSTALLED: OnceLock<bool> = OnceLock::new();

/// Install the capture logger (idempotent) and set the max level to `Debug` (the level the py crate
/// uses, DESIGN §2.9). Returns `false` if another logger already owned the global slot.
pub fn install() -> bool {
    *INSTALLED.get_or_init(|| {
        let logger = LOGGER.get_or_init(|| CaptureLogger {
            records: Mutex::new(Vec::new()),
        });
        let ok = ::log::set_logger(logger).is_ok();
        ::log::set_max_level(::log::LevelFilter::Debug);
        ok
    })
}

/// Remove and return every captured record whose message contains `needle`.
pub fn take_matching(needle: &str) -> Vec<CapturedRecord> {
    let Some(logger) = LOGGER.get() else {
        return Vec::new();
    };
    let mut all = lock_unpoisoned(&logger.records);
    let (hit, keep): (Vec<_>, Vec<_>) = all.drain(..).partition(|r| r.message.contains(needle));
    *all = keep;
    hit
}
