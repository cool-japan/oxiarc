//! A drop-in shaped facade for the `image` crate's JPEG encoder.
//!
//! Call sites that use `image::codecs::jpeg::JpegEncoder` only to write a
//! JPEG can move here with a `use` change:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::compat::{ColorType, JpegEncoder};
//!
//! let mut out = Vec::new();
//! let mut encoder = JpegEncoder::new_with_quality(&mut out, 85);
//! encoder.encode(&[128u8; 8 * 8 * 3], 8, 8, ColorType::Rgb8)?;
//! assert_eq!(&out[..2], &[0xFF, 0xD8]);
//! # Ok(())
//! # }
//! ```
//!
//! It is a migration aid, not a reimplementation of `image`: nothing here
//! knows about `DynamicImage`, `GenericImageView` or `ImageResult`. The
//! native [`crate::Encoder`] is a strictly larger API — progressive, lossless,
//! twelve-bit, restart intervals, custom scan scripts and TIFF's abbreviated
//! mode all live there and have no `image` equivalent.
//!
//! [`zune`] and [`jpeg_decoder`] are the decode-side counterparts, shaped
//! after the `zune_jpeg` and `jpeg-decoder` crates respectively — the same
//! "drop in with a `use` change, then reach for [`crate::Decoder`] once you
//! need what those crates cannot express" migration aid, for decoding
//! instead of encoding.

pub mod jpeg_decoder;
pub mod zune;

use std::io::Write;

use crate::encoder::{Density, EncodeOptions, Encoder, InputColor, Subsampling};
use crate::error::{JpegError, Result};

/// The pixel layouts this facade accepts, named as `image` names them.
///
/// Alpha is discarded, because JPEG cannot store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorType {
    /// Eight-bit luminance.
    L8,
    /// Eight-bit luminance with an alpha channel that is dropped.
    La8,
    /// Eight-bit red, green, blue.
    Rgb8,
    /// Eight-bit red, green, blue with an alpha channel that is dropped.
    Rgba8,
    /// Eight-bit cyan, magenta, yellow, black.
    Cmyk8,
}

impl ColorType {
    /// Samples per pixel in the caller's buffer.
    #[must_use]
    pub const fn channel_count(self) -> u8 {
        match self {
            ColorType::L8 => 1,
            ColorType::La8 => 2,
            ColorType::Rgb8 => 3,
            ColorType::Rgba8 | ColorType::Cmyk8 => 4,
        }
    }

    const fn to_input(self) -> InputColor {
        match self {
            ColorType::L8 => InputColor::Luma,
            ColorType::La8 => InputColor::LumaAlpha,
            ColorType::Rgb8 => InputColor::Rgb,
            ColorType::Rgba8 => InputColor::Rgba,
            ColorType::Cmyk8 => InputColor::Cmyk,
        }
    }
}

/// How a [`PixelDensity`] is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PixelDensityUnit {
    /// The two numbers are an aspect ratio, not a physical density.
    #[default]
    PixelAspectRatio,
    /// Pixels per inch.
    Inches,
    /// Pixels per centimetre.
    Centimeters,
}

/// The density written into the `JFIF` `APP0` segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelDensity {
    /// Horizontal and vertical density.
    pub density: (u16, u16),
    /// What the numbers mean.
    pub unit: PixelDensityUnit,
}

impl Default for PixelDensity {
    fn default() -> Self {
        Self {
            density: (1, 1),
            unit: PixelDensityUnit::PixelAspectRatio,
        }
    }
}

impl PixelDensity {
    /// A square-pixel aspect ratio of `value` by `value`.
    #[must_use]
    pub fn dpi(value: u16) -> Self {
        Self {
            density: (value, value),
            unit: PixelDensityUnit::Inches,
        }
    }
}

/// A JPEG encoder with the method names `image::codecs::jpeg::JpegEncoder`
/// uses.
#[derive(Debug)]
pub struct JpegEncoder<W: Write> {
    inner: Encoder<W>,
}

impl<W: Write> JpegEncoder<W> {
    /// A new encoder at quality 75, matching `image`'s default.
    pub fn new(writer: W) -> Self {
        Self::new_with_quality(writer, 75)
    }

