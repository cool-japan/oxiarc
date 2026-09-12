//! [`DynamicImage`]: an enum over the ten pixel-typed [`crate::ImageBuffer`]
//! variants, matching `image` 0.25's `DynamicImage`.

use std::io::{Seek, Write};
use std::path::Path;

use crate::buffer::{
    Gray16Image, GrayAlpha16Image, GrayAlphaImage, GrayImage, ImageBuffer, Rgb16Image, Rgb32FImage,
    RgbImage, Rgba16Image, Rgba32FImage, RgbaImage,
};
use crate::color::{ColorType, ExtendedColorType, sample_to_u16};
use crate::error::{ImageError, ImageResult, ParameterError, ParameterErrorKind};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder};

/// An image whose pixel type is decided at runtime.
///
/// One variant per (channel layout, sample type) combination this crate's
/// three codecs can produce or accept — the same ten `image::DynamicImage`
/// has.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DynamicImage {
    /// 8-bit luminance.
    ImageLuma8(GrayImage),
    /// 8-bit luminance with alpha.
    ImageLumaA8(GrayAlphaImage),
    /// 8-bit RGB.
    ImageRgb8(RgbImage),
    /// 8-bit RGBA.
    ImageRgba8(RgbaImage),
    /// 16-bit luminance.
    ImageLuma16(Gray16Image),
    /// 16-bit luminance with alpha.
    ImageLumaA16(GrayAlpha16Image),
    /// 16-bit RGB.
    ImageRgb16(Rgb16Image),
    /// 16-bit RGBA.
    ImageRgba16(Rgba16Image),
    /// 32-bit float RGB.
    ImageRgb32F(Rgb32FImage),
    /// 32-bit float RGBA.
    ImageRgba32F(Rgba32FImage),
}

/// Widen one channel to `u16`, given the source sample already scaled to
/// `[0, DEFAULT_MAX_VALUE]` for its own type.
fn widen<S: crate::color::Primitive + Into<f64>>(v: S) -> u16 {
    sample_to_u16(v)
}

/// Widen a `u8` sample to `u16`.
///
/// The exact integer form of [`widen`] for a `u8` source: `v * 65535 / 255`
/// is `v * 257` with no remainder, so no rounding decision arises. Proven
/// equal to `widen::<u8>` for all 256 inputs by `color.rs`'s
/// `integer_widening_agrees_with_sample_to_u16_on_every_u8`.
const fn u8_to_u16(v: u8) -> u16 {
    v as u16 * 257
}

/// Narrow a `u16` sample to `u8`: round-half-up of `v * 255 / 65535`.
///
/// The exact integer form of the `(v as f64 / 65535.0 * 255.0).round()`
/// this crate used before the conversions were rewritten to integer
/// arithmetic; `color.rs`'s
/// `integer_narrowing_agrees_with_the_float_formula_on_every_u16` proves the
/// two agree on all 65 536 inputs, so the rewrite changed no output byte.
const fn u16_to_u8(v: u16) -> u8 {
    ((v as u32 * 255 + 32767) / 65535) as u8
}

/// ITU-R Rec. 709 / sRGB luma of one 8-bit RGB triple, in exactly the
/// integer form `image` 0.25 uses (`SRGB_LUMA = [2126, 7152, 722]` over
/// `SRGB_LUMA_DIV = 10000`, truncating rather than rounding). See
/// [`DynamicImage::to_luma8`] for why the coefficients are the sRGB ones and
/// not BT.601's.
const fn luma8(r: u8, g: u8, b: u8) -> u8 {
    let sum = 2126 * r as u32 + 7152 * g as u32 + 722 * b as u32;
    (sum / 10000) as u8
}

/// [`luma8`] at 16-bit width. `2126 * 65535 * 3` still fits a `u32`, but the
/// sum is taken in `u64` to keep the widening obviously total.
const fn luma16(r: u16, g: u16, b: u16) -> u16 {
    let sum = 2126 * r as u64 + 7152 * g as u64 + 722 * b as u64;
    (sum / 10000) as u16
}

