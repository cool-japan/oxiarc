//! A drop-in shaped facade for `zune_jpeg::JpegDecoder`.
//!
//! Call sites that decode a JPEG with `zune_jpeg` can move here with a `use`
//! change and, for `RGB`/`Luma` output, nothing else:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::compat::zune::JpegDecoder;
//!
//! let bytes: &[u8] = &oxiarc_jpeg::sample::RGB_8X8_420;
//! let mut decoder = JpegDecoder::new(bytes);
//! let pixels = decoder.decode()?;
//! let info = decoder.info().expect("decode() populates it");
//! assert_eq!(pixels.len(), usize::from(info.width) * usize::from(info.height) * 3);
//! # Ok(())
//! # }
//! ```
//!
//! Shaped after `zune-jpeg` 0.5's public API (`JpegDecoder::{new,
//! new_with_options, decode, decode_headers, info, output_buffer_size,
//! input_colorspace, output_colorspace, set_options, icc_profile, exif}`),
//! read directly from its source rather than from memory so the method names
//! and shapes are not guessed. Not `#[deprecated]`: this is a migration aid
//! for call sites that use a JPEG decoder directly, not a reimplementation of
//! `zune_jpeg`'s SIMD-tuned internals. [`crate::Decoder`] is the strictly
//! larger native API — progressive control, twelve-bit, restart intervals,
//! [`crate::DecodeOptions::scale`] and TIFF's abbreviated mode all live there
//! and have no counterpart in this facade or in `zune_jpeg`.

use crate::color::ColorSpace as NativeColorSpace;
use crate::decoder::{DecodeOptions as NativeOptions, Decoder as NativeDecoder};
#[cfg(test)]
use crate::error::JpegError;
use crate::error::{Result, UnsupportedFeature};

/// The output pixel layouts this facade can select, named as
/// `zune_core::colorspace::ColorSpace` names them. A subset: `zune_core`'s
/// real enum also has `BGR`, `BGRA`, `ARGB`, `HSL`, `HSV` and more, which
/// this crate's colour pipeline has no equivalent of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    /// Three 8-bit samples per pixel, red first.
    Rgb,
    /// Four 8-bit samples per pixel, red first, alpha last. A JPEG carries
    /// no alpha channel; the fourth byte is always `255`, matching
    /// `zune_jpeg`'s own synthesised alpha for the same reason.
    Rgba,
    /// One 8-bit luminance sample per pixel.
    Luma,
    /// Y, Cb and Cr, interleaved, chroma already upsampled to full
    /// resolution but with no colour transform applied — this crate's
    /// [`crate::DecodeOptions::raw`].
    YCbCr,
}

impl ColorSpace {
    /// Samples per pixel this colour space writes.
    #[must_use]
    pub const fn num_components(self) -> usize {
        match self {
            ColorSpace::Rgb | ColorSpace::YCbCr => 3,
            ColorSpace::Rgba => 4,
            ColorSpace::Luma => 1,
        }
    }
}

/// Decoder options, shaped after `zune_core::options::DecoderOptions`'s
/// builder style (`decoder.options().jpeg_set_out_colorspace(cs)`). The one
/// setting this facade exposes is the output colour space; `zune_core`'s
/// real type also carries strictness flags, a maximum size and a thread
/// count, which this crate expresses instead through
/// [`crate::DecodeOptions`] and [`crate::DecodeLimits`] on
/// [`crate::Decoder`] directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderOptions {
    out_colorspace: ColorSpace,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            out_colorspace: ColorSpace::Rgb,
        }
    }
}

impl DecoderOptions {
    /// Select the output colour space. Consumes and returns `self`, matching
    /// `zune_core::options::DecoderOptions`'s own builder methods.
    #[must_use]
    pub const fn jpeg_set_out_colorspace(mut self, colorspace: ColorSpace) -> Self {
        self.out_colorspace = colorspace;
        self
    }

    /// The output colour space this instance currently selects.
    #[must_use]
    pub const fn out_colorspace(self) -> ColorSpace {
        self.out_colorspace
    }
}

/// Image metadata, shaped after `zune_jpeg::ImageInfo`'s most-used fields
/// (its real struct also carries pixel density, gain-map and multi-picture
/// fields this crate's decoder does not read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Width of the image, in pixels.
    pub width: u16,
    /// Height of the image, in pixels.
    pub height: u16,
    /// Number of components the `SOF` declared (before any colour
    /// transform; `output_buffer_size` accounts for the transform).
    pub components: u8,
}

