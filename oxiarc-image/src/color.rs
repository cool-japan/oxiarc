//! Colour types and pixel representations, shaped after `image` 0.25's
//! [`ColorType`]/[`ExtendedColorType`]/[`Pixel`]/[`Luma`]/[`LumaA`]/[`Rgb`]/
//! [`Rgba`].
//!
//! # Deviation from `image`: no zero-copy `Pixel::from_slice`
//!
//! The real crate reinterprets a `&[Subpixel]` of the right length as a
//! `&Pixel` in place; that cast has no safe expression in Rust (there is no
//! `TryFrom<&[T]> for &SomeStruct([T; N])`), so it is implemented with an
//! `unsafe` pointer cast. This crate is `#![forbid(unsafe_code)]`, so
//! [`Pixel`] only ever hands out *owned* pixel values — see
//! [`crate::ImageBuffer::pixel_slice`] for the zero-copy alternative when a
//! caller needs one.

/// A numeric sample type a pixel's channels are stored in.
///
/// A closed, `sealed`-style trait: implemented only for `u8`, `u16` and
/// `f32`, the three sample types this crate's typed buffers use.
pub trait Primitive: Copy + Clone + PartialEq + std::fmt::Debug + Default + 'static {
    /// The value representing "fully on" for this sample type: `255` for
    /// `u8`, `65535` for `u16`, `1.0` for `f32`.
    const DEFAULT_MAX_VALUE: Self;
    /// The value representing "fully off": `0` for every implementor.
    const DEFAULT_MIN_VALUE: Self;
}

impl Primitive for u8 {
    const DEFAULT_MAX_VALUE: Self = u8::MAX;
    const DEFAULT_MIN_VALUE: Self = 0;
}

impl Primitive for u16 {
    const DEFAULT_MAX_VALUE: Self = u16::MAX;
    const DEFAULT_MIN_VALUE: Self = 0;
}

impl Primitive for f32 {
    const DEFAULT_MAX_VALUE: Self = 1.0;
    const DEFAULT_MIN_VALUE: Self = 0.0;
}

/// Scale one sample from `S::DEFAULT_MAX_VALUE` down to a `u8`.
pub(crate) fn sample_to_u8<S: Primitive + Into<f64>>(v: S) -> u8 {
    let max: f64 = S::DEFAULT_MAX_VALUE.into();
    if max <= 0.0 {
        return 0;
    }
    let scaled = (v.into() / max) * 255.0;
    scaled.round().clamp(0.0, 255.0) as u8
}

/// Scale one sample from `S::DEFAULT_MAX_VALUE` up to a `u16`.
pub(crate) fn sample_to_u16<S: Primitive + Into<f64>>(v: S) -> u16 {
    let max: f64 = S::DEFAULT_MAX_VALUE.into();
    if max <= 0.0 {
        return 0;
    }
    let scaled = (v.into() / max) * 65535.0;
    scaled.round().clamp(0.0, 65535.0) as u16
}

/// A pixel type: a fixed number of channels of one [`Primitive`] sample
/// type.
///
/// Implemented by [`Luma`], [`LumaA`], [`Rgb`] and [`Rgba`], each over `u8`,
/// `u16` and `f32`. Every method returns or accepts *owned* pixel values —
/// see the module docs for why.
pub trait Pixel: Copy + Clone + PartialEq + std::fmt::Debug + Default {
    /// The sample type each channel is stored in.
    type Subpixel: Primitive;
    /// Number of channels (1 for [`Luma`], 4 for [`Rgba`], ...).
    const CHANNEL_COUNT: u8;
    /// This pixel's channels, in storage order.
    fn channels(&self) -> &[Self::Subpixel];
    /// This pixel's channels, mutably, in storage order.
    fn channels_mut(&mut self) -> &mut [Self::Subpixel];
    /// Build a pixel by copying exactly [`Self::CHANNEL_COUNT`] samples out
    /// of `slice`.
    ///
    /// # Panics
    /// If `slice.len() != Self::CHANNEL_COUNT as usize`. Every caller in
    /// this crate slices a buffer to the exact channel count first, so the
    /// panic is unreachable in practice; it exists to catch a genuine
    /// programming error rather than to be a documented failure mode.
    fn from_slice(slice: &[Self::Subpixel]) -> Self;
}

