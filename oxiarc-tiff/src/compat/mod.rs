//! A `tiff`-0.11.3-shaped facade over this crate's native API.
//!
//! Migrating a consumer of the real `tiff` crate (or, transitively,
//! `image`'s `tiff` codec) is meant to be a mechanical import-path swap:
//!
//! ```text
//! - use tiff::decoder::{Decoder, DecodingResult};
//! - use tiff::{ColorType, TiffError};
//! - use tiff::tags::Tag;
//! + use oxiarc_tiff::compat::decoder::{Decoder, DecodingResult};
//! + use oxiarc_tiff::compat::{ColorType, TiffError};
//! + use oxiarc_tiff::compat::tags::Tag;
//! ```
//!
//! See the crate's top-level docs for the full migration guide, and
//! `tests/compat_api.rs` for the exact call sequence
//! `image-0.25.10/src/codecs/tiff.rs` performs against this module,
//! reproduced and asserted so an accidental shape break fails a test rather
//! than surfacing downstream.
//!
//! # What this is, and is not
//!
//! Every type and method here is a **thin adapter**: it translates
//! arguments and results at the boundary and calls straight into the native
//! [`crate::reader::Decoder`] / [`crate::writer::Encoder`] for the actual
//! work. There is no second decode or encode implementation in this module
//! -- a bug fixed in the native path is fixed here too, automatically.
//!
//! # Frozen shapes (critique.md section 2.11, "non-negotiable")
//!
//! * [`decoder::DecodingResult`] has **exactly** the eleven upstream
//!   variants and is **not** `#[non_exhaustive]`: `image` 0.25.10 matches it
//!   exhaustively, with no wildcard arm. Adding a twelfth variant, or
//!   marking it `#[non_exhaustive]`, is a breaking change to this module
//!   even though the crate as a whole treats new enum variants as additive.
//! * [`ColorType`] similarly keeps all ten upstream variants and stays
//!   exhaustively matchable.
//! * [`TiffError`] has exactly the six upstream variants, also not
//!   `#[non_exhaustive]`.
//! * [`decoder::DecodingResult::F16`] carries real `half::f16` values (the
//!   `half` dependency is pulled in **only** by this feature -- the native
//!   API's [`crate::Samples::F16`] stays raw `u16` bits, see its own docs).
//!
//! # Deliberate deviations from upstream (documented, not accidental)
//!
//! * [`encoder::ImageEncoder`]'s `resolution*` setters return
//!   [`TiffResult<()>`](error::TiffResult) instead of `unwrap()`-ing
//!   internally the way upstream's do (`encoder/mod.rs:826-851` in the real
//!   crate) -- `image` never calls them, so this is a safe, policy-required
//!   change (no `unwrap()` in library code).
//! * [`decoder::DecodingBuffer::to_bytes`] returns an owned `Vec<u8>`
//!   instead of upstream's zero-copy `&[u8]` view, because that view needs
//!   an `unsafe` reinterpret-cast this crate does not permit. See its own
//!   docs.
//! * `encoder::Predictor` is a re-export of [`tags::Predictor`] rather than
//!   a second, independent enum -- upstream has two nominally distinct
//!   `Predictor` types (`tags::Predictor` and `encoder::Predictor`) that are
//!   identical in shape; this module gives them one definition.

pub mod colortype;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod tags;

pub use error::{TiffError, TiffFormatError, TiffResult, TiffUnsupportedError, UsageError};

/// A directory of IFD entries.
///
/// A plain re-export of the native type ([`crate::Directory`]): both crates
/// use this purely as an opaque, iterable tag-value map, and duplicating the
/// (large) value-decoding logic behind a second `Directory` type would
/// violate this module's "thin adapter, never a second implementation"
/// rule.
pub type Directory = crate::Directory;

/// A coarse description of the decoded pixel layout, `tiff`-0.11 shaped.
///
/// **Exactly** the ten upstream variants; deliberately **not**
/// `#[non_exhaustive]` -- see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorType {
    /// One greyscale channel.
    Gray(u8),
    /// Red, green, blue.
    RGB(u8),
    /// Palette indices, expanded through a `ColorMap`.
    Palette(u8),
    /// Greyscale plus alpha.
    GrayA(u8),
    /// Red, green, blue, alpha.
    RGBA(u8),
    /// Cyan, magenta, yellow, black.
    CMYK(u8),
    /// Cyan, magenta, yellow, black, alpha.
    CMYKA(u8),
    /// Luma plus two chroma channels.
    YCbCr(u8),
    /// CIE L*a*b* (or an ICC/ITU flavour of it).
    Lab(u8),
    /// Anything else: `num_samples` channels of `bit_depth` bits.
    Multiband {
        /// Bits per channel.
        bit_depth: u8,
        /// Channels per pixel.
        num_samples: u16,
    },
}