/// A JPEG decoder with the method names `zune_jpeg::JpegDecoder` uses.
pub struct JpegDecoder<'a> {
    bytes: &'a [u8],
    options: DecoderOptions,
    info: Option<crate::ImageInfo>,
}

impl<'a> JpegDecoder<'a> {
    /// A new decoder over `bytes`, with `Rgb` as the default output colour
    /// space (matching `zune_jpeg`'s own default).
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self::new_with_options(bytes, DecoderOptions::default())
    }

    /// A new decoder with explicit options.
    #[must_use]
    pub fn new_with_options(bytes: &'a [u8], options: DecoderOptions) -> Self {
        Self {
            bytes,
            options,
            info: None,
        }
    }

    /// Parse headers without decoding pixels. After this returns `Ok`,
    /// [`JpegDecoder::info`] and [`JpegDecoder::dimensions`] are populated.
    ///
    /// # Errors
    ///
    /// Whatever [`crate::Decoder::read_info`] returns.
    pub fn decode_headers(&mut self) -> Result<()> {
        let mut decoder = NativeDecoder::new(self.bytes);
        let info = decoder.read_info()?;
        self.info = Some(info);
        Ok(())
    }

    /// Image information. `None` until [`JpegDecoder::decode_headers`] or
    /// [`JpegDecoder::decode`] has returned `Ok`.
    #[must_use]
    pub fn info(&self) -> Option<ImageInfo> {
        self.info.map(|info| ImageInfo {
            width: info.width,
            height: info.height,
            components: info.num_components,
        })
    }

    /// `(width, height)`, as `usize`. `None` before headers are read.
    #[must_use]
    pub fn dimensions(&self) -> Option<(usize, usize)> {
        self.info
            .map(|info| (usize::from(info.width), usize::from(info.height)))
    }

    /// Bytes a decode into [`JpegDecoder::options`]'s colour space will
    /// occupy — exactly [`JpegDecoder::decode`]'s output length whenever that
    /// decode succeeds. `None` before headers are read.
    ///
    /// [`ColorSpace::YCbCr`] is the one request whose width is not the
    /// colour space's own [`ColorSpace::num_components`]: it is a raw,
    /// untransformed passthrough of *the source's* components (this crate's
    /// [`crate::DecodeOptions::raw`]), so a one-component source produces one
    /// sample per pixel and a four-component (CMYK/YCCK) source produces
    /// four. Reporting the enum's nominal `3` for those told a caller to
    /// allocate a third too much for a grayscale frame and a quarter too
    /// little for a CMYK one — measured on a 9x7 fixture: `189` reported
    /// against `63` produced for grayscale, and `189` against `252` for
    /// CMYK.
    #[must_use]
    pub fn output_buffer_size(&self) -> Option<usize> {
        let info = self.info?;
        Some(usize::from(info.width) * usize::from(info.height) * self.produced_components(info))
    }

    /// Samples per pixel [`JpegDecoder::decode`] actually writes.
    fn produced_components(&self, info: crate::ImageInfo) -> usize {
        match self.options.out_colorspace {
            ColorSpace::YCbCr => usize::from(info.num_components),
            other => other.num_components(),
        }
    }

    /// The colour space the `SOF`/`APP14` declare the source to be in.
    /// `None` before headers are read.
    #[must_use]
    pub fn input_colorspace(&self) -> Option<NativeColorSpace> {
        self.info.map(|info| info.input_color_space)
    }

    /// The colour space a decode will actually produce: whatever
    /// [`JpegDecoder::options`] asks for. `None` before headers are read.
    ///
    /// This used to answer [`ColorSpace::Luma`] for a single-component source
    /// whatever was requested, on the stated grounds that it mirrored
    /// `zune_jpeg`. It does not, and it contradicted this facade's own
    /// [`JpegDecoder::decode`], which expands a grayscale source to the
    /// requested width (three bytes per pixel for [`ColorSpace::Rgb`], four
    /// for [`ColorSpace::Rgba`]) exactly as `zune_jpeg`'s `worker.rs`
    /// `(Luma, RGB)` / `(Luma, RGBA)` arms do — real `zune_jpeg` 0.5.15
    /// returns `self.options.jpeg_get_out_colorspace()` unmodified, and the
    /// line in its `headers.rs` that would have forced `Luma` for a
    /// one-component frame is commented out. All three accessors now agree
    /// with each other and with the crate this facade is shaped after.
    #[must_use]
    pub fn output_colorspace(&self) -> Option<ColorSpace> {
        self.info.map(|_| self.options.out_colorspace)
    }

    /// Current decoder options.
    #[must_use]
    pub const fn options(&self) -> DecoderOptions {
        self.options
    }

    /// Replace the decoder options. Has no effect on a decode already in
    /// progress; call before [`JpegDecoder::decode`].
    pub fn set_options(&mut self, options: DecoderOptions) {
        self.options = options;
    }

    /// The embedded ICC profile, reassembled from its `APP2` chunks, if any.
    ///
    /// # Errors
    ///
    /// [`JpegError`] if the `APP2` chunks are malformed (a chunk-count
    /// disagreement, a duplicate sequence number, or a total size over the
    /// crate's cap).
    pub fn icc_profile(&mut self) -> Result<Option<Vec<u8>>> {
        let mut decoder = NativeDecoder::new(self.bytes);
        decoder.read_info()?;
        decoder.icc_profile()
    }

    /// The raw EXIF payload (`APP1`, `Exif\0\0` prefix removed), if any.
    ///
    /// # Errors
    ///
    /// Whatever [`crate::Decoder::read_info`] returns.
    pub fn exif(&mut self) -> Result<Option<Vec<u8>>> {
        let mut decoder = NativeDecoder::new(self.bytes);
        decoder.read_info()?;
        Ok(decoder.exif().map(<[u8]>::to_vec))
    }

    /// Decode the whole image into [`JpegDecoder::options`]'s colour space.
    ///
    /// # Errors
    ///
    /// [`JpegError::Unsupported`] for a colour-space request this crate's
    /// pipeline cannot produce from the source (e.g. `Rgb` from a CMYK
    /// source — `zune_jpeg` and this crate both convert YCbCr/YCCK sources
    /// but neither converts CMYK), and whatever
    /// [`crate::Decoder::read_info`]/[`crate::Decoder::decode`] return
    /// otherwise.
    pub fn decode(&mut self) -> Result<Vec<u8>> {
        let requested = self.options.out_colorspace;
        let native_options = match requested {
            ColorSpace::YCbCr => NativeOptions::raw(),
            ColorSpace::Luma => NativeOptions {
                output_color_space: Some(NativeColorSpace::Luma),
                ..NativeOptions::default()
            },
            ColorSpace::Rgb | ColorSpace::Rgba => NativeOptions {
                output_color_space: Some(NativeColorSpace::Rgb),
                ..NativeOptions::default()
            },
        };
        let mut decoder = NativeDecoder::with_options(self.bytes, native_options);
        let info = decoder.read_info()?;
        self.info = Some(info);

        // A single-component source has no chroma to convert: honour the
        // request by expanding, the same way `zune_jpeg` does, rather than
        // erroring or silently ignoring the request.
        if info.num_components == 1 && requested != ColorSpace::YCbCr {
            let mut luma_decoder = NativeDecoder::with_options(
                self.bytes,
                NativeOptions {
                    output_color_space: Some(NativeColorSpace::Luma),
                    ..NativeOptions::default()
                },
            );
            luma_decoder.read_info()?;
            let luma = luma_decoder.decode()?;
            return Ok(expand_luma(&luma, requested));
        }

        match requested {
            ColorSpace::YCbCr | ColorSpace::Luma => decoder.decode(),
            ColorSpace::Rgb => decoder.decode(),
            ColorSpace::Rgba => {
                let rgb = decoder.decode()?;
                Ok(rgb_to_rgba(&rgb))
            }
        }
    }
}

