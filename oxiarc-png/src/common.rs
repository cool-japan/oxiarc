//! Shared PNG value types.
//!
//! These mirror the shapes of the `png` crate's `common` module — same names,
//! same variants, same numeric values — with a few additive extensions, all of
//! which are listed in the crate-level
//! [compatibility table](crate#compatibility-with-the-png-crate).

use std::fmt;

/// Output transformations requested from the decoder.
///
/// This is a bit set. The four values that the `png` crate defines have
/// identical numeric values here, so code that writes
/// `Transformations::EXPAND | Transformations::STRIP_16` keeps working.
///
/// ```
/// use oxiarc_png::Transformations;
/// let t = Transformations::normalize_to_color8();
/// assert!(t.contains(Transformations::EXPAND));
/// assert!(t.contains(Transformations::STRIP_16));
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Default)]
pub struct Transformations(u32);

impl Transformations {
    /// No transformation: rows are delivered in the file's own colour type and
    /// bit depth.
    pub const IDENTITY: Transformations = Transformations(0x0_0000);
    /// Strip 16-bit samples down to 8 bits by keeping the high byte.
    pub const STRIP_16: Transformations = Transformations(0x0_0001);
    /// Expand palette images to RGB, sub-byte grayscale to 8-bit grayscale and
    /// a `tRNS` chunk into a real alpha channel.
    pub const EXPAND: Transformations = Transformations(0x0_0010);
    /// Always produce an alpha channel. Implies [`Transformations::EXPAND`].
    pub const ALPHA: Transformations = Transformations(0x1_0000);

    /// Transform every input into 8-bit grayscale or colour.
    #[must_use]
    pub fn normalize_to_color8() -> Transformations {
        Transformations::EXPAND | Transformations::STRIP_16
    }

    /// Raw bit representation.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Construct from a raw bit representation, keeping only known bits.
    #[must_use]
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Transformations(bits & (0x0_0001 | 0x0_0010 | 0x1_0000))
    }

    /// True if every bit of `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Transformations) -> bool {
        (self.0 & other.0) == other.0
    }

    /// True if any bit of `other` is set in `self`.
    #[must_use]
    pub const fn intersects(self, other: Transformations) -> bool {
        (self.0 & other.0) != 0
    }

    /// True if no transformation at all is requested.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Transformations {
    type Output = Transformations;
    fn bitor(self, rhs: Transformations) -> Transformations {
        Transformations(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Transformations {
    fn bitor_assign(&mut self, rhs: Transformations) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for Transformations {
    type Output = Transformations;
    fn bitand(self, rhs: Transformations) -> Transformations {
        Transformations(self.0 & rhs.0)
    }
}

impl std::ops::Sub for Transformations {
    type Output = Transformations;
    fn sub(self, rhs: Transformations) -> Transformations {
        Transformations(self.0 & !rhs.0)
    }
}

/// Physical pixel dimensions, from the `pHYs` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelDimensions {
    /// Pixels per unit, horizontal.
    pub xppu: u32,
    /// Pixels per unit, vertical.
    pub yppu: u32,
    /// The unit `xppu`/`yppu` are expressed in.
    pub unit: Unit,
}

/// Physical unit of a [`PixelDimensions`] measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Unit {
    /// The unit is unknown; only the aspect ratio is meaningful.
    Unspecified = 0,
    /// Pixels per metre.
    Meter = 1,
}

impl Unit {
    /// Parse from the byte stored in a `pHYs` chunk.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<Unit> {
        match n {
            0 => Some(Unit::Unspecified),
            1 => Some(Unit::Meter),
            _ => None,
        }
    }
}

/// How an APNG frame's buffer is disposed of before the next frame is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DisposeOp {
    /// Leave the output buffer as it is.
    None = 0,
    /// Clear the frame's region to fully transparent black.
    Background = 1,
    /// Restore the region to its contents before this frame was rendered.
    Previous = 2,
}

impl DisposeOp {
    /// Parse from the byte stored in an `fcTL` chunk.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<DisposeOp> {
        match n {
            0 => Some(DisposeOp::None),
            1 => Some(DisposeOp::Background),
            2 => Some(DisposeOp::Previous),
            _ => None,
        }
    }
}

impl fmt::Display for DisposeOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            DisposeOp::None => "DISPOSE_OP_NONE",
            DisposeOp::Background => "DISPOSE_OP_BACKGROUND",
            DisposeOp::Previous => "DISPOSE_OP_PREVIOUS",
        };
        write!(f, "{name}")
    }
}

