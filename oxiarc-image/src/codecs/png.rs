//! PNG decoding and encoding, over `oxiarc-png`.
//!
//! # 16-bit byte order
//!
//! PNG always stores multi-byte samples big-endian, with no per-file choice
//! (unlike TIFF). [`crate::ImageDecoder`]/[`crate::ImageEncoder`] promise
//! *native*-endian bytes at their boundary (so `[u16]`/`[f32]` buffers can be
//! reinterpreted as `[u8]` freely by the caller), and `oxiarc_png`'s own
//! `Writer::write_image_data`/`Reader::next_frame` do **no** endian
//! conversion of their own — confirmed by reading `encoder/image_data.rs`
//! (no `to_be_bytes` on sample data) and by the same absence in the
//! reference `png` crate's `encoder.rs`. So both directions here swap every
//! 16-bit sample explicitly: decode converts the file's big-endian pairs to
//! native, encode converts the caller's native pairs to big-endian. Getting
//! this backwards would silently byte-swap every 16-bit PNG this crate
//! writes or reads; `tests/roundtrip.rs`'s 16-bit cases decode the encoded
//! bytes through `oxiarc_png::decode` directly (an independent path) and
//! check the *raw* big-endian bytes, not just facade-to-facade agreement.

use std::io::{Read, Write};

use oxiarc_png::{BitDepth as PngBitDepth, ColorType as PngColorType, Transformations};

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, unsupported_color};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder, check_read_buffer};

fn expanded_to_color_type(color: PngColorType, depth: PngBitDepth) -> ImageResult<ColorType> {
    Ok(match (color, depth) {
        (PngColorType::Grayscale, PngBitDepth::Eight) => ColorType::L8,
        (PngColorType::Grayscale, PngBitDepth::Sixteen) => ColorType::L16,
        (PngColorType::GrayscaleAlpha, PngBitDepth::Eight) => ColorType::La8,
        (PngColorType::GrayscaleAlpha, PngBitDepth::Sixteen) => ColorType::La16,
        (PngColorType::Rgb, PngBitDepth::Eight) => ColorType::Rgb8,
        (PngColorType::Rgb, PngBitDepth::Sixteen) => ColorType::Rgb16,
        (PngColorType::Rgba, PngBitDepth::Eight) => ColorType::Rgba8,
        (PngColorType::Rgba, PngBitDepth::Sixteen) => ColorType::Rgba16,
        // Unreachable for a spec-valid file once `Transformations::EXPAND`
        // has run (it lifts every sub-8-bit and Indexed combination into one
        // of the eight rows above); kept as a named error rather than a
        // panic in case a future decoder change surfaces a new combination.
        (_, other_depth) => {
            return Err(unsupported_color(
                ImageFormat::Png,
                ExtendedColorType::Unknown(other_depth as u8),
            ));
        }
    })
}

fn color_type_to_png(color: ExtendedColorType) -> ImageResult<(PngColorType, PngBitDepth)> {
    Ok(match color {
        ExtendedColorType::L8 => (PngColorType::Grayscale, PngBitDepth::Eight),
        ExtendedColorType::La8 => (PngColorType::GrayscaleAlpha, PngBitDepth::Eight),
        ExtendedColorType::Rgb8 => (PngColorType::Rgb, PngBitDepth::Eight),
        ExtendedColorType::Rgba8 => (PngColorType::Rgba, PngBitDepth::Eight),
        ExtendedColorType::L16 => (PngColorType::Grayscale, PngBitDepth::Sixteen),
        ExtendedColorType::La16 => (PngColorType::GrayscaleAlpha, PngBitDepth::Sixteen),
        ExtendedColorType::Rgb16 => (PngColorType::Rgb, PngBitDepth::Sixteen),
        ExtendedColorType::Rgba16 => (PngColorType::Rgba, PngBitDepth::Sixteen),
        other => return Err(unsupported_color(ImageFormat::Png, other)),
    })
}

/// Every 16-bit sample in `buf`, read as native-endian, rewritten as
/// big-endian: the PNG *decode* direction.
fn be_pairs_to_native(buf: &mut [u8]) {
    for pair in buf.chunks_exact_mut(2) {
        let native = u16::from_be_bytes([pair[0], pair[1]]);
        pair.copy_from_slice(&native.to_ne_bytes());
    }
}

