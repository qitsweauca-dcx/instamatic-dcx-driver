//! Typed Stream V2 messages for the receive paths (docs/DESIGN.md §4.3, §2.6–§2.7; PROTOCOL §8.2, §10, §13).
//!
//! Classification uses the codec's µs top-level scan ([`dectris_codec::scan_header`]); nothing here
//! decodes or decompresses `data`, so the raw (turbo) path hands out the wire bytes unchanged
//! (DESIGN §5 Q9). Start messages are additionally scanned for their scalars
//! ([`dectris_codec::StartSummary`]) and kept, with a copy of their bytes, in a [`StartCache`]
//! (the py layer builds `_stream_start_config` / `_start_config` from `raw`).

use std::sync::Arc;

use dectris_codec::{scan_header, Header, MessageKind, StartSummary};
use dectris_common::Result;

use crate::zmq_link::RawMsg;

/// The cached start message of the armed series (DESIGN §3.2 `start_cache`).
#[derive(Debug, Clone, PartialEq)]
pub struct StartCache {
    /// Scalars of the start (`count_time`, `frame_time`, image size, dtype, ids, …).
    pub summary: StartSummary,
    /// The complete start message as received (arrays included; decode lazily).
    pub raw: Vec<u8>,
}

impl StartCache {
    /// Scan a raw start message.
    pub fn from_raw(raw: &[u8]) -> Result<(Header, StartCache)> {
        let (h, summary) = StartSummary::scan(raw)?;
        Ok((
            h,
            StartCache {
                summary,
                raw: raw.to_vec(),
            },
        ))
    }

    pub fn series_id(&self) -> Option<u64> {
        self.summary.series_id
    }

    pub fn series_unique_id(&self) -> Option<&str> {
        self.summary.series_unique_id.as_deref()
    }

    /// `count_time` (s) for the per-frame metadata (DESIGN §2.7).
    pub fn count_time(&self) -> Option<f64> {
        self.summary.count_time
    }

    /// `frame_time` (s) for the receive window (DESIGN §4.3 Window).
    pub fn frame_time(&self) -> Option<f64> {
        self.summary.frame_time
    }
}

/// A received message after the header scan.
#[derive(Debug)]
pub struct Incoming {
    pub msg: RawMsg,
    pub header: Header,
    /// Present for `type = start` (scan of the start scalars + a copy of the bytes).
    pub start: Option<StartCache>,
}

impl Incoming {
    /// Header scan (and, for a start, the start scan). Errors are `StreamDecodeError`s.
    pub fn scan(msg: RawMsg) -> Result<Incoming> {
        let header = scan_header(&msg)?;
        let start = if header.kind == MessageKind::Start {
            Some(StartCache::from_raw(&msg)?.1)
        } else {
            None
        };
        Ok(Incoming { msg, header, start })
    }
}

/// How a delivered image was attributed (DESIGN §4.3 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// Armed series, after its start (row 1, or row 3/4 after adoption).
    Armed,
    /// No attribution possible: `series_attribution: off` or no driver arm in this process (row 5).
    Unattributed,
    /// A stale image delivered because `stale_series_policy: deliver` (`count_time = None`).
    Stale,
    /// An armed-id image delivered without its start after one window [G2-H] (`count_time = None`).
    StartMissing,
}

/// An image handed to a receive path.
#[derive(Debug)]
pub struct Delivery {
    /// The complete wire message (raw path: byte-identical; decoded path: decode from it).
    pub msg: RawMsg,
    pub header: Header,
    /// The start attributed to this image (`None` → `count_time = None`).
    pub start: Option<Arc<StartCache>>,
    pub attribution: Attribution,
}

/// The four-key per-frame metadata of DESIGN §2.7 / PROTOCOL §10.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMeta {
    pub series_id: Option<u64>,
    pub image_id: Option<u64>,
    pub count_time: Option<f64>,
    pub real_time: Option<[u64; 2]>,
}

impl Delivery {
    /// `count_time` from the attributed start.
    pub fn count_time(&self) -> Option<f64> {
        self.start.as_ref().and_then(|s| s.count_time())
    }

    /// `{'series_id', 'image_id', 'count_time', 'real_time'}`.
    pub fn meta(&self) -> FrameMeta {
        FrameMeta {
            series_id: self.header.series_id,
            image_id: self.header.image_id,
            count_time: self.count_time(),
            real_time: self.header.real_time,
        }
    }

    /// The wire bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dectris_codec::encode::{
        end_message, image_message, start_message, ImageSpec, Payload, StartSpec,
    };

    #[test]
    fn scan_start_image_end() {
        let mut s = StartSpec::new(7, (4, 6), "uint16");
        s.count_time = 0.25;
        s.frame_time = 0.3;
        let start = start_message(&s).unwrap();
        let inc = Incoming::scan(RawMsg::from_slice(&start)).unwrap();
        assert_eq!(inc.header.kind, MessageKind::Start);
        let sc = inc.start.unwrap();
        assert_eq!(sc.series_id(), Some(7));
        assert_eq!(sc.count_time(), Some(0.25));
        assert_eq!(sc.frame_time(), Some(0.3));
        assert_eq!(sc.raw, start);
        assert_eq!(sc.series_unique_id(), Some(s.series_unique_id.as_str()));

        let data = vec![1u8; 4 * 6 * 2];
        let img = image_message(&ImageSpec::new(7, 3, (4, 6), 69, &data, Payload::BSLZ4)).unwrap();
        let inc = Incoming::scan(RawMsg::from_slice(&img)).unwrap();
        assert_eq!(inc.header.kind, MessageKind::Image);
        assert_eq!(inc.header.image_id, Some(3));
        assert!(inc.start.is_none());
        let d = Delivery {
            msg: inc.msg,
            header: inc.header,
            start: Some(Arc::new(sc)),
            attribution: Attribution::Armed,
        };
        let m = d.meta();
        assert_eq!(m.series_id, Some(7));
        assert_eq!(m.image_id, Some(3));
        assert_eq!(m.count_time, Some(0.25));
        assert_eq!(m.real_time, Some([4_000_000, 40_000_000]));
        assert_eq!(d.bytes(), img.as_slice());

        let end = end_message(7, "u");
        let inc = Incoming::scan(RawMsg::from_slice(&end)).unwrap();
        assert_eq!(inc.header.kind, MessageKind::End);
    }

    #[test]
    fn garbage_is_a_decode_error() {
        let e = Incoming::scan(RawMsg::from_slice(&[0xff, 0x00])).unwrap_err();
        assert!(matches!(e, dectris_common::DriverError::Decode { .. }));
    }
}