/// How an APNG frame is combined with the current canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BlendOp {
    /// Overwrite the region, alpha included.
    Source = 0,
    /// Composite the frame over the canvas with standard source-over alpha.
    Over = 1,
}

impl BlendOp {
    /// Parse from the byte stored in an `fcTL` chunk.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<BlendOp> {
        match n {
            0 => Some(BlendOp::Source),
            1 => Some(BlendOp::Over),
            _ => None,
        }
    }
}

impl fmt::Display for BlendOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            BlendOp::Source => "BLEND_OP_SOURCE",
            BlendOp::Over => "BLEND_OP_OVER",
        };
        write!(f, "{name}")
    }
}

/// The `fcTL` frame-control chunk of an APNG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameControl {
    /// Sequence number of this chunk in the shared `fcTL`/`fdAT` counter.
    pub sequence_number: u32,
    /// Width of the sub-frame.
    pub width: u32,
    /// Height of the sub-frame.
    pub height: u32,
    /// X position of the sub-frame on the canvas.
    pub x_offset: u32,
    /// Y position of the sub-frame on the canvas.
    pub y_offset: u32,
    /// Frame delay numerator, in seconds.
    pub delay_num: u16,
    /// Frame delay denominator. A value of `0` is to be read as `100`.
    pub delay_den: u16,
    /// Disposal to apply after this frame has been rendered.
    pub dispose_op: DisposeOp,
    /// How this frame is combined with the canvas.
    pub blend_op: BlendOp,
}

impl Default for FrameControl {
    fn default() -> FrameControl {
        FrameControl {
            sequence_number: 0,
            width: 0,
            height: 0,
            x_offset: 0,
            y_offset: 0,
            delay_num: 0,
            delay_den: 0,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
        }
    }
}

impl FrameControl {
    /// Set the frame delay.
    pub fn set_seq_num(&mut self, s: u32) {
        self.sequence_number = s;
    }

    /// Increment the sequence number by `n`.
    pub fn inc_seq_num(&mut self, n: u32) {
        self.sequence_number += n;
    }

    /// The frame delay as a [`std::time::Duration`].
    ///
    /// A `delay_den` of zero is interpreted as `100`, per the APNG
    /// specification.
    #[must_use]
    pub fn delay(&self) -> std::time::Duration {
        let den = if self.delay_den == 0 {
            100u32
        } else {
            u32::from(self.delay_den)
        };
        std::time::Duration::from_secs_f64(f64::from(self.delay_num) / f64::from(den))
    }
}

/// The `acTL` animation-control chunk of an APNG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimationControl {
    /// Number of frames in the animation.
    pub num_frames: u32,
    /// Number of times to loop, `0` meaning "forever".
    pub num_plays: u32,
}

/// An unsigned integer scaled version of a floating point value, equivalent to
/// an integer quotient with a fixed denominator of [`ScaledFloat::SCALING`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaledFloat(u32);

impl ScaledFloat {
    /// The fixed denominator: values are stored multiplied by `100_000`.
    pub const SCALING: f32 = 100_000.0;

    /// Whether `value` is inside the representable range.
    #[must_use]
    pub fn in_range(value: f32) -> bool {
        value >= 0.0 && (value * Self::SCALING).floor() <= u32::MAX as f32
    }

    /// Whether `value` survives a round trip exactly.
    #[must_use]
    pub fn exact(value: f32) -> bool {
        let there = Self::forward(value);
        let back = Self::reverse(there);
        #[allow(clippy::float_cmp)]
        {
            value == back
        }
    }

    fn forward(value: f32) -> u32 {
        (value.max(0.0) * Self::SCALING).floor() as u32
    }

    fn reverse(encoded: u32) -> f32 {
        encoded as f32 / Self::SCALING
    }

    /// Quantize a floating point value, clamping it into range.
    #[must_use]
    pub fn new(value: f32) -> Self {
        Self(Self::forward(value))
    }

    /// Construct from an already-scaled value, as stored in a chunk.
    #[must_use]
    pub fn from_scaled(val: u32) -> Self {
        Self(val)
    }

    /// The scaled value, as stored in a chunk.
    #[must_use]
    pub fn into_scaled(self) -> u32 {
        self.0
    }

    /// The unscaled value.
    #[must_use]
    pub fn into_value(self) -> f32 {
        Self::reverse(self.0)
    }
}

