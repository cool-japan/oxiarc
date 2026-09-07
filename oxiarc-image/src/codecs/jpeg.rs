//! JPEG decoding and encoding, over `oxiarc-jpeg`.
//!
//! # 16-bit samples never need an endian swap here
//!
//! Unlike PNG, this module never touches a `[u8]` buffer of 16-bit samples:
//! [`oxiarc_jpeg::Decoder::decode_into_u16`]/`encode_u16` are `[u16]`-typed,
//! so the platform's own native representation is already correct on both
//! sides and there is nothing to swap.
//!
//! # …but they do need a range rescale
//!
//! A JPEG frame declares its own sample precision `P` (8 for baseline, 12
//! for extended sequential/progressive, 2..=16 for lossless SOF3).
//! `oxiarc_jpeg::Decoder::decode_into_u16` writes the frame's samples
//! *unscaled*, so a 12-bit frame fills only `0..=4095` of the `u16` it hands
//! back. This crate's [`ColorType::L16`]/[`ColorType::Rgb16`] mean a full
//! `0..=65535` range — that is what
//! [`crate::color::Primitive::DEFAULT_MAX_VALUE`] says, and what every
//! conversion in [`crate::dynamic`] assumes when it narrows or widens a
//! sample. Handing back raw 12-bit values under a `ColorType::L16` label
//! would make a perfectly bright 12-bit JPEG decode sixteen times too dark
//! through `to_rgba8()`, silently. This module's private
//! `scale_to_full_range` therefore lifts every sample from
//! `0..=(2^P - 1)` to `0..=65535` (a no-op at `P == 16`),
//! and `precision_scaling_is_the_identity_at_sixteen_bits` /
//! `a_twelve_bit_jpeg_decodes_at_the_full_sixteen_bit_scale` pin both ends.
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

use oxiarc_jpeg::{InputColor, PixelFormat};

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, unsupported_color};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder, check_read_buffer};

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

/// Lift one sample from a `precision`-bit frame to the full `u16` range.
///
/// `v * 65535 / (2^precision - 1)`, rounded half up, which is the identity
/// at `precision == 16` and maps `2^precision - 1` to exactly `65535` at
/// every other width. A `precision` outside `1..=16` (which no frame this
/// decoder accepts can declare, since `oxiarc-jpeg` validates `P` while
/// parsing the `SOF`) is treated as "already full range" rather than
/// dividing by zero.
fn scale_to_full_range(v: u16, precision: u8) -> u16 {
    if precision == 0 || precision >= 16 {
        return v;
    }
    let max = (1u32 << precision) - 1;
    let clamped = u32::from(v).min(max);
    ((clamped * 65535 + max / 2) / max) as u16
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
    /// The frame's declared sample precision `P`, needed to rescale a
    /// 9..=15-bit frame's samples up to the full `u16` range this crate's
    /// 16-bit [`ColorType`]s mean. See the module docs.
    precision: u8,
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
            precision: info.precision,
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
        check_read_buffer(buf, self.total_bytes())?;
        if matches!(self.color_type, ColorType::L16 | ColorType::Rgb16) {
            // `decode_into_u16` wants `[u16]`; `buf` is the byte-oriented
            // `ImageDecoder` contract, so view it through a same-length
            // native `Vec<u16>` and copy back -- no byte-order conversion,
            // see the module docs.
            let samples = buf.len() / 2;
            let mut wide = vec![0u16; samples];
            self.decoder.decode_into_u16(&mut wide)?;
            let precision = self.precision;
            for (chunk, v) in buf.chunks_exact_mut(2).zip(wide) {
                chunk.copy_from_slice(&scale_to_full_range(v, precision).to_ne_bytes());
            }
        } else {
            self.decoder.decode_into(buf)?;
        }
        Ok(())
    }
}

