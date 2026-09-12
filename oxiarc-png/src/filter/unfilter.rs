//! Scanline reconstruction (the decode direction).
//!
//! Every kernel works **in place** on the current row and reads the previous
//! row through a shared reference, which is exactly the shape the two-row
//! scratch buffer in [`crate::decoder`] provides.

use crate::filter::RowFilter;
use crate::filter::paeth::paeth_stbi;
use crate::header::BytesPerPixel;

/// Reconstruct one scanline in place.
///
/// `previous` is the already-reconstructed row above, or an empty slice for
/// the first row of an image (or of an Adam7 pass), in which case the filters
/// degenerate exactly as the specification requires: `Up` becomes `None`,
/// `Paeth` becomes `Sub`, and `Avg` uses only its left neighbour.
///
/// A `previous` whose length differs from `current` is treated as absent, so a
/// caller that gets its bookkeeping wrong produces a wrong image rather than a
/// panic or a read of uninitialised state.
///
/// ```
/// use oxiarc_png::filter::{unfilter, RowFilter};
/// use oxiarc_png::BytesPerPixel;
/// // Sub filter over three 1-byte pixels: 1, 1, 1 reconstructs to 1, 2, 3.
/// let mut row = [1u8, 1, 1];
/// unfilter(RowFilter::Sub, BytesPerPixel::One, &[], &mut row);
/// assert_eq!(row, [1, 2, 3]);
/// ```
pub fn unfilter(filter: RowFilter, bpp: BytesPerPixel, previous: &[u8], current: &mut [u8]) {
    let previous = if previous.len() == current.len() {
        previous
    } else {
        &[]
    };
    match bpp {
        BytesPerPixel::One => unfilter_bpp::<1>(filter, previous, current),
        BytesPerPixel::Two => unfilter_bpp::<2>(filter, previous, current),
        BytesPerPixel::Three => unfilter_bpp::<3>(filter, previous, current),
        BytesPerPixel::Four => unfilter_bpp::<4>(filter, previous, current),
        BytesPerPixel::Six => unfilter_bpp::<6>(filter, previous, current),
        BytesPerPixel::Eight => unfilter_bpp::<8>(filter, previous, current),
    }
}

#[inline(always)]
fn unfilter_bpp<const BPP: usize>(filter: RowFilter, previous: &[u8], current: &mut [u8]) {
    match (filter, previous.is_empty()) {
        (RowFilter::NoFilter, _) => {}
        // First row: `Up` has nothing above it, so it is the identity.
        (RowFilter::Up, true) => {}
        (RowFilter::Sub, _) | (RowFilter::Paeth, true) => sub::<BPP>(current),
        (RowFilter::Up, false) => up(previous, current),
        (RowFilter::Avg, true) => avg_first_row::<BPP>(current),
        (RowFilter::Avg, false) => avg::<BPP>(previous, current),
        (RowFilter::Paeth, false) => paeth::<BPP>(previous, current),
    }
}

#[inline(always)]
fn sub<const BPP: usize>(current: &mut [u8]) {
    let len = current.len();
    let mut i = BPP;
    while i < len {
        current[i] = current[i].wrapping_add(current[i - BPP]);
        i += 1;
    }
}

#[inline(always)]
fn up(previous: &[u8], current: &mut [u8]) {
    for (cur, prev) in current.iter_mut().zip(previous.iter()) {
        *cur = cur.wrapping_add(*prev);
    }
}

/// `Avg` on the first row of an image or pass: the upper neighbour is zero.
#[inline(always)]
fn avg_first_row<const BPP: usize>(current: &mut [u8]) {
    let len = current.len();
    let mut i = BPP;
    while i < len {
        current[i] = current[i].wrapping_add(current[i - BPP] >> 1);
        i += 1;
    }
}

#[inline(always)]
fn avg<const BPP: usize>(previous: &[u8], current: &mut [u8]) {
    let len = current.len();
    let head = BPP.min(len);
    for i in 0..head {
        current[i] = current[i].wrapping_add(previous[i] >> 1);
    }
    let mut i = BPP;
    while i < len {
        let sum = u16::from(current[i - BPP]) + u16::from(previous[i]);
        current[i] = current[i].wrapping_add((sum >> 1) as u8);
        i += 1;
    }
}