/// Chromaticities of the source system, from a `cHRM` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceChromaticities {
    /// White point (x, y).
    pub white: (ScaledFloat, ScaledFloat),
    /// Red primary (x, y).
    pub red: (ScaledFloat, ScaledFloat),
    /// Green primary (x, y).
    pub green: (ScaledFloat, ScaledFloat),
    /// Blue primary (x, y).
    pub blue: (ScaledFloat, ScaledFloat),
}

impl SourceChromaticities {
    /// Construct from four (x, y) pairs.
    #[must_use]
    pub fn new(
        white: (ScaledFloat, ScaledFloat),
        red: (ScaledFloat, ScaledFloat),
        green: (ScaledFloat, ScaledFloat),
        blue: (ScaledFloat, ScaledFloat),
    ) -> Self {
        SourceChromaticities {
            white,
            red,
            green,
            blue,
        }
    }

    /// The sRGB chromaticities, used as the replacement values when an `sRGB`
    /// chunk is present.
    #[must_use]
    pub fn from_srgb() -> Self {
        let s = ScaledFloat::from_scaled;
        SourceChromaticities {
            white: (s(31270), s(32900)),
            red: (s(64000), s(33000)),
            green: (s(30000), s(60000)),
            blue: (s(15000), s(6000)),
        }
    }

    /// The eight scaled values in chunk order.
    #[must_use]
    pub fn to_be_bytes(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        let vals = [
            self.white.0,
            self.white.1,
            self.red.0,
            self.red.1,
            self.green.0,
            self.green.1,
            self.blue.0,
            self.blue.1,
        ];
        for (i, v) in vals.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.into_scaled().to_be_bytes());
        }
        out
    }
}

/// The rendering intent of an sRGB image, from an `sRGB` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SrgbRenderingIntent {
    /// For images preferring good adaptation to the output device gamut.
    Perceptual = 0,
    /// For images requiring colour appearance matching.
    RelativeColorimetric = 1,
    /// For images preferring preservation of saturation.
    Saturation = 2,
    /// For images requiring preservation of absolute colorimetry.
    AbsoluteColorimetric = 3,
}

impl SrgbRenderingIntent {
    /// The byte as stored in the chunk.
    #[must_use]
    pub fn into_raw(self) -> u8 {
        self as u8
    }

    /// Parse from the byte stored in an `sRGB` chunk.
    #[must_use]
    pub fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(SrgbRenderingIntent::Perceptual),
            1 => Some(SrgbRenderingIntent::RelativeColorimetric),
            2 => Some(SrgbRenderingIntent::Saturation),
            3 => Some(SrgbRenderingIntent::AbsoluteColorimetric),
            _ => None,
        }
    }
}

/// Coding-independent code points, from a `cICP` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodingIndependentCodePoints {
    /// Rec. ITU-T H.273 ColourPrimaries code point.
    pub color_primaries: u8,
    /// Rec. ITU-T H.273 TransferCharacteristics code point.
    pub transfer_function: u8,
    /// Rec. ITU-T H.273 MatrixCoefficients code point. PNG requires `0`.
    pub matrix_coefficients: u8,
    /// Whether the image is full-range (`true`) or narrow-range video.
    pub is_video_full_range_image: bool,
}

/// Mastering display colour volume, from an `mDCv` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasteringDisplayColorVolume {
    /// Chromaticities of the mastering display.
    pub chromaticities: SourceChromaticities,
    /// Maximum luminance, in units of 0.0001 cd/m^2.
    pub max_luminance: u32,
    /// Minimum luminance, in units of 0.0001 cd/m^2.
    pub min_luminance: u32,
}

/// Content light level information, from a `cLLi` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLightLevelInfo {
    /// Maximum content light level, in units of 0.0001 cd/m^2.
    pub max_content_light_level: u32,
    /// Maximum frame-average light level, in units of 0.0001 cd/m^2.
    pub max_frame_average_light_level: u32,
}

/// Compression level presets, mirroring the `png` crate's enum.
///
/// These map onto `oxiarc-deflate` levels 0-9 rather than onto `flate2`
/// levels, so the produced bytes differ from the `png` crate's while the
/// semantics are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Compression {
    /// Store the data without compressing it (DEFLATE level 0).
    NoCompression,
    /// The fastest setting (level 1).
    Fastest,
    /// Fast, still compressing (level 3).
    Fast,
    /// The default trade-off (level 6).
    #[default]
    Balanced,
    /// The smallest output this crate produces without an optimal parser
    /// (level 9).
    High,
}

