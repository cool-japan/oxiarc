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
//! the 16-bit and 32-bit-float cases that convenience method does not
//! cover.
//!
//! # 32-bit-per-channel is always IEEE float, never integer
//!
//! This crate's [`ColorType`] has no unsigned-32-bit variant (neither does
//! `image`'s own ten), so this module's private `color_type_to_tiff`'s only
//! source of a 32-bit-per-channel `TiffColorType` is
//! `ExtendedColorType::Rgb32F`/`Rgba32F`. [`TiffEncoder::write_image`]
//! overrides `ImageSpec`'s default `SampleFormat::Uint` to `IeeeFp` for
//! exactly that case, and [`TiffDecoder`]'s fast path only trusts a
//! `Rgb(32)`/`Rgba(32)` file as `Rgb32F`/`Rgba32F` once it has confirmed the
//! file's own declared `SampleFormat` really is `IeeeFp` — a genuine 32-bit
//! *integer* RGB TIFF (rare, but real, and not one this crate's ten colour
//! types can name) falls back to `Decoder::read_image_rgba8` like any other
//! type outside the clean table, rather than being silently reinterpreted
//! as float.
//!
//! # Everything that is not one of the ten clean combinations
//!
//! `Gray/GrayA/Rgb/Rgba` at 8 or 16 bits, plus float `Rgb/Rgba` at 32 bits,
//! map directly. A palette, CMYK, `YCbCr`, `Lab`, `Multiband` image, a
//! 32-bit-per-channel image that is genuinely integer rather than float, or
//! any other sample width or type, goes through
//! [`oxiarc_tiff::reader::Decoder::read_image_rgba8`] instead —
//! `oxiarc-tiff` already owns that colour math (palette lookup, `YCbCr`
//! matrix, CMYK-to-RGB, ...); reimplementing it here would not be "thin".
//!
//! **Known gap, inside `oxiarc-tiff`, not this crate:** that fallback's own
//! `colour::to_rgba8` scales every photometric branch (`Rgb`,
//! `BlackIsZero`/`WhiteIsZero`, ...) through `sample_at`, which for a
//! `Samples::F32`/`F64` source truncates the `[0.0, 1.0]`-range value to a
//! `u64` with `as` *before* the `bits`-wide integer scale runs. For any
//! sample in that normalised range the truncated integer is `0`, and
//! `scale_to_u8(0, 32)` is `0`, so a float TIFF that is not one of this
//! module's two clean `Rgb32F`/`Rgba32F` rows (a float grayscale image,
//! say — there is no float-luma [`ColorType`] to decode one into anyway)
//! decodes as **exactly black**, uniformly across the whole normalised
//! input range, rather than erroring or converting correctly — not merely
//! darkened, and not proportional to the source value at all. This crate's
//! own `Rgb32F`/`Rgba32F` fast path never reaches this: it reads
//! [`Samples::F32`] directly off `Decoder::read_image` and copies the
//! bytes, never through `to_rgba8`/`sample_at`. Verified two ways, not
//! assumed: by reading `oxiarc-tiff/src/colour.rs`'s `sample_at`/`to_rgba8`
//! directly, and by the
//! `known_gap_a_float_gray_tiff_outside_the_clean_table_decodes_as_pure_black`
//! regression test below, which actually encodes a mid-grey (0.5) `Gray(32)`
//! `IeeeFp` file and asserts the decoded pixel; out of this track's
//! ownership to fix (see the `oxiarc-image` handoff file for the report to
//! whoever owns `oxiarc-tiff` next).

use std::io::{Read, Seek, Write};

use oxiarc_tiff::{ColorType as TiffColorType, ExtraSamples, SampleFormat, Samples};

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, unsupported_color};
use crate::format::ImageFormat;
use crate::traits::{ImageDecoder, ImageEncoder, check_read_buffer};