/// The PNG *encode* direction of the same swap.
fn native_pairs_to_be(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    for pair in buf.chunks_exact(2) {
        let native = u16::from_ne_bytes([pair[0], pair[1]]);
        out.extend_from_slice(&native.to_be_bytes());
    }
    out
}

/// A PNG decoder, over any [`Read`] source.
///
/// Decodes with `oxiarc_png::Transformations::EXPAND`: palette and `tRNS`
/// expand to RGB(A), and sub-8-bit grayscale expands to 8-bit — the same
/// transformation `image::codecs::png::PngDecoder` applies, so the same ten
/// (colour type, bit depth) combinations are the only ones that can reach
/// [`ColorType`] here.
///
/// This is the entry point [`crate::codecs::png`] wraps; most callers reach
/// it indirectly through [`crate::DynamicImage`]/[`crate::ImageReader`] and
/// only need this type directly for the PNG-specific encoder options below.
///
/// ```
/// use oxiarc_image::codecs::png::{PngDecoder, PngEncoder};
/// use oxiarc_image::traits::{ImageDecoder, ImageEncoder};
/// use oxiarc_image::ExtendedColorType;
/// use std::io::Cursor;
///
/// let mut bytes = Vec::new();
/// PngEncoder::new(&mut bytes).write_image(&[0, 128, 255, 64], 2, 2, ExtendedColorType::L8)?;
///
/// let decoder = PngDecoder::new(Cursor::new(bytes))?;
/// assert_eq!(decoder.dimensions(), (2, 2));
/// let mut pixels = vec![0u8; decoder.total_bytes() as usize];
/// decoder.read_image(&mut pixels)?;
/// assert_eq!(pixels, vec![0, 128, 255, 64]);
/// # Ok::<(), oxiarc_image::ImageError>(())
/// ```
pub struct PngDecoder<R: Read> {
    reader: oxiarc_png::Reader<R>,
    color_type: ColorType,
    width: u32,
    height: u32,
}

impl<R: Read> PngDecoder<R> {
    /// Start decoding `r`.
    ///
    /// # Errors
    /// Any malformed PNG, or a colour type/bit-depth combination that does
    /// not survive `EXPAND` (there is none for a spec-valid file).
    pub fn new(r: R) -> ImageResult<Self> {
        let mut decoder = oxiarc_png::Decoder::new(r);
        decoder.set_transformations(Transformations::EXPAND);
        let reader = decoder.read_info()?;
        let (png_color, png_depth) = reader.output_color_type();
        let color_type = expanded_to_color_type(png_color, png_depth)?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        Ok(Self {
            reader,
            color_type,
            width,
            height,
        })
    }
}

impl<R: Read> ImageDecoder for PngDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn read_image(mut self, buf: &mut [u8]) -> ImageResult<()> {
        check_read_buffer(buf, self.total_bytes())?;
        self.reader.next_frame(buf)?;
        if matches!(
            self.color_type,
            ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16
        ) {
            be_pairs_to_native(buf);
        }
        Ok(())
    }
}

/// DEFLATE compression effort. Matches `image::codecs::png::CompressionType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionType {
    /// The library's own balanced default (level 6).
    Default,
    /// Fast, minimal compression. `PngEncoder::new`'s own default.
    #[default]
    Fast,
    /// The smallest output this crate reaches without an optimal parser.
    Best,
    /// Store, uncompressed.
    Uncompressed,
    /// An explicit `oxiarc-deflate` level in `0..=9`.
    ///
    /// A level above 9 is clamped to 9 rather than passed through (see the
    /// private `deflate_level` helper), since `oxiarc-deflate` has no
    /// defined meaning above 9.
    Level(u8),
}

