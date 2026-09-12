//! A drop-in shaped facade for `jpeg_decoder::Decoder`.
//!
//! Call sites that decode a JPEG with `jpeg_decoder` can move here with a
//! `use` change:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::compat::jpeg_decoder::Decoder;
//!
//! let bytes: &[u8] = &oxiarc_jpeg::sample::RGB_8X8_420;
//! let mut decoder = Decoder::new(bytes);
//! decoder.read_info()?;
//! let info = decoder.info()?.expect("read_info populates it");
//! let pixels = decoder.decode()?;
//! assert_eq!(
//!     pixels.len(),
//!     usize::from(info.width) * usize::from(info.height) * info.pixel_format.pixel_bytes()
//! );
//! # Ok(())
//! # }
//! ```
//!
//! Shaped after `jpeg-decoder` 0.3's public API
//! (`Decoder::{new, read_info, info, decode, icc_profile, exif_data}`,
//! `ImageInfo { width, height, pixel_format, coding_process }`,
//! `PixelFormat::{L8, L16, RGB24, CMYK32}`,
//! `CodingProcess::{DctSequential, DctProgressive, Lossless}`), read directly
//! from its source rather than from memory. Not `#[deprecated]`: a migration
//! aid, not a reimplementation — [`crate::Decoder`] is the strictly larger
//! native API.
//!
//! **One deliberate signature change.** Real `jpeg_decoder::Decoder::info()`
//! is infallible (`Option<ImageInfo>`) and panics for a component count
//! outside `{1, 3, 4}` (its `PixelFormat` has no representation for `2`, and
//! this crate can decode `Nf` up to `4` for combinations `jpeg_decoder`
//! itself never produces). This crate's no-panic policy means that case must
//! be a value, not a crash, so [`Decoder::info`] here returns
//! `Result<Option<ImageInfo>, JpegError>` instead —
//! `Err(JpegError::Unsupported(UnsupportedFeature::ComponentCount(_)))`
//! rather than a panic.
//!
//! **One deliberate capability limit.** `PixelFormat` has exactly the real
//! crate's four variants, and the only wide one is `L16` — one component.
//! A frame with a sample precision above eight *and* more than one component
//! (a twelve-bit `SOF1` colour image, which [`crate::Decoder`] decodes
//! perfectly well through [`crate::Decoder::decode_u16`]) therefore has no
//! `PixelFormat` at all, and both [`Decoder::info`] and [`Decoder::decode`]
//! report `Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(_)))`
//! for it rather than disagreeing with each other. They used to: `info()`
//! answered `Rgb24` (three bytes per pixel) while `decode()` returned
//! big-endian `u16` samples (six), so a caller that sized a buffer from
//! `info()` — which is exactly what this module's own example does — read
//! half an image. Reach for [`Decoder::inner_mut`] and
//! [`crate::Decoder::decode_u16`] for those frames.

use std::io::Read;

use crate::decoder::Decoder as NativeDecoder;
use crate::error::{JpegError, Result, UnsupportedFeature};
use crate::frame::CodingProcess as NativeCodingProcess;

/// A pixel layout, named as `jpeg_decoder::PixelFormat` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Luminance (grayscale), 8 bits.
    L8,
    /// Luminance (grayscale), 16 bits (a 9..=16-bit lossless frame).
    L16,
    /// RGB, 8 bits per channel.
    Rgb24,
    /// CMYK, 8 bits per channel.
    Cmyk32,
}

impl PixelFormat {
    /// Bytes per pixel in this format.
    #[must_use]
    pub const fn pixel_bytes(self) -> usize {
        match self {
            PixelFormat::L8 => 1,
            PixelFormat::L16 => 2,
            PixelFormat::Rgb24 => 3,
            PixelFormat::Cmyk32 => 4,
        }
    }
}

/// Which coding process a frame uses, named as `jpeg_decoder::CodingProcess`
/// names them. Coarser than [`crate::CodingProcess`]: baseline and extended
/// sequential both map to `DctSequential`, matching how the real crate does
/// not distinguish them at this level either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingProcess {
    /// Sequential discrete cosine transform.
    DctSequential,
    /// Progressive discrete cosine transform.
    DctProgressive,
    /// Lossless (predictive).
    Lossless,
}

