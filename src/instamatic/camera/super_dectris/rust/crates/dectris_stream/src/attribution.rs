//! Frame-to-series attribution (docs/DESIGN.md §4.3 [R12] [G2-D] [G2-H]; PROTOCOL §7.3, §10).
//!
//! [`classify`] is a pure function of the scanned header and an [`ArmedSeries`] view; it is
//! table-tested row by row. Messages are matched by **equality, never ordering**: the sequence id
//! resets on initialize (User Manual §7.3.1).
//!
//! | situation | start | image | end |
//! |---|---|---|---|
//! | 1 armed id known, `series_id == armed` | own start (once), pin `series_unique_id` | deliver if own start seen and unique id matches | End if own start seen and unique id matches |
//! | 2 known, `≠ armed`, own start not seen | stale | stale (drop / deliver per policy) | stale |
//! | 3 known, `≠ armed`, own start seen | **adopt** (newer series armed outside the driver) | stale | stale |
//! | 4 driver armed, id unknown | first start → adopt | stale | stale |
//! | 5 no driver arm in this process / mode `off` | cache | deliver | End |
//!
//! [G2-H] An armed-id image or end before the armed start is stale; if armed-id images keep
//! arriving for one receive window without the armed start, they are delivered with
//! `count_time = None` ([`Class::DeliverStartMissing`]). The window is counted from the **first**
//! such image (DESIGN §4.3).

use std::sync::Arc;
use std::time::{Duration, Instant};

use dectris_codec::{Header, MessageKind};

use crate::message::StartCache;

/// `series_attribution` option (DESIGN §2.2, §4.3 [G2-D]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttributionMode {
    /// The table above, armed id = the arm body's `sequence_id` (default).
    #[default]
    SequenceId,
    /// Ignore the arm body id: adopt the first start received after `arm()` (row 4 every epoch).
    Fifo,
    /// Row 5 always: cache every start, deliver everything, every end is terminal.
    Off,
}

impl AttributionMode {
    /// `'sequence_id'` / `'fifo'` / `'off'` (case-insensitive, surrounding blanks ignored).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sequence_id" => Some(Self::SequenceId),
            "fifo" => Some(Self::Fifo),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SequenceId => "sequence_id",
            Self::Fifo => "fifo",
            Self::Off => "off",
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::SequenceId => 0,
            Self::Fifo => 1,
            Self::Off => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Fifo,
            2 => Self::Off,
            _ => Self::SequenceId,
        }
    }
}

/// `stale_series_policy` option: governs stale **images** only (DESIGN §2.2 [R12][G2-D]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StalePolicy {
    /// Drop provably-stale images (default).
    #[default]
    Drop,
    /// Deliver them with `count_time = None`.
    Deliver,
}

impl StalePolicy {
    /// `'drop'` / `'deliver'` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "drop" => Some(Self::Drop),
            "deliver" => Some(Self::Deliver),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drop => "drop",
            Self::Deliver => "deliver",
        }
    }
}

/// Inputs of [`classify`]: the series state at the time of the message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArmedSeries {
    /// The driver armed at least once in this process (row 5 when false).
    pub driver_armed: bool,
    /// Armed series id (`None` = unknown: arm body without id, `fifo`, or not yet adopted).
    pub armed_id: Option<u64>,
    /// The armed series' start was seen (and cached).
    pub own_start_seen: bool,
    /// `series_unique_id` pinned from the armed start [G2-H].
    pub pinned_unique_id: Option<String>,
    /// [G2-H] fallback active: armed-id images are delivered without their start.
    pub start_missing_fallback: bool,
    /// [G2-H] when the first armed-id image arrived before the armed start.
    pub orphan_since: Option<Instant>,
}

/// How a start that becomes the armed start was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopt {
    /// Row 1: `series_id == armed id`.
    No,
    /// Row 4: armed id unknown, first start of the epoch.
    FirstStart,
    /// Row 3: a newer series armed outside the driver (only a START can adopt).
    Newer,
}