/// Expand one luminance sample per pixel into `requested`'s component count.
fn expand_luma(luma: &[u8], requested: ColorSpace) -> Vec<u8> {
    match requested {
        ColorSpace::Luma => luma.to_vec(),
        ColorSpace::Rgb => luma.iter().flat_map(|&l| [l, l, l]).collect(),
        ColorSpace::Rgba => luma.iter().flat_map(|&l| [l, l, l, 255]).collect(),
        ColorSpace::YCbCr => luma.to_vec(),
    }
}

/// Append an opaque alpha byte after every RGB triple.
fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect()
}

/// A convenience alias so a call site that pattern-matches
/// `JpegError::Unsupported(UnsupportedFeature::ColorTransform(_))` (the error
/// [`JpegDecoder::decode`] returns for a colour-space conversion this crate
/// cannot perform) does not have to import [`UnsupportedFeature`] separately.
pub type ColorTransformError = UnsupportedFeature;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_decode_default_to_rgb() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = JpegDecoder::new(bytes);
        let pixels = decoder.decode().expect("decode");
        assert_eq!(pixels.len(), 8 * 8 * 3);
    }

    #[test]
    fn new_with_options_selects_luma() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
        let mut decoder = JpegDecoder::new_with_options(bytes, options);
        let pixels = decoder.decode().expect("decode");
        assert_eq!(pixels.len(), 8 * 8);
    }

    #[test]
    fn decode_headers_populates_info_and_dimensions_without_decoding_pixels() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = JpegDecoder::new(bytes);
        assert!(decoder.info().is_none());
        assert!(decoder.dimensions().is_none());
        decoder.decode_headers().expect("decode_headers");
        let info = decoder.info().expect("info");
        assert_eq!((info.width, info.height, info.components), (8, 8, 3));
        assert_eq!(decoder.dimensions(), Some((8, 8)));
    }

    #[test]
    fn output_buffer_size_matches_the_requested_colour_space() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        for (colorspace, components) in [
            (ColorSpace::Rgb, 3),
            (ColorSpace::Rgba, 4),
            (ColorSpace::Luma, 1),
            (ColorSpace::YCbCr, 3),
        ] {
            let options = DecoderOptions::default().jpeg_set_out_colorspace(colorspace);
            let mut decoder = JpegDecoder::new_with_options(bytes, options);
            assert_eq!(decoder.output_buffer_size(), None, "before headers");
            decoder.decode_headers().expect("decode_headers");
            assert_eq!(decoder.output_buffer_size(), Some(8 * 8 * components));
            let pixels = decoder.decode().expect("decode");
            assert_eq!(pixels.len(), 8 * 8 * components);
        }
    }

    #[test]
    fn input_and_output_colorspace_report_the_source_and_the_request() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = JpegDecoder::new(bytes);
        assert!(decoder.input_colorspace().is_none());
        assert!(decoder.output_colorspace().is_none());
        decoder.decode_headers().expect("decode_headers");
        assert_eq!(decoder.input_colorspace(), Some(NativeColorSpace::Ycbcr));
        assert_eq!(decoder.output_colorspace(), Some(ColorSpace::Rgb));
    }

    /// A grayscale source honours a wider request by expanding, so
    /// `output_colorspace` must name the *requested* space — which is what
    /// real `zune_jpeg` 0.5.15 returns, and what `decode()` here actually
    /// produces. Both are asserted together so they cannot drift apart
    /// again: this used to report `Luma` while `decode()` returned three
    /// bytes per pixel.
    #[test]
    fn output_colorspace_names_the_request_a_grayscale_source_is_expanded_to() {
        let bytes: &[u8] = &crate::sample::GRAY_1X1;
        for (colorspace, components) in [
            (ColorSpace::Rgb, 3usize),
            (ColorSpace::Rgba, 4),
            (ColorSpace::Luma, 1),
        ] {
            let options = DecoderOptions::default().jpeg_set_out_colorspace(colorspace);
            let mut decoder = JpegDecoder::new_with_options(bytes, options);
            decoder.decode_headers().expect("decode_headers");
            assert_eq!(decoder.output_colorspace(), Some(colorspace));
            assert_eq!(decoder.output_buffer_size(), Some(components));
            let pixels = decoder.decode().expect("decode");
            assert_eq!(pixels.len(), components, "{colorspace:?}");
        }
    }

    /// `output_buffer_size()` is the buffer a caller allocates, so it must
    /// equal `decode()`'s own length for every source/request pair that
    /// decodes at all.
    ///
    /// Regression test: `ColorSpace::YCbCr` is a raw passthrough of the
    /// *source's* components, but the size was computed from the enum's
    /// nominal three. Measured on a 9x7 fixture before the fix: grayscale
    /// reported 189 and produced 63; CMYK reported 189 and produced 252 —
    /// i.e. it told a caller to allocate less than `decode()` returns.
    #[test]
    fn output_buffer_size_equals_what_decode_produces_for_every_source() {
        use crate::encoder::{EncodeOptions, InputColor, encode_to_vec_with_options};
        let sources: [(&str, InputColor, usize); 3] = [
            ("gray", InputColor::Luma, 1),
            ("rgb", InputColor::Rgb, 3),
            ("cmyk", InputColor::Cmyk, 4),
        ];
        for (name, color, channels) in sources {
            let pixels: Vec<u8> = (0..9u32 * 7 * channels as u32)
                .map(|i| (i % 251) as u8)
                .collect();
            let jpeg = encode_to_vec_with_options(&pixels, 9, 7, color, &EncodeOptions::default())
                .expect("encode");
            for colorspace in [
                ColorSpace::Rgb,
                ColorSpace::Rgba,
                ColorSpace::Luma,
                ColorSpace::YCbCr,
            ] {
                let options = DecoderOptions::default().jpeg_set_out_colorspace(colorspace);
                let mut decoder = JpegDecoder::new_with_options(&jpeg, options);
                decoder.decode_headers().expect("decode_headers");
                let reported = decoder.output_buffer_size().expect("headers read");
                let mut decoder = JpegDecoder::new_with_options(&jpeg, options);
                if let Ok(pixels) = decoder.decode() {
                    assert_eq!(
                        pixels.len(),
                        reported,
                        "{name} -> {colorspace:?}: output_buffer_size disagrees with decode"
                    );
                }
            }
        }
    }

    #[test]
    fn set_options_takes_effect_on_the_next_decode() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = JpegDecoder::new(bytes);
        assert_eq!(decoder.options().out_colorspace(), ColorSpace::Rgb);
        decoder.set_options(DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Rgba));
        assert_eq!(decoder.options().out_colorspace(), ColorSpace::Rgba);
        let pixels = decoder.decode().expect("decode");
        assert_eq!(pixels.len(), 8 * 8 * 4);
        assert!(pixels.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn a_grayscale_source_expands_to_rgb_and_rgba_on_request() {
        let bytes: &[u8] = &crate::sample::GRAY_1X1;
        for (colorspace, components) in [(ColorSpace::Rgb, 3), (ColorSpace::Rgba, 4)] {
            let options = DecoderOptions::default().jpeg_set_out_colorspace(colorspace);
            let mut decoder = JpegDecoder::new_with_options(bytes, options);
            let pixels = decoder.decode().expect("decode");
            assert_eq!(pixels.len(), components);
        }
    }

    #[test]
    fn ycbcr_output_matches_raw_components() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr);
        let mut decoder = JpegDecoder::new_with_options(bytes, options);
        let ours = decoder.decode().expect("decode");

        let mut native = NativeDecoder::with_options(bytes, NativeOptions::raw());
        native.read_info().expect("read_info");
        let reference = native.decode().expect("decode");
        assert_eq!(ours, reference);
    }

    #[test]
    fn icc_profile_and_exif_are_reachable_and_absent_on_the_sample_fixture() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = JpegDecoder::new(bytes);
        assert_eq!(decoder.icc_profile().expect("icc_profile"), None);
        assert_eq!(decoder.exif().expect("exif"), None);
    }

    #[test]
    fn a_cmyk_request_on_a_cmyk_source_is_an_unsupported_named_error() {
        use crate::encoder::{EncodeOptions, InputColor, encode_to_vec_with_options};
        let pixels = [10u8, 20, 30, 40].repeat(9 * 7);
        let jpeg =
            encode_to_vec_with_options(&pixels, 9, 7, InputColor::Cmyk, &EncodeOptions::default())
                .expect("encode a CMYK source");
        let mut decoder = JpegDecoder::new(&jpeg);
        let err = decoder
            .decode()
            .expect_err("CMYK -> RGB is not a supported conversion");
        assert!(matches!(
            err,
            JpegError::Unsupported(UnsupportedFeature::ColorTransform(_))
        ));
    }

    #[test]
    fn color_space_component_counts() {
        assert_eq!(ColorSpace::Rgb.num_components(), 3);
        assert_eq!(ColorSpace::Rgba.num_components(), 4);
        assert_eq!(ColorSpace::Luma.num_components(), 1);
        assert_eq!(ColorSpace::YCbCr.num_components(), 3);
    }
}
