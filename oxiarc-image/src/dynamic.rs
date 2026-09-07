//! [`DynamicImage`]: an enum over the ten pixel-typed [`crate::ImageBuffer`]
//! variants, matching `image` 0.25's `DynamicImage`.

use std::io::{Seek, Write};
use std::path::Path;

use crate::buffer::{
    Gray16Image, GrayAlpha16Image, GrayAlphaImage, GrayImage, ImageBuffer, Rgb16Image, Rgb32FImage,
    RgbImage, Rgba16Image, Rgba32FImage, RgbaImage,
};
use crate::color::{
    ColorType, ExtendedColorType, Luma, LumaA, Pixel, Rgb, Rgba, sample_to_u8, sample_to_u16,
};
use crate::error::{ImageError, ImageResult, ParameterError, ParameterErrorKind};
use crate::format::ImageFormat;
use crate::traits::ImageEncoder;

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

/// Narrow one channel to `u8`.
fn narrow<S: crate::color::Primitive + Into<f64>>(v: S) -> u8 {
    sample_to_u8(v)
}

/// `[l, l, l, opaque]`: a one-channel pixel replicated to four, fully
/// opaque. A plain generic function rather than a macro so `S` is inferred
/// the ordinary way, from `l`'s own type, at every call site.
fn luma_to_rgba_max<S: crate::color::Primitive>(l: S) -> [S; 4] {
    [l, l, l, S::DEFAULT_MAX_VALUE]
}

/// `[l, l, l, a]`: a two-channel (luminance, alpha) pixel replicated to
/// four.
fn luma_alpha_to_rgba<S: crate::color::Primitive>(l: S, a: S) -> [S; 4] {
    [l, l, l, a]
}

