//! Stream V2 over ZeroMQ (docs/DESIGN.md §4.3, §3.7, §3.2 R2/R5). No pyo3.
//! Reached as `dectris_core::stream::…`.
//!
//! | module | contents | DESIGN |
//! |---|---|---|
//! | [`zmq_link`] | PULL socket (RCVHWM before connect, LINGER 0, keepalive), poll+recv, close; the **only** file that names `zmq` types; [`zmq_link::PushServer`] test publisher | §4.3 Socket, §3.7 |
//! | [`message`] | header scan → [`message::Incoming`], [`message::StartCache`], [`message::Delivery`] + 4-key metadata | §2.6, §2.7 |
//! | [`attribution`] | pure [`attribution::classify`] (table rows 1–5, [G2-H]), modes [G2-D], per-epoch [`attribution::SeriesState`] | §4.3 [R12] |
//! | [`receiver`] | [`receiver::Stream`]: receive loop, window, series tail, 50 ms drain, fresh frame, flush, reconnect, connect/release, self-checks | §4.3, §4.2, §2.6 |
//! | [`registry`] | owner-thread-scoped receiver registry (RAII [`registry::ReceiverReg`]) | §3.2 R2 [R5], R7 |
//! | [`guard`] | process-wide single-PULL guard per endpoint | §3.7 [G1-L] |
//! | [`stats`] | stream-owned `StreamStats` counters | §2.1 |
//!
//! Every [`receiver::Stream`] method is meant to run detached (inside `py.detach`) and never logs:
//! records go into a caller-provided `dectris_common::log::Deferred` (DESIGN §3.2 R6).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#![allow(rustdoc::broken_intra_doc_links)] // the [Rn]/[G2-x] design tags are not links

pub mod attribution;
pub mod guard;
pub mod message;
pub mod receiver;
pub mod registry;
pub mod stats;
pub mod zmq_link;

pub use attribution::{
    classify, Adopt, ArmedSeries, AttributionMode, Class, SeriesState, StalePolicy,
};
pub use message::{Attribution, Delivery, FrameMeta, Incoming, StartCache};
pub use receiver::{
    window_for, ConnectOutcome, ConnectReport, FlushOutcome, LocalSignals, ReconnectOutcome,
    ReconnectReport, RecvEvent, RecvOptions, RecvSession, ReleaseReport, Stream, StreamSignals,
    StreamTuning,
};
pub use registry::{BusyOwner, ReceiverReg, Registry};
pub use stats::{StreamStats, StreamStatsSnapshot};
pub use zmq_link::{LinkConfig, PushServer, RawMsg, ZmqLink, DEFAULT_RCVBUF};