macro_rules! define_pixel {
    ($name:ident, $count:literal, $doc:literal, [$($field:ident),+]) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
        pub struct $name<T>(pub [T; $count]);

        impl<T: Primitive> $name<T> {
            #[doc = concat!("Build a new `", stringify!($name), "` from its channels.")]
            #[must_use]
            pub const fn new($($field: T),+) -> Self {
                Self([$($field),+])
            }
        }

        impl<T: Primitive> Pixel for $name<T> {
            type Subpixel = T;
            const CHANNEL_COUNT: u8 = $count;

            fn channels(&self) -> &[T] {
                &self.0
            }

            fn channels_mut(&mut self) -> &mut [T] {
                &mut self.0
            }

            fn from_slice(slice: &[T]) -> Self {
                assert_eq!(
                    slice.len(),
                    $count,
                    concat!(stringify!($name), "::from_slice needs exactly ", $count, " samples")
                );
                let mut out = [T::DEFAULT_MIN_VALUE; $count];
                out.copy_from_slice(slice);
                Self(out)
            }
        }

        impl<T> std::ops::Index<usize> for $name<T> {
            type Output = T;
            fn index(&self, i: usize) -> &T {
                &self.0[i]
            }
        }

        impl<T> std::ops::IndexMut<usize> for $name<T> {
            fn index_mut(&mut self, i: usize) -> &mut T {
                &mut self.0[i]
            }
        }
    };
}

define_pixel!(Luma, 1, "One channel: luminance.", [l]);
define_pixel!(LumaA, 2, "Two channels: luminance, alpha.", [l, a]);
define_pixel!(Rgb, 3, "Three channels: red, green, blue.", [r, g, b]);
define_pixel!(
    Rgba,
    4,
    "Four channels: red, green, blue, alpha.",
    [r, g, b, a]
);

/// An enumeration of the pixel layouts a [`crate::DynamicImage`] can hold.
///
/// The ten variants match `image` 0.25's `ColorType` exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum ColorType {
    /// 8-bit luminance.
    L8,
    /// 8-bit luminance with alpha.
    La8,
    /// 8-bit red, green, blue.
    Rgb8,
    /// 8-bit red, green, blue, alpha.
    Rgba8,
    /// 16-bit luminance.
    L16,
    /// 16-bit luminance with alpha.
    La16,
    /// 16-bit red, green, blue.
    Rgb16,
    /// 16-bit red, green, blue, alpha.
    Rgba16,
    /// 32-bit float red, green, blue.
    Rgb32F,
    /// 32-bit float red, green, blue, alpha.
    Rgba32F,
}

impl ColorType {
    /// Bytes one pixel occupies.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> u8 {
        match self {
            Self::L8 => 1,
            Self::L16 | Self::La8 => 2,
            Self::Rgb8 => 3,
            Self::Rgba8 | Self::La16 => 4,
            Self::Rgb16 => 6,
            Self::Rgba16 => 8,
            Self::Rgb32F => 12,
            Self::Rgba32F => 16,
        }
    }

    /// Bits one pixel occupies (always a multiple of 8).
    #[must_use]
    pub const fn bits_per_pixel(self) -> u16 {
        self.bytes_per_pixel() as u16 * 8
    }

    /// Number of channels.
    #[must_use]
    pub const fn channel_count(self) -> u8 {
        match self {
            Self::L8 | Self::L16 => 1,
            Self::La8 | Self::La16 => 2,
            Self::Rgb8 | Self::Rgb16 | Self::Rgb32F => 3,
            Self::Rgba8 | Self::Rgba16 | Self::Rgba32F => 4,
        }
    }

    /// Whether this colour type carries an alpha channel.
    #[must_use]
    pub const fn has_alpha(self) -> bool {
        matches!(
            self,
            Self::La8 | Self::La16 | Self::Rgba8 | Self::Rgba16 | Self::Rgba32F
        )
    }

    /// Whether this colour type is chromatic (as opposed to grayscale).
    #[must_use]
    pub const fn has_color(self) -> bool {
        matches!(
            self,
            Self::Rgb8 | Self::Rgb16 | Self::Rgba8 | Self::Rgba16 | Self::Rgb32F | Self::Rgba32F
        )
    }
}

