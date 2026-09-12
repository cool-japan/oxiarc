//! Arithmetic entropy coding: the QM coder of ITU-T T.81 Annex D and the
//! conditioning models of Annexes F, G and H.
//!
//! Enabled by the default `arithmetic` feature. It covers `SOF9` (extended
//! sequential), `SOF10` (progressive) and `SOF11` (lossless), in both
//! directions, including `DAC` conditioning and restart intervals.
//!
//! # What lives where
//!
//! * [`qm`] — the probability estimation state machine, T.81 Table D.3.
//! * [`decoder`] / [`encoder`] — the coder itself plus its byte source and
//!   sink, T.81 D.1 and D.2.
//! * This module — the statistics areas and the context arithmetic that the
//!   DCT and lossless models share.
//! * `crate::decoder::arith` / `crate::encoder::arith` — the scan walks.
//!
//! # Conditioning
//!
//! Every model classifies a coded difference into one of five categories:
//! zero, small positive, small negative, large positive, large negative,
//! where "small" and "large" are separated by the `DAC` bounds `L` and `U`.
//! The DCT DC model (T.81 F.1.4.4.1.2) conditions the next difference on the
//! previous one in the same component, giving context indices `0, 4, 8, 12,
//! 16`. The lossless model (T.81 H.1.2.3.1) conditions on the differences
//! coded to the left **and** above, giving the 25 states of Figure H.2 —
//! which is the same table with the DC case as its one-dimensional
//! degenerate form.

pub(crate) mod decoder;
pub(crate) mod encoder;
pub(crate) mod qm;

use crate::frame::ArithmeticConditioning;
use qm::Bin;

/// Statistics bins in one DC conditioning area (T.81 F.1.4.4.1.3 needs 49;
/// libjpeg rounds to 64 and so do we, so that a corrupt index cannot escape
/// the area).
pub(crate) const DC_BINS: usize = 64;

/// Statistics bins in one AC conditioning area (T.81 F.1.4.4.2 needs 245).
pub(crate) const AC_BINS: usize = 256;

/// Statistics bins in one lossless conditioning area (T.81 H.1.2.3.2).
pub(crate) const LOSSLESS_BINS: usize = 158;

/// First bin of the DC magnitude-category chain, `X1` in Table F.4.
pub(crate) const DC_X1: usize = 20;

/// First bin of the AC magnitude-category chain for `k <= Kx` (Table F.5).
pub(crate) const AC_X1_LOW: usize = 189;

/// First bin of the AC magnitude-category chain for `k > Kx`.
pub(crate) const AC_X1_HIGH: usize = 217;

/// Offset from a magnitude-category bin to its magnitude-bits bin, `M(k) =
/// X(k) + 14` in Tables F.4, F.5 and H.3.
pub(crate) const MAGNITUDE_OFFSET: usize = 14;

/// First bin of the lossless magnitude-category chain when the sample above
/// was in the zero or small categories (T.81 H.1.2.3.2, `X1_Context`).
pub(crate) const LOSSLESS_X1_SMALL: usize = 100;

/// First bin of the lossless magnitude-category chain when the sample above
/// was in a large category.
pub(crate) const LOSSLESS_X1_LARGE: usize = 129;

/// The magnitude class at which a difference has overflowed what T.81 can
/// code (`m` doubles per category and must stay below `0x8000`).
pub(crate) const MAGNITUDE_LIMIT: u32 = 0x8000;

/// Conditioning category of a coded difference.
///
/// The five values are T.81 F.1.4.4.1.2's, in the order Figure H.2 lays them
/// out: zero, small positive, small negative, large positive, large negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    /// The difference was zero, or small enough that `L` folds it into zero.
    Zero = 0,
    /// Positive, magnitude class at most `1 << U >> 1`.
    SmallPositive = 1,
    /// Negative, magnitude class at most `1 << U >> 1`.
    SmallNegative = 2,
    /// Positive, magnitude class above `1 << U >> 1`.
    LargePositive = 3,
    /// Negative, magnitude class above `1 << U >> 1`.
    LargeNegative = 4,
}

impl Category {
    /// The DC statistics area index this category conditions, i.e. Table
    /// F.4's `S0` (`0`, `4`, `8`, `12` or `16`).
    pub(crate) fn dc_context(self) -> usize {
        4 * (self as usize)
    }

    /// `true` for the two large categories, which is what `X1_Context`
    /// switches on in the lossless model.
    pub(crate) fn is_large(self) -> bool {
        matches!(self, Category::LargePositive | Category::LargeNegative)
    }
}

