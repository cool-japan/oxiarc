//! Per-row filter selection for the encoder.
//!
//! Two heuristics are provided, both evaluated over the five candidate
//! filters:
//!
//! * **Adaptive** (libpng's): score a filtered row by the sum of the absolute
//!   values of its bytes read as signed. Smallest wins.
//! * **MinEntropy** (`oxipng`'s): score by `sum over bytes of ilog2(count)`
//!   across four interleaved byte classes, a cheap proxy for how well DEFLATE
//!   will do.
//!
//! Both evaluate `Paeth` last so that a Paeth win needs no second filtering
//! pass, which is where the scratch buffer's contents are already correct.

use crate::filter::apply::filter_row_into;
use crate::filter::{Filter, RowFilter};
use crate::header::BytesPerPixel;

/// Reusable scratch space for [`select_filter`].
///
/// Allocate one per encoder and reuse it for every row; the buffers grow to
/// the widest row and then stay put.
#[derive(Debug, Default, Clone)]
pub struct AdaptiveScratch {
    candidate: Vec<u8>,
    best: Vec<u8>,
}

impl AdaptiveScratch {
    /// An empty scratch buffer.
    #[must_use]
    pub fn new() -> AdaptiveScratch {
        AdaptiveScratch::default()
    }

    /// The filtered row produced by the last [`select_filter`] call.
    #[must_use]
    pub fn filtered(&self) -> &[u8] {
        &self.best
    }
}

/// The sum of absolute values of the row's bytes read as signed, libpng's
/// minimum-sum-of-absolute-differences score.
#[must_use]
pub fn msad_score(row: &[u8]) -> u64 {
    row.iter()
        .map(|b| u64::from((*b as i8).unsigned_abs()))
        .sum()
}

/// `oxipng`'s entropy estimate: cheap `ilog2` of the byte histogram, computed
/// over four interleaved classes so that per-channel structure is visible.
#[must_use]
pub fn entropy_score(row: &[u8]) -> u64 {
    let mut counts = [[0u32; 256]; 4];
    for (i, b) in row.iter().enumerate() {
        counts[i % 4][usize::from(*b)] += 1;
    }
    let mut total = 0u64;
    for class in &counts {
        for count in class.iter().copied() {
            if count > 1 {
                total += u64::from(count) * u64::from(count.ilog2());
            }
        }
    }
    total
}

/// Choose a filter for one scanline and leave the filtered bytes in `scratch`.
///
/// Returns the chosen filter; the filtered row is
/// [`AdaptiveScratch::filtered`]. `previous` is the **raw** row above, empty
/// for the first row of an image or Adam7 pass.
///
/// ```
/// use oxiarc_png::filter::{select_filter, AdaptiveScratch, Filter, RowFilter};
/// use oxiarc_png::BytesPerPixel;
/// let mut scratch = AdaptiveScratch::new();
/// // A constant row is free under `Sub`.
/// let chosen = select_filter(Filter::Adaptive, BytesPerPixel::One, &[], &[7; 16], &mut scratch);
/// assert_eq!(chosen, RowFilter::Sub);
/// assert_eq!(&scratch.filtered()[1..], &[0; 15]);
/// ```
pub fn select_filter(
    strategy: Filter,
    bpp: BytesPerPixel,
    previous: &[u8],
    raw: &[u8],
    scratch: &mut AdaptiveScratch,
) -> RowFilter {
    scratch.best.resize(raw.len(), 0);
    scratch.candidate.resize(raw.len(), 0);

    if let Some(fixed) = strategy.fixed() {
        filter_row_into(fixed, bpp, previous, raw, &mut scratch.best);
        return fixed;
    }

    let score: fn(&[u8]) -> u64 = match strategy {
        Filter::MinEntropy => entropy_score,
        _ => msad_score,
    };

    // `Paeth` is evaluated last so a win leaves the right bytes in `best`
    // without a second pass; ties keep the earlier candidate.
    let order = [
        RowFilter::NoFilter,
        RowFilter::Up,
        RowFilter::Sub,
        RowFilter::Avg,
        RowFilter::Paeth,
    ];
    let mut chosen = RowFilter::NoFilter;
    let mut best_score = u64::MAX;
    for filter in order {
        filter_row_into(filter, bpp, previous, raw, &mut scratch.candidate);
        let s = score(&scratch.candidate);
        if s < best_score {
            best_score = s;
            chosen = filter;
            scratch.best.copy_from_slice(&scratch.candidate);
        }
    }
    chosen
}