/// A wider colour-type enumeration used by raw, format-agnostic encoding:
/// [`crate::ImageEncoder::write_image`], `save_buffer`.
///
/// Shaped after `image` 0.25's `ExtendedColorType`. Every variant the real
/// crate has is present, even the sub-8-bit and CMYK ones this crate never
/// produces from a decode, so encoder call sites that build one from a
/// literal (`ExtendedColorType::Rgb8`) keep compiling; encoding one this
/// crate's three codecs cannot represent fails with
/// [`crate::ImageError::Unsupported`], never silently.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum ExtendedColorType {
    /// 8-bit alpha only.
    A8,
    /// 1-bit luminance.
    L1,
    /// 1-bit luminance with alpha.
    La1,
    /// 1-bit red, green, blue.
    Rgb1,
    /// 1-bit red, green, blue, alpha.
    Rgba1,
    /// 2-bit luminance.
    L2,
    /// 2-bit luminance with alpha.
    La2,
    /// 2-bit red, green, blue.
    Rgb2,
    /// 2-bit red, green, blue, alpha.
    Rgba2,
    /// 4-bit luminance.
    L4,
    /// 4-bit luminance with alpha.
    La4,
    /// 4-bit red, green, blue.
    Rgb4,
    /// 4-bit red, green, blue, alpha.
    Rgba4,
    /// 8-bit luminance.
    L8,
    /// 8-bit luminance with alpha.
    La8,
    /// 8-bit red, green, blue.
    Rgb8,
    /// 8-bit red, green, blue, alpha.
    Rgba8,
    /// 16-bit luminance.
    L16,
    /// 16-bit luminance with alpha.
    La16,
    /// 16-bit red, green, blue.
    Rgb16,
    /// 16-bit red, green, blue, alpha.
    Rgba16,
    /// 8-bit blue, green, red.
    Bgr8,
    /// 8-bit blue, green, red, alpha.
    Bgra8,
    /// 32-bit float red, green, blue.
    Rgb32F,
    /// 32-bit float red, green, blue, alpha.
    Rgba32F,
    /// 8-bit cyan, magenta, yellow, black.
    Cmyk8,
    /// 16-bit cyan, magenta, yellow, black.
    Cmyk16,
    /// Anything else: `bit_depth` bits, unknown channel semantics.
    Unknown(u8),
}

impl ExtendedColorType {
    /// Number of channels, when that is a fixed, known quantity.
    #[must_use]
    pub const fn channel_count(self) -> u8 {
        match self {
            Self::L1 | Self::L2 | Self::L4 | Self::L8 | Self::L16 | Self::A8 | Self::Unknown(_) => {
                1
            }
            Self::La1 | Self::La2 | Self::La4 | Self::La8 | Self::La16 => 2,
            Self::Rgb1
            | Self::Rgb2
            | Self::Rgb4
            | Self::Rgb8
            | Self::Rgb16
            | Self::Bgr8
            | Self::Rgb32F => 3,
            Self::Rgba1
            | Self::Rgba2
            | Self::Rgba4
            | Self::Rgba8
            | Self::Rgba16
            | Self::Bgra8
            | Self::Rgba32F
            | Self::Cmyk8
            | Self::Cmyk16 => 4,
        }
    }

    /// Bits each channel occupies.
    #[must_use]
    pub const fn bits_per_channel(self) -> u8 {
        match self {
            Self::L1 | Self::La1 | Self::Rgb1 | Self::Rgba1 => 1,
            Self::L2 | Self::La2 | Self::Rgb2 | Self::Rgba2 => 2,
            Self::L4 | Self::La4 | Self::Rgb4 | Self::Rgba4 => 4,
            Self::L8
            | Self::La8
            | Self::Rgb8
            | Self::Rgba8
            | Self::A8
            | Self::Bgr8
            | Self::Bgra8
            | Self::Cmyk8 => 8,
            Self::L16 | Self::La16 | Self::Rgb16 | Self::Rgba16 | Self::Cmyk16 => 16,
            Self::Rgb32F | Self::Rgba32F => 32,
            Self::Unknown(bits) => bits,
        }
    }

    /// Bits the whole pixel occupies, when every channel shares a width.
    #[must_use]
    pub const fn bits_per_pixel(self) -> u16 {
        self.bits_per_channel() as u16 * self.channel_count() as u16
    }
}

impl From<ColorType> for ExtendedColorType {
    fn from(c: ColorType) -> Self {
        match c {
            ColorType::L8 => Self::L8,
            ColorType::La8 => Self::La8,
            ColorType::Rgb8 => Self::Rgb8,
            ColorType::Rgba8 => Self::Rgba8,
            ColorType::L16 => Self::L16,
            ColorType::La16 => Self::La16,
            ColorType::Rgb16 => Self::Rgb16,
            ColorType::Rgba16 => Self::Rgba16,
            ColorType::Rgb32F => Self::Rgb32F,
            ColorType::Rgba32F => Self::Rgba32F,
        }
    }
}

