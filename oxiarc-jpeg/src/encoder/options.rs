//! Encoder configuration.
//!
//! [`EncodeOptions`] is deliberately **not** `#[non_exhaustive]`, matching
//! [`crate::DecodeOptions`]: callers are expected to write
//! `EncodeOptions { quality: 90, ..Default::default() }`, which needs the
//! struct-update syntax to be legal outside this crate.

use crate::color::ColorSpace;
use crate::downsample::Downsampling;
use crate::frame::{ArithmeticConditioning, EntropyCoding};
use crate::quant::QuantTable;

/// How the caller's samples are laid out, and what they mean.
///
/// The variants that carry alpha exist because callers usually have RGBA
/// buffers; by default the alpha channel is dropped, since none of JPEG's
/// *named* colour spaces has a place to put it.
/// [`LumaAlpha`](InputColor::LumaAlpha) is the one exception: asking for
/// [`ColorSpace::Unknown(2)`](crate::ColorSpace::Unknown) instead of the
/// default `Luma` keeps the alpha channel, as a second, untransformed
/// component (see [`EncodeOptions::jpeg_color_space`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputColor {
    /// One channel of luminance.
    Luma,
    /// Luminance followed by an alpha channel.
    ///
    /// Dropped by default (the target colour space is `Luma`, one
    /// component); kept, as a second component with no colour transform, by
    /// asking for [`ColorSpace::Unknown(2)`](crate::ColorSpace::Unknown)
    /// explicitly.
    LumaAlpha,
    /// Red, green, blue.
    Rgb,
    /// Red, green, blue, alpha; alpha is discarded.
    Rgba,
    /// Blue, green, red.
    Bgr,
    /// Blue, green, red, alpha; alpha is discarded.
    Bgra,
    /// Samples that are already `Y`, `Cb`, `Cr`.
    Ycbcr,
    /// Cyan, magenta, yellow, black.
    Cmyk,
    /// Samples that are already `Y`, `Cb`, `Cr`, `K`.
    Ycck,
}

impl InputColor {
    /// Samples per pixel in the caller's buffer.
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            InputColor::Luma => 1,
            InputColor::LumaAlpha => 2,
            InputColor::Rgb | InputColor::Bgr | InputColor::Ycbcr => 3,
            InputColor::Rgba | InputColor::Bgra | InputColor::Cmyk | InputColor::Ycck => 4,
        }
    }

    /// The JPEG colour space libjpeg would choose for this input.
    ///
    /// Grey stays grey, RGB-like inputs become `YCbCr`, `CMYK` stays `CMYK`
    /// (Adobe's own encoder converts to `YCCK`, but libjpeg does not unless
    /// asked), and pre-converted inputs pass through.
    #[must_use]
    pub const fn default_jpeg_color_space(self) -> ColorSpace {
        match self {
            InputColor::Luma | InputColor::LumaAlpha => ColorSpace::Luma,
            InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra => {
                ColorSpace::Ycbcr
            }
            InputColor::Ycbcr => ColorSpace::Ycbcr,
            InputColor::Cmyk => ColorSpace::Cmyk,
            InputColor::Ycck => ColorSpace::Ycck,
        }
    }
}

/// Chroma sampling factors, named the way photographers name them.
///
/// The factors apply to the *luminance* component; chroma is always `1x1`,
/// which is how libjpeg and every other encoder spell subsampling. They are
/// honoured only for the `YCbCr` and `YCCK` colour spaces — an RGB or CMYK
/// frame has no chroma to decimate — unless [`Subsampling::Custom`] names
/// factors explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Subsampling {
    /// 4:4:4 — no decimation, luma `1x1`.
    S444,
    /// 4:2:2 — horizontal halving, luma `2x1`.
    S422,
    /// 4:4:0 — vertical halving, luma `1x2`.
    S440,
    /// 4:2:0 — both halved, luma `2x2`. libjpeg's default, and ours.
    #[default]
    S420,
    /// 4:1:1 — horizontal quartering, luma `4x1`.
    S411,
    /// Explicit `(H, V)` per component, in component order.
    Custom([(u8, u8); 4]),
}