/// The clean, directly-representable `(image color type, channels, expected
/// `SampleFormat`)` combinations, or `None` for anything that must go
/// through `read_image_rgba8`.
///
/// A file whose colour type matches one of these rows but whose *declared*
/// [`SampleFormat`] does not match the paired one (checked in
/// [`TiffDecoder::new`], not here) still falls back to `read_image_rgba8`:
/// `Gray(32)` with `SampleFormat::Uint`, say, is a real (if unusual) 32-bit
/// integer TIFF, not the `IeeeFp` float one this table's `Rgb(32)`/`Rgba(32)`
/// rows mean.
fn clean_mapping(tiff_color: TiffColorType) -> Option<(ColorType, u16, SampleFormat)> {
    match tiff_color {
        TiffColorType::Gray(8) => Some((ColorType::L8, 1, SampleFormat::Uint)),
        TiffColorType::Gray(16) => Some((ColorType::L16, 1, SampleFormat::Uint)),
        TiffColorType::GrayA(8) => Some((ColorType::La8, 2, SampleFormat::Uint)),
        TiffColorType::GrayA(16) => Some((ColorType::La16, 2, SampleFormat::Uint)),
        TiffColorType::Rgb(8) => Some((ColorType::Rgb8, 3, SampleFormat::Uint)),
        TiffColorType::Rgb(16) => Some((ColorType::Rgb16, 3, SampleFormat::Uint)),
        TiffColorType::Rgba(8) => Some((ColorType::Rgba8, 4, SampleFormat::Uint)),
        TiffColorType::Rgba(16) => Some((ColorType::Rgba16, 4, SampleFormat::Uint)),
        // `image`/`tiff` decode a 32-bit-per-channel IEEE float RGB(A) TIFF
        // to `ColorType::Rgb32F`/`Rgba32F`; there is no unsigned-32-bit
        // `ColorType` in this crate (nor in `image`'s own ten), so `Rgb(32)`
        // only ever reaches here when `SampleFormat` is confirmed `IeeeFp`.
        TiffColorType::Rgb(32) => Some((ColorType::Rgb32F, 3, SampleFormat::IeeeFp)),
        TiffColorType::Rgba(32) => Some((ColorType::Rgba32F, 4, SampleFormat::IeeeFp)),
        _ => None,
    }
}

/// Build a [`ImageError::Decoding`] naming why a decoded sample buffer could
/// not be handed back through the [`ImageDecoder::read_image`] contract.
fn length_mismatch(expected: usize, got: usize) -> ImageError {
    ImageError::Decoding(crate::error::DecodingError::new(
        ImageFormat::Tiff.into(),
        format!("decoded {got} bytes but the image header promised {expected}"),
    ))
}

/// [`slice::copy_from_slice`], returning a named error instead of panicking
/// when `data`'s length (set by the decoder's own output) disagrees with
/// `buf`'s length (set by the header this crate read first) — a lenient or
/// truncated file can make those disagree, and `read_image`'s contract is a
/// decode error, never a panic on untrusted input.
fn copy_checked(buf: &mut [u8], data: &[u8]) -> ImageResult<()> {
    if buf.len() != data.len() {
        return Err(length_mismatch(buf.len(), data.len()));
    }
    buf.copy_from_slice(data);
    Ok(())
}

/// As [`copy_checked`], widening each native-endian `u16` sample to two
/// bytes.
fn copy_u16_checked(buf: &mut [u8], data: &[u16]) -> ImageResult<()> {
    if buf.len() != data.len() * 2 {
        return Err(length_mismatch(buf.len(), data.len() * 2));
    }
    for (chunk, v) in buf.chunks_exact_mut(2).zip(data) {
        chunk.copy_from_slice(&v.to_ne_bytes());
    }
    Ok(())
}

/// As [`copy_checked`], widening each native-endian `f32` sample to four
/// bytes.
fn copy_f32_checked(buf: &mut [u8], data: &[f32]) -> ImageResult<()> {
    if buf.len() != data.len() * 4 {
        return Err(length_mismatch(buf.len(), data.len() * 4));
    }
    for (chunk, v) in buf.chunks_exact_mut(4).zip(data) {
        chunk.copy_from_slice(&v.to_ne_bytes());
    }
    Ok(())
}

