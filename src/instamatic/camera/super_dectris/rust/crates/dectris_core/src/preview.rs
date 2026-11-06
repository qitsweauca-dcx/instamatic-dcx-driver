//! Scale a Monitor frame for the live view (`CameraDectris.get_preview_image`).
//!
//! The same mapping as `scipy.ndimage.zoom(img, scale, order=1)` with `scale = maxdim / max(shape)`,
//! the way `instamatic.image_utils.autoscale` calls it: the output shape is `round(n * scale)` per
//! axis (ties to even, as Python), the corners map to the corners, and the values are bilinear, in
//! the frame's dtype (integers round half up, as scipy). One difference: the last row and column
//! always sample the frame's edge, where a rounding error can make scipy read its constant 0.

use dectris_common::{DriverError, Result};
use dectris_simplon::Pixels;

/// The output shape for a frame of `rows` x `cols` (both non-zero).
pub fn scaled_shape(rows: usize, cols: usize, maxdim: usize) -> (usize, usize) {
    let scale = maxdim as f64 / rows.max(cols) as f64;
    let scaled = |n: usize| (n as f64 * scale).round_ties_even() as usize;
    (scaled(rows), scaled(cols))
}

/// For each output index along one axis: the input index below, the one above and the weight of
/// the one above.
fn axis_map(input: usize, output: usize) -> Vec<(usize, usize, f64)> {
    let step = if output > 1 {
        (input - 1) as f64 / (output - 1) as f64
    } else {
        1.0
    };
    (0..output)
        .map(|o| {
            let at = (o as f64 * step).min((input - 1) as f64);
            let low = at as usize;
            (low, (low + 1).min(input - 1), at - low as f64)
        })
        .collect()
}

fn zoom<T: Copy>(
    src: &[T],
    cols: usize,
    rows_map: &[(usize, usize, f64)],
    cols_map: &[(usize, usize, f64)],
    to_f64: impl Fn(T) -> f64,
    from_f64: impl Fn(f64) -> T,
) -> Vec<T> {
    let mut out = Vec::with_capacity(rows_map.len() * cols_map.len());
    for &(r0, r1, wr) in rows_map {
        let (top, bottom) = (&src[r0 * cols..][..cols], &src[r1 * cols..][..cols]);
        for &(c0, c1, wc) in cols_map {
            let upper = to_f64(top[c0]) * (1.0 - wc) + to_f64(top[c1]) * wc;
            let lower = to_f64(bottom[c0]) * (1.0 - wc) + to_f64(bottom[c1]) * wc;
            out.push(from_f64(upper * (1.0 - wr) + lower * wr));
        }
    }
    out
}

/// `frame` scaled to fit `maxdim` → `(rows, cols, pixels)`.
pub fn fit(
    rows: usize,
    cols: usize,
    frame: &Pixels,
    maxdim: usize,
) -> Result<(usize, usize, Pixels)> {
    if rows == 0 || cols == 0 || frame.len() != rows * cols {
        return Err(DriverError::InvalidArg(format!(
            "cannot scale a frame of {rows} x {cols} with {} pixels",
            frame.len()
        )));
    }
    let (out_rows, out_cols) = scaled_shape(rows, cols, maxdim);
    let (rmap, cmap) = (axis_map(rows, out_rows), axis_map(cols, out_cols));
    // Rust's float-to-integer cast saturates, as the clamp in scipy's output step does.
    let pixels = match frame {
        Pixels::U8(v) => Pixels::U8(zoom(
            v,
            cols,
            &rmap,
            &cmap,
            f64::from,
            |x| (x + 0.5) as u8,
        )),
        Pixels::U16(v) => Pixels::U16(zoom(
            v,
            cols,
            &rmap,
            &cmap,
            f64::from,
            |x| (x + 0.5) as u16,
        )),
        Pixels::U32(v) => Pixels::U32(zoom(
            v,
            cols,
            &rmap,
            &cmap,
            f64::from,
            |x| (x + 0.5) as u32,
        )),
        Pixels::F32(v) => Pixels::F32(zoom(v, cols, &rmap, &cmap, f64::from, |x| x as f32)),
    };
    Ok((out_rows, out_cols, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_singla_frame_fits_640() {
        assert_eq!(scaled_shape(1028, 1062, 640), (620, 640));
        assert_eq!(scaled_shape(1062, 1028, 640), (640, 620));
        assert_eq!(scaled_shape(640, 640, 640), (640, 640));
    }

    #[test]
    fn corners_stay_and_the_middle_interpolates() {
        let frame = Pixels::U16(vec![0, 10, 20, 30, 40, 50, 60, 70, 80]);
        let (r, c, out) = fit(3, 3, &frame, 5).unwrap();
        assert_eq!((r, c), (5, 5));
        let Pixels::U16(v) = out else { panic!() };
        assert_eq!(v[0], 0);
        assert_eq!(v[4], 20);
        assert_eq!(v[12], 40);
        assert_eq!(v[24], 80);
        assert_eq!(v[1], 5);
    }

    #[test]
    fn same_size_returns_the_frame() {
        let values: Vec<u32> = (0..12).map(|x| x * 1000).collect();
        let (r, c, out) = fit(3, 4, &Pixels::U32(values.clone()), 4).unwrap();
        assert_eq!((r, c), (3, 4));
        assert_eq!(out, Pixels::U32(values));
    }

    #[test]
    fn one_output_row_reads_the_first_input_row() {
        let (r, c, out) = fit(1, 8, &Pixels::U8((0..8).collect()), 4).unwrap();
        assert_eq!((r, c), (0, 4));
        assert!(out.is_empty());
        let (r, c, out) = fit(3, 6, &Pixels::U8((0..18).collect()), 2).unwrap();
        assert_eq!((r, c), (1, 2));
        assert_eq!(out, Pixels::U8(vec![0, 5]));
    }

    #[test]
    fn float_frames_stay_float() {
        let (r, c, out) = fit(1, 3, &Pixels::F32(vec![0.0, 0.5, 1.0]), 5).unwrap();
        assert_eq!((r, c), (2, 5));
        let row = [0.0, 0.25, 0.5, 0.75, 1.0];
        assert_eq!(out, Pixels::F32([row, row].concat()));
    }

    #[test]
    fn a_wrong_size_is_an_error() {
        assert!(fit(0, 3, &Pixels::U8(vec![]), 5).is_err());
        assert!(fit(2, 3, &Pixels::U8(vec![0; 5]), 5).is_err());
    }
}
