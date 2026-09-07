//! TIFF decoding and encoding, over `oxiarc-tiff`.
//!
//! # Endianness
//!
//! TIFF's multi-byte samples follow the *file's own* declared byte order
//! (`II`/little or `MM`/big), unlike PNG's fixed big-endian. Decode uses
//! `oxiarc_tiff::Samples::U16(Vec<u16>)` (already native, per its own type),
//! never the raw-byte `read_image_bytes`, so there is no swap to get wrong.
//! Encode hands `oxiarc_tiff::Encoder::write_image` the caller's
//! already-native-endian bytes directly: `writer/image.rs`'s
//! `encode_chunk_pure` calls `endian.from_native_in_place(..)` on exactly
//! this buffer, i.e. the file-order conversion happens inside
//! `oxiarc-tiff`, not here.
//!
//! # Premultiplied alpha
//!
//! An `Rgba`/`GrayA` TIFF may declare `ExtraSamples::AssociatedAlpha`
//! (premultiplied). [`crate::DynamicImage`]'s alpha convention is straight
//! (matching PNG/JPEG/`image`), so the fast typed-`Samples` path checks that
//! tag and un-premultiplies when it is set — the same correction
//! [`oxiarc_tiff::reader::Decoder::read_image_rgba8`]'s own doc promises
//! ("Associated (premultiplied) alpha is undone"), reimplemented here for
//! the 16-bit case that convenience method does not cover.
//!
//! # Everything that is not one of eight clean combinations
//!
//! `Gray/GrayA/Rgb/Rgba` at 8 or 16 bits map directly. A palette, CMYK,
//! `YCbCr`, `Lab`, `Multiband` image, or any other sample width or type,
//! goes through [`oxiarc_tiff::reader::Decoder::read_image_rgba8`] instead
//! — `oxiarc-tiff` already owns that colour math (palette lookup, `YCbCr`
//! matrix, CMYK-to-RGB, ...); reimplementing it here would not be "thin".

use std::io::{Read, Seek, Write};

use oxiarc_tiff::{ColorType as TiffColorType, ExtraSamples, Samples};

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, unsupported_color};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder};

/// The clean, directly-representable `(image color type, channels, is 16-bit)`
/// combinations, or `None` for anything that must go through
/// `read_image_rgba8`.
fn clean_mapping(tiff_color: TiffColorType) -> Option<(ColorType, u16, bool)> {
    match tiff_color {
        TiffColorType::Gray(8) => Some((ColorType::L8, 1, false)),
        TiffColorType::Gray(16) => Some((ColorType::L16, 1, true)),
        TiffColorType::GrayA(8) => Some((ColorType::La8, 2, false)),
        TiffColorType::GrayA(16) => Some((ColorType::La16, 2, true)),
        TiffColorType::Rgb(8) => Some((ColorType::Rgb8, 3, false)),
        TiffColorType::Rgb(16) => Some((ColorType::Rgb16, 3, true)),
        TiffColorType::Rgba(8) => Some((ColorType::Rgba8, 4, false)),
        TiffColorType::Rgba(16) => Some((ColorType::Rgba16, 4, true)),
        _ => None,
    }
}

fn un_premultiply_u8(data: &mut [u8], channels: usize) {
    for px in data.chunks_exact_mut(channels) {
        let (color, alpha) = px.split_at_mut(channels - 1);
        let a = u32::from(alpha[0]);
        for c in color {
            *c = if a == 0 {
                0
            } else {
                ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8
            };
        }
    }
}

fn un_premultiply_u16(data: &mut [u16], channels: usize) {
    for px in data.chunks_exact_mut(channels) {
        let (color, alpha) = px.split_at_mut(channels - 1);
        let a = u64::from(alpha[0]);
        for c in color {
            *c = if a == 0 {
                0
            } else {
                ((u64::from(*c) * 65535 + a / 2) / a).min(65535) as u16
            };
        }
    }
}

/// A TIFF decoder, over any [`Read`] + [`Seek`] source.
/// The fast path's shape, decided once in [`TiffDecoder::new`] from the
/// file's colour type *and* `SampleFormat` (a `Gray(8)` image with
/// `SampleFormat::Int` or `IeeeFp` does not decode to `Samples::U8`, so the
/// clean 8-combo table alone is not enough to pick the fast path safely).
#[derive(Clone, Copy)]
struct FastPath {
    channels: u16,
    is_wide: bool,
    associated_alpha: bool,
}

/// A TIFF decoder, over any [`Read`] + [`Seek`] source.
pub struct TiffDecoder<R: Read + Seek> {
    decoder: oxiarc_tiff::Decoder<R>,
    color_type: ColorType,
    width: u32,
    height: u32,
    fast: Option<FastPath>,
}

impl<R: Read + Seek> TiffDecoder<R> {
    /// Start decoding `r`.
    ///
    /// # Errors
    /// Any malformed TIFF, or an unsupported codec/compression.
    pub fn new(r: R) -> ImageResult<Self> {
        let mut decoder = oxiarc_tiff::Decoder::new(r)?;
        let (width, height) = decoder.dimensions()?;
        let tiff_color = decoder.color_type()?;
        let mapping = clean_mapping(tiff_color);
        let info = decoder.info()?;
        let all_uint = info
            .sample_format
            .iter()
            .all(|f| matches!(f, oxiarc_tiff::SampleFormat::Uint));
        let associated_alpha = info.extra_samples.contains(&ExtraSamples::AssociatedAlpha);

        let (color_type, fast) = match mapping {
            Some((color_type, channels, is_wide)) if all_uint => (
                color_type,
                Some(FastPath {
                    channels,
                    is_wide,
                    associated_alpha: channels > 1 && associated_alpha,
                }),
            ),
            _ => (ColorType::Rgba8, None),
        };

        Ok(Self {
            decoder,
            color_type,
            width,
            height,
            fast,
        })
    }
}