impl Subsampling {
    /// The luminance sampling factors this ratio implies.
    #[must_use]
    pub const fn luma_factors(self) -> (u8, u8) {
        match self {
            Subsampling::S444 => (1, 1),
            Subsampling::S422 => (2, 1),
            Subsampling::S440 => (1, 2),
            Subsampling::S420 => (2, 2),
            Subsampling::S411 => (4, 1),
            Subsampling::Custom(factors) => factors[0],
        }
    }
}

/// Which coding process the frame uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EncodeProcess {
    /// Sequential DCT. Emits `SOF0` when the result is baseline-conformant
    /// and `SOF1` otherwise; see [`crate::Encoder`] for the exact rule.
    #[default]
    Sequential,
    /// Progressive DCT (`SOF2`), Annex G.
    Progressive,
    /// Lossless predictive (`SOF3`), Annex H.
    Lossless {
        /// Predictor selector `Psv`, `1..=7`.
        predictor: u8,
        /// Point transform `Pt`, `0..=15`; samples are shifted right by it.
        point_transform: u8,
    },
}

/// Where restart markers go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RestartInterval {
    /// No `DRI`, no `RSTn`.
    #[default]
    None,
    /// A restart marker every `n` MCUs, written straight into `DRI`.
    Mcus(u16),
    /// A restart marker every `n` MCU rows, which is `cjpeg -restart n`.
    ///
    /// The interval is resolved per scan, because a non-interleaved scan's
    /// MCU row is one block row of one component and not the frame's MCU row.
    McuRows(u16),
}

/// Which identifiers the `SOF` gives its components.
///
/// [`ComponentIds::Rgb`] writes `'R'`, `'G'`, `'B'` and
/// [`ComponentIds::Cmyk`] writes `'C'`, `'M'`, `'Y'`, `'K'`. Both are needed
/// to reproduce libtiff's `Compression = 7` output byte for byte: a TIFF strip
/// carries no `JFIF` and no `Adobe` marker, so the identifiers are the only
/// colour signal the decoder gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ComponentIds {
    /// The identifiers libjpeg picks for the colour space: `1..` for grey and
    /// `YCbCr`, `'R','G','B'` for RGB, `'C','M','Y','K'` for CMYK, `1..4` for
    /// `YCCK`.
    #[default]
    Auto,
    /// `1`, `2`, `3`, `4` regardless of colour space.
    Sequential,
    /// `'R'`, `'G'`, `'B'`.
    Rgb,
    /// `'C'`, `'M'`, `'Y'`, `'K'`.
    Cmyk,
    /// Caller-chosen identifiers, in component order.
    Custom([u8; 4]),
}

/// Where the quantisation tables come from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum QuantTableSource {
    /// Annex K's example tables scaled by [`EncodeOptions::quality`], which is
    /// what every reference encoder does by default.
    #[default]
    AnnexK,
    /// Caller-supplied tables, used verbatim — [`EncodeOptions::quality`] is
    /// **not** applied. Scale them yourself with
    /// [`QuantTable::scaled_for_quality`] if you want that.
    ///
    /// Every value must be `1..=65535`: T.81 B.2.4.1 has no zero quantiser,
    /// and one would divide by zero. A table carrying one is rejected with
    /// [`crate::JpegError::InvalidEncodeParameter`], as is a slot a component
    /// references but the array leaves empty.
    Custom(Box<[Option<QuantTable>; 4]>),
    /// Every entry of every table set to one value. `Flat(1)` is the finest
    /// quantisation a JPEG can express.
    Flat(u16),
}

/// Whether a metadata marker is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MarkerPolicy {
    /// Write it when libjpeg would: `JFIF` for grey and `YCbCr`, `Adobe` for
    /// RGB, `CMYK` and `YCCK`.
    #[default]
    Auto,
    /// Always write it.
    Always,
    /// Never write it. TIFF `Compression = 7` strips need this for both
    /// markers.
    Never,
}

/// The pixel density recorded in the `JFIF` `APP0` segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Density {
    /// `0` = aspect ratio only, `1` = pixels per inch, `2` = pixels per cm.
    pub units: u8,
    /// Horizontal density.
    pub x: u16,
    /// Vertical density.
    pub y: u16,
}

impl Default for Density {
    fn default() -> Self {
        Self {
            units: 0,
            x: 1,
            y: 1,
        }
    }
}

