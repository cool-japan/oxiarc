//! Scanline filtering (the encode direction).
//!
//! Filtering reads the **raw** previous row, not the filtered one, so bands of
//! rows can be filtered independently as long as each band carries its
//! predecessor. Compression, in contrast, is strictly serial for a single
//! DEFLATE stream.

use crate::filter::RowFilter;
use crate::filter::paeth::paeth_fpnge;
use crate::header::BytesPerPixel;

/// Filter `raw` into `out`, which must have the same length.
///
/// `previous` is the raw (unfiltered) row above, or an empty slice for the
/// first row of an image or Adam7 pass.
///
/// ```
/// use oxiarc_png::filter::{filter_row_into, unfilter, RowFilter};
/// use oxiarc_png::BytesPerPixel;
/// let raw = [1u8, 2, 3, 4];
/// let mut filtered = [0u8; 4];
/// filter_row_into(RowFilter::Sub, BytesPerPixel::One, &[], &raw, &mut filtered);
/// assert_eq!(filtered, [1, 1, 1, 1]);
/// unfilter(RowFilter::Sub, BytesPerPixel::One, &[], &mut filtered);
/// assert_eq!(filtered, raw);
/// ```
pub fn filter_row_into(
    filter: RowFilter,
    bpp: BytesPerPixel,
    previous: &[u8],
    raw: &[u8],
    out: &mut [u8],
) {
    let previous = if previous.len() == raw.len() {
        previous
    } else {
        &[]
    };
    let n = raw.len().min(out.len());
    let raw = &raw[..n];
    let out = &mut out[..n];
    match bpp {
        BytesPerPixel::One => filter_bpp::<1>(filter, previous, raw, out),
        BytesPerPixel::Two => filter_bpp::<2>(filter, previous, raw, out),
        BytesPerPixel::Three => filter_bpp::<3>(filter, previous, raw, out),
        BytesPerPixel::Four => filter_bpp::<4>(filter, previous, raw, out),
        BytesPerPixel::Six => filter_bpp::<6>(filter, previous, raw, out),
        BytesPerPixel::Eight => filter_bpp::<8>(filter, previous, raw, out),
    }
}

/// Filter `raw` in place into a freshly allocated row.
///
/// Convenience wrapper over [`filter_row_into`] for call sites that do not
/// keep a scratch buffer.
#[must_use]
pub fn apply_filter(filter: RowFilter, bpp: BytesPerPixel, previous: &[u8], raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; raw.len()];
    filter_row_into(filter, bpp, previous, raw, &mut out);
    out
}

#[inline(always)]
fn filter_bpp<const BPP: usize>(filter: RowFilter, previous: &[u8], raw: &[u8], out: &mut [u8]) {
    let len = raw.len();
    let head = BPP.min(len);
    let zero_above = previous.is_empty();
    match filter {
        RowFilter::NoFilter => out.copy_from_slice(raw),
        RowFilter::Sub => {
            out[..head].copy_from_slice(&raw[..head]);
            for i in BPP..len {
                out[i] = raw[i].wrapping_sub(raw[i - BPP]);
            }
        }
        RowFilter::Up => {
            if zero_above {
                out.copy_from_slice(raw);
            } else {
                for i in 0..len {
                    out[i] = raw[i].wrapping_sub(previous[i]);
                }
            }
        }
        RowFilter::Avg => {
            if zero_above {
                out[..head].copy_from_slice(&raw[..head]);
                for i in BPP..len {
                    out[i] = raw[i].wrapping_sub(raw[i - BPP] >> 1);
                }
            } else {
                for i in 0..head {
                    out[i] = raw[i].wrapping_sub(previous[i] >> 1);
                }
                for i in BPP..len {
                    let sum = u16::from(raw[i - BPP]) + u16::from(previous[i]);
                    out[i] = raw[i].wrapping_sub((sum >> 1) as u8);
                }
            }
        }
        RowFilter::Paeth => {
            if zero_above {
                out[..head].copy_from_slice(&raw[..head]);
                for i in BPP..len {
                    out[i] = raw[i].wrapping_sub(raw[i - BPP]);
                }
            } else {
                for i in 0..head {
                    out[i] = raw[i].wrapping_sub(previous[i]);
                }
                for i in BPP..len {
                    let pred = paeth_fpnge(raw[i - BPP], previous[i], previous[i - BPP]);
                    out[i] = raw[i].wrapping_sub(pred);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::unfilter::unfilter;

    #[test]
    fn no_filter_copies() {
        let raw = [9u8, 8, 7];
        let out = apply_filter(RowFilter::NoFilter, BytesPerPixel::One, &[1, 2, 3], &raw);
        assert_eq!(out, raw);
    }

    #[test]
    fn up_on_the_first_row_copies() {
        let raw = [9u8, 8, 7];
        let out = apply_filter(RowFilter::Up, BytesPerPixel::One, &[], &raw);
        assert_eq!(out, raw);
    }

    #[test]
    fn round_trips_for_every_bpp_and_filter() {
        let bpps = [
            BytesPerPixel::One,
            BytesPerPixel::Two,
            BytesPerPixel::Three,
            BytesPerPixel::Four,
            BytesPerPixel::Six,
            BytesPerPixel::Eight,
        ];
        let raw: Vec<u8> = (0..64u16).map(|i| (i * 7 % 251) as u8).collect();
        let prev: Vec<u8> = (0..64u16).map(|i| (i * 13 % 241) as u8).collect();
        for bpp in bpps {
            for filter in RowFilter::ALL {
                for previous in [&[][..], &prev[..]] {
                    let mut filtered = apply_filter(filter, bpp, previous, &raw);
                    unfilter(filter, bpp, previous, &mut filtered);
                    assert_eq!(filtered, raw, "{bpp:?} {filter:?}");
                }
            }
        }
    }

    #[test]
    fn short_output_buffers_are_truncated_not_panicking() {
        let raw = [1u8, 2, 3, 4];
        let mut out = [0u8; 2];
        filter_row_into(RowFilter::Sub, BytesPerPixel::One, &[], &raw, &mut out);
        assert_eq!(out, [1, 1]);
    }
}