/// The `DAC` conditioning in the form the coders want it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Conditioning {
    /// `(1 << L) >> 1` per DC/lossless table slot.
    lower: [u32; 4],
    /// `(1 << U) >> 1` per DC/lossless table slot.
    upper: [u32; 4],
    /// `Kx` per AC table slot.
    kx: [u32; 4],
}

impl Conditioning {
    /// Derive the bounds from a parsed `DAC` segment.
    pub(crate) fn new(tables: &ArithmeticConditioning) -> Self {
        let mut lower = [0u32; 4];
        let mut upper = [0u32; 4];
        for slot in 0..4 {
            let cs = tables.dc[slot];
            let l = u32::from(cs & 0x0F);
            let u = u32::from(cs >> 4);
            lower[slot] = (1u32 << l) >> 1;
            upper[slot] = (1u32 << u) >> 1;
        }
        let kx = std::array::from_fn(|slot| u32::from(tables.ac[slot]));
        Self { lower, upper, kx }
    }

    /// Classify a coded difference whose magnitude class is `m` (the power of
    /// two at the base of its magnitude category) and whose sign is
    /// `negative`, for DC/lossless table `slot`.
    pub(crate) fn classify(&self, slot: usize, m: u32, negative: bool) -> Category {
        let slot = slot & 3;
        if m < self.lower[slot] {
            Category::Zero
        } else if m > self.upper[slot] {
            if negative {
                Category::LargeNegative
            } else {
                Category::LargePositive
            }
        } else if negative {
            Category::SmallNegative
        } else {
            Category::SmallPositive
        }
    }

    /// `Kx` for AC table `slot`: the spectral index at which the magnitude
    /// chain moves to its second statistics block (T.81 F.1.4.4.2).
    pub(crate) fn kx(&self, slot: usize) -> u32 {
        self.kx[slot & 3]
    }
}

/// The DC and AC statistics areas of one scan.
///
/// Areas are indexed by **table slot**, not by component: two chroma
/// components that name the same table share one adaptive state, which is
/// what T.81 F.1.4.4.1.3 means by "statistics area" and what libjpeg does.
pub(crate) struct DctStats {
    dc: [[Bin; DC_BINS]; 4],
    ac: [[Bin; AC_BINS]; 4],
}

impl Default for DctStats {
    fn default() -> Self {
        Self::new()
    }
}

impl DctStats {
    /// All bins at the initial state (index 0, MPS 0), as T.81 D.1.7 and
    /// D.2.7 require at the start of a scan and at every restart.
    pub(crate) fn new() -> Self {
        Self {
            dc: [[0; DC_BINS]; 4],
            ac: [[0; AC_BINS]; 4],
        }
    }

    /// One DC area.
    pub(crate) fn dc(&mut self, slot: usize) -> &mut [Bin; DC_BINS] {
        &mut self.dc[slot & 3]
    }

    /// One AC area.
    pub(crate) fn ac(&mut self, slot: usize) -> &mut [Bin; AC_BINS] {
        &mut self.ac[slot & 3]
    }

    /// Reset one DC area (restart interval, T.81 D.2.7).
    pub(crate) fn reset_dc(&mut self, slot: usize) {
        self.dc[slot & 3] = [0; DC_BINS];
    }

    /// Reset one AC area.
    pub(crate) fn reset_ac(&mut self, slot: usize) {
        self.ac[slot & 3] = [0; AC_BINS];
    }
}

/// The lossless statistics areas of one scan (T.81 H.1.2.3.2).
pub(crate) struct LosslessStats {
    areas: [[Bin; LOSSLESS_BINS]; 4],
}

impl Default for LosslessStats {
    fn default() -> Self {
        Self::new()
    }
}

impl LosslessStats {
    /// All bins at the initial state.
    pub(crate) fn new() -> Self {
        Self {
            areas: [[0; LOSSLESS_BINS]; 4],
        }
    }

    /// One statistics area.
    pub(crate) fn area(&mut self, slot: usize) -> &mut [Bin; LOSSLESS_BINS] {
        &mut self.areas[slot & 3]
    }

    /// Reset one area.
    pub(crate) fn reset(&mut self, slot: usize) {
        self.areas[slot & 3] = [0; LOSSLESS_BINS];
    }
}

/// Figure H.2's conditioning index for a lossless difference, given the
/// categories of the differences to the left (`a`) and above (`b`).
pub(crate) fn lossless_context(left: Category, above: Category) -> usize {
    20 * (left as usize) + 4 * (above as usize)
}