impl<R: Read + Seek> ImageDecoder for TiffDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        let Some(fast) = self.fast else {
            let rgba = self.decoder.read_image_rgba8()?;
            buf.copy_from_slice(&rgba);
            return Ok(());
        };

        let samples = self.decoder.read_image()?;
        match samples {
            Samples::U8(mut data) => {
                if fast.associated_alpha {
                    un_premultiply_u8(&mut data, fast.channels as usize);
                }
                buf.copy_from_slice(&data);
            }
            Samples::U16(mut data) => {
                if fast.associated_alpha {
                    un_premultiply_u16(&mut data, fast.channels as usize);
                }
                debug_assert!(fast.is_wide);
                for (chunk, v) in buf.chunks_exact_mut(2).zip(data) {
                    chunk.copy_from_slice(&v.to_ne_bytes());
                }
            }
            other => {
                // `all_uint` in `new` guarantees `read_image` returns `U8`
                // or `U16` for a `Uint`-sample-format file; kept as a named
                // error rather than a panic in case a future `oxiarc-tiff`
                // release changes what `Uint` + these bit depths produce.
                return Err(ImageError::Decoding(crate::error::DecodingError::new(
                    ImageFormat::Tiff.into(),
                    format!("unexpected sample type for a clean colour mapping: {other:?}"),
                )));
            }
        }
        Ok(())
    }
}

fn color_type_to_tiff(color: ExtendedColorType) -> ImageResult<TiffColorType> {
    Ok(match color {
        ExtendedColorType::L8 => TiffColorType::Gray(8),
        ExtendedColorType::La8 => TiffColorType::GrayA(8),
        ExtendedColorType::Rgb8 => TiffColorType::Rgb(8),
        ExtendedColorType::Rgba8 => TiffColorType::Rgba(8),
        ExtendedColorType::L16 => TiffColorType::Gray(16),
        ExtendedColorType::La16 => TiffColorType::GrayA(16),
        ExtendedColorType::Rgb16 => TiffColorType::Rgb(16),
        ExtendedColorType::Rgba16 => TiffColorType::Rgba(16),
        ExtendedColorType::Cmyk8 => TiffColorType::Cmyk(8),
        ExtendedColorType::Cmyk16 => TiffColorType::Cmyk(16),
        other => return Err(unsupported_color(ImageFormat::Tiff, other)),
    })
}

/// A TIFF encoder, over any [`Write`] + [`Seek`] sink (TIFF's directory
/// offsets must be patched after the strips are written, so unlike PNG/JPEG
/// this one needs [`Seek`]).
pub struct TiffEncoder<W: Write + Seek> {
    w: W,
}

impl<W: Write + Seek> TiffEncoder<W> {
    /// A new encoder.
    pub fn new(w: W) -> Self {
        Self { w }
    }
}

impl<W: Write + Seek> ImageEncoder for TiffEncoder<W> {
    fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ExtendedColorType,
    ) -> ImageResult<()> {
        let tiff_color = color_type_to_tiff(color_type)?;
        let spec = oxiarc_tiff::ImageSpec::new(width, height, tiff_color);
        let mut encoder = oxiarc_tiff::Encoder::new(self.w)?;
        encoder.write_image(&spec, buf)?;
        encoder.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A `Vec<u8>`-backed sink that also implements `Seek`, the way
    /// `TiffEncoder<W: Write + Seek>` needs; `out` still holds the bytes
    /// once the `Cursor` (and the encoder wrapping it) is dropped.
    fn encode_tiff(width: u32, height: u32, color: ExtendedColorType, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        TiffEncoder::new(Cursor::new(&mut out))
            .write_image(pixels, width, height, color)
            .expect("encode");
        out
    }

    #[test]
    fn round_trip_rgb8() {
        let pixels: Vec<u8> = (0..(4 * 3 * 3)).map(|i| (i * 7) as u8).collect();
        let bytes = encode_tiff(4, 3, ExtendedColorType::Rgb8, &pixels);

        let decoder = TiffDecoder::new(Cursor::new(bytes)).expect("decode header");
        assert_eq!(decoder.dimensions(), (4, 3));
        assert_eq!(decoder.color_type(), ColorType::Rgb8);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, pixels);
    }

    #[test]
    fn round_trip_sixteen_bit_grayscale_native_endian() {
        let native: Vec<u16> = vec![0x1234, 0xABCD, 0x0001, 0xFFFF];
        let mut bytes = Vec::new();
        for v in &native {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        let file = encode_tiff(2, 2, ExtendedColorType::L16, &bytes);

        let decoder = TiffDecoder::new(Cursor::new(file)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::L16);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, bytes);
    }

    #[test]
    fn premultiplied_alpha_is_undone_on_the_fast_path() {
        // Half-intensity red at half alpha, premultiplied: stored (128, 0, 0, 128).
        // Straight equivalent is (255, 0, 0, 128).
        let mut data = vec![128u8, 0, 0, 128];
        un_premultiply_u8(&mut data, 4);
        assert_eq!(data, vec![255, 0, 0, 128]);
    }

    #[test]
    fn zero_alpha_un_premultiplies_to_zero_color() {
        let mut data = vec![200u8, 100, 50, 0];
        un_premultiply_u8(&mut data, 4);
        assert_eq!(data, vec![0, 0, 0, 0]);
    }

    #[test]
    fn unsupported_encode_color_is_a_named_error() {
        let mut out = Vec::new();
        let err = TiffEncoder::new(Cursor::new(&mut out))
            .write_image(&[0u8; 4], 2, 2, ExtendedColorType::Bgra8)
            .unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
    }
}
