//! Monitor interface: mode, `images/monitor` fetch + decode + orientation, the ≤ 10 Hz rate limit
//! (DESIGN §12 #18), 408 → cached frame, offline back-off [R10], initialize-in-flight silence [G1-H]
//! (docs/DESIGN.md §2.2, §3.5, §4.1; PROTOCOL §6.5, §11).

use std::sync::Arc;
use std::time::{Duration, Instant};

use dectris_common::sync::lock_unpoisoned;
use dectris_common::{dlog, DriverError, Result};
use serde_json::json;

use crate::client::{require_200, Module, Task, TIFF_LIMIT};
use crate::detector::Detector;
use crate::tiff::{DecodedImage, Pixels};

/// An oriented monitor frame (`fliplr → flipud → rot90(k=-1)`, net `rot90(k=+1)`, PROTOCOL §11) [G1-U],
/// row-major `rows × cols`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorFrame {
    pub rows: usize,
    pub cols: usize,
    pub pixels: Pixels,
}

/// `MonitorCache{last_frame, last_tiff, last_get_at}` (DESIGN §3.2) + the parsed tag 51192
/// (`_last_monitor_metadata`).
#[derive(Debug, Default)]
pub struct MonitorCache {
    pub last_frame: Option<Arc<MonitorFrame>>,
    pub last_tiff: Option<Arc<Vec<u8>>>,
    pub last_get_at: Option<Instant>,
    pub metadata: Vec<(String, String)>,
}

/// `numpy.fliplr` of a row-major `h × w` buffer.
pub fn fliplr<T: Copy>(a: &[T], h: usize, w: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    for r in 0..h {
        if let Some(row) = a.get(r * w..(r + 1) * w) {
            out.extend(row.iter().rev());
        }
    }
    out
}

/// `numpy.flipud` of a row-major `h × w` buffer.
pub fn flipud<T: Copy>(a: &[T], h: usize, w: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    for r in (0..h).rev() {
        if let Some(row) = a.get(r * w..(r + 1) * w) {
            out.extend_from_slice(row);
        }
    }
    out
}

/// `numpy.rot90(a, k=-1)` (clockwise) of a row-major `h × w` buffer → `w × h`:
/// `out[i][j] = a[h-1-j][i]`.
pub fn rot90_cw<T: Copy>(a: &[T], h: usize, w: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    for i in 0..w {
        for j in 0..h {
            if let Some(v) = a.get((h - 1 - j) * w + i) {
                out.push(*v);
            }
        }
    }
    out
}

/// The preview orientation applied in one pass: `rot90(k=+1)` (counter-clockwise) →
/// `out[i][j] = a[j][w-1-i]`, shape `w × h`. Equal to `rot90_cw(flipud(fliplr(a)))` (unit-tested).
pub fn orient_ccw<T: Copy>(a: &[T], h: usize, w: usize) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    for i in 0..w {
        for j in 0..h {
            if let Some(v) = a.get(j * w + (w - 1 - i)) {
                out.push(*v);
            }
        }
    }
    out
}

/// Orient a decoded image (PROTOCOL §11).
pub fn orient(img: &DecodedImage) -> MonitorFrame {
    let (h, w) = (img.height, img.width);
    let pixels = match &img.pixels {
        Pixels::U8(v) => Pixels::U8(orient_ccw(v, h, w)),
        Pixels::U16(v) => Pixels::U16(orient_ccw(v, h, w)),
        Pixels::U32(v) => Pixels::U32(orient_ccw(v, h, w)),
        Pixels::F32(v) => Pixels::F32(orient_ccw(v, h, w)),
    };
    MonitorFrame {
        rows: w,
        cols: h,
        pixels,
    }
}

impl Detector {
    /// `enable_monitor()` [#12 keep] (also the internal enable in `_connect` and after
    /// initialize): PUT monitor `config/mode enabled` (3 s), WARNING on failure; never raises.
    pub fn enable_monitor(&self) {
        match self.client.put_config(
            &self.client.url(Module::Monitor, Task::Config, "mode"),
            json!("enabled"),
            self.budgets().put_monitor,
        ) {
            Ok(_) => dlog!(Debug, "Monitor enabled"),
            Err(e) => dlog!(Warn, "Failed to enable the Monitor interface: {e}"),
        }
    }

    fn monitor_interval(&self) -> Duration {
        let hz = self.settings().monitor_max_hz;
        let hz = if hz.is_finite() && hz > 0.0 { hz } else { 10.0 };
        Duration::from_secs_f64(1.0 / hz)
    }

    fn cached_monitor_frame(&self) -> Option<Arc<MonitorFrame>> {
        lock_unpoisoned(&self.monitor).last_frame.clone()
    }