fn to_coding_process(process: NativeCodingProcess) -> CodingProcess {
    match process {
        NativeCodingProcess::Baseline | NativeCodingProcess::ExtendedSequential => {
            CodingProcess::DctSequential
        }
        NativeCodingProcess::Progressive => CodingProcess::DctProgressive,
        NativeCodingProcess::Lossless => CodingProcess::Lossless,
    }
}

/// Image metadata, shaped exactly like `jpeg_decoder::ImageInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// The width of the image, in pixels.
    pub width: u16,
    /// The height of the image, in pixels.
    pub height: u16,
    /// The pixel format a decode will produce.
    pub pixel_format: PixelFormat,
    /// The coding process of the image.
    pub coding_process: CodingProcess,
}

/// A JPEG decoder with the method names `jpeg_decoder::Decoder` uses.
pub struct Decoder<R: Read> {
    inner: NativeDecoder<R>,
}

impl<R: Read> Decoder<R> {
    /// A new decoder over `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: NativeDecoder::new(reader),
        }
    }

    /// Parse markers up to and including the frame header, without decoding
    /// pixels.
    ///
    /// # Errors
    ///
    /// Whatever [`crate::Decoder::read_info`] returns.
    pub fn read_info(&mut self) -> Result<()> {
        self.inner.read_info()?;
        Ok(())
    }

    /// Image information. `None` until [`Decoder::read_info`] or
    /// [`Decoder::decode`] has returned `Ok`.
    ///
    /// # Errors
    ///
    /// See this module's doc for why this differs from the real
    /// `jpeg_decoder` crate: `Err` rather than a panic when the frame's
    /// component count has no [`PixelFormat`] (outside `{1, 3, 4}`).
    pub fn info(&self) -> Result<Option<ImageInfo>> {
        let Some(info) = self.inner.info() else {
            return Ok(None);
        };
        // `L16` is the only wide `PixelFormat` the real crate defines, and it
        // is single-component. A wide *colour* frame has no representation
        // here at all, so it is reported rather than described as something
        // `decode()` would not produce — see this module's doc.
        if info.precision > 8 && info.num_components != 1 {
            return Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                info.precision,
            )));
        }
        let pixel_format = match info.num_components {
            1 if info.precision <= 8 => PixelFormat::L8,
            1 => PixelFormat::L16,
            3 => PixelFormat::Rgb24,
            4 => PixelFormat::Cmyk32,
            n => {
                return Err(JpegError::Unsupported(UnsupportedFeature::ComponentCount(
                    n,
                )));
            }
        };
        Ok(Some(ImageInfo {
            width: info.width,
            height: info.height,
            pixel_format,
            coding_process: to_coding_process(info.process),
        }))
    }

    /// Decode the image, producing the pixel format [`Decoder::info`]
    /// reports (`RGB24` and `CMYK32` frames are transformed the way
    /// `jpeg_decoder` transforms them — YCbCr to RGB, YCCK to CMYK — a
    /// `raw_components` decode is not available through this facade).
    ///
    /// # Errors
    ///
    /// Whatever [`crate::Decoder::read_info`]/[`crate::Decoder::decode`]/
    /// [`crate::Decoder::decode_u16`] return, plus the component-count error
    /// [`Decoder::info`] documents.
    pub fn decode(&mut self) -> Result<Vec<u8>> {
        let raw_info = self
            .inner
            .info()
            .ok_or(JpegError::AbbreviatedWithoutFrame)?;
        if raw_info.precision > 8 {
            // Keep `decode()` and `info()` in step: a wide colour frame has
            // no `PixelFormat`, so it is refused here too rather than
            // returning twice the bytes `info()` implies.
            if raw_info.num_components != 1 {
                return Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                    raw_info.precision,
                )));
            }
            let samples = self.inner.decode_u16()?;
            let mut out = Vec::with_capacity(samples.len() * 2);
            for sample in samples {
                out.extend_from_slice(&sample.to_be_bytes());
            }
            return Ok(out);
        }
        if !matches!(raw_info.num_components, 1 | 3 | 4) {
            return Err(JpegError::Unsupported(UnsupportedFeature::ComponentCount(
                raw_info.num_components,
            )));
        }
        self.inner.decode()
    }

    /// The embedded ICC profile, reassembled from its `APP2` chunks.
    ///
    /// Real `jpeg_decoder::Decoder::icc_profile` cannot fail (malformed
    /// chunks simply read back as `None`); this crate's own reassembly can
    /// detect malformed chunk metadata and reports it instead of discarding
    /// it silently, so this keeps the `Result`.
    ///
    /// # Errors
    ///
    /// [`JpegError`] if the `APP2` chunks are malformed.
    pub fn icc_profile(&self) -> Result<Option<Vec<u8>>> {
        self.inner.icc_profile()
    }

    /// Raw EXIF data, with its `Exif\0\0` prefix removed, if any.
    #[must_use]
    pub fn exif_data(&self) -> Option<&[u8]> {
        self.inner.exif()
    }

    /// The native decoder underneath, for the entry points this facade does
    /// not expose: [`crate::Decoder::decode_u16`] (twelve-bit and wide
    /// lossless frames), [`crate::Decoder::decode_into_strided`] and
    /// [`crate::Decoder::decode_into_u16_strided`] (a destination that is a
    /// sub-rectangle of a larger image), and the frame header itself.
    ///
    /// It does **not** reach [`crate::DecodeOptions`] — scale, limits, raw
    /// components and the output colour space are fixed when the decoder is
    /// built, so a call site that needs one of those builds a
    /// [`crate::Decoder`] with [`crate::Decoder::with_options`] directly
    /// instead of going through this facade at all.
    pub fn inner_mut(&mut self) -> &mut NativeDecoder<R> {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_read_info_and_decode() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = Decoder::new(bytes);
        decoder.read_info().expect("read_info");
        let pixels = decoder.decode().expect("decode");
        assert_eq!(pixels.len(), 8 * 8 * 3);
    }

    #[test]
    fn info_is_none_before_read_info() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let decoder = Decoder::new(bytes);
        assert_eq!(decoder.info().expect("info"), None);
    }

    #[test]
    fn info_matches_the_real_crates_shape() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = Decoder::new(bytes);
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!((info.width, info.height), (8, 8));
        assert_eq!(info.pixel_format, PixelFormat::Rgb24);
        assert_eq!(info.coding_process, CodingProcess::DctSequential);
    }

    #[test]
    fn grayscale_reports_l8() {
        let bytes: &[u8] = &crate::sample::GRAY_1X1;
        let mut decoder = Decoder::new(bytes);
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.pixel_format, PixelFormat::L8);
    }

    #[test]
    fn twelve_bit_grayscale_reports_l16_and_decodes_to_big_endian_bytes() {
        use crate::encoder::{EncodeOptions, InputColor, encode_u16_to_vec_with_options};
        let pixels: Vec<u16> = (0..9 * 7).map(|i| (i * 37) % 4096).collect();
        let options = EncodeOptions {
            precision: 12,
            ..EncodeOptions::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 9, 7, InputColor::Luma, &options)
            .expect("encode a 12-bit source");
        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.pixel_format, PixelFormat::L16);
        let bytes = decoder.decode().expect("decode");
        assert_eq!(bytes.len(), 9 * 7 * 2);
    }

    /// `info()` and `decode()` must never disagree about how many bytes a
    /// pixel occupies.
    ///
    /// Regression test: a twelve-bit *colour* frame used to report
    /// `PixelFormat::Rgb24` (three bytes per pixel) from `info()` while
    /// `decode()` returned big-endian `u16` samples (six) — measured on a
    /// 9x7 twelve-bit RGB frame, `decode()` gave 378 bytes where `info()`
    /// implied 189, so a caller sizing a buffer the way this module's own
    /// example does read half the image. Both now report the same
    /// `Unsupported`, and the native decoder still decodes the frame.
    #[test]
    fn a_wide_colour_frame_is_refused_identically_by_info_and_decode() {
        use crate::encoder::{EncodeOptions, InputColor, encode_u16_to_vec_with_options};
        let pixels: Vec<u16> = (0..9u32 * 7 * 3)
            .map(|i| ((i * 37) % 4096) as u16)
            .collect();
        let options = EncodeOptions {
            precision: 12,
            ..EncodeOptions::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 9, 7, InputColor::Rgb, &options)
            .expect("encode a 12-bit colour source");

        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        assert!(matches!(
            decoder.info(),
            Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                12
            )))
        ));
        assert!(matches!(
            decoder.decode(),
            Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                12
            )))
        ));

        // The frame itself is perfectly decodable; only this facade's
        // `PixelFormat` vocabulary cannot name it.
        let samples = decoder.inner_mut().decode_u16().expect("native decode_u16");
        assert_eq!(samples.len(), 9 * 7 * 3);
    }

    /// A twelve-bit *grayscale* frame still works: `L16` names it, and
    /// `decode()`'s big-endian bytes match `pixel_bytes()`.
    #[test]
    fn a_wide_grayscale_frame_still_agrees_with_its_pixel_format() {
        use crate::encoder::{EncodeOptions, InputColor, encode_u16_to_vec_with_options};
        let pixels: Vec<u16> = (0..9u32 * 7).map(|i| ((i * 37) % 4096) as u16).collect();
        let options = EncodeOptions {
            precision: 12,
            ..EncodeOptions::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 9, 7, InputColor::Luma, &options)
            .expect("encode a 12-bit grayscale source");
        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.pixel_format, PixelFormat::L16);
        let bytes = decoder.decode().expect("decode");
        assert_eq!(
            bytes.len(),
            usize::from(info.width) * usize::from(info.height) * info.pixel_format.pixel_bytes()
        );
    }

    #[test]
    fn cmyk_reports_cmyk32() {
        use crate::encoder::{EncodeOptions, InputColor, encode_to_vec_with_options};
        let pixels = [10u8, 20, 30, 40].repeat(9 * 7);
        let jpeg =
            encode_to_vec_with_options(&pixels, 9, 7, InputColor::Cmyk, &EncodeOptions::default())
                .expect("encode a CMYK source");
        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.pixel_format, PixelFormat::Cmyk32);
        let out = decoder.decode().expect("decode");
        assert_eq!(out.len(), 9 * 7 * 4);
    }

    #[test]
    fn progressive_reports_dct_progressive() {
        use crate::encoder::{
            EncodeOptions, EncodeProcess, InputColor, encode_to_vec_with_options,
        };
        let pixels = vec![88u8; 16 * 16 * 3];
        let options = EncodeOptions {
            process: EncodeProcess::Progressive,
            ..EncodeOptions::default()
        };
        let jpeg = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options)
            .expect("encode progressive");
        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.coding_process, CodingProcess::DctProgressive);
    }

    #[test]
    fn lossless_reports_lossless() {
        use crate::encoder::{
            EncodeOptions, EncodeProcess, InputColor, encode_to_vec_with_options,
        };
        let pixels = vec![50u8; 16 * 16 * 3];
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor: 1,
                point_transform: 0,
            },
            ..EncodeOptions::default()
        };
        let jpeg = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options)
            .expect("encode lossless");
        let mut decoder = Decoder::new(jpeg.as_slice());
        decoder.read_info().expect("read_info");
        let info = decoder.info().expect("info").expect("populated");
        assert_eq!(info.coding_process, CodingProcess::Lossless);
    }

    #[test]
    fn icc_profile_and_exif_data_are_reachable() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = Decoder::new(bytes);
        decoder.read_info().expect("read_info");
        assert_eq!(decoder.icc_profile().expect("icc_profile"), None);
        assert_eq!(decoder.exif_data(), None);
    }

    #[test]
    fn inner_mut_reaches_the_native_decoder() {
        let bytes: &[u8] = &crate::sample::RGB_8X8_420;
        let mut decoder = Decoder::new(bytes);
        decoder.read_info().expect("read_info");
        assert!(decoder.inner_mut().frame_header().is_some());
    }

    #[test]
    fn pixel_format_byte_counts() {
        assert_eq!(PixelFormat::L8.pixel_bytes(), 1);
        assert_eq!(PixelFormat::L16.pixel_bytes(), 2);
        assert_eq!(PixelFormat::Rgb24.pixel_bytes(), 3);
        assert_eq!(PixelFormat::Cmyk32.pixel_bytes(), 4);
    }
}