/// Classification of one message (DESIGN §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The armed start: cache it, `own_start_seen = true`, pin `series_unique_id`.
    OwnStart(Adopt),
    /// The armed start again (same id and unique id): ignored, cache kept ("cache once").
    DuplicateStart,
    /// Row 5 / `off`: cache it (replacing any previous start).
    UnattributedStart,
    /// Stale start: never cached.
    StaleStart,
    /// Armed-series image.
    Deliver,
    /// Row 5 / `off`: deliver with whatever start is cached.
    DeliverUnattributed,
    /// [G2-H] armed-id image without its start after one window: deliver, `count_time = None`.
    DeliverStartMissing,
    /// Stale image. `deliver` = `stale_series_policy: deliver`; `armed_id` = it carries the armed
    /// id but precedes the armed start (starts the [G2-H] clock).
    StaleImage { deliver: bool, armed_id: bool },
    /// The armed series' end: terminal, sets `end_seen`.
    ArmedEnd,
    /// Row 5 / `off`: every end is terminal.
    UnattributedEnd,
    /// Stale end: ignored, not terminal.
    StaleEnd,
    /// `type` missing or not start/image/end: one WARNING, skipped.
    Unknown,
}

impl Class {
    /// Counted in `stats.stale`.
    pub fn is_stale(self) -> bool {
        matches!(
            self,
            Class::StaleStart | Class::StaleImage { .. } | Class::StaleEnd
        )
    }

    /// Ends the receive (`'end'`, `_STREAM_END`).
    pub fn is_terminal(self) -> bool {
        matches!(self, Class::ArmedEnd | Class::UnattributedEnd)
    }
}

fn unique_ok(h: &Header, pin: &Option<String>) -> bool {
    match (pin, &h.series_unique_id) {
        (Some(p), Some(u)) => p == u,
        // No pin (start without series_unique_id) or a message without one: lenient (DESIGN §4.3).
        _ => true,
    }
}

/// Classify one scanned message (pure). `now`/`window` drive the [G2-H] start-missing fallback.
pub fn classify(
    h: &Header,
    s: &ArmedSeries,
    mode: AttributionMode,
    policy: StalePolicy,
    now: Instant,
    window: Duration,
) -> Class {
    let deliver_stale = policy == StalePolicy::Deliver;
    if mode == AttributionMode::Off || !s.driver_armed {
        return match h.kind {
            MessageKind::Start => Class::UnattributedStart,
            MessageKind::Image => Class::DeliverUnattributed,
            MessageKind::End => Class::UnattributedEnd,
            _ => Class::Unknown,
        };
    }
    let Some(armed) = s.armed_id else {
        // Row 4: driver armed, id unknown.
        return match h.kind {
            MessageKind::Start if h.series_id.is_some() => Class::OwnStart(Adopt::FirstStart),
            MessageKind::Start => Class::StaleStart,
            MessageKind::Image => Class::StaleImage {
                deliver: deliver_stale,
                armed_id: false,
            },
            MessageKind::End => Class::StaleEnd,
            _ => Class::Unknown,
        };
    };
    let same = h.series_id == Some(armed);
    match h.kind {
        MessageKind::Start => {
            if h.series_id.is_none() {
                Class::StaleStart
            } else if same && !s.own_start_seen {
                Class::OwnStart(Adopt::No)
            } else if same {
                if unique_ok(h, &s.pinned_unique_id) {
                    Class::DuplicateStart
                } else {
                    // Same id, new unique id after the armed start: a newer series (id reset).
                    Class::OwnStart(Adopt::Newer)
                }
            } else if s.own_start_seen {
                Class::OwnStart(Adopt::Newer)
            } else {
                Class::StaleStart
            }
        }
        MessageKind::Image => {
            if same && s.own_start_seen {
                if unique_ok(h, &s.pinned_unique_id) {
                    Class::Deliver
                } else {
                    Class::StaleImage {
                        deliver: deliver_stale,
                        armed_id: false,
                    }
                }
            } else if same {
                let window_passed = s
                    .orphan_since
                    .is_some_and(|t| now.saturating_duration_since(t) >= window);
                if s.start_missing_fallback || window_passed {
                    Class::DeliverStartMissing
                } else {
                    Class::StaleImage {
                        deliver: deliver_stale,
                        armed_id: true,
                    }
                }
            } else {
                Class::StaleImage {
                    deliver: deliver_stale,
                    armed_id: false,
                }
            }
        }
        MessageKind::End => {
            let own = s.own_start_seen && unique_ok(h, &s.pinned_unique_id);
            // [G2-H] fallback: the armed start never came, its images were delivered → its end ends.
            let fallback = !s.own_start_seen && s.start_missing_fallback;
            if same && (own || fallback) {
                Class::ArmedEnd
            } else {
                Class::StaleEnd
            }
        }
        _ => Class::Unknown,
    }
}