    /// `_monitor_frame()` (DESIGN §2.2, §3.5):
    /// 1. ≤ `monitor_max_hz` (default 10 Hz) rate-limit sleep (always, also during back-off);
    /// 2. initialize in flight or offline back-off → the cached frame or `None`, no I/O;
    /// 3. GET `images/monitor` (2 s); 408 → the cached frame (or `None`); other non-200 → `Http`;
    /// 4. decode (swappable decoder; failure → `Decode`, the raw bytes stay available through
    ///    [`last_monitor_tiff`](Self::last_monitor_tiff)) → orient → cache.
    pub fn monitor_frame(&self) -> Result<Option<Arc<MonitorFrame>>> {
        let interval = self.monitor_interval();
        let wait = {
            let mut g = lock_unpoisoned(&self.monitor);
            let now = Instant::now();
            let slot = match g.last_get_at {
                Some(t) if t + interval > now => t + interval,
                _ => now,
            };
            g.last_get_at = Some(slot);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            self.signals.sleep(wait)?;
        }
        if self.init_in_flight() || self.client.in_backoff() {
            return Ok(self.cached_monitor_frame());
        }
        let url = self.client.url(Module::Monitor, Task::Images, "monitor");
        let reply = self
            .client
            .get_bytes(&url, self.budgets().monitor_get, TIFF_LIMIT)?;
        if reply.status == 408 {
            if dectris_common::log::log_frames() {
                dlog!(Debug, "Monitor: no new image (408); keeping the last frame");
            }
            return Ok(self.cached_monitor_frame());
        }
        let reply = require_200(reply, &url)?;
        let tiff = Arc::new(reply.body);
        lock_unpoisoned(&self.monitor).last_tiff = Some(Arc::clone(&tiff));
        let t0 = Instant::now();
        let img = self.decoder.decode(&tiff)?;
        let frame = Arc::new(orient(&img));
        {
            let mut g = lock_unpoisoned(&self.monitor);
            g.last_frame = Some(Arc::clone(&frame));
            g.metadata = img.metadata;
        }
        if dectris_common::log::log_frames() {
            dlog!(
                Debug,
                "Preview frame acquired in {:.1} ms",
                t0.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(Some(frame))
    }

    /// The raw bytes of the last 200 monitor response, decodable or not.
    pub fn last_monitor_tiff(&self) -> Option<Arc<Vec<u8>>> {
        lock_unpoisoned(&self.monitor).last_tiff.clone()
    }

    /// `_last_monitor_metadata` (tag 51192: `key=value` lines, or the DECTRIS private IFD as name/value strings; no reader, DESIGN §11.6).
    pub fn last_monitor_metadata(&self) -> Vec<(String, String)> {
        lock_unpoisoned(&self.monitor).metadata.clone()
    }

    /// Wrap a decode error from an external decoder.
    pub fn monitor_decode_error(msg: &str) -> DriverError {
        DriverError::decode(0, format!("monitor TIFF: {msg}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_matches_numpy_rot90_k1() {
        // a = [[0,1,2],[3,4,5]] (2x3); np.rot90(a, 1) = [[2,5],[1,4],[0,3]]
        let a = [0, 1, 2, 3, 4, 5];
        assert_eq!(orient_ccw(&a, 2, 3), vec![2, 5, 1, 4, 0, 3]);
        let lit = rot90_cw(&flipud(&fliplr(&a, 2, 3), 2, 3), 2, 3);
        assert_eq!(lit, orient_ccw(&a, 2, 3));
        // np.rot90(a, -1) = [[3,0],[4,1],[5,2]]
        assert_eq!(rot90_cw(&a, 2, 3), vec![3, 0, 4, 1, 5, 2]);
        assert_eq!(fliplr(&a, 2, 3), vec![2, 1, 0, 5, 4, 3]);
        assert_eq!(flipud(&a, 2, 3), vec![3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn orientation_literal_equals_net_on_random_shapes() {
        for (h, w) in [(1, 1), (1, 7), (7, 1), (5, 9), (16, 3)] {
            let a: Vec<u32> = (0..(h * w) as u32)
                .map(|x| x.wrapping_mul(2_654_435_761))
                .collect();
            let lit = rot90_cw(&flipud(&fliplr(&a, h, w), h, w), h, w);
            assert_eq!(lit, orient_ccw(&a, h, w), "{h}x{w}");
        }
    }

    #[test]
    fn orient_swaps_shape() {
        let img = DecodedImage {
            height: 2,
            width: 3,
            pixels: Pixels::U16(vec![0, 1, 2, 3, 4, 5]),
            metadata: vec![],
        };
        let f = orient(&img);
        assert_eq!((f.rows, f.cols), (3, 2));
        assert_eq!(f.pixels, Pixels::U16(vec![2, 5, 1, 4, 0, 3]));
    }
}