/// `[r, g, b, opaque]`.
fn rgb_to_rgba_max<S: crate::color::Primitive>(r: S, g: S, b: S) -> [S; 4] {
    [r, g, b, S::DEFAULT_MAX_VALUE]
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
    #[must_use]
    pub fn to_rgba16(&self) -> Rgba16Image {
        let (w, h) = self.dimensions();
        ImageBuffer::from_fn(w, h, |x, y| {
            let [r, g, b, a] = match self {
                Self::ImageLuma8(buf) => {
                    luma_to_rgba_max(buf.get_pixel(x, y).0[0]).map(|v| widen(v))
                }
                Self::ImageLumaA8(buf) => {
                    let p = buf.get_pixel(x, y);
                    luma_alpha_to_rgba(p.0[0], p.0[1]).map(|v| widen(v))
                }
                Self::ImageRgb8(buf) => {
                    let p = buf.get_pixel(x, y);
                    rgb_to_rgba_max(p.0[0], p.0[1], p.0[2]).map(|v| widen(v))
                }
                Self::ImageRgba8(buf) => buf.get_pixel(x, y).0.map(|v| widen(v)),
                Self::ImageLuma16(buf) => luma_to_rgba_max(buf.get_pixel(x, y).0[0]),
                Self::ImageLumaA16(buf) => {
                    let p = buf.get_pixel(x, y);
                    luma_alpha_to_rgba(p.0[0], p.0[1])
                }
                Self::ImageRgb16(buf) => {
                    let p = buf.get_pixel(x, y);
                    rgb_to_rgba_max(p.0[0], p.0[1], p.0[2])
                }
                Self::ImageRgba16(buf) => buf.get_pixel(x, y).0,
                Self::ImageRgb32F(buf) => {
                    let p = buf.get_pixel(x, y);
                    rgb_to_rgba_max(p.0[0], p.0[1], p.0[2]).map(|v| widen(v))
                }
                Self::ImageRgba32F(buf) => buf.get_pixel(x, y).0.map(|v| widen(v)),
            };
            Rgba::new(r, g, b, a)
        })
    }

    /// Every pixel as non-premultiplied 8-bit RGBA.
    #[must_use]
    pub fn to_rgba8(&self) -> RgbaImage {
        if let Self::ImageRgba8(buf) = self {
            return buf.clone();
        }
        let wide = self.to_rgba16();
        ImageBuffer::from_fn(wide.width(), wide.height(), |x, y| {
            let p = wide.get_pixel(x, y);
            Rgba::new(
                narrow(p.0[0]),
                narrow(p.0[1]),
                narrow(p.0[2]),
                narrow(p.0[3]),
            )
        })
    }

    /// Every pixel as 16-bit RGB, discarding any alpha.
    #[must_use]
    pub fn to_rgb16(&self) -> Rgb16Image {
        if let Self::ImageRgb16(buf) = self {
            return buf.clone();
        }
        let wide = self.to_rgba16();
        ImageBuffer::from_fn(wide.width(), wide.height(), |x, y| {
            let p = wide.get_pixel(x, y);
            Rgb::new(p.0[0], p.0[1], p.0[2])
        })
    }

    /// Every pixel as 8-bit RGB, discarding any alpha.
    #[must_use]
    pub fn to_rgb8(&self) -> RgbImage {
        if let Self::ImageRgb8(buf) = self {
            return buf.clone();
        }
        let wide = self.to_rgba8();
        ImageBuffer::from_fn(wide.width(), wide.height(), |x, y| {
            let p = wide.get_pixel(x, y);
            Rgb::new(p.0[0], p.0[1], p.0[2])
        })
    }

    /// Every pixel as 8-bit luminance, by ITU-R BT.601 luma weights over
    /// the RGB form.
    #[must_use]
    pub fn to_luma8(&self) -> GrayImage {
        if let Self::ImageLuma8(buf) = self {
            return buf.clone();
        }
        let rgb = self.to_rgb8();
        ImageBuffer::from_fn(rgb.width(), rgb.height(), |x, y| {
            let p = rgb.get_pixel(x, y);
            let y = (299 * u32::from(p.0[0]) + 587 * u32::from(p.0[1]) + 114 * u32::from(p.0[2]))
                / 1000;
            Luma::new(y as u8)
        })
    }

    /// Every pixel as 16-bit luminance.
    #[must_use]
    pub fn to_luma16(&self) -> Gray16Image {
        if let Self::ImageLuma16(buf) = self {
            return buf.clone();
        }
        let rgb = self.to_rgb16();
        ImageBuffer::from_fn(rgb.width(), rgb.height(), |x, y| {
            let p = rgb.get_pixel(x, y);
            let y = (299 * u64::from(p.0[0]) + 587 * u64::from(p.0[1]) + 114 * u64::from(p.0[2]))
                / 1000;
            Luma::new(y as u16)
        })
    }

    /// Every pixel as 8-bit luminance with alpha.
    #[must_use]
    pub fn to_luma_alpha8(&self) -> GrayAlphaImage {
        if let Self::ImageLumaA8(buf) = self {
            return buf.clone();
        }
        let luma = self.to_luma8();
        let rgba = self.to_rgba8();
        ImageBuffer::from_fn(luma.width(), luma.height(), |x, y| {
            LumaA::new(luma.get_pixel(x, y).0[0], rgba.get_pixel(x, y).0[3])
        })
    }

    /// Every pixel as 16-bit luminance with alpha.
    #[must_use]
    pub fn to_luma_alpha16(&self) -> GrayAlpha16Image {
        if let Self::ImageLumaA16(buf) = self {
            return buf.clone();
        }
        let luma = self.to_luma16();
        let rgba = self.to_rgba16();
        ImageBuffer::from_fn(luma.width(), luma.height(), |x, y| {
            LumaA::new(luma.get_pixel(x, y).0[0], rgba.get_pixel(x, y).0[3])
        })
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

    /// This image's samples, copied into a fresh native-endian byte buffer
    /// — the format [`crate::ImageEncoder::write_image`] wants.
    fn to_native_bytes(&self) -> Vec<u8> {
        match self {
            Self::ImageLuma8(b) => b.as_raw().clone(),
            Self::ImageLumaA8(b) => b.as_raw().clone(),
            Self::ImageRgb8(b) => b.as_raw().clone(),
            Self::ImageRgba8(b) => b.as_raw().clone(),
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
    /// own colour type unchanged (via [`Self::to_native_bytes`]), so an
    /// encoder that cannot represent it — a [`crate::codecs::jpeg::
    /// JpegEncoder`] given a 16-bit source, say — returns
    /// [`crate::ImageError::Unsupported`] rather than silently narrowing.
    /// Convert explicitly first (`img.to_rgb8()`) when that matters; for
    /// PNG/TIFF, which carry every colour type this crate produces, it
    /// never does.
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