fn un_premultiply_u8(data: &mut [u8], channels: usize) {
    for px in data.chunks_exact_mut(channels) {
        let (color, alpha) = px.split_at_mut(channels - 1);
        let a = u32::from(alpha[0]);
        for c in color {
            let numerator = u32::from(*c) * 255 + a / 2;
            *c = numerator.checked_div(a).map_or(0, |v| v.min(255) as u8);
        }
    }
}

fn un_premultiply_u16(data: &mut [u16], channels: usize) {
    for px in data.chunks_exact_mut(channels) {
        let (color, alpha) = px.split_at_mut(channels - 1);
        let a = u64::from(alpha[0]);
        for c in color {
            let numerator = u64::from(*c) * 65535 + a / 2;
            *c = numerator.checked_div(a).map_or(0, |v| v.min(65535) as u16);
        }
    }
}

/// As [`un_premultiply_u8`]/[`un_premultiply_u16`], for the `[0.0, 1.0]`
/// range [`crate::color::Primitive::DEFAULT_MAX_VALUE`] gives `f32`
/// channels — no `* 255`/`* 65535` scale factor, since "fully on" is
/// already `1.0`.
///
/// Deliberately **not** clamped to `1.0`. The whole point of a float TIFF is
/// that it can carry values outside `[0, 1]`, and the straight-alpha path
/// right beside this one ([`copy_f32_checked`] with `associated_alpha`
/// unset) copies such samples through untouched. Clamping only the
/// premultiplied path would make a file's alpha convention — an
/// `ExtraSamples` tag the caller never sees — decide silently whether a
/// highlight above `1.0` survives the decode. A caller that wants `[0, 1]`
/// samples clamps them itself; [`crate::DynamicImage::to_rgba8`]/`to_rgba16`
/// already do, on their way down to an integer buffer.
///
/// A zero alpha still yields zero colour (the premultiplied value was
/// necessarily zero too, and `0.0 / 0.0` is NaN, which is not a colour).
fn un_premultiply_f32(data: &mut [f32], channels: usize) {
    for px in data.chunks_exact_mut(channels) {
        let (color, alpha) = px.split_at_mut(channels - 1);
        let a = alpha[0];
        for c in color {
            *c = if a == 0.0 { 0.0 } else { *c / a };
        }
    }
}

/// The fast path's shape, decided once in [`TiffDecoder::new`] from the
/// file's colour type *and* `SampleFormat` (a `Gray(8)` image with
/// `SampleFormat::Int` or `IeeeFp` does not decode to `Samples::U8`, so the
/// clean 8-combo table alone is not enough to pick the fast path safely).
/// Which numeric width to expect is then read straight off the
/// [`Samples`] variant [`TiffDecoder::read_image`] actually gets back, so
/// this only needs to carry what that match cannot: the channel count (for
/// un-premultiplying) and whether to.
#[derive(Clone, Copy)]
struct FastPath {
    channels: u16,
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
        let associated_alpha = info.extra_samples.contains(&ExtraSamples::AssociatedAlpha);