/// Everything the receive paths know about the armed series (one `Mutex`, µs sections, never
/// nested with the socket mutex — DESIGN §3.2 R2/R3). Reset by [`SeriesState::on_arm_reset`].
#[derive(Debug, Default)]
pub struct SeriesState {
    /// `driver_armed`: sticky for the process (row 5 = never armed by the driver).
    pub driver_armed: bool,
    /// `armed_series_id` (`None` = unknown).
    pub armed_id: Option<u64>,
    /// The id parsed from the arm body (kept for diagnostics, also in `fifo` mode).
    pub arm_body_id: Option<u64>,
    pub own_start_seen: bool,
    pub pinned_unique_id: Option<String>,
    /// `start_cache`.
    pub start_cache: Option<Arc<StartCache>>,
    /// Epoch passed to the last `on_arm_reset`.
    pub epoch: u64,
    /// When `on_arm_reset` ran.
    pub armed_at: Option<Instant>,
    /// `arm()` returned (`on_armed` ran) in this epoch.
    pub arm_returned: bool,
    /// [G2-D] a message carrying the armed id was seen in this epoch.
    pub armed_msg_seen: bool,
    /// [G2-D] `series_id` of a START classified stale after `arm()` returned.
    pub stale_start_after_arm: Option<u64>,
    /// [G2-D] the ERROR self-check already fired in this epoch.
    pub mismatch_reported: bool,
    /// [G2-H] first armed-id image before the armed start.
    pub orphan_since: Option<Instant>,
    /// [G2-H] fallback active.
    pub start_missing_fallback: bool,
    /// The armed series' end was seen (by a receive path or a flush).
    pub armed_end_seen: bool,
    /// DESIGN §10 "no Stream V2 start within <w> s of arm" WARNING already logged.
    pub no_start_warned: bool,
    /// Row 5 "series attribution unavailable" WARNING already logged.
    pub unattributed_warned: bool,
    /// "count_time metadata will be None" WARNING already logged for this series.
    pub no_count_time_warned: bool,
    /// R18 modifier ≠ itemsize WARNING already logged for this series.
    pub modifier_warned: bool,
    /// Series ids already warned about as stale (bounded, reset per arm).
    pub stale_warned: Vec<Option<u64>>,
}

/// Bound of [`SeriesState::stale_warned`].
const STALE_WARNED_CAP: usize = 32;

impl SeriesState {
    /// The [`classify`] view.
    pub fn view(&self) -> ArmedSeries {
        ArmedSeries {
            driver_armed: self.driver_armed,
            armed_id: self.armed_id,
            own_start_seen: self.own_start_seen,
            pinned_unique_id: self.pinned_unique_id.clone(),
            start_missing_fallback: self.start_missing_fallback,
            orphan_since: self.orphan_since,
        }
    }

    /// `arm()` step 3 (before the arm PUT): clear the start cache, `own_start_seen`, the pin and
    /// the per-epoch bookkeeping; armed id unknown; `driver_armed = true`.
    pub fn on_arm_reset(&mut self, epoch: u64, now: Instant) {
        *self = SeriesState {
            driver_armed: true,
            epoch,
            armed_at: Some(now),
            ..SeriesState::default()
        };
    }

