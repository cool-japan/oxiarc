//! JPEG decoding and encoding, over `oxiarc-jpeg`.
//!
//! # 16-bit samples never need an endian swap here
//!
//! Unlike PNG, this module never touches a `[u8]` buffer of 16-bit samples:
//! [`oxiarc_jpeg::Decoder::decode_into_u16`]/`encode_u16` are `[u16]`-typed,
//! so the platform's own native representation is already correct on both
//! sides and there is nothing to swap.
//!
//! # CMYK / YCCK is a named `Unsupported`, matching `image`
//! [`crate::ColorType`] has no CMYK variant — neither does `image` 0.25's
//! `ColorType`, and its own JPEG decoder (`codecs/jpeg/decoder.rs`) has no
//! CMYK handling at all. Decoding a CMYK or YCCK JPEG through
//! [`JpegDecoder`] therefore returns [`crate::ImageError::Unsupported`]
//! rather than inventing an un-validated CMYK-to-RGB conversion; *encoding*
//! `ExtendedColorType::Cmyk8` is still supported, since
//! `oxiarc_jpeg::InputColor::Cmyk` exists and needs no such conversion.

use std::io::{Read, Write};

use oxiarc_jpeg::{DecodeOptions, InputColor, PixelFormat};

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, unsupported_color};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder};

fn pixel_format_to_color_type(format: PixelFormat) -> ImageResult<ColorType> {
    match format {
        PixelFormat::L8 => Ok(ColorType::L8),
        PixelFormat::L16 => Ok(ColorType::L16),
        PixelFormat::Rgb8 => Ok(ColorType::Rgb8),
        PixelFormat::Rgb16 => Ok(ColorType::Rgb16),
        PixelFormat::Cmyk8 | PixelFormat::Raw8(_) => Err(unsupported_color(
            ImageFormat::Jpeg,
            ExtendedColorType::Unknown(8),
        )),
        PixelFormat::Cmyk16 | PixelFormat::Raw16(_) => Err(unsupported_color(
            ImageFormat::Jpeg,
            ExtendedColorType::Unknown(16),
        )),
        // `PixelFormat` is `#[non_exhaustive]`: a future `oxiarc-jpeg`
        // release could add a variant (e.g. a wider raw layout). Until this
        // crate is updated to understand it, report it by name rather than
        // fail to compile or, worse, panic.
        _ => Err(unsupported_color(
            ImageFormat::Jpeg,
            ExtendedColorType::Unknown(0),
        )),
    }
}

/// A JPEG decoder, over any [`Read`] source.
///
/// Decodes to RGB (or, for a grayscale source, luminance): the same
/// `output_color_space` default `oxiarc_jpeg::Decoder` itself uses (YCbCr
/// becomes RGB, everything else passes through).
pub struct JpegDecoder<R: Read> {
    decoder: oxiarc_jpeg::Decoder<R>,
    color_type: ColorType,
    width: u32,
    height: u32,
}

impl<R: Read> JpegDecoder<R> {
    /// Start decoding `r`.
    ///
    /// # Errors
    /// Any malformed JPEG, or a CMYK/YCCK/raw-component source — see the
    /// module docs.
    pub fn new(r: R) -> ImageResult<Self> {
        let mut decoder = oxiarc_jpeg::Decoder::new(r);
        let info = decoder.read_info()?;
        let color_type = pixel_format_to_color_type(info.pixel_format())?;
        Ok(Self {
            decoder,
            color_type,
            width: u32::from(info.width),
            height: u32::from(info.height),
        })
    }
}

impl<R: Read> ImageDecoder for JpegDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        if matches!(self.color_type, ColorType::L16 | ColorType::Rgb16) {
            // `decode_into_u16` wants `[u16]`; `buf` is the byte-oriented
            // `ImageDecoder` contract, so view it through a same-length
            // native `Vec<u16>` and copy back -- no byte-order conversion,
            // see the module docs.
            let samples = buf.len() / 2;
            let mut wide = vec![0u16; samples];
            self.decoder.decode_into_u16(&mut wide)?;
            for (chunk, v) in buf.chunks_exact_mut(2).zip(wide) {
                chunk.copy_from_slice(&v.to_ne_bytes());
            }
        } else {
            self.decoder.decode_into(buf)?;
        }
        Ok(())
    }
}