impl CompressionType {
    /// The `oxiarc-deflate` level this setting asks for, `None` for the
    /// named presets that map onto an `oxiarc_png::Compression` instead.
    ///
    /// [`CompressionType::Level`] documents a `0..=9` range but is a plain
    /// `u8`, so a caller can construct `Level(200)`. Clamping here keeps
    /// that from reaching `oxiarc_png::DeflateCompression::Level`, whose own
    /// range is the same `0..=9` and which has no defined meaning above it.
    const fn deflate_level(self) -> Option<u8> {
        match self {
            Self::Level(0) => None,
            Self::Level(level) => Some(if level > 9 { 9 } else { level }),
            _ => None,
        }
    }
}

/// Which filter heuristic to use. Matches
/// `image::codecs::png::FilterType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterType {
    /// Never filter.
    NoFilter,
    /// Always [`oxiarc_png::Filter::Sub`].
    Sub,
    /// Always [`oxiarc_png::Filter::Up`].
    Up,
    /// Always [`oxiarc_png::Filter::Avg`].
    Avg,
    /// Always [`oxiarc_png::Filter::Paeth`].
    Paeth,
    /// Pick per row by minimum-sum-of-absolute-differences.
    #[default]
    Adaptive,
}

/// A PNG encoder, over any [`Write`] sink.
pub struct PngEncoder<W: Write> {
    w: W,
    compression: CompressionType,
    filter: FilterType,
}

impl<W: Write> PngEncoder<W> {
    /// A new encoder with the fast/adaptive defaults.
    pub fn new(w: W) -> Self {
        Self {
            w,
            compression: CompressionType::default(),
            filter: FilterType::default(),
        }
    }

    /// A new encoder with explicit compression and filter settings.
    pub fn new_with_quality(w: W, compression: CompressionType, filter: FilterType) -> Self {
        Self {
            w,
            compression,
            filter,
        }
    }
}