/// `width * height * channels` as a `usize` for a `Vec::with_capacity`
/// hint, saturating: an image whose sample count cannot be expressed as a
/// `usize` cannot exist in the first place (its source buffer would not
/// fit either), so the saturated value is only ever a capacity request the
/// allocator will reject exactly as the source allocation already would.
fn rgba_capacity(image: &DynamicImage, channels: usize) -> usize {
    let (width, height) = image.dimensions();
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(channels)
}

impl DynamicImage {
    /// `(width, height)`.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::ImageLuma8(b) => b.dimensions(),
            Self::ImageLumaA8(b) => b.dimensions(),
            Self::ImageRgb8(b) => b.dimensions(),
            Self::ImageRgba8(b) => b.dimensions(),
            Self::ImageLuma16(b) => b.dimensions(),
            Self::ImageLumaA16(b) => b.dimensions(),
            Self::ImageRgb16(b) => b.dimensions(),
            Self::ImageRgba16(b) => b.dimensions(),
            Self::ImageRgb32F(b) => b.dimensions(),
            Self::ImageRgba32F(b) => b.dimensions(),
        }
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.dimensions().0
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.dimensions().1
    }

    /// This image's colour type.
    #[must_use]
    pub fn color(&self) -> ColorType {
        match self {
            Self::ImageLuma8(_) => ColorType::L8,
            Self::ImageLumaA8(_) => ColorType::La8,
            Self::ImageRgb8(_) => ColorType::Rgb8,
            Self::ImageRgba8(_) => ColorType::Rgba8,
            Self::ImageLuma16(_) => ColorType::L16,
            Self::ImageLumaA16(_) => ColorType::La16,
            Self::ImageRgb16(_) => ColorType::Rgb16,
            Self::ImageRgba16(_) => ColorType::Rgba16,
            Self::ImageRgb32F(_) => ColorType::Rgb32F,
            Self::ImageRgba32F(_) => ColorType::Rgba32F,
        }
    }

    /// Whether this image carries an alpha channel.
    #[must_use]
    pub fn has_alpha(&self) -> bool {
        self.color().has_alpha()
    }

    /// Every pixel as non-premultiplied 16-bit RGBA, upscaling narrower
    /// samples and adding a fully-opaque alpha where none exists.
    ///
    /// Sample scaling is `image` 0.25's: a `u8` widens by `* 257` (exact, no
    /// rounding), a `f32` scales by `DEFAULT_MAX_VALUE` (`1.0`) and clamps
    /// into `[0, 65535]`, so a float sample outside `[0.0, 1.0]` saturates
    /// rather than wrapping.
    #[must_use]
    pub fn to_rgba16(&self) -> Rgba16Image {
        let (width, height) = self.dimensions();
        let mut out: Vec<u16> = Vec::with_capacity(rgba_capacity(self, 4));
        match self {
            Self::ImageLuma8(b) => {
                for &l in b.as_raw() {
                    let l = u8_to_u16(l);
                    out.extend_from_slice(&[l, l, l, u16::MAX]);
                }
            }
            Self::ImageLumaA8(b) => {
                for p in b.as_raw().chunks_exact(2) {
                    let l = u8_to_u16(p[0]);
                    out.extend_from_slice(&[l, l, l, u8_to_u16(p[1])]);
                }
            }
            Self::ImageRgb8(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[
                        u8_to_u16(p[0]),
                        u8_to_u16(p[1]),
                        u8_to_u16(p[2]),
                        u16::MAX,
                    ]);
                }
            }
            Self::ImageRgba8(b) => {
                for p in b.as_raw().chunks_exact(4) {
                    out.extend_from_slice(&[
                        u8_to_u16(p[0]),
                        u8_to_u16(p[1]),
                        u8_to_u16(p[2]),
                        u8_to_u16(p[3]),
                    ]);
                }
            }
            Self::ImageLuma16(b) => {
                for &l in b.as_raw() {
                    out.extend_from_slice(&[l, l, l, u16::MAX]);
                }
            }
            Self::ImageLumaA16(b) => {
                for p in b.as_raw().chunks_exact(2) {
                    out.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            Self::ImageRgb16(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[p[0], p[1], p[2], u16::MAX]);
                }
            }
            Self::ImageRgba16(b) => out.extend_from_slice(b.as_raw()),
            Self::ImageRgb32F(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[widen(p[0]), widen(p[1]), widen(p[2]), u16::MAX]);
                }
            }
            Self::ImageRgba32F(b) => {
                for p in b.as_raw().chunks_exact(4) {
                    out.extend_from_slice(&[widen(p[0]), widen(p[1]), widen(p[2]), widen(p[3])]);
                }
            }
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as non-premultiplied 8-bit RGBA.
    ///
    /// An already-`Rgba8` image is a plain buffer clone; every 8-bit source
    /// copies its samples straight across (widening a `u8` to 16 bits and
    /// narrowing it back is the identity, proven exhaustively by
    /// `color.rs`'s `widening_then_narrowing_a_u8_is_the_identity`); a
    /// 16-bit or float source narrows exactly as [`Self::to_rgba16`]
    /// followed by the 16-to-8-bit rounding would, without materialising the
    /// intermediate 16-bit buffer.
    ///
    /// That two-step narrowing is what `image` 0.25 reaches in one step
    /// (`(v.clamp(0.0, 1.0) * 255.0).round()` for a float source). They are
    /// not obviously the same — double rounding usually is not — so
    /// `tests/conversions.rs`'s
    /// `two_step_float_narrowing_agrees_with_the_single_step_form` checks:
    /// over every 8-bit level, a stride through the 16-bit levels, 20 000
    /// deterministic pseudo-random samples and the out-of-range and
    /// non-finite cases, the two forms **never** disagree. Float-to-8-bit
    /// output here is byte-identical to `image`'s.
    #[must_use]
    pub fn to_rgba8(&self) -> RgbaImage {
        let (width, height) = self.dimensions();
        if let Self::ImageRgba8(buf) = self {
            return buf.clone();
        }
        let mut out: Vec<u8> = Vec::with_capacity(rgba_capacity(self, 4));
        match self {
            Self::ImageLuma8(b) => {
                for &l in b.as_raw() {
                    out.extend_from_slice(&[l, l, l, u8::MAX]);
                }
            }
            Self::ImageLumaA8(b) => {
                for p in b.as_raw().chunks_exact(2) {
                    out.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            Self::ImageRgb8(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[p[0], p[1], p[2], u8::MAX]);
                }
            }
            // Handled by the early return above; kept as an explicit arm so
            // a future variant cannot silently fall into a catch-all.
            Self::ImageRgba8(b) => out.extend_from_slice(b.as_raw()),
            Self::ImageLuma16(b) => {
                for &l in b.as_raw() {
                    let l = u16_to_u8(l);
                    out.extend_from_slice(&[l, l, l, u8::MAX]);
                }
            }
            Self::ImageLumaA16(b) => {
                for p in b.as_raw().chunks_exact(2) {
                    let l = u16_to_u8(p[0]);
                    out.extend_from_slice(&[l, l, l, u16_to_u8(p[1])]);
                }
            }
            Self::ImageRgb16(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[
                        u16_to_u8(p[0]),
                        u16_to_u8(p[1]),
                        u16_to_u8(p[2]),
                        u8::MAX,
                    ]);
                }
            }
            Self::ImageRgba16(b) => {
                for p in b.as_raw().chunks_exact(4) {
                    out.extend_from_slice(&[
                        u16_to_u8(p[0]),
                        u16_to_u8(p[1]),
                        u16_to_u8(p[2]),
                        u16_to_u8(p[3]),
                    ]);
                }
            }
            // Float narrows through the 16-bit form, exactly as routing via
            // `to_rgba16` did: `narrow(widen(v))`, not `narrow(v)` — the two
            // differ for some samples because of the second rounding step,
            // and this crate's published behaviour is the two-step one.
            Self::ImageRgb32F(b) => {
                for p in b.as_raw().chunks_exact(3) {
                    out.extend_from_slice(&[
                        u16_to_u8(widen(p[0])),
                        u16_to_u8(widen(p[1])),
                        u16_to_u8(widen(p[2])),
                        u8::MAX,
                    ]);
                }
            }
            Self::ImageRgba32F(b) => {
                for p in b.as_raw().chunks_exact(4) {
                    out.extend_from_slice(&[
                        u16_to_u8(widen(p[0])),
                        u16_to_u8(widen(p[1])),
                        u16_to_u8(widen(p[2])),
                        u16_to_u8(widen(p[3])),
                    ]);
                }
            }
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 16-bit RGB, discarding any alpha.
    #[must_use]
    pub fn to_rgb16(&self) -> Rgb16Image {
        if let Self::ImageRgb16(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgba = self.to_rgba16().into_raw();
        let mut out: Vec<u16> = Vec::with_capacity(rgba.len() / 4 * 3);
        for p in rgba.chunks_exact(4) {
            out.extend_from_slice(&p[..3]);
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 8-bit RGB, discarding any alpha.
    #[must_use]
    pub fn to_rgb8(&self) -> RgbImage {
        if let Self::ImageRgb8(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgba = self.to_rgba8().into_raw();
        let mut out: Vec<u8> = Vec::with_capacity(rgba.len() / 4 * 3);
        for p in rgba.chunks_exact(4) {
            out.extend_from_slice(&p[..3]);
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 8-bit luminance.
    ///
    /// # Which luma weights
    ///
    /// The sRGB / ITU-R Rec. 709 ones (`0.2126 R + 0.7152 G + 0.0722 B`),
    /// in `image` 0.25's exact integer form — `(2126 R + 7152 G + 722 B) /
    /// 10000`, truncating — so that a caller who swapped `image` for this
    /// crate gets the same grayscale bytes, not merely a plausible grayscale.
    /// (The older BT.601 weights `0.299/0.587/0.114` differ by up to 22
    /// levels on a saturated primary, which would be a visible, silent change
    /// for a drop-in replacement.)
    ///
    /// Parity with `image` is exact for the four 8-bit variants, whose RGB
    /// form this computes luma from directly. A 16-bit or float source
    /// narrows to 8-bit *first* here, where `image` computes luma at the
    /// source width and narrows the result, so the two can differ by a
    /// level on those six variants; use [`Self::to_luma16`] when that
    /// matters.
    #[must_use]
    pub fn to_luma8(&self) -> GrayImage {
        if let Self::ImageLuma8(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgb = self.to_rgb8().into_raw();
        let mut out: Vec<u8> = Vec::with_capacity(rgb.len() / 3);
        for p in rgb.chunks_exact(3) {
            out.push(luma8(p[0], p[1], p[2]));
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 16-bit luminance, by the same sRGB weights
    /// [`Self::to_luma8`] documents.
    #[must_use]
    pub fn to_luma16(&self) -> Gray16Image {
        if let Self::ImageLuma16(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgb = self.to_rgb16().into_raw();
        let mut out: Vec<u16> = Vec::with_capacity(rgb.len() / 3);
        for p in rgb.chunks_exact(3) {
            out.push(luma16(p[0], p[1], p[2]));
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 8-bit luminance with alpha: [`Self::to_luma8`]'s
    /// luminance paired with [`Self::to_rgba8`]'s alpha, both taken from a
    /// single pass over the RGBA form.
    #[must_use]
    pub fn to_luma_alpha8(&self) -> GrayAlphaImage {
        if let Self::ImageLumaA8(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgba = self.to_rgba8().into_raw();
        let mut out: Vec<u8> = Vec::with_capacity(rgba.len() / 2);
        for p in rgba.chunks_exact(4) {
            out.extend_from_slice(&[luma8(p[0], p[1], p[2]), p[3]]);
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// Every pixel as 16-bit luminance with alpha.
    #[must_use]
    pub fn to_luma_alpha16(&self) -> GrayAlpha16Image {
        if let Self::ImageLumaA16(buf) = self {
            return buf.clone();
        }
        let (width, height) = self.dimensions();
        let rgba = self.to_rgba16().into_raw();
        let mut out: Vec<u16> = Vec::with_capacity(rgba.len() / 2);
        for p in rgba.chunks_exact(4) {
            out.extend_from_slice(&[luma16(p[0], p[1], p[2]), p[3]]);
        }
        ImageBuffer::from_raw_sized(width, height, out)
    }

    /// [`Self::to_rgba8`], consuming `self` and reusing the buffer when it
    /// is already `ImageRgba8`.
    #[must_use]
    pub fn into_rgba8(self) -> RgbaImage {
        match self {
            Self::ImageRgba8(buf) => buf,
            other => other.to_rgba8(),
        }
    }

    /// [`Self::to_rgb8`], consuming `self` and reusing the buffer when it is
    /// already `ImageRgb8`.
    #[must_use]
    pub fn into_rgb8(self) -> RgbImage {
        match self {
            Self::ImageRgb8(buf) => buf,
            other => other.to_rgb8(),
        }
    }

    /// [`Self::to_luma8`], consuming `self` and reusing the buffer when it
    /// is already `ImageLuma8`.
    #[must_use]
    pub fn into_luma8(self) -> GrayImage {
        match self {
            Self::ImageLuma8(buf) => buf,
            other => other.to_luma8(),
        }
    }

    /// [`Self::to_rgba16`], consuming `self` and reusing the buffer when it
    /// is already `ImageRgba16`.
    #[must_use]
    pub fn into_rgba16(self) -> Rgba16Image {
        match self {
            Self::ImageRgba16(buf) => buf,
            other => other.to_rgba16(),
        }
    }

    /// Assemble a [`DynamicImage`] from one codec's decoded output.
    ///
    /// `bytes` is native-endian, row-major, no padding — the
    /// [`crate::ImageDecoder::read_image`] contract.
    pub(crate) fn from_decoded(
        color_type: ColorType,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) -> ImageResult<Self> {
        let dim_err = || {
            ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            ))
        };
        Ok(match color_type {
            ColorType::L8 => {
                Self::ImageLuma8(ImageBuffer::from_raw(width, height, bytes).ok_or_else(dim_err)?)
            }
            ColorType::La8 => {
                Self::ImageLumaA8(ImageBuffer::from_raw(width, height, bytes).ok_or_else(dim_err)?)
            }
            ColorType::Rgb8 => {
                Self::ImageRgb8(ImageBuffer::from_raw(width, height, bytes).ok_or_else(dim_err)?)
            }
            ColorType::Rgba8 => {
                Self::ImageRgba8(ImageBuffer::from_raw(width, height, bytes).ok_or_else(dim_err)?)
            }
            ColorType::L16 => Self::ImageLuma16(
                ImageBuffer::from_raw(width, height, bytes_to_u16(&bytes)).ok_or_else(dim_err)?,
            ),
            ColorType::La16 => Self::ImageLumaA16(
                ImageBuffer::from_raw(width, height, bytes_to_u16(&bytes)).ok_or_else(dim_err)?,
            ),
            ColorType::Rgb16 => Self::ImageRgb16(
                ImageBuffer::from_raw(width, height, bytes_to_u16(&bytes)).ok_or_else(dim_err)?,
            ),
            ColorType::Rgba16 => Self::ImageRgba16(
                ImageBuffer::from_raw(width, height, bytes_to_u16(&bytes)).ok_or_else(dim_err)?,
            ),
            ColorType::Rgb32F => Self::ImageRgb32F(
                ImageBuffer::from_raw(width, height, bytes_to_f32(&bytes)).ok_or_else(dim_err)?,
            ),
            ColorType::Rgba32F => Self::ImageRgba32F(
                ImageBuffer::from_raw(width, height, bytes_to_f32(&bytes)).ok_or_else(dim_err)?,
            ),
        })
    }

    /// Run one [`crate::traits::ImageDecoder`] to completion and assemble
    /// its output into a [`DynamicImage`].
    ///
    /// The `image`-shaped entry point for a caller already holding a
    /// concrete decoder (a `codecs::png::PngDecoder`, say) instead of going
    /// through [`crate::ImageReader`]'s format dispatch — which itself
    /// calls this for each of the three formats.
    ///
    /// # Errors
    /// Whatever [`crate::traits::ImageDecoder::read_image`] returns, or a
    /// colour type paired with dimensions that cannot assemble into one of
    /// this crate's ten typed buffers.
    ///
    /// ```
    /// use oxiarc_image::codecs::png::PngEncoder;
    /// use oxiarc_image::traits::ImageEncoder;
    /// use oxiarc_image::{DynamicImage, ExtendedColorType};
    /// use std::io::Cursor;
    ///
    /// let mut bytes = Vec::new();
    /// PngEncoder::new(&mut bytes).write_image(&[1, 2, 3, 4], 2, 2, ExtendedColorType::L8)?;
    ///
    /// let decoder = oxiarc_image::codecs::png::PngDecoder::new(Cursor::new(bytes))?;
    /// let image = DynamicImage::from_decoder(decoder)?;
    /// assert_eq!(image.dimensions(), (2, 2));
    /// # Ok::<(), oxiarc_image::ImageError>(())
    /// ```
    pub fn from_decoder(decoder: impl ImageDecoder) -> ImageResult<Self> {
        let (width, height) = decoder.dimensions();
        let color_type = decoder.color_type();
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf)?;
        Self::from_decoded(color_type, width, height, buf)
    }

    /// This image's samples, copied into a fresh native-endian byte buffer
    /// — the format [`crate::ImageEncoder::write_image`] wants.
    fn to_native_bytes(&self) -> Vec<u8> {
        match self {
            Self::ImageLuma8(b) => b.as_raw().to_vec(),
            Self::ImageLumaA8(b) => b.as_raw().to_vec(),
            Self::ImageRgb8(b) => b.as_raw().to_vec(),
            Self::ImageRgba8(b) => b.as_raw().to_vec(),
            Self::ImageLuma16(b) => u16s_to_bytes(b.as_raw()),
            Self::ImageLumaA16(b) => u16s_to_bytes(b.as_raw()),
            Self::ImageRgb16(b) => u16s_to_bytes(b.as_raw()),
            Self::ImageRgba16(b) => u16s_to_bytes(b.as_raw()),
            Self::ImageRgb32F(b) => f32s_to_bytes(b.as_raw()),
            Self::ImageRgba32F(b) => f32s_to_bytes(b.as_raw()),
        }
    }

    /// Encode this image and write it to `w` in `format`.
    ///
    /// # Errors
    /// `format` is not one this crate encodes, the colour type cannot be
    /// represented (only PNG and TIFF can carry 16-bit or wider samples;
    /// JPEG always narrows through [`Self::to_rgb8`]/[`Self::to_luma8`]
    /// first automatically), or the underlying writer fails.
    pub fn write_to<W: Write + Seek>(&self, w: W, format: ImageFormat) -> ImageResult<()> {
        match format {
            // JPEG has no 16-bit or float encode path in this crate and no
            // alpha channel at all, so narrow first: a fundamentally
            // grayscale source (`L*`/`La*`, any bit depth) goes through
            // `to_luma8`, everything else through `to_rgb8`.
            ImageFormat::Jpeg => {
                let (bytes, width, height, encode_color) = if self.color().has_color() {
                    let rgb = self.to_rgb8();
                    let (width, height) = rgb.dimensions();
                    (rgb.into_raw(), width, height, ExtendedColorType::Rgb8)
                } else {
                    let luma = self.to_luma8();
                    let (width, height) = luma.dimensions();
                    (luma.into_raw(), width, height, ExtendedColorType::L8)
                };
                crate::codecs::jpeg::JpegEncoder::new(w).write_image(
                    &bytes,
                    width,
                    height,
                    encode_color,
                )
            }
            // PNG and TIFF both carry every one of the ten `ColorType`s
            // (including 16-bit and, for TIFF, float) natively, so no
            // narrowing is needed.
            ImageFormat::Png => {
                let (width, height) = self.dimensions();
                crate::codecs::png::PngEncoder::new(w).write_image(
                    &self.to_native_bytes(),
                    width,
                    height,
                    self.color().into(),
                )
            }
            ImageFormat::Tiff => {
                let (width, height) = self.dimensions();
                crate::codecs::tiff::TiffEncoder::new(w).write_image(
                    &self.to_native_bytes(),
                    width,
                    height,
                    self.color().into(),
                )
            }
            other => Err(ImageError::Unsupported(
                crate::error::UnsupportedError::from_format_and_kind(
                    other.into(),
                    crate::error::UnsupportedErrorKind::Format(other.into()),
                ),
            )),
        }
    }

    /// Encode with a caller-supplied encoder.
    ///
    /// # Deviation from `image`
    /// `image::DynamicImage::write_with_encoder` auto-converts to whatever
    /// colour type the encoder prefers, through a sealed
    /// `make_compatible_img` hook each of its encoders implements. This
    /// crate has no such hook: this method always hands over this image's
    /// own colour type unchanged (via `Self::to_native_bytes`) and lets the
    /// encoder decide. Two outcomes are possible, and the difference
    /// matters:
    ///
    /// * The encoder cannot accept the layout at all — a
    ///   [`crate::codecs::jpeg::JpegEncoder`] given a 16-bit or float
    ///   source, or a [`crate::codecs::png::PngEncoder`] given a float one
    ///   — and returns [`crate::ImageError::Unsupported`] by name.
    /// * The encoder accepts the layout but the *format* cannot store every
    ///   channel. JPEG has no alpha channel, yet
    ///   `oxiarc_jpeg::InputColor::{Rgba, LumaAlpha, Bgra}` accept a
    ///   four-/two-channel buffer and drop the alpha during colour
    ///   conversion, so `write_with_encoder(JpegEncoder::new(w))` on an
    ///   `ImageRgba8`/`ImageLumaA8` **succeeds and loses the alpha**. (This
    ///   is a superset of `image` 0.25, whose `JpegEncoder::write_image`
    ///   accepts only `L8`/`Rgb8` through the `ImageEncoder` trait and
    ///   rejects `Rgba8` outright.)
    ///
    /// [`Self::write_to`] never has the second problem: it narrows a
    /// coloured source through [`Self::to_rgb8`] and a grayscale one through
    /// [`Self::to_luma8`] before the JPEG encoder ever sees it, so the alpha
    /// loss is explicit in that path's own documentation. For PNG and TIFF,
    /// which carry every colour type this crate produces (float only for
    /// TIFF), neither problem arises.
    ///
    /// # Errors
    /// Whatever the encoder's `write_image` returns.
    pub fn write_with_encoder(&self, encoder: impl ImageEncoder) -> ImageResult<()> {
        let (width, height) = self.dimensions();
        encoder.write_image(&self.to_native_bytes(), width, height, self.color().into())
    }

    /// Save to `path`, guessing the format from its extension.
    ///
    /// # Errors
    /// The extension names an unsupported or unrecognised format, or
    /// [`Self::write_to`]'s errors.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> ImageResult<()> {
        let format = ImageFormat::from_path(path.as_ref())?;
        self.save_with_format(path, format)
    }

    /// Save to `path` in the given format.
    ///
    /// # Errors
    /// [`Self::write_to`]'s errors, plus any I/O failure creating the file.
    pub fn save_with_format<P: AsRef<Path>>(
        &self,
        path: P,
        format: ImageFormat,
    ) -> ImageResult<()> {
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        self.write_to(file, format)
    }
}

fn bytes_to_u16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect()
}

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn u16s_to_bytes(samples: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for v in samples {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn f32s_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for v in samples {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

impl From<GrayImage> for DynamicImage {
    fn from(buf: GrayImage) -> Self {
        Self::ImageLuma8(buf)
    }
}

impl From<GrayAlphaImage> for DynamicImage {
    fn from(buf: GrayAlphaImage) -> Self {
        Self::ImageLumaA8(buf)
    }
}

impl From<RgbImage> for DynamicImage {
    fn from(buf: RgbImage) -> Self {
        Self::ImageRgb8(buf)
    }
}

impl From<RgbaImage> for DynamicImage {
    fn from(buf: RgbaImage) -> Self {
        Self::ImageRgba8(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{Luma, Rgb, Rgba};

    fn checker() -> RgbaImage {
        ImageBuffer::from_fn(2, 2, |x, y| {
            if (x + y) % 2 == 0 {
                Rgba::new(255, 0, 0, 255)
            } else {
                Rgba::new(0, 255, 0, 128)
            }
        })
    }

    #[test]
    fn dimensions_color_and_alpha() {
        let img = DynamicImage::ImageRgba8(checker());
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 2);
        assert_eq!(img.color(), ColorType::Rgba8);
        assert!(img.has_alpha());
        assert!(!DynamicImage::ImageRgb8(img.to_rgb8()).has_alpha());
    }

    #[test]
    fn to_rgba8_on_matching_variant_is_a_cheap_clone_not_a_recompute() {
        let buf = checker();
        let img = DynamicImage::ImageRgba8(buf.clone());
        assert_eq!(img.to_rgba8().into_raw(), buf.into_raw());
    }

    #[test]
    fn luma_conversion_adds_full_alpha() {
        let img = DynamicImage::ImageLuma8(ImageBuffer::from_pixel(1, 1, Luma::new(200)));
        let rgba = img.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0), Rgba::new(200, 200, 200, 255));
    }

    #[test]
    fn sixteen_bit_round_trips_through_to_rgba16() {
        let buf: Rgb16Image = ImageBuffer::from_pixel(1, 1, Rgb::new(0x1234, 0x5678, 0x9ABC));
        let img = DynamicImage::ImageRgb16(buf);
        assert_eq!(
            img.to_rgba16().get_pixel(0, 0),
            Rgba::new(0x1234, 0x5678, 0x9ABC, 0xFFFF)
        );
    }

    #[test]
    fn into_rgba8_reuses_the_buffer_when_already_rgba8() {
        let buf = checker();
        let img = DynamicImage::ImageRgba8(buf.clone());
        assert_eq!(img.into_rgba8().into_raw(), buf.into_raw());
    }

    #[test]
    fn save_and_reopen_round_trips_through_a_real_file() {
        let img = DynamicImage::ImageRgb8(ImageBuffer::from_fn(3, 2, |x, y| {
            Rgb::new(x as u8 * 50, y as u8 * 50, 10)
        }));
        let path = std::env::temp_dir().join(format!(
            "oxiarc_image_dynamic_test_{}.png",
            std::process::id()
        ));
        img.save(&path).expect("save");
        let reopened = crate::open(&path).expect("reopen");
        assert_eq!(reopened.dimensions(), (3, 2));
        assert_eq!(reopened.to_rgb8().into_raw(), img.to_rgb8().into_raw());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_to_jpeg_narrows_rgba_to_rgb_automatically() {
        let img = DynamicImage::ImageRgba8(checker());
        let mut out = Vec::new();
        img.write_to(std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
            .expect("encode");
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn write_to_unsupported_format_is_a_named_error() {
        let img = DynamicImage::ImageRgb8(RgbImage::new(1, 1));
        let err = img
            .write_to(std::io::Cursor::new(Vec::new()), ImageFormat::Gif)
            .unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
    }
}