/// A JPEG encoder, over any [`Write`] sink.
pub struct JpegEncoder<W: Write> {
    w: W,
    quality: u8,
}

impl<W: Write> JpegEncoder<W> {
    /// A new encoder at quality 75, `oxiarc_jpeg`'s own default.
    pub fn new(w: W) -> Self {
        Self { w, quality: 75 }
    }

    /// A new encoder at the given quality (`1..=100`).
    pub fn new_with_quality(w: W, quality: u8) -> Self {
        Self { w, quality }
    }
}

fn extended_color_to_input(color: ExtendedColorType) -> ImageResult<InputColor> {
    Ok(match color {
        ExtendedColorType::L8 => InputColor::Luma,
        ExtendedColorType::La8 => InputColor::LumaAlpha,
        ExtendedColorType::Rgb8 => InputColor::Rgb,
        ExtendedColorType::Rgba8 => InputColor::Rgba,
        ExtendedColorType::Bgr8 => InputColor::Bgr,
        ExtendedColorType::Bgra8 => InputColor::Bgra,
        ExtendedColorType::Cmyk8 => InputColor::Cmyk,
        other => return Err(unsupported_color(ImageFormat::Jpeg, other)),
    })
}

fn to_jpeg_dimension(value: u32) -> ImageResult<u16> {
    u16::try_from(value).map_err(|_| {
        ImageError::Parameter(crate::error::ParameterError::from_kind(
            crate::error::ParameterErrorKind::DimensionMismatch,
        ))
    })
}

impl<W: Write> ImageEncoder for JpegEncoder<W> {
    fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ExtendedColorType,
    ) -> ImageResult<()> {
        let input_color = extended_color_to_input(color_type)?;
        let jpeg_width = to_jpeg_dimension(width)?;
        let jpeg_height = to_jpeg_dimension(height)?;
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(input_color.channels());
        if buf.len() != expected {
            return Err(ImageError::Parameter(
                crate::error::ParameterError::from_kind(
                    crate::error::ParameterErrorKind::DimensionMismatch,
                ),
            ));
        }

        let options = oxiarc_jpeg::EncodeOptions {
            quality: self.quality,
            ..Default::default()
        };
        let mut encoder = oxiarc_jpeg::Encoder::with_options(self.w, options);
        encoder.encode(buf, jpeg_width, jpeg_height, input_color)?;
        encoder.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_rgb8_stays_close_to_the_source() {
        let pixels = vec![90u8; 16 * 16 * 3];
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 92)
            .write_image(&pixels, 16, 16, ExtendedColorType::Rgb8)
            .expect("encode");

        let decoder = JpegDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.dimensions(), (16, 16));
        assert_eq!(decoder.color_type(), ColorType::Rgb8);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert!(buf.iter().all(|&v| v.abs_diff(90) <= 4));
    }

    #[test]
    fn round_trip_luma8() {
        let pixels = vec![200u8; 8 * 8];
        let mut out = Vec::new();
        JpegEncoder::new(&mut out)
            .write_image(&pixels, 8, 8, ExtendedColorType::L8)
            .expect("encode");
        let decoder = JpegDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::L8);
    }

    #[test]
    fn dimension_too_large_for_jpeg_is_a_parameter_error() {
        let err = to_jpeg_dimension(u32::from(u16::MAX) + 1).unwrap_err();
        assert!(matches!(err, ImageError::Parameter(_)));
    }

    #[test]
    fn wrong_buffer_length_is_a_parameter_error() {
        let mut out = Vec::new();
        let err = JpegEncoder::new(&mut out)
            .write_image(&[0u8; 5], 4, 4, ExtendedColorType::L8)
            .unwrap_err();
        assert!(matches!(err, ImageError::Parameter(_)));
    }

    #[test]
    fn default_quality_matches_oxiarc_jpeg() {
        let mut out = Vec::new();
        let encoder = JpegEncoder::new(&mut out);
        assert_eq!(encoder.quality, 75);
    }
}