/// A DEFLATE compression setting with the raw level exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DeflateCompression {
    /// Store, do not compress.
    NoCompression,
    /// The fastest compressing setting available.
    FdeflateUltraFast,
    /// An explicit `oxiarc-deflate` level in `0..=9`.
    Level(u8),
    /// The default: level 6.
    #[default]
    Balanced,
}

impl DeflateCompression {
    /// The `oxiarc-deflate` level this setting maps to.
    #[must_use]
    pub fn level(self) -> u8 {
        match self {
            DeflateCompression::NoCompression => 0,
            DeflateCompression::FdeflateUltraFast => 1,
            DeflateCompression::Level(n) => n.min(9),
            DeflateCompression::Balanced => 6,
        }
    }

    /// Build from a [`Compression`] preset.
    #[must_use]
    pub fn from_compression(c: Compression) -> Self {
        match c {
            Compression::NoCompression => DeflateCompression::NoCompression,
            Compression::Fastest => DeflateCompression::Level(1),
            Compression::Fast => DeflateCompression::Level(3),
            Compression::Balanced => DeflateCompression::Level(6),
            Compression::High => DeflateCompression::Level(9),
        }
    }
}

/// The `sTER` stereo-image indicator chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StereoLayout {
    /// Cross-fuse layout: the right-eye image is on the left.
    CrossFuse = 0,
    /// Diverging-fuse layout: the left-eye image is on the left.
    DivergingFuse = 1,
}

impl StereoLayout {
    /// Parse from the byte stored in an `sTER` chunk.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<StereoLayout> {
        match n {
            0 => Some(StereoLayout::CrossFuse),
            1 => Some(StereoLayout::DivergingFuse),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transformations_bit_values_match_png() {
        assert_eq!(Transformations::IDENTITY.bits(), 0);
        assert_eq!(Transformations::STRIP_16.bits(), 0x1);
        assert_eq!(Transformations::EXPAND.bits(), 0x10);
        assert_eq!(Transformations::ALPHA.bits(), 0x1_0000);
        assert_eq!(Transformations::normalize_to_color8().bits(), 0x11);
    }

    #[test]
    fn transformations_set_ops() {
        let t = Transformations::EXPAND | Transformations::ALPHA;
        assert!(t.contains(Transformations::EXPAND));
        assert!(t.intersects(Transformations::ALPHA));
        assert!(!t.contains(Transformations::STRIP_16));
        assert!((t - Transformations::ALPHA).contains(Transformations::EXPAND));
        assert!(Transformations::IDENTITY.is_empty());
    }

    #[test]
    fn frame_delay_treats_zero_denominator_as_100() {
        let mut fc = FrameControl {
            delay_num: 50,
            delay_den: 0,
            ..FrameControl::default()
        };
        assert_eq!(fc.delay(), std::time::Duration::from_millis(500));
        fc.delay_den = 1000;
        assert_eq!(fc.delay(), std::time::Duration::from_millis(50));
    }

    #[test]
    fn scaled_float_round_trip() {
        let v = ScaledFloat::from_scaled(45455);
        assert_eq!(v.into_scaled(), 45455);
        assert!((v.into_value() - 0.45455).abs() < 1e-6);
        assert!(ScaledFloat::in_range(1.0));
        assert!(!ScaledFloat::in_range(-1.0));
    }

    #[test]
    fn srgb_chromaticities_are_the_spec_values() {
        let c = SourceChromaticities::from_srgb();
        assert_eq!(c.white.0.into_scaled(), 31270);
        assert_eq!(c.blue.1.into_scaled(), 6000);
        assert_eq!(c.to_be_bytes().len(), 32);
    }

    #[test]
    fn dispose_blend_parse() {
        assert_eq!(DisposeOp::from_u8(2), Some(DisposeOp::Previous));
        assert_eq!(DisposeOp::from_u8(3), None);
        assert_eq!(BlendOp::from_u8(1), Some(BlendOp::Over));
        assert_eq!(BlendOp::from_u8(2), None);
        assert_eq!(
            format!("{}", DisposeOp::Background),
            "DISPOSE_OP_BACKGROUND"
        );
        assert_eq!(format!("{}", BlendOp::Source), "BLEND_OP_SOURCE");
    }

    #[test]
    fn deflate_compression_levels() {
        assert_eq!(
            DeflateCompression::from_compression(Compression::High).level(),
            9
        );
        assert_eq!(DeflateCompression::Level(42).level(), 9);
        assert_eq!(DeflateCompression::default().level(), 6);
    }
}