/// `X1_Context(Db)` of T.81 H.1.2.3.2.
pub(crate) fn lossless_x1(above: Category) -> usize {
    if above.is_large() {
        LOSSLESS_X1_LARGE
    } else {
        LOSSLESS_X1_SMALL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bounds_are_l_zero_u_one() {
        let conditioning = Conditioning::new(&ArithmeticConditioning::default());
        // L = 0 => (1 << 0) >> 1 == 0, so nothing is folded into "zero".
        assert_eq!(conditioning.lower[0], 0);
        // U = 1 => (1 << 1) >> 1 == 1, so classes 0 and 1 are "small".
        assert_eq!(conditioning.upper[0], 1);
        assert_eq!(conditioning.kx(0), 5);
    }

    #[test]
    fn classification_follows_the_dc_rule() {
        let conditioning = Conditioning::new(&ArithmeticConditioning::default());
        assert_eq!(conditioning.classify(0, 0, false), Category::SmallPositive);
        assert_eq!(conditioning.classify(0, 1, true), Category::SmallNegative);
        assert_eq!(conditioning.classify(0, 2, false), Category::LargePositive);
        assert_eq!(conditioning.classify(0, 8, true), Category::LargeNegative);
    }

    #[test]
    fn a_wide_l_folds_small_differences_into_the_zero_category() {
        let mut tables = ArithmeticConditioning::default();
        // L = 2, U = 3.
        tables.dc[0] = 0x32;
        let conditioning = Conditioning::new(&tables);
        assert_eq!(conditioning.lower[0], 2);
        assert_eq!(conditioning.upper[0], 4);
        assert_eq!(conditioning.classify(0, 1, false), Category::Zero);
        assert_eq!(conditioning.classify(0, 2, false), Category::SmallPositive);
        assert_eq!(conditioning.classify(0, 8, false), Category::LargePositive);
    }

    #[test]
    fn dc_contexts_are_the_multiples_of_four_from_table_f4() {
        assert_eq!(Category::Zero.dc_context(), 0);
        assert_eq!(Category::SmallPositive.dc_context(), 4);
        assert_eq!(Category::SmallNegative.dc_context(), 8);
        assert_eq!(Category::LargePositive.dc_context(), 12);
        assert_eq!(Category::LargeNegative.dc_context(), 16);
    }

    /// Figure H.2, read straight off the standard's 5x5 array.
    #[test]
    fn the_lossless_context_array_matches_figure_h2() {
        assert_eq!(lossless_context(Category::Zero, Category::Zero), 0);
        assert_eq!(
            lossless_context(Category::Zero, Category::LargeNegative),
            16
        );
        assert_eq!(
            lossless_context(Category::SmallPositive, Category::Zero),
            20
        );
        assert_eq!(
            lossless_context(Category::SmallNegative, Category::SmallNegative),
            48
        );
        assert_eq!(
            lossless_context(Category::LargeNegative, Category::LargeNegative),
            96
        );
        // The whole array stays inside the first 100 bins.
        for left in [
            Category::Zero,
            Category::SmallPositive,
            Category::SmallNegative,
            Category::LargePositive,
            Category::LargeNegative,
        ] {
            for above in [
                Category::Zero,
                Category::SmallPositive,
                Category::SmallNegative,
                Category::LargePositive,
                Category::LargeNegative,
            ] {
                assert!(lossless_context(left, above) + 3 < LOSSLESS_X1_SMALL);
            }
        }
    }

    #[test]
    fn the_lossless_magnitude_chains_fit_the_area() {
        assert_eq!(lossless_x1(Category::Zero), LOSSLESS_X1_SMALL);
        assert_eq!(lossless_x1(Category::SmallNegative), LOSSLESS_X1_SMALL);
        assert_eq!(lossless_x1(Category::LargePositive), LOSSLESS_X1_LARGE);
        // X15 + 14 is the last bin of each chain.
        const {
            assert!(LOSSLESS_X1_SMALL + 14 + MAGNITUDE_OFFSET < LOSSLESS_BINS);
            assert!(LOSSLESS_X1_LARGE + 14 + MAGNITUDE_OFFSET == LOSSLESS_BINS - 1);
        }
    }

    #[test]
    fn the_dct_areas_are_shared_by_table_slot() {
        let mut stats = DctStats::new();
        stats.dc(1)[3] = 0x81;
        assert_eq!(stats.dc(1)[3], 0x81);
        assert_eq!(stats.dc(0)[3], 0);
        stats.reset_dc(1);
        assert_eq!(stats.dc(1)[3], 0);
    }
}