        let (color_type, fast) = match mapping {
            Some((color_type, channels, expected_format))
                if info.sample_format.iter().all(|f| *f == expected_format) =>
            {
                (
                    color_type,
                    Some(FastPath {
                        channels,
                        associated_alpha: channels > 1 && associated_alpha,
                    }),
                )
            }
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
        check_read_buffer(buf, self.total_bytes())?;
        let Some(fast) = self.fast else {
            let rgba = self.decoder.read_image_rgba8()?;
            return copy_checked(buf, &rgba);
        };

        let samples = self.decoder.read_image()?;
        match samples {
            Samples::U8(mut data) => {
                if fast.associated_alpha {
                    un_premultiply_u8(&mut data, fast.channels as usize);
                }
                copy_checked(buf, &data)
            }
            Samples::U16(mut data) => {
                if fast.associated_alpha {
                    un_premultiply_u16(&mut data, fast.channels as usize);
                }
                copy_u16_checked(buf, &data)
            }
            Samples::F32(mut data) => {
                if fast.associated_alpha {
                    un_premultiply_f32(&mut data, fast.channels as usize);
                }
                copy_f32_checked(buf, &data)
            }
            other => {
                // The `SampleFormat` guard in `new` guarantees `read_image`
                // returns `U8`/`U16` for a `Uint`-format clean mapping and
                // `F32` for the `IeeeFp` one; kept as a named error rather
                // than a panic in case a future `oxiarc-tiff` release
                // changes what these (bit depth, format) pairs produce.
                Err(ImageError::Decoding(crate::error::DecodingError::new(
                    ImageFormat::Tiff.into(),
                    format!("unexpected sample type for a clean colour mapping: {other:?}"),
                )))
            }
        }
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
        // `ImageSpec::new` defaults every channel's `SampleFormat` to
        // `Uint`; `TiffEncoder::write_image` below overrides it to
        // `IeeeFp` for exactly these two colour types, which is what makes
        // a 32-bit-per-channel file an IEEE float TIFF instead of a
        // (much rarer, and not one this crate's `ColorType` can even name)
        // 32-bit unsigned integer one.
        ExtendedColorType::Rgb32F => TiffColorType::Rgb(32),
        ExtendedColorType::Rgba32F => TiffColorType::Rgba(32),
        ExtendedColorType::Cmyk8 => TiffColorType::Cmyk(8),
        ExtendedColorType::Cmyk16 => TiffColorType::Cmyk(16),
        other => return Err(unsupported_color(ImageFormat::Tiff, other)),
    })
}