/// One scan of a progressive scan script (T.81 Annex G.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSpec {
    /// Indices of the components in this scan, in scan order. A DC scan may
    /// interleave up to four; an AC scan must name exactly one.
    pub components: Vec<usize>,
    /// `Ss`, the first coefficient of the band.
    pub spectral_start: u8,
    /// `Se`, the last coefficient of the band.
    pub spectral_end: u8,
    /// `Ah`, the bit position already sent (`0` for a first pass).
    pub approx_high: u8,
    /// `Al`, the bit position this scan sends.
    pub approx_low: u8,
}

impl ScanSpec {
    /// A DC scan over `components`.
    #[must_use]
    pub fn dc(components: Vec<usize>, approx_high: u8, approx_low: u8) -> Self {
        Self {
            components,
            spectral_start: 0,
            spectral_end: 0,
            approx_high,
            approx_low,
        }
    }

    /// An AC scan over one component and one spectral band.
    #[must_use]
    pub fn ac(
        component: usize,
        spectral_start: u8,
        spectral_end: u8,
        approx_high: u8,
        approx_low: u8,
    ) -> Self {
        Self {
            components: vec![component],
            spectral_start,
            spectral_end,
            approx_high,
            approx_low,
        }
    }

    /// `true` when this scan carries DC coefficients.
    #[must_use]
    pub fn is_dc(&self) -> bool {
        self.spectral_start == 0
    }
}

/// Everything the encoder needs beyond the pixels themselves.
///
/// # Examples
///
/// ```
/// use oxiarc_jpeg::{EncodeOptions, Subsampling};
///
/// let options = EncodeOptions {
///     quality: 90,
///     subsampling: Subsampling::S444,
///     ..Default::default()
/// };
/// assert_eq!(options.quality, 90);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Quality `1..=100`, scaled into the quantisation tables by libjpeg's
    /// `jpeg_quality_scaling`. Values outside the range are clamped, as
    /// libjpeg clamps them.
    pub quality: u8,
    /// Which quantisation tables to use.
    pub quant_tables: QuantTableSource,
    /// Chroma sampling factors.
    pub subsampling: Subsampling,
    /// The decimation filter. libjpeg's default is a box average;
    /// [`Downsampling::Smooth`] is its `-smooth N`.
    pub downsampling: Downsampling,
    /// Sequential, progressive or lossless.
    pub process: EncodeProcess,
    /// Generate Huffman tables from the image's own statistics
    /// (`cjpeg -optimize`) instead of using Annex K.3's.
    ///
    /// Forced on for progressive frames and for sample precisions above
    /// eight, both of which libjpeg also forces, because Annex K.3's tables
    /// cannot encode the symbols those produce.
    pub optimize_huffman: bool,
    /// Restart marker spacing.
    pub restart_interval: RestartInterval,
    /// Sample precision `P`: `8` or `12` for DCT processes, `2..=16` for
    /// lossless.
    pub precision: u8,
    /// Clamp quantiser values to `1..=255` so the frame stays baseline.
    ///
    /// `false` (the default, and `cjpeg`'s) allows 16-bit `DQT` entries, which
    /// low qualities need; the frame is then written as `SOF1`.
    pub force_baseline: bool,
    /// Override the JPEG colour space. `None` takes
    /// [`InputColor::default_jpeg_color_space`].
    pub jpeg_color_space: Option<ColorSpace>,
    /// Which identifiers the `SOF` gives its components.
    pub component_ids: ComponentIds,
    /// Whether to write the `JFIF` `APP0` segment.
    pub write_jfif: MarkerPolicy,
    /// Whether to write the Adobe `APP14` segment.
    pub write_adobe: MarkerPolicy,
    /// Density recorded in `APP0`.
    pub density: Density,
    /// A custom progressive scan script. `None` uses libjpeg's
    /// `jpeg_simple_progression`.
    pub progressive_script: Option<Vec<ScanSpec>>,
    /// Huffman or arithmetic entropy coding.
    ///
    /// [`EntropyCoding::Arithmetic`] writes `SOF9`, `SOF10` or `SOF11` and a
    /// `DAC` segment per scan, exactly as `cjpeg -arithmetic` does, and needs
    /// no Huffman tables at all — [`EncodeOptions::optimize_huffman`] is then
    /// ignored. Requires the `arithmetic` feature (on by default); without it
    /// the encoder returns [`crate::UnsupportedFeature::ArithmeticCoding`].
    pub entropy: EntropyCoding,
    /// Conditioning bounds written in the `DAC` segment.
    ///
    /// The default is T.81's (`L = 0`, `U = 1`, `Kx = 5`), which is what
    /// every libjpeg-derived encoder writes. Only meaningful when
    /// `entropy` is [`EntropyCoding::Arithmetic`], and only then is it
    /// checked: T.81 B.2.4.3 requires `0 <= L <= U <= 15` for each DC slot and
    /// `1 <= Kx <= 63` for each AC one, and anything else is rejected with
    /// [`crate::JpegError::InvalidEncodeParameter`] rather than written into a
    /// `DAC` segment no conforming decoder would accept.
    pub arithmetic: ArithmeticConditioning,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: 75,
            quant_tables: QuantTableSource::AnnexK,
            subsampling: Subsampling::S420,
            downsampling: Downsampling::Box,
            process: EncodeProcess::Sequential,
            optimize_huffman: false,
            restart_interval: RestartInterval::None,
            precision: 8,
            force_baseline: false,
            jpeg_color_space: None,
            component_ids: ComponentIds::Auto,
            write_jfif: MarkerPolicy::Auto,
            write_adobe: MarkerPolicy::Auto,
            density: Density::default(),
            progressive_script: None,
            entropy: EntropyCoding::Huffman,
            arithmetic: ArithmeticConditioning::default(),
        }
    }
}