/// Failure to narrow an [`ExtendedColorType`] down to a buffer-representable
/// [`ColorType`].
#[derive(Clone, Debug)]
pub struct TryFromExtendedColorError {
    pub(crate) was: ExtendedColorType,
}

impl std::fmt::Display for TryFromExtendedColorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the pixel layout {:?} is not one of the ten buffer ColorTypes",
            self.was
        )
    }
}

impl std::error::Error for TryFromExtendedColorError {}

impl TryFrom<ExtendedColorType> for ColorType {
    type Error = TryFromExtendedColorError;

    fn try_from(e: ExtendedColorType) -> Result<Self, Self::Error> {
        Ok(match e {
            ExtendedColorType::L8 => Self::L8,
            ExtendedColorType::La8 => Self::La8,
            ExtendedColorType::Rgb8 => Self::Rgb8,
            ExtendedColorType::Rgba8 => Self::Rgba8,
            ExtendedColorType::L16 => Self::L16,
            ExtendedColorType::La16 => Self::La16,
            ExtendedColorType::Rgb16 => Self::Rgb16,
            ExtendedColorType::Rgba16 => Self::Rgba16,
            ExtendedColorType::Rgb32F => Self::Rgb32F,
            ExtendedColorType::Rgba32F => Self::Rgba32F,
            was => return Err(TryFromExtendedColorError { was }),
        })
    }
}

impl From<TryFromExtendedColorError> for crate::error::ImageError {
    fn from(err: TryFromExtendedColorError) -> Self {
        crate::error::ImageError::Unsupported(crate::error::UnsupportedError::from_format_and_kind(
            crate::error::ImageFormatHint::Unknown,
            crate::error::UnsupportedErrorKind::Color(err.was),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_channels_and_index() {
        let mut px = Rgba::new(1u8, 2, 3, 4);
        assert_eq!(px.channels(), &[1, 2, 3, 4]);
        assert_eq!(px[2], 3);
        px[0] = 9;
        assert_eq!(px.channels_mut(), &[9, 2, 3, 4]);
        assert_eq!(Rgba::<u8>::CHANNEL_COUNT, 4);
    }

    #[test]
    fn from_slice_builds_the_right_pixel() {
        let px: Rgb<u8> = Pixel::from_slice(&[10, 20, 30]);
        assert_eq!(px.0, [10, 20, 30]);
        let px: Luma<u16> = Pixel::from_slice(&[4000]);
        assert_eq!(px.0, [4000]);
    }

    #[test]
    #[should_panic(expected = "from_slice needs exactly")]
    fn from_slice_panics_on_wrong_length() {
        let _: Rgb<u8> = Pixel::from_slice(&[1, 2]);
    }

    #[test]
    fn color_type_geometry_matches_the_ten_variants() {
        assert_eq!(ColorType::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(ColorType::Rgb16.bytes_per_pixel(), 6);
        assert_eq!(ColorType::Rgba32F.bytes_per_pixel(), 16);
        assert!(ColorType::Rgba8.has_alpha());
        assert!(!ColorType::Rgb8.has_alpha());
        assert!(ColorType::Rgb8.has_color());
        assert!(!ColorType::L8.has_color());
        assert_eq!(ColorType::La16.channel_count(), 2);
    }

    #[test]
    fn extended_color_type_round_trips_through_color_type() {
        for c in [
            ColorType::L8,
            ColorType::La8,
            ColorType::Rgb8,
            ColorType::Rgba8,
            ColorType::L16,
            ColorType::La16,
            ColorType::Rgb16,
            ColorType::Rgba16,
            ColorType::Rgb32F,
            ColorType::Rgba32F,
        ] {
            let e: ExtendedColorType = c.into();
            assert_eq!(ColorType::try_from(e).unwrap(), c);
        }
    }

    #[test]
    fn extended_color_type_rejects_non_buffer_variants() {
        assert!(ColorType::try_from(ExtendedColorType::L1).is_err());
        assert!(ColorType::try_from(ExtendedColorType::Cmyk8).is_err());
    }

    #[test]
    fn sample_scaling_round_trips_at_the_endpoints() {
        assert_eq!(sample_to_u8(0u16), 0);
        assert_eq!(sample_to_u8(65535u16), 255);
        assert_eq!(sample_to_u16(0u8), 0);
        assert_eq!(sample_to_u16(255u8), 65535);
        assert_eq!(sample_to_u8(1.0f32), 255);
        assert_eq!(sample_to_u8(0.0f32), 0);
    }
}