/// The strategy a well-behaved encoder should default to for a given image.
///
/// Filtering usually *hurts* palette images and any sub-byte depth, because
/// neighbouring bytes are unrelated once several pixels share a byte; libpng
/// and `oxipng` both special-case this and so do we.
#[must_use]
pub fn default_strategy(color_type: crate::ColorType, bit_depth: crate::BitDepth) -> Filter {
    if color_type == crate::ColorType::Indexed || (bit_depth as u8) < 8 {
        Filter::NoFilter
    } else {
        Filter::Adaptive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::unfilter::unfilter;

    #[test]
    fn fixed_strategies_are_honoured() {
        let mut scratch = AdaptiveScratch::new();
        for (strategy, want) in [
            (Filter::NoFilter, RowFilter::NoFilter),
            (Filter::Sub, RowFilter::Sub),
            (Filter::Up, RowFilter::Up),
            (Filter::Avg, RowFilter::Avg),
            (Filter::Paeth, RowFilter::Paeth),
        ] {
            let got = select_filter(
                strategy,
                BytesPerPixel::One,
                &[1, 2, 3, 4],
                &[9, 9, 9, 9],
                &mut scratch,
            );
            assert_eq!(got, want);
        }
    }

    #[test]
    fn selection_always_produces_an_invertible_row() {
        let raw: Vec<u8> = (0..96u16).map(|i| (i * 31 % 253) as u8).collect();
        let prev: Vec<u8> = (0..96u16).map(|i| (i * 17 % 251) as u8).collect();
        let mut scratch = AdaptiveScratch::new();
        for strategy in [Filter::Adaptive, Filter::MinEntropy] {
            for previous in [&[][..], &prev[..]] {
                let chosen =
                    select_filter(strategy, BytesPerPixel::Three, previous, &raw, &mut scratch);
                let mut row = scratch.filtered().to_vec();
                unfilter(chosen, BytesPerPixel::Three, previous, &mut row);
                assert_eq!(row, raw, "{strategy:?}");
            }
        }
    }

    #[test]
    fn adaptive_prefers_up_on_a_vertically_constant_image() {
        let row = vec![200u8; 32];
        let mut scratch = AdaptiveScratch::new();
        let chosen = select_filter(
            Filter::Adaptive,
            BytesPerPixel::One,
            &row,
            &row,
            &mut scratch,
        );
        assert_eq!(chosen, RowFilter::Up);
        assert!(scratch.filtered().iter().all(|b| *b == 0));
    }

    #[test]
    fn scores_behave() {
        assert_eq!(msad_score(&[0, 0, 0]), 0);
        assert_eq!(msad_score(&[255]), 1); // -1 as i8
        assert_eq!(msad_score(&[128]), 128); // -128 as i8
        assert!(entropy_score(&[7; 64]) > entropy_score(&(0..64u8).collect::<Vec<_>>()));
    }

    #[test]
    fn default_strategy_avoids_filtering_indexed_and_sub_byte() {
        use crate::{BitDepth, ColorType};
        assert_eq!(
            default_strategy(ColorType::Indexed, BitDepth::Eight),
            Filter::NoFilter
        );
        assert_eq!(
            default_strategy(ColorType::Grayscale, BitDepth::Four),
            Filter::NoFilter
        );
        assert_eq!(
            default_strategy(ColorType::Rgba, BitDepth::Eight),
            Filter::Adaptive
        );
    }
}