impl EncodeOptions {
    /// The options a TIFF `Compression = 7` strip needs: no `JFIF`, no
    /// `Adobe`, and identifiers that carry the colour space instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::{EncodeOptions, MarkerPolicy};
    ///
    /// let options = EncodeOptions::tiff_strip(75);
    /// assert_eq!(options.write_jfif, MarkerPolicy::Never);
    /// assert_eq!(options.write_adobe, MarkerPolicy::Never);
    /// ```
    #[must_use]
    pub fn tiff_strip(quality: u8) -> Self {
        Self {
            quality,
            write_jfif: MarkerPolicy::Never,
            write_adobe: MarkerPolicy::Never,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_cjpeg() {
        let options = EncodeOptions::default();
        assert_eq!(options.quality, 75);
        assert_eq!(options.subsampling, Subsampling::S420);
        assert_eq!(options.precision, 8);
        assert!(!options.force_baseline, "cjpeg allows 16-bit quantisers");
        assert!(!options.optimize_huffman);
    }

    #[test]
    fn input_colours_report_their_channel_counts() {
        assert_eq!(InputColor::Luma.channels(), 1);
        assert_eq!(InputColor::LumaAlpha.channels(), 2);
        assert_eq!(InputColor::Rgb.channels(), 3);
        assert_eq!(InputColor::Rgba.channels(), 4);
        assert_eq!(InputColor::Cmyk.channels(), 4);
    }

    #[test]
    fn default_colour_spaces_follow_libjpeg() {
        assert_eq!(
            InputColor::Rgb.default_jpeg_color_space(),
            ColorSpace::Ycbcr
        );
        assert_eq!(
            InputColor::Cmyk.default_jpeg_color_space(),
            ColorSpace::Cmyk
        );
        assert_eq!(
            InputColor::LumaAlpha.default_jpeg_color_space(),
            ColorSpace::Luma
        );
    }

    #[test]
    fn subsampling_names_map_to_factors() {
        assert_eq!(Subsampling::S444.luma_factors(), (1, 1));
        assert_eq!(Subsampling::S422.luma_factors(), (2, 1));
        assert_eq!(Subsampling::S440.luma_factors(), (1, 2));
        assert_eq!(Subsampling::S420.luma_factors(), (2, 2));
        assert_eq!(Subsampling::S411.luma_factors(), (4, 1));
    }

    #[test]
    fn scan_spec_helpers_build_the_expected_shapes() {
        let dc = ScanSpec::dc(vec![0, 1, 2], 0, 1);
        assert!(dc.is_dc());
        assert_eq!((dc.spectral_start, dc.spectral_end), (0, 0));
        let ac = ScanSpec::ac(0, 1, 5, 0, 2);
        assert!(!ac.is_dc());
        assert_eq!(ac.components, vec![0]);
    }

    #[test]
    fn struct_update_syntax_works() {
        let options = EncodeOptions {
            quality: 42,
            ..Default::default()
        };
        assert_eq!(options.quality, 42);
        assert_eq!(options.subsampling, Subsampling::S420);
    }
}