    /// A new encoder at the given quality, `1..=100`.
    ///
    /// `image` uses 4:2:0 chroma subsampling below quality 90 and 4:4:4 at 90
    /// and above; this reproduces that so a migrated call site keeps the file
    /// sizes it had.
    pub fn new_with_quality(writer: W, quality: u8) -> Self {
        let options = EncodeOptions {
            quality,
            subsampling: if quality >= 90 {
                Subsampling::S444
            } else {
                Subsampling::S420
            },
            ..Default::default()
        };
        Self {
            inner: Encoder::with_options(writer, options),
        }
    }

    /// Set the density recorded in the `JFIF` segment.
    pub fn set_pixel_density(&mut self, density: PixelDensity) {
        self.inner.set_density(Density {
            units: match density.unit {
                PixelDensityUnit::PixelAspectRatio => 0,
                PixelDensityUnit::Inches => 1,
                PixelDensityUnit::Centimeters => 2,
            },
            x: density.density.0,
            y: density.density.1,
        });
    }

    /// Encode one image.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] when a dimension exceeds
    /// 65 535 (JPEG's limit), and [`JpegError::BufferTooSmall`] when `buf` is
    /// shorter than `width * height * channels`.
    pub fn encode(
        &mut self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ColorType,
    ) -> Result<()> {
        let width = u16::try_from(width).map_err(|_| JpegError::InvalidEncodeParameter {
            parameter: "width",
            reason: "a JPEG frame is at most 65 535 pixels wide",
        })?;
        let height = u16::try_from(height).map_err(|_| JpegError::InvalidEncodeParameter {
            parameter: "height",
            reason: "a JPEG frame is at most 65 535 pixels tall",
        })?;
        self.inner.encode(buf, width, height, color_type.to_input())
    }

    /// The native encoder underneath, for the settings this facade does not
    /// expose.
    pub fn inner_mut(&mut self) -> &mut Encoder<W> {
        &mut self.inner
    }

    /// Flush and return the writer.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::Io`] if the flush fails.
    pub fn finish(self) -> Result<W> {
        self.inner.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;

    #[test]
    fn every_colour_type_encodes_and_decodes() {
        for color in [
            ColorType::L8,
            ColorType::La8,
            ColorType::Rgb8,
            ColorType::Rgba8,
            ColorType::Cmyk8,
        ] {
            let channels = usize::from(color.channel_count());
            let pixels = vec![100u8; 9 * 7 * channels];
            let mut out = Vec::new();
            let mut encoder = JpegEncoder::new(&mut out);
            encoder.encode(&pixels, 9, 7, color).expect("encode");
            encoder.finish().expect("finish");

            let mut decoder = Decoder::new(&out[..]);
            let info = decoder.read_info().expect("info");
            assert_eq!((info.width, info.height), (9, 7));
            decoder.decode().expect("decode");
        }
    }

    #[test]
    fn quality_selects_the_subsampling_image_would_use() {
        let mut out = Vec::new();
        let encoder = JpegEncoder::new_with_quality(&mut out, 89);
        assert_eq!(encoder.inner.options().subsampling, Subsampling::S420);
        let mut out = Vec::new();
        let encoder = JpegEncoder::new_with_quality(&mut out, 90);
        assert_eq!(encoder.inner.options().subsampling, Subsampling::S444);
    }

    #[test]
    fn pixel_density_reaches_the_jfif_segment() {
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new(&mut out);
        encoder.set_pixel_density(PixelDensity::dpi(300));
        encoder
            .encode(&[7u8; 8 * 8], 8, 8, ColorType::L8)
            .expect("encode");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(&out[..]);
        decoder.read_info().expect("info");
        let jfif = decoder.jfif().expect("JFIF");
        assert_eq!(jfif.units, 1);
        assert_eq!((jfif.x_density, jfif.y_density), (300, 300));
    }

    #[test]
    fn oversized_dimensions_are_rejected() {
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new(&mut out);
        assert!(encoder.encode(&[0u8; 4], 70_000, 1, ColorType::L8).is_err());
        assert!(encoder.encode(&[0u8; 4], 1, 70_000, ColorType::L8).is_err());
    }

    #[test]
    fn the_native_encoder_is_reachable() {
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new(&mut out);
        encoder
            .inner_mut()
            .set_progressive(true)
            .set_optimize_huffman(true);
        encoder
            .encode(&[9u8; 16 * 16], 16, 16, ColorType::L8)
            .expect("encode");
        encoder.finish().expect("finish");
        let info = Decoder::new(&out[..]).read_info().expect("info");
        assert_eq!(info.process, crate::CodingProcess::Progressive);
    }
}