impl<W: Write> ImageEncoder for PngEncoder<W> {
    fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ExtendedColorType,
    ) -> ImageResult<()> {
        let (png_color, png_depth) = color_type_to_png(color_type)?;
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(png_color.samples())
            .saturating_mul(if png_depth == PngBitDepth::Sixteen {
                2
            } else {
                1
            });
        if buf.len() != expected {
            return Err(ImageError::Parameter(
                crate::error::ParameterError::from_kind(
                    crate::error::ParameterErrorKind::DimensionMismatch,
                ),
            ));
        }

        let mut encoder = oxiarc_png::Encoder::new(self.w, width, height);
        encoder.set_color(png_color);
        encoder.set_depth(png_depth);
        encoder.set_compression(match self.compression {
            CompressionType::Default => oxiarc_png::Compression::Balanced,
            CompressionType::Fast => oxiarc_png::Compression::Fastest,
            CompressionType::Best => oxiarc_png::Compression::High,
            CompressionType::Uncompressed => oxiarc_png::Compression::NoCompression,
            CompressionType::Level(0) => oxiarc_png::Compression::NoCompression,
            CompressionType::Level(_) => oxiarc_png::Compression::Balanced,
        });
        if let Some(level) = self.compression.deflate_level() {
            encoder.set_deflate_compression(oxiarc_png::DeflateCompression::Level(level));
        }
        encoder.set_filter(match self.filter {
            FilterType::NoFilter => oxiarc_png::Filter::NoFilter,
            FilterType::Sub => oxiarc_png::Filter::Sub,
            FilterType::Up => oxiarc_png::Filter::Up,
            FilterType::Avg => oxiarc_png::Filter::Avg,
            FilterType::Paeth => oxiarc_png::Filter::Paeth,
            FilterType::Adaptive => oxiarc_png::Filter::Adaptive,
        });

        let mut writer = encoder.write_header()?;
        if png_depth == PngBitDepth::Sixteen {
            writer.write_image_data(&native_pairs_to_be(buf))?;
        } else {
            writer.write_image_data(buf)?;
        }
        writer.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode_l8(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(pixels, width, height, ExtendedColorType::L8)
            .expect("encode");
        out
    }

    #[test]
    fn round_trip_8_bit_grayscale() {
        let png = encode_l8(2, 2, &[0, 64, 128, 255]);
        let decoder = PngDecoder::new(Cursor::new(png)).expect("decode header");
        assert_eq!(decoder.dimensions(), (2, 2));
        assert_eq!(decoder.color_type(), ColorType::L8);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, vec![0, 64, 128, 255]);
    }

    #[test]
    fn sixteen_bit_round_trips_through_the_facade_and_matches_the_raw_be_bytes() {
        // 0x1234 and 0xABCD as two native u16 samples.
        let native: [u16; 2] = [0x1234, 0xABCD];
        let mut buf = Vec::new();
        for v in native {
            buf.extend_from_slice(&v.to_ne_bytes());
        }
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(&buf, 2, 1, ExtendedColorType::L16)
            .expect("encode 16-bit");

        // Independent check: decode with oxiarc_png's own native `Image`
        // API, whose `data` field is documented big-endian, and assert the
        // exact on-disk bytes -- not merely that this crate's own encoder
        // and decoder agree with each other.
        let image = oxiarc_png::decode(&out).expect("oxiarc_png::decode");
        assert_eq!(image.data, vec![0x12, 0x34, 0xAB, 0xCD]);

        // And through this crate's own ImageDecoder, which must hand back
        // native-endian bytes per the trait contract.
        let decoder = PngDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::L16);
        let mut round_tripped = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut round_tripped).expect("read");
        assert_eq!(round_tripped, buf);
    }

    #[test]
    fn wrong_buffer_length_is_a_parameter_error() {
        let mut out = Vec::new();
        let err = PngEncoder::new(&mut out)
            .write_image(&[0u8; 3], 2, 2, ExtendedColorType::L8)
            .unwrap_err();
        assert!(matches!(err, ImageError::Parameter(_)));
    }

    #[test]
    fn unsupported_color_type_is_a_named_error() {
        let mut out = Vec::new();
        let err = PngEncoder::new(&mut out)
            .write_image(&[0u8; 1], 1, 1, ExtendedColorType::Cmyk8)
            .unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
    }

    #[test]
    fn an_out_of_range_compression_level_is_clamped_not_passed_through() {
        assert_eq!(CompressionType::Level(0).deflate_level(), None);
        assert_eq!(CompressionType::Level(1).deflate_level(), Some(1));
        assert_eq!(CompressionType::Level(9).deflate_level(), Some(9));
        assert_eq!(CompressionType::Level(10).deflate_level(), Some(9));
        assert_eq!(CompressionType::Level(u8::MAX).deflate_level(), Some(9));
        assert_eq!(CompressionType::Best.deflate_level(), None);

        // And end to end: an absurd level still produces a file this crate's
        // own decoder reads back byte-for-byte.
        let pixels = [7u8, 200, 3, 90];
        let mut out = Vec::new();
        PngEncoder::new_with_quality(&mut out, CompressionType::Level(200), FilterType::Adaptive)
            .write_image(&pixels, 2, 2, ExtendedColorType::L8)
            .expect("an out-of-range level must still encode");
        let decoder = PngDecoder::new(Cursor::new(out)).expect("decode");
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, pixels);
    }

    #[test]
    fn read_image_rejects_a_buffer_that_is_not_exactly_total_bytes() {
        let png = encode_l8(2, 2, &[0, 64, 128, 255]);

        let decoder = PngDecoder::new(Cursor::new(png.clone())).expect("decode header");
        let mut too_small = vec![0u8; 3];
        assert!(matches!(
            decoder.read_image(&mut too_small),
            Err(ImageError::Parameter(_))
        ));

        // The over-sized case is the one that used to succeed silently,
        // leaving the tail of the caller's buffer untouched with no error.
        let decoder = PngDecoder::new(Cursor::new(png)).expect("decode header");
        let mut too_big = vec![0u8; 4 + 16];
        assert!(matches!(
            decoder.read_image(&mut too_big),
            Err(ImageError::Parameter(_))
        ));
    }

    #[test]
    fn rgba8_round_trips() {
        let pixels = vec![10, 20, 30, 255, 40, 50, 60, 128];
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(&pixels, 2, 1, ExtendedColorType::Rgba8)
            .expect("encode");
        let decoder = PngDecoder::new(Cursor::new(out)).expect("decode");
        assert_eq!(decoder.color_type(), ColorType::Rgba8);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, pixels);
    }
}
