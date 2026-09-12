//! Scanline filtering: reconstruction (decode), application (encode) and
//! strategy selection.
//!
//! PNG filters operate on **bytes**, with a stride of
//! `max(1, bits_per_pixel / 8)` ([`crate::BytesPerPixel`]) as the distance to
//! the left neighbour. Only six stride values are reachable across the whole
//! of Table 11.1, so every kernel is monomorphised for exactly that set.

pub mod apply;
mod paeth;
pub mod select;
pub mod unfilter;

pub use apply::{apply_filter, filter_row_into};
pub use paeth::{paeth_fpnge, paeth_spec, paeth_stbi};
pub use select::{AdaptiveScratch, select_filter};
pub use unfilter::unfilter;

use crate::error::{DecodingError, FormatErrorKind};

/// The five per-scanline filter types defined by the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum RowFilter {
    /// The scanline is stored verbatim.
    #[default]
    NoFilter = 0,
    /// Each byte is stored as the difference from the byte one pixel to the
    /// left.
    Sub = 1,
    /// Each byte is stored as the difference from the byte above.
    Up = 2,
    /// Each byte is stored as the difference from the average of the left and
    /// upper neighbours.
    Avg = 3,
    /// Each byte is stored as the difference from the Paeth predictor of its
    /// left, upper and upper-left neighbours.
    Paeth = 4,
}

impl RowFilter {
    /// Parse a scanline's leading filter byte.
    pub fn from_u8(n: u8) -> Result<RowFilter, DecodingError> {
        match n {
            0 => Ok(RowFilter::NoFilter),
            1 => Ok(RowFilter::Sub),
            2 => Ok(RowFilter::Up),
            3 => Ok(RowFilter::Avg),
            4 => Ok(RowFilter::Paeth),
            code => Err(FormatErrorKind::InvalidRowFilter { code }.into()),
        }
    }

    /// The byte written at the start of a filtered scanline.
    #[must_use]
    pub fn into_u8(self) -> u8 {
        self as u8
    }

    /// All five filters, in numeric order.
    pub const ALL: [RowFilter; 5] = [
        RowFilter::NoFilter,
        RowFilter::Sub,
        RowFilter::Up,
        RowFilter::Avg,
        RowFilter::Paeth,
    ];
}

/// The filtering strategy an encoder should use.
///
/// The five fixed values force one filter for every scanline; the remaining
/// variants pick per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Filter {
    /// Never filter.
    NoFilter,
    /// Always use [`RowFilter::Sub`].
    Sub,
    /// Always use [`RowFilter::Up`].
    Up,
    /// Always use [`RowFilter::Avg`].
    Avg,
    /// Always use [`RowFilter::Paeth`].
    Paeth,
    /// Pick per row with libpng's minimum-sum-of-absolute-differences
    /// heuristic. This is the default.
    #[default]
    Adaptive,
    /// Pick per row with an entropy estimate, as `oxipng` does. Slower than
    /// [`Filter::Adaptive`] and usually a little smaller.
    MinEntropy,
}

impl Filter {
    /// The fixed [`RowFilter`] this strategy always uses, if it is a fixed one.
    #[must_use]
    pub fn fixed(self) -> Option<RowFilter> {
        match self {
            Filter::NoFilter => Some(RowFilter::NoFilter),
            Filter::Sub => Some(RowFilter::Sub),
            Filter::Up => Some(RowFilter::Up),
            Filter::Avg => Some(RowFilter::Avg),
            Filter::Paeth => Some(RowFilter::Paeth),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_filter_parsing() {
        for (n, want) in RowFilter::ALL.iter().enumerate() {
            assert_eq!(RowFilter::from_u8(n as u8).expect("valid"), *want);
            assert_eq!(want.into_u8(), n as u8);
        }
        assert!(RowFilter::from_u8(5).is_err());
        assert!(RowFilter::from_u8(255).is_err());
    }

    #[test]
    fn filter_strategy_fixed_mapping() {
        assert_eq!(Filter::Paeth.fixed(), Some(RowFilter::Paeth));
        assert_eq!(Filter::Adaptive.fixed(), None);
        assert_eq!(Filter::default(), Filter::Adaptive);
    }
}