impl ColorType {
    /// Bits per channel.
    #[must_use]
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::Gray(b)
            | Self::RGB(b)
            | Self::Palette(b)
            | Self::GrayA(b)
            | Self::RGBA(b)
            | Self::CMYK(b)
            | Self::CMYKA(b)
            | Self::YCbCr(b)
            | Self::Lab(b) => b,
            Self::Multiband { bit_depth, .. } => bit_depth,
        }
    }

    /// Channels per pixel.
    #[must_use]
    pub const fn num_samples(self) -> u16 {
        match self {
            Self::Gray(_) | Self::Palette(_) => 1,
            Self::GrayA(_) => 2,
            Self::RGB(_) | Self::YCbCr(_) | Self::Lab(_) => 3,
            Self::RGBA(_) | Self::CMYK(_) => 4,
            Self::CMYKA(_) => 5,
            Self::Multiband { num_samples, .. } => num_samples,
        }
    }

    pub(crate) fn from_native(ty: crate::ColorType) -> Self {
        match ty {
            crate::ColorType::Gray(b) => Self::Gray(b),
            crate::ColorType::Rgb(b) => Self::RGB(b),
            crate::ColorType::Palette(b) => Self::Palette(b),
            crate::ColorType::GrayA(b) => Self::GrayA(b),
            crate::ColorType::Rgba(b) => Self::RGBA(b),
            crate::ColorType::Cmyk(b) => Self::CMYK(b),
            crate::ColorType::CmykA(b) => Self::CMYKA(b),
            crate::ColorType::YCbCr(b) => Self::YCbCr(b),
            crate::ColorType::Lab(b) => Self::Lab(b),
            crate::ColorType::Multiband {
                bit_depth,
                num_samples,
            } => Self::Multiband {
                bit_depth,
                num_samples,
            },
            // `#[non_exhaustive]` on the native type is an external-crate
            // restriction only; from inside this crate, the match above is
            // already exhaustive over every variant that exists today. A
            // wildcard arm here would be unreachable (and clippy flags it),
            // so a future native variant is a compile error at this match,
            // not a silent fallback -- exactly the failure mode this
            // module's whole design tries to avoid.
        }
    }

    /// The equivalent native [`crate::ColorType`].
    #[must_use]
    pub const fn to_native(self) -> crate::ColorType {
        match self {
            Self::Gray(b) => crate::ColorType::Gray(b),
            Self::RGB(b) => crate::ColorType::Rgb(b),
            Self::Palette(b) => crate::ColorType::Palette(b),
            Self::GrayA(b) => crate::ColorType::GrayA(b),
            Self::RGBA(b) => crate::ColorType::Rgba(b),
            Self::CMYK(b) => crate::ColorType::Cmyk(b),
            Self::CMYKA(b) => crate::ColorType::CmykA(b),
            Self::YCbCr(b) => crate::ColorType::YCbCr(b),
            Self::Lab(b) => crate::ColorType::Lab(b),
            Self::Multiband {
                bit_depth,
                num_samples,
            } => crate::ColorType::Multiband {
                bit_depth,
                num_samples,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_type_matches_exhaustively_with_no_wildcard() {
        // Compiles only as long as `ColorType` stays exactly these ten
        // variants and non-`#[non_exhaustive]`.
        fn describe(c: ColorType) -> &'static str {
            match c {
                ColorType::Gray(_) => "gray",
                ColorType::RGB(_) => "rgb",
                ColorType::Palette(_) => "palette",
                ColorType::GrayA(_) => "graya",
                ColorType::RGBA(_) => "rgba",
                ColorType::CMYK(_) => "cmyk",
                ColorType::CMYKA(_) => "cmyka",
                ColorType::YCbCr(_) => "ycbcr",
                ColorType::Lab(_) => "lab",
                ColorType::Multiband { .. } => "multiband",
            }
        }
        assert_eq!(describe(ColorType::RGB(8)), "rgb");
        assert_eq!(
            describe(ColorType::Multiband {
                bit_depth: 16,
                num_samples: 5
            }),
            "multiband"
        );
    }

    #[test]
    fn bit_depth_and_num_samples_match_native() {
        assert_eq!(ColorType::RGBA(16).bit_depth(), 16);
        assert_eq!(ColorType::RGBA(16).num_samples(), 4);
        assert_eq!(ColorType::CMYKA(8).num_samples(), 5);
    }

    #[test]
    fn color_type_round_trips_through_the_native_type() {
        for ty in [
            ColorType::Gray(8),
            ColorType::RGB(16),
            ColorType::Palette(4),
            ColorType::GrayA(8),
            ColorType::RGBA(8),
            ColorType::CMYK(8),
            ColorType::CMYKA(16),
            ColorType::YCbCr(8),
            ColorType::Lab(8),
            ColorType::Multiband {
                bit_depth: 32,
                num_samples: 7,
            },
        ] {
            assert_eq!(ColorType::from_native(ty.to_native()), ty);
        }
    }
}