    /// `arm()` steps 5–6: store the parsed sequence id (ignored in `fifo` mode) and run the
    /// PUT→parse window defence: a cached start whose `series_id` differs is dropped.
    /// Returns `true` if the defence cleared the cache.
    pub fn on_armed(&mut self, sequence_id: Option<u64>, mode: AttributionMode) -> bool {
        self.arm_returned = true;
        self.arm_body_id = sequence_id;
        if mode == AttributionMode::Fifo {
            return false;
        }
        let Some(id) = sequence_id else {
            return false;
        };
        self.armed_id = Some(id);
        let mismatch = self
            .start_cache
            .as_ref()
            .is_some_and(|c| c.series_id() != Some(id));
        if mismatch {
            self.start_cache = None;
            self.own_start_seen = false;
            self.pinned_unique_id = None;
        }
        mismatch
    }

    /// Take the armed start (rows 1/3/4): cache, `own_start_seen`, pin, adopt the id.
    pub fn accept_own_start(&mut self, h: &Header, start: StartCache) -> Arc<StartCache> {
        if let Some(id) = h.series_id {
            self.armed_id = Some(id);
        }
        self.own_start_seen = true;
        self.pinned_unique_id = h.series_unique_id.clone();
        self.orphan_since = None;
        self.start_missing_fallback = false;
        self.armed_end_seen = false;
        self.no_count_time_warned = false;
        self.modifier_warned = false;
        let a = Arc::new(start);
        self.start_cache = Some(Arc::clone(&a));
        a
    }

    /// Row 5 / `off`: cache every start.
    pub fn accept_unattributed_start(&mut self, start: StartCache) -> Arc<StartCache> {
        self.no_count_time_warned = false;
        self.modifier_warned = false;
        let a = Arc::new(start);
        self.start_cache = Some(Arc::clone(&a));
        a
    }

    /// `true` the first time a stale message of `series_id` is seen in this epoch.
    pub fn first_stale_warning(&mut self, series_id: Option<u64>) -> bool {
        if self.stale_warned.contains(&series_id) {
            return false;
        }
        if self.stale_warned.len() < STALE_WARNED_CAP {
            self.stale_warned.push(series_id);
        }
        true
    }