/// A TIFF encoder, over any [`Write`] + [`Seek`] sink (TIFF's directory
/// offsets must be patched after the strips are written, so unlike PNG/JPEG
/// this one needs [`Seek`]).
///
/// ```
/// use oxiarc_image::codecs::tiff::{TiffDecoder, TiffEncoder};
/// use oxiarc_image::traits::{ImageDecoder, ImageEncoder};
/// use oxiarc_image::ExtendedColorType;
/// use std::io::Cursor;
///
/// let mut bytes = Vec::new();
/// TiffEncoder::new(Cursor::new(&mut bytes))
///     .write_image(&[10, 20, 30, 40], 2, 2, ExtendedColorType::L8)?;
///
/// let decoder = TiffDecoder::new(Cursor::new(bytes))?;
/// assert_eq!(decoder.dimensions(), (2, 2));
/// let mut pixels = vec![0u8; decoder.total_bytes() as usize];
/// decoder.read_image(&mut pixels)?;
/// assert_eq!(pixels, vec![10, 20, 30, 40]);
/// # Ok::<(), oxiarc_image::ImageError>(())
/// ```
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
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(usize::from(color_type.bits_per_pixel() / 8));
        if buf.len() != expected {
            return Err(ImageError::Parameter(
                crate::error::ParameterError::from_kind(
                    crate::error::ParameterErrorKind::DimensionMismatch,
                ),
            ));
        }

        let mut spec = oxiarc_tiff::ImageSpec::new(width, height, tiff_color);
        // `ImageSpec::new` always defaults `sample_format` to `Uint`; a
        // 32-bit-per-channel image from this crate is always the IEEE
        // float `Rgb32F`/`Rgba32F` `DynamicImage` variant (see
        // `color_type_to_tiff`), so it is the one case that needs the
        // override to actually round-trip as float rather than as a
        // 32-bit unsigned integer TIFF.
        if matches!(
            color_type,
            ExtendedColorType::Rgb32F | ExtendedColorType::Rgba32F
        ) {
            spec.sample_format =
                vec![oxiarc_tiff::SampleFormat::IeeeFp; spec.samples_per_pixel as usize];
        }

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

    #[test]
    fn round_trip_32_bit_float_rgb() {
        let native: Vec<f32> = vec![0.0, 0.25, 0.5, 1.0, -1.0, 3.5, f32::MIN_POSITIVE, 0.1];
        let mut bytes = Vec::new();
        for v in &native {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        // 8 f32 samples / 3 channels is not a whole number of pixels; pad to
        // 9 (3 RGB pixels) so the buffer matches `width * height * 3`.
        bytes.extend_from_slice(&0.0f32.to_ne_bytes());
        let file = encode_tiff(3, 1, ExtendedColorType::Rgb32F, &bytes);

        let decoder = TiffDecoder::new(Cursor::new(file)).expect("decode header");
        assert_eq!(decoder.dimensions(), (3, 1));
        assert_eq!(decoder.color_type(), ColorType::Rgb32F);
        assert_eq!(decoder.total_bytes(), bytes.len() as u64);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(buf, bytes, "float TIFF round trip must be byte-exact");
    }

    #[test]
    fn round_trip_32_bit_float_rgba_with_straight_alpha() {
        let native: Vec<f32> = vec![
            1.0, 0.0, 0.0, 0.5, // half-alpha red
            0.0, 1.0, 0.0, 1.0, // opaque green
        ];
        let mut bytes = Vec::new();
        for v in &native {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        let file = encode_tiff(2, 1, ExtendedColorType::Rgba32F, &bytes);

        let decoder = TiffDecoder::new(Cursor::new(file)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::Rgba32F);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        // `ImageSpec::new` records `Rgba` as `ExtraSamples::UnassociatedAlpha`
        // (straight), matching this crate's own convention, so no
        // premultiply/un-premultiply round trip is involved here and the
        // bytes must come back exactly as written.
        assert_eq!(buf, bytes);
    }

    #[test]
    fn un_premultiply_f32_matches_the_integer_versions_at_the_same_ratios() {
        // Half-intensity red at half alpha, premultiplied: (0.5, 0.0, 0.0, 0.5).
        // Straight equivalent is (1.0, 0.0, 0.0, 0.5) -- the same ratio
        // `premultiplied_alpha_is_undone_on_the_fast_path` proves for `u8`.
        let mut data = vec![0.5f32, 0.0, 0.0, 0.5];
        un_premultiply_f32(&mut data, 4);
        assert_eq!(data, vec![1.0, 0.0, 0.0, 0.5]);
    }

    /// A premultiplied float sample whose straight value is above `1.0` is a
    /// legitimate HDR highlight, not an error: un-premultiplying must
    /// reproduce it, not clamp it away. (`0.6 / 0.5 == 1.2`.)
    #[test]
    fn un_premultiply_f32_preserves_values_above_one() {
        let mut data = vec![0.6f32, 1.0, 0.05, 0.5];
        un_premultiply_f32(&mut data, 4);
        assert_eq!(data, vec![1.2, 2.0, 0.1, 0.5]);
    }

    /// End to end through a real file, not just the helper: the same
    /// above-`1.0` highlight must survive encode + decode when the file
    /// declares associated (premultiplied) alpha.
    #[test]
    fn an_hdr_highlight_survives_a_premultiplied_float_tiff_round_trip() {
        let mut spec = oxiarc_tiff::ImageSpec::new(1, 1, TiffColorType::Rgba(32));
        spec.sample_format = vec![SampleFormat::IeeeFp; spec.samples_per_pixel as usize];
        spec.extra_samples = vec![ExtraSamples::AssociatedAlpha];
        // Straight (1.2, 2.0, 0.1) at alpha 0.5, stored premultiplied.
        let stored: [f32; 4] = [0.6, 1.0, 0.05, 0.5];
        let mut bytes = Vec::new();
        for v in stored {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        let mut out = Vec::new();
        let mut encoder =
            oxiarc_tiff::Encoder::new(Cursor::new(&mut out)).expect("construct encoder");
        encoder.write_image(&spec, &bytes).expect("encode");
        encoder.finish().expect("finish");

        let decoder = TiffDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::Rgba32F);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        let decoded: Vec<f32> = buf
            .chunks_exact(4)
            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(decoded, vec![1.2, 2.0, 0.1, 0.5]);
    }

    #[test]
    fn read_image_rejects_a_buffer_that_is_not_exactly_total_bytes() {
        let bytes = encode_tiff(2, 2, ExtendedColorType::L8, &[1, 2, 3, 4]);
        let decoder = TiffDecoder::new(Cursor::new(bytes.clone())).expect("decode header");
        let mut too_small = vec![0u8; 3];
        assert!(matches!(
            decoder.read_image(&mut too_small),
            Err(ImageError::Parameter(_))
        ));
        let decoder = TiffDecoder::new(Cursor::new(bytes)).expect("decode header");
        let mut too_big = vec![0u8; 40];
        assert!(matches!(
            decoder.read_image(&mut too_big),
            Err(ImageError::Parameter(_))
        ));
    }

    #[test]
    fn un_premultiply_f32_zero_alpha_is_zero_color() {
        let mut data = vec![0.7f32, 0.3, 0.1, 0.0];
        un_premultiply_f32(&mut data, 4);
        assert_eq!(data, vec![0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_genuine_32_bit_integer_rgb_tiff_is_not_misread_as_float() {
        // Build the file directly through `oxiarc_tiff`'s native writer
        // (bypassing this crate's own `color_type_to_tiff`, which never
        // produces an `Uint`-format 32-bit file) so the fast-path guard is
        // exercised against a file this crate cannot itself construct.
        let spec = oxiarc_tiff::ImageSpec::new(2, 1, TiffColorType::Rgb(32));
        assert_eq!(spec.sample_format, vec![SampleFormat::Uint; 3]);
        let native: Vec<u32> = vec![0, 1_000_000, 2_000_000, 3_000_000, 4_000_000, 5_000_000];
        let mut bytes = Vec::new();
        for v in &native {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        let mut out = Vec::new();
        let mut encoder =
            oxiarc_tiff::Encoder::new(Cursor::new(&mut out)).expect("construct encoder");
        encoder.write_image(&spec, &bytes).expect("encode");
        encoder.finish().expect("finish");

        let decoder = TiffDecoder::new(Cursor::new(out)).expect("decode header");
        // The `SampleFormat` guard must reject the fast path (this is an
        // integer file, not `IeeeFp`): falls back to `Rgba8`, never claims
        // `Rgb32F` for data that is not actually float.
        assert_eq!(decoder.color_type(), ColorType::Rgba8);
    }

    #[test]
    fn known_gap_a_float_gray_tiff_outside_the_clean_table_decodes_as_pure_black() {
        // `Gray(32)` has no clean-table row (this crate's `ColorType` has
        // no float-luma variant), so it always falls back to
        // `Decoder::read_image_rgba8`. This regression test pins down, by
        // actually running it rather than by reasoning about `colour.rs`,
        // exactly how wrong that fallback is for float data: a real
        // mid-grey sample (0.5) comes back as pure black, not a merely
        // darkened grey, and not an error either. See the module doc's
        // "Known gap, inside `oxiarc-tiff`, not this crate" note. If
        // `oxiarc-tiff` ever fixes `sample_at`'s float truncation, this
        // assertion (not just the doc comment) will start failing and say
        // so.
        let mut spec = oxiarc_tiff::ImageSpec::new(1, 1, TiffColorType::Gray(32));
        spec.sample_format = vec![SampleFormat::IeeeFp; spec.samples_per_pixel as usize];
        let bytes = 0.5f32.to_ne_bytes();
        let mut out = Vec::new();
        let mut encoder =
            oxiarc_tiff::Encoder::new(Cursor::new(&mut out)).expect("construct encoder");
        encoder.write_image(&spec, &bytes).expect("encode");
        encoder.finish().expect("finish");

        let decoder = TiffDecoder::new(Cursor::new(out)).expect("decode header");
        assert_eq!(decoder.color_type(), ColorType::Rgba8);
        let mut buf = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut buf).expect("read");
        assert_eq!(
            buf,
            vec![0, 0, 0, 255],
            "0.5 truncates to integer 0 before the bit-depth scale runs, per the module doc"
        );
    }

    #[test]
    fn wrong_buffer_length_is_a_parameter_error() {
        let mut out = Vec::new();
        let err = TiffEncoder::new(Cursor::new(&mut out))
            .write_image(&[0u8; 5], 2, 2, ExtendedColorType::L8)
            .unwrap_err();
        assert!(matches!(err, ImageError::Parameter(_)));
    }
}