/// A JPEG encoder, over any [`Write`] sink.
///
/// ```
/// use oxiarc_image::codecs::jpeg::{JpegDecoder, JpegEncoder};
/// use oxiarc_image::traits::{ImageDecoder, ImageEncoder};
/// use oxiarc_image::ExtendedColorType;
/// use std::io::Cursor;
///
/// let pixels = vec![120u8; 8 * 8 * 3]; // a flat 8x8 RGB square
/// let mut bytes = Vec::new();
/// JpegEncoder::new_with_quality(&mut bytes, 90).write_image(&pixels, 8, 8, ExtendedColorType::Rgb8)?;
///
/// let decoder = JpegDecoder::new(Cursor::new(bytes))?;
/// assert_eq!(decoder.dimensions(), (8, 8));
/// # Ok::<(), oxiarc_image::ImageError>(())
/// ```
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

    /// A 12-bit JPEG is a `ColorType::L16` image, and the samples it hands
    /// back must occupy the whole 16-bit range — not the bottom sixteenth of
    /// it. `oxiarc_jpeg::Decoder::decode_into_u16` writes the frame's own
    /// unscaled `0..=4095`, so this exercises the rescale in `read_image`
    /// end to end: encode through `oxiarc_jpeg`'s native 12-bit encoder
    /// (this facade's `JpegEncoder` has no 12-bit path to build the fixture
    /// with), decode through the facade, and check the *scale*, not just the
    /// round trip.
    #[test]
    fn a_twelve_bit_jpeg_decodes_at_the_full_sixteen_bit_scale() {
        let (width, height) = (16u16, 16u16);
        // A flat mid-grey at 12-bit scale: 2048/4095 -> 32776/65535.
        let source: Vec<u16> = vec![2048; usize::from(width) * usize::from(height)];
        let options = oxiarc_jpeg::EncodeOptions {
            quality: 100,
            precision: 12,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut encoder = oxiarc_jpeg::Encoder::with_options(&mut out, options);
        encoder
            .encode_u16(&source, width, height, InputColor::Luma)
            .expect("12-bit encode");
        encoder.finish().expect("finish");

        let decoder = JpegDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::L16);
        assert_eq!(decoder.dimensions(), (16, 16));
        assert_eq!(decoder.total_bytes(), 16 * 16 * 2);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        let decoded: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();

        let expected = scale_to_full_range(2048, 12);
        assert_eq!(expected, 32776, "12-bit 2048 must map to 32776, not 2048");
        for (i, &v) in decoded.iter().enumerate() {
            assert!(
                v.abs_diff(expected) <= 64,
                "sample {i} decoded as {v}, expected ~{expected} (a raw 12-bit \
                 2048 here would mean the range rescale is missing)"
            );
        }
    }

    #[test]
    fn precision_scaling_is_the_identity_at_sixteen_bits() {
        for v in [0u16, 1, 4095, 32768, 65534, 65535] {
            assert_eq!(scale_to_full_range(v, 16), v);
        }
        // A precision the SOF parser cannot produce must not divide by zero.
        assert_eq!(scale_to_full_range(1234, 0), 1234);
        assert_eq!(scale_to_full_range(1234, 17), 1234);
    }

    #[test]
    fn precision_scaling_maps_each_widths_endpoints_exactly() {
        for precision in 1u8..16 {
            let max = (1u32 << precision) - 1;
            assert_eq!(scale_to_full_range(0, precision), 0, "P={precision} floor");
            assert_eq!(
                scale_to_full_range(max as u16, precision),
                65535,
                "P={precision} ceiling"
            );
            // Monotonic across the *whole* width (not a sampled prefix:
            // 32767 iterations at P=15 is trivially fast), and never above
            // the ceiling even for an out-of-range sample.
            let mut previous = 0u16;
            for v in 0..=max {
                let scaled = scale_to_full_range(v as u16, precision);
                assert!(scaled >= previous, "P={precision} not monotonic at {v}");
                previous = scaled;
            }
            assert_eq!(scale_to_full_range(u16::MAX, precision), 65535);
        }
    }

    /// The buffer-length check has to guard the 16-bit branch too, not only
    /// the 8-bit one — that branch derives its `Vec<u16>` length from
    /// `buf.len() / 2`, so a wrong length there would have decoded into a
    /// mis-sized buffer rather than erroring.
    #[test]
    fn read_image_rejects_a_wrong_buffer_length_on_the_sixteen_bit_path_too() {
        let (width, height) = (8u16, 8u16);
        let source: Vec<u16> = vec![1000; usize::from(width) * usize::from(height)];
        let options = oxiarc_jpeg::EncodeOptions {
            quality: 90,
            precision: 12,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut encoder = oxiarc_jpeg::Encoder::with_options(&mut out, options);
        encoder
            .encode_u16(&source, width, height, InputColor::Luma)
            .expect("12-bit encode");
        encoder.finish().expect("finish");

        let decoder = JpegDecoder::new(Cursor::new(out.clone())).expect("decode header");
        let mut too_small = vec![0u8; 8 * 8];
        assert!(matches!(
            decoder.read_image(&mut too_small),
            Err(ImageError::Parameter(_))
        ));
        let decoder = JpegDecoder::new(Cursor::new(out)).expect("decode header");
        let mut odd = vec![0u8; 8 * 8 * 2 + 1];
        assert!(matches!(
            decoder.read_image(&mut odd),
            Err(ImageError::Parameter(_))
        ));
    }

    #[test]
    fn read_image_rejects_a_buffer_that_is_not_exactly_total_bytes() {
        let pixels = vec![90u8; 8 * 8];
        let mut out = Vec::new();
        JpegEncoder::new(&mut out)
            .write_image(&pixels, 8, 8, ExtendedColorType::L8)
            .expect("encode");

        let decoder = JpegDecoder::new(Cursor::new(out.clone())).expect("decode header");
        let mut too_small = vec![0u8; 8];
        assert!(matches!(
            decoder.read_image(&mut too_small),
            Err(ImageError::Parameter(_))
        ));

        let decoder = JpegDecoder::new(Cursor::new(out)).expect("decode header");
        let mut too_big = vec![0u8; 8 * 8 + 7];
        assert!(matches!(
            decoder.read_image(&mut too_big),
            Err(ImageError::Parameter(_))
        ));
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