    /// Book-keeping common to every classified message (self-check inputs, [G2-H] clock).
    pub fn note(&mut self, h: &Header, class: Class, now: Instant) {
        if self.armed_id.is_some() && h.series_id == self.armed_id {
            self.armed_msg_seen = true;
        }
        match class {
            Class::StaleStart if self.arm_returned => {
                self.stale_start_after_arm = h.series_id.or(self.stale_start_after_arm);
            }
            Class::StaleImage { armed_id: true, .. } if self.orphan_since.is_none() => {
                self.orphan_since = Some(now);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(kind: MessageKind, sid: Option<u64>, uid: Option<&str>) -> Header {
        Header {
            kind,
            series_id: sid,
            series_unique_id: uid.map(str::to_owned),
            image_id: Some(1),
            real_time: None,
        }
    }

    const W: Duration = Duration::from_millis(2000);

    fn c(h: &Header, s: &ArmedSeries, mode: AttributionMode, p: StalePolicy) -> Class {
        let now = Instant::now();
        classify(h, s, mode, p, now, W)
    }

    fn armed(id: Option<u64>, seen: bool, pin: Option<&str>) -> ArmedSeries {
        ArmedSeries {
            driver_armed: true,
            armed_id: id,
            own_start_seen: seen,
            pinned_unique_id: pin.map(str::to_owned),
            start_missing_fallback: false,
            orphan_since: None,
        }
    }

    use AttributionMode::*;
    use MessageKind as K;
    use StalePolicy::{Deliver as PD, Drop as PDrop};

    #[test]
    fn row1_armed_id_matches() {
        let s0 = armed(Some(5), false, None);
        assert_eq!(
            c(&hdr(K::Start, Some(5), Some("u5")), &s0, SequenceId, PDrop),
            Class::OwnStart(Adopt::No)
        );
        let s = armed(Some(5), true, Some("u5"));
        assert_eq!(
            c(&hdr(K::Start, Some(5), Some("u5")), &s, SequenceId, PDrop),
            Class::DuplicateStart
        );
        assert_eq!(
            c(&hdr(K::Image, Some(5), Some("u5")), &s, SequenceId, PDrop),
            Class::Deliver
        );
        assert_eq!(
            c(&hdr(K::End, Some(5), Some("u5")), &s, SequenceId, PDrop),
            Class::ArmedEnd
        );
        // [G2-H] pinned unique id mismatch → stale (same id, earlier series)
        assert_eq!(
            c(&hdr(K::Image, Some(5), Some("old")), &s, SequenceId, PDrop),
            Class::StaleImage {
                deliver: false,
                armed_id: false
            }
        );
        assert_eq!(
            c(&hdr(K::End, Some(5), Some("old")), &s, SequenceId, PDrop),
            Class::StaleEnd
        );
        // lenient when the message has no unique id
        assert_eq!(
            c(&hdr(K::Image, Some(5), None), &s, SequenceId, PDrop),
            Class::Deliver
        );
        // same id, different unique id START after own start → newer series (adopt)
        assert_eq!(
            c(&hdr(K::Start, Some(5), Some("new")), &s, SequenceId, PDrop),
            Class::OwnStart(Adopt::Newer)
        );
    }

    #[test]
    fn row1_g2h_armed_id_before_start_is_stale() {
        let s = armed(Some(5), false, None);
        assert_eq!(
            c(&hdr(K::Image, Some(5), Some("x")), &s, SequenceId, PDrop),
            Class::StaleImage {
                deliver: false,
                armed_id: true
            }
        );
        assert_eq!(
            c(&hdr(K::End, Some(5), Some("x")), &s, SequenceId, PDrop),
            Class::StaleEnd
        );
    }

    #[test]
    fn g2h_start_missing_fallback_after_one_window() {
        let t0 = Instant::now();
        let mut s = armed(Some(5), false, None);
        s.orphan_since = Some(t0);
        let h = hdr(K::Image, Some(5), Some("x"));
        assert!(matches!(
            classify(
                &h,
                &s,
                SequenceId,
                PDrop,
                t0 + Duration::from_millis(1999),
                W
            ),
            Class::StaleImage { armed_id: true, .. }
        ));
        assert_eq!(
            classify(&h, &s, SequenceId, PDrop, t0 + W, W),
            Class::DeliverStartMissing
        );
        s.start_missing_fallback = true;
        assert_eq!(
            classify(&h, &s, SequenceId, PDrop, t0, W),
            Class::DeliverStartMissing
        );
        assert_eq!(
            classify(&hdr(K::End, Some(5), None), &s, SequenceId, PDrop, t0, W),
            Class::ArmedEnd
        );
        // other ids stay stale
        assert!(classify(&hdr(K::Image, Some(4), None), &s, SequenceId, PDrop, t0, W).is_stale());
    }

    #[test]
    fn row2_other_id_before_own_start() {
        let s = armed(Some(5), false, None);
        assert_eq!(
            c(&hdr(K::Start, Some(4), None), &s, SequenceId, PDrop),
            Class::StaleStart
        );
        assert_eq!(
            c(&hdr(K::Image, Some(4), None), &s, SequenceId, PDrop),
            Class::StaleImage {
                deliver: false,
                armed_id: false
            }
        );
        assert_eq!(
            c(&hdr(K::Image, Some(4), None), &s, SequenceId, PD),
            Class::StaleImage {
                deliver: true,
                armed_id: false
            }
        );
        assert_eq!(
            c(&hdr(K::End, Some(4), None), &s, SequenceId, PDrop),
            Class::StaleEnd
        );
        assert!(!Class::StaleEnd.is_terminal());
        // a start without series_id can never be attributed
        assert_eq!(
            c(&hdr(K::Start, None, None), &s, SequenceId, PDrop),
            Class::StaleStart
        );
    }

    #[test]
    fn row3_newer_series_adopted_only_by_start() {
        let s = armed(Some(5), true, Some("u5"));
        assert!(c(&hdr(K::Image, Some(6), None), &s, SequenceId, PDrop).is_stale());
        assert_eq!(
            c(&hdr(K::End, Some(6), None), &s, SequenceId, PDrop),
            Class::StaleEnd
        );
        assert_eq!(
            c(&hdr(K::Start, Some(6), Some("u6")), &s, SequenceId, PDrop),
            Class::OwnStart(Adopt::Newer)
        );
        // after adoption (armed_id=6, pinned u6) → row 1
        let s2 = armed(Some(6), true, Some("u6"));
        assert_eq!(
            c(&hdr(K::Image, Some(6), Some("u6")), &s2, SequenceId, PDrop),
            Class::Deliver
        );
        assert_eq!(
            c(&hdr(K::End, Some(6), Some("u6")), &s2, SequenceId, PDrop),
            Class::ArmedEnd
        );
        assert!(c(&hdr(K::Image, Some(5), Some("u5")), &s2, SequenceId, PDrop).is_stale());
    }

    #[test]
    fn row4_unknown_id_first_start_adopts() {
        let s = armed(None, false, None);
        assert!(c(&hdr(K::Image, Some(3), None), &s, SequenceId, PDrop).is_stale());
        assert_eq!(
            c(&hdr(K::End, Some(3), None), &s, SequenceId, PDrop),
            Class::StaleEnd
        );
        assert_eq!(
            c(&hdr(K::Start, Some(3), None), &s, SequenceId, PDrop),
            Class::OwnStart(Adopt::FirstStart)
        );
        assert_eq!(
            c(&hdr(K::Start, Some(3), None), &s, Fifo, PDrop),
            Class::OwnStart(Adopt::FirstStart)
        );
    }

    #[test]
    fn row5_and_off_mode() {
        let never = ArmedSeries::default();
        for mode in [SequenceId, Fifo, Off] {
            assert_eq!(
                c(&hdr(K::Start, Some(1), None), &never, mode, PDrop),
                Class::UnattributedStart
            );
            assert_eq!(
                c(&hdr(K::Image, Some(9), None), &never, mode, PDrop),
                Class::DeliverUnattributed
            );
            assert_eq!(
                c(&hdr(K::End, Some(9), None), &never, mode, PDrop),
                Class::UnattributedEnd
            );
        }
        let s = armed(Some(5), true, Some("u5"));
        assert_eq!(
            c(&hdr(K::Image, Some(9), None), &s, Off, PDrop),
            Class::DeliverUnattributed
        );
        assert_eq!(
            c(&hdr(K::End, Some(9), None), &s, Off, PDrop),
            Class::UnattributedEnd
        );
        assert!(Class::UnattributedEnd.is_terminal());
    }

    #[test]
    fn unknown_types() {
        let s = armed(Some(5), true, None);
        assert_eq!(
            c(
                &hdr(K::Other("x".into()), Some(5), None),
                &s,
                SequenceId,
                PDrop
            ),
            Class::Unknown
        );
        assert_eq!(
            c(
                &hdr(K::Missing, Some(5), None),
                &ArmedSeries::default(),
                Off,
                PDrop
            ),
            Class::Unknown
        );
    }

    #[test]
    fn options_parse() {
        assert_eq!(AttributionMode::parse(" FIFO "), Some(Fifo));
        assert_eq!(AttributionMode::parse("sequence_id"), Some(SequenceId));
        assert_eq!(AttributionMode::parse("off"), Some(Off));
        assert_eq!(AttributionMode::parse("name"), None);
        for m in [SequenceId, Fifo, Off] {
            assert_eq!(AttributionMode::from_u8(m.to_u8()), m);
            assert_eq!(AttributionMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(StalePolicy::parse("Deliver"), Some(StalePolicy::Deliver));
        assert_eq!(StalePolicy::parse("drop"), Some(StalePolicy::Drop));
        assert_eq!(StalePolicy::parse("keep"), None);
    }

    fn start(sid: u64, uid: &str) -> (Header, StartCache) {
        let mut spec = dectris_codec::encode::StartSpec::new(sid, (2, 2), "uint16");
        spec.series_unique_id = uid.to_owned();
        let raw = dectris_codec::encode::start_message(&spec).unwrap();
        StartCache::from_raw(&raw).unwrap()
    }

    #[test]
    fn state_arm_cycle_and_defence() {
        let mut st = SeriesState::default();
        let now = Instant::now();
        st.on_arm_reset(3, now);
        assert!(st.driver_armed && st.armed_id.is_none() && !st.own_start_seen);
        // a start lands in the PUT→parse window (row 4 adoption), arm body says 8
        let (h, sc) = start(7, "u7");
        st.accept_own_start(&h, sc);
        assert_eq!(st.armed_id, Some(7));
        assert!(
            st.on_armed(Some(8), SequenceId),
            "defence must clear the mismatching start"
        );
        assert!(st.start_cache.is_none() && !st.own_start_seen && st.pinned_unique_id.is_none());
        assert_eq!(st.armed_id, Some(8));
        // matching id keeps it
        st.on_arm_reset(4, now);
        let (h, sc) = start(9, "u9");
        st.accept_own_start(&h, sc);
        assert!(!st.on_armed(Some(9), SequenceId));
        assert!(st.own_start_seen && st.start_cache.is_some());
        assert_eq!(st.pinned_unique_id.as_deref(), Some("u9"));
        // fifo ignores the body id
        st.on_arm_reset(5, now);
        assert!(!st.on_armed(Some(11), Fifo));
        assert_eq!(st.armed_id, None);
        assert_eq!(st.arm_body_id, Some(11));
        assert!(st.arm_returned);
        // reset keeps driver_armed sticky and clears the rest
        st.stale_start_after_arm = Some(1);
        st.mismatch_reported = true;
        st.on_arm_reset(6, now);
        assert!(st.driver_armed);
        assert_eq!(st.epoch, 6);
        assert!(st.stale_start_after_arm.is_none() && !st.mismatch_reported && !st.arm_returned);
    }

    #[test]
    fn note_tracks_self_check_and_orphans() {
        let mut st = SeriesState::default();
        let now = Instant::now();
        st.on_arm_reset(1, now);
        st.on_armed(Some(5), SequenceId);
        let h = hdr(K::Start, Some(6), None);
        st.note(&h, Class::StaleStart, now);
        assert_eq!(st.stale_start_after_arm, Some(6));
        assert!(!st.armed_msg_seen);
        let h = hdr(K::Image, Some(5), None);
        st.note(
            &h,
            Class::StaleImage {
                deliver: false,
                armed_id: true,
            },
            now,
        );
        assert!(st.armed_msg_seen);
        assert_eq!(st.orphan_since, Some(now));
        st.note(
            &h,
            Class::StaleImage {
                deliver: false,
                armed_id: true,
            },
            now + W,
        );
        assert_eq!(
            st.orphan_since,
            Some(now),
            "clock starts at the first orphan"
        );
    }

    #[test]
    fn stale_warning_once_per_series() {
        let mut st = SeriesState::default();
        assert!(st.first_stale_warning(Some(4)));
        assert!(!st.first_stale_warning(Some(4)));
        assert!(st.first_stale_warning(Some(3)));
        assert!(st.first_stale_warning(None));
        assert!(!st.first_stale_warning(None));
        for i in 100..200 {
            st.first_stale_warning(Some(i));
        }
        assert!(st.stale_warned.len() <= STALE_WARNED_CAP);
    }
}
