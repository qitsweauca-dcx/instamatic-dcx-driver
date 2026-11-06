//! Shared test helpers: the fake SIMPLON server and detector builders.

#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]

pub use dectris_simplon::testing as fake_simplon;

use std::sync::Arc;
use std::time::Duration;

use dectris_simplon::{Budgets, Detector, Settings, Signals, WorkerPool};

pub use dectris_simplon::testing::fast_budgets;
pub use fake_simplon::{Action, ArmBody, FakeSimplon, FakeState, FtModel, TriggerHold};

/// A detector against `fake` with fast budgets, its own signals and a private worker pool.
pub fn detector(fake: &FakeSimplon) -> Detector {
    detector_with(fake, Settings::default())
}

pub fn detector_with(fake: &FakeSimplon, settings: Settings) -> Detector {
    Detector::builder(fake.endpoint())
        .user_agent("instamatic-dcx7/test")
        .budgets(fast_budgets())
        .signals(Signals::new())
        .pool(WorkerPool::new(8))
        .settings(settings)
        .build()
        .unwrap()
}

pub fn arc(d: Detector) -> Arc<Detector> {
    Arc::new(d)
}

/// Keys of the fake's request log that match `pred`.
pub fn keys_matching(fake: &FakeSimplon, pred: impl Fn(&str) -> bool) -> Vec<String> {
    fake.keys().into_iter().filter(|k| pred(k)).collect()
}