#[inline(always)]
fn paeth<const BPP: usize>(previous: &[u8], current: &mut [u8]) {
    let len = current.len();
    let head = BPP.min(len);
    // With no left neighbour the predictor reduces to the upper byte.
    for i in 0..head {
        current[i] = current[i].wrapping_add(previous[i]);
    }
    let mut i = BPP;
    while i < len {
        let pred = paeth_stbi(current[i - BPP], previous[i], previous[i - BPP]);
        current[i] = current[i].wrapping_add(pred);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::apply::filter_row_into;

    const BPPS: [BytesPerPixel; 6] = [
        BytesPerPixel::One,
        BytesPerPixel::Two,
        BytesPerPixel::Three,
        BytesPerPixel::Four,
        BytesPerPixel::Six,
        BytesPerPixel::Eight,
    ];

    fn pseudo_random(seed: &mut u64, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (*seed >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn filter_then_unfilter_is_the_identity() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        for bpp in BPPS {
            for len in [0usize, 1, 3, 7, 8, 16, 31, 64] {
                let prev = pseudo_random(&mut seed, len);
                let raw = pseudo_random(&mut seed, len);
                for filter in RowFilter::ALL {
                    for use_prev in [false, true] {
                        let previous: &[u8] = if use_prev { &prev } else { &[] };
                        let mut filtered = vec![0u8; len];
                        filter_row_into(filter, bpp, previous, &raw, &mut filtered);
                        unfilter(filter, bpp, previous, &mut filtered);
                        assert_eq!(
                            filtered, raw,
                            "bpp={bpp:?} len={len} filter={filter:?} prev={use_prev}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn known_reconstructions() {
        // Sub with bpp 3.
        let mut row = [1u8, 2, 3, 1, 1, 1];
        unfilter(RowFilter::Sub, BytesPerPixel::Three, &[], &mut row);
        assert_eq!(row, [1, 2, 3, 2, 3, 4]);

        // Up.
        let mut row = [1u8, 2, 3];
        unfilter(RowFilter::Up, BytesPerPixel::One, &[10, 20, 30], &mut row);
        assert_eq!(row, [11, 22, 33]);

        // Avg: floor((left + up) / 2), computed in 9 bits.
        let mut row = [0u8, 0];
        unfilter(RowFilter::Avg, BytesPerPixel::One, &[255, 255], &mut row);
        assert_eq!(row, [127, 191]);

        // Paeth on the first row degenerates to Sub.
        let mut row = [5u8, 5, 5];
        unfilter(RowFilter::Paeth, BytesPerPixel::One, &[], &mut row);
        assert_eq!(row, [5, 10, 15]);
    }

    #[test]
    fn up_on_the_first_row_is_the_identity() {
        let mut row = [7u8, 9, 11];
        unfilter(RowFilter::Up, BytesPerPixel::Two, &[], &mut row);
        assert_eq!(row, [7, 9, 11]);
    }

    #[test]
    fn avg_carries_are_computed_in_nine_bits() {
        // left = 200, up = 200 -> (400 >> 1) = 200, not ((200 + 200) as u8) >> 1 = 72.
        let mut row = [200u8, 0];
        unfilter(RowFilter::Avg, BytesPerPixel::One, &[0, 200], &mut row);
        assert_eq!(row, [200, 200]);
    }

    #[test]
    fn mismatched_previous_length_is_treated_as_absent() {
        let mut row = [1u8, 1, 1];
        unfilter(RowFilter::Up, BytesPerPixel::One, &[9, 9], &mut row);
        assert_eq!(row, [1, 1, 1]);
    }

    #[test]
    fn empty_rows_do_nothing() {
        for bpp in BPPS {
            for filter in RowFilter::ALL {
                let mut row: [u8; 0] = [];
                unfilter(filter, bpp, &[], &mut row);
            }
        }
    }

    #[test]
    fn rows_shorter_than_one_pixel_do_not_panic() {
        for bpp in BPPS {
            for filter in RowFilter::ALL {
                let mut row = [1u8];
                let prev = [2u8];
                unfilter(filter, bpp, &prev, &mut row);
            }
        }
    }
}
