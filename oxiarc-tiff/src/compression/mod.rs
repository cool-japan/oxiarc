//! Codec dispatch.
//!
//! Every registered TIFF compression value has a named variant in
//! [`crate::CompressionMethod`], and every one of them reaches this dispatch:
//! there is no wildcard arm that turns an unknown method into silence. Methods
//! whose decoder is scheduled but not yet present return
//! [`crate::UnsupportedError::NotYetAvailable`] with the method number *and*
//! its name, so the message is actionable.
//!
//! # Extending
//!
//! Adding a codec means adding one module beside this one and one arm to
//! [`decode_into`] / [`encode`]; nothing in `reader.rs` or `writer/` changes.
//! Out-of-tree codecs (LERC, WebP, JPEG XL) go through the [`Codec`] trait and
//! a [`CodecRegistry`], so a consumer can register a decoder without this crate
//! growing a dependency on it.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, CodecContext};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! let cx = CodecContext::new(CompressionMethod::PackBits, 4, 1, &[8], 1, Endian::Little);
//! let mut out = [0u8; 4];
//! // PackBits: literal run of four bytes.
//! let n = decode_into(&[3, 1, 2, 3, 4], &mut out, &cx)?;
//! assert_eq!(n, 4);
//! assert_eq!(out, [1, 2, 3, 4]);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

pub mod none;
pub mod packbits;

use std::sync::Arc;

use crate::byteorder::Endian;
use crate::error::{Result, TiffError, UnsupportedError};
use crate::tags::{
    CompressionMethod, FillOrder, PhotometricInterpretation, PlanarConfiguration, T4Options,
    T6Options,
};

/// Everything a codec needs to know about the chunk it is decoding.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CodecContext<'a> {
    /// The compression method being applied.
    pub compression: CompressionMethod,
    /// The image's photometric interpretation (CCITT and JPEG need it).
    pub photometric: PhotometricInterpretation,
    /// The image's fill order.
    pub fill_order: FillOrder,
    /// Coded chunk width in pixels.
    pub width: usize,
    /// Coded chunk height in rows.
    pub height: usize,
    /// Bit depths of the channels carried by this chunk.
    pub bits_per_sample: &'a [u16],
    /// Channels carried by this chunk (1 for planar).
    pub samples_per_pixel: u16,
    /// Chunky or planar.
    pub planar: PlanarConfiguration,
    /// Which plane this chunk belongs to.
    pub plane: u16,
    /// `T4Options` for the CCITT Group 3 codecs.
    pub t4_options: T4Options,
    /// `T6Options` for the CCITT Group 4 codec.
    pub t6_options: T6Options,
    /// The abbreviated JPEG table stream from tag 347, if any.
    pub jpeg_tables: Option<&'a [u8]>,
    /// The file's byte order.
    pub endian: Endian,
}

impl<'a> CodecContext<'a> {
    /// A context with the defaults every simple codec needs.
    #[must_use]
    pub fn new(
        compression: CompressionMethod,
        width: usize,
        height: usize,
        bits_per_sample: &'a [u16],
        samples_per_pixel: u16,
        endian: Endian,
    ) -> Self {
        Self {
            compression,
            photometric: PhotometricInterpretation::BlackIsZero,
            fill_order: FillOrder::Msb2Lsb,
            width,
            height,
            bits_per_sample,
            samples_per_pixel,
            planar: PlanarConfiguration::Chunky,
            plane: 0,
            t4_options: T4Options::default(),
            t6_options: T6Options::default(),
            jpeg_tables: None,
            endian,
        }
    }

    /// Bytes one packed row of this chunk occupies.
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        let samples = self
            .width
            .saturating_mul(usize::from(self.samples_per_pixel));
        crate::sample::packed_row_bytes(self.bits_per_sample, samples) as usize
    }
}

/// How hard an encoder should work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecLevel {
    /// The codec's own default effort.
    Default,
    /// A numeric effort level, interpreted per codec.
    Level(i32),
}

impl Default for CodecLevel {
    fn default() -> Self {
        Self::Default
    }
}

/// An out-of-tree codec.
///
/// Implementors are registered in a [`CodecRegistry`], which a
/// [`crate::Decoder`] or [`crate::Encoder`] is built with. The registry is
/// intended to be constructed once per application and cloned into each
/// decoder; `Arc` keeps that cheap and the `Send + Sync` bound keeps it usable
/// from a parallel decode.
pub trait Codec: Send + Sync + core::fmt::Debug {
    /// The TIFF compression value this codec handles.
    fn method(&self) -> u16;

    /// Decodes one chunk into `dst`, returning the number of bytes written.
    ///
    /// # Errors
    /// Whatever the codec needs to report; use
    /// [`crate::FormatError::Codec`] for stream-level defects.
    fn decode_into(&self, src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize>;

    /// Encodes one chunk.
    ///
    /// # Errors
    /// Defaults to [`UnsupportedError::Compression`] for decode-only codecs.
    fn encode(&self, src: &[u8], cx: &CodecContext<'_>) -> Result<Vec<u8>> {
        let _ = src;
        Err(TiffError::Unsupported(UnsupportedError::Compression(
            cx.compression.to_u16(),
        )))
    }
}

/// A set of out-of-tree codecs, consulted before the built-in dispatch.
#[derive(Clone, Debug, Default)]
pub struct CodecRegistry {
    extra: Vec<Arc<dyn Codec>>,
}

impl CodecRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a codec; a later registration wins over an earlier one.
    pub fn register(&mut self, codec: Arc<dyn Codec>) {
        self.extra.push(codec);
    }

    /// Finds the codec registered for a compression value.
    #[must_use]
    pub fn find(&self, method: u16) -> Option<&Arc<dyn Codec>> {
        self.extra.iter().rev().find(|c| c.method() == method)
    }

    /// Number of registered codecs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.extra.len()
    }

    /// `true` when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extra.is_empty()
    }
}

/// A short human name for a compression method, used in error messages.
#[must_use]
pub fn method_name(method: CompressionMethod) -> &'static str {
    match method {
        CompressionMethod::None => "uncompressed",
        CompressionMethod::CcittRle => "CCITT RLE",
        CompressionMethod::CcittFax3 => "CCITT Group 3",
        CompressionMethod::CcittFax4 => "CCITT Group 4",
        CompressionMethod::Lzw => "LZW",
        CompressionMethod::OldJpeg => "old-style JPEG",
        CompressionMethod::Jpeg => "JPEG",
        CompressionMethod::AdobeDeflate8 => "Deflate (8)",
        CompressionMethod::Deflate => "Deflate (32946)",
        CompressionMethod::PackBits => "PackBits",
        CompressionMethod::Lzma => "LZMA",
        CompressionMethod::Zstd => "Zstandard",
        CompressionMethod::CcittRleWord => "CCITT RLE word-aligned",
        other => other.name(),
    }
}

/// Decodes one chunk of `src` into `dst`.
///
/// `dst` is pre-sized to the chunk's expected packed length; the return value
/// is the number of bytes actually written.
///
/// # Errors
/// * [`UnsupportedError::NotYetAvailable`] for a registered method whose
///   decoder has not landed yet;
/// * [`UnsupportedError::Compression`] for a method outside every registry;
/// * whatever the codec reports for a malformed stream.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    decode_into_with(src, dst, cx, None)
}

/// [`decode_into`], consulting an optional registry of out-of-tree codecs first.
///
/// # Errors
/// The same set as [`decode_into`].
pub fn decode_into_with(
    src: &[u8],
    dst: &mut [u8],
    cx: &CodecContext<'_>,
    registry: Option<&CodecRegistry>,
) -> Result<usize> {
    if let Some(registry) = registry {
        if let Some(codec) = registry.find(cx.compression.to_u16()) {
            return codec.decode_into(src, dst, cx);
        }
    }
    match cx.compression {
        CompressionMethod::None => none::decode_into(src, dst),
        CompressionMethod::PackBits => packbits::decode_into(src, dst),
        other => Err(unavailable(other)),
    }
}

/// Encodes one chunk.
///
/// # Errors
/// The same set as [`decode_into`].
pub fn encode(src: &[u8], cx: &CodecContext<'_>, level: CodecLevel) -> Result<Vec<u8>> {
    encode_with(src, cx, level, None)
}

/// [`encode`], consulting an optional registry of out-of-tree codecs first.
///
/// # Errors
/// The same set as [`decode_into`].
pub fn encode_with(
    src: &[u8],
    cx: &CodecContext<'_>,
    level: CodecLevel,
    registry: Option<&CodecRegistry>,
) -> Result<Vec<u8>> {
    let _ = level;
    if let Some(registry) = registry {
        if let Some(codec) = registry.find(cx.compression.to_u16()) {
            return codec.encode(src, cx);
        }
    }
    match cx.compression {
        CompressionMethod::None => Ok(src.to_vec()),
        CompressionMethod::PackBits => Ok(packbits::encode(src, cx.row_bytes())),
        other => Err(unavailable(other)),
    }
}

/// The error a not-yet-wired-up or unknown compression value produces.
fn unavailable(method: CompressionMethod) -> TiffError {
    if method.is_scheduled() {
        TiffError::Unsupported(UnsupportedError::NotYetAvailable {
            method: method.to_u16(),
            name: method_name(method),
        })
    } else {
        TiffError::Unsupported(UnsupportedError::Compression(method.to_u16()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(method: CompressionMethod) -> CodecContext<'static> {
        const BITS: &[u16] = &[8];
        CodecContext::new(method, 4, 1, BITS, 1, Endian::Little)
    }

    #[test]
    fn uncompressed_and_packbits_are_wired_up() {
        let cx = context(CompressionMethod::None);
        let mut dst = [0u8; 4];
        assert_eq!(decode_into(&[1, 2, 3, 4], &mut dst, &cx).expect("none"), 4);
        assert_eq!(dst, [1, 2, 3, 4]);
        assert_eq!(
            encode(&[1, 2, 3, 4], &cx, CodecLevel::Default).expect("none"),
            vec![1, 2, 3, 4]
        );

        let cx = context(CompressionMethod::PackBits);
        let encoded = encode(&[1, 2, 3, 4], &cx, CodecLevel::Default).expect("packbits");
        let mut dst = [0u8; 4];
        assert_eq!(
            decode_into(&encoded, &mut dst, &cx).expect("packbits decode"),
            4
        );
        assert_eq!(dst, [1, 2, 3, 4]);
    }

    #[test]
    fn every_scheduled_codec_reports_not_yet_available_by_name() {
        for method in [
            CompressionMethod::CcittRle,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
            CompressionMethod::Lzw,
            CompressionMethod::OldJpeg,
            CompressionMethod::Jpeg,
            CompressionMethod::AdobeDeflate8,
            CompressionMethod::Deflate,
            CompressionMethod::Lzma,
            CompressionMethod::Zstd,
        ] {
            let cx = context(method);
            let mut dst = [0u8; 4];
            let err = decode_into(&[0; 4], &mut dst, &cx).expect_err("not available yet");
            match err {
                TiffError::Unsupported(UnsupportedError::NotYetAvailable { method: m, name }) => {
                    assert_eq!(m, method.to_u16());
                    assert!(!name.is_empty());
                }
                other => panic!("{method} produced {other}"),
            }
            assert!(encode(&[0; 4], &cx, CodecLevel::Default).is_err());
        }
    }

    #[test]
    fn genuinely_unknown_methods_report_the_number() {
        for method in [
            CompressionMethod::Webp,
            CompressionMethod::JpegXl,
            CompressionMethod::Jbig,
            CompressionMethod::Next,
            CompressionMethod::CcittRleWord,
            CompressionMethod::Dcs,
            CompressionMethod::It8Ctpad,
            CompressionMethod::Unknown(60000),
        ] {
            let cx = context(method);
            let mut dst = [0u8; 4];
            let err = decode_into(&[0; 4], &mut dst, &cx).expect_err("unsupported");
            assert!(matches!(
                err,
                TiffError::Unsupported(UnsupportedError::Compression(_))
            ));
        }
    }

    #[derive(Debug)]
    struct DoublingCodec;

    impl Codec for DoublingCodec {
        fn method(&self) -> u16 {
            50001
        }
        fn decode_into(&self, src: &[u8], dst: &mut [u8], _cx: &CodecContext<'_>) -> Result<usize> {
            let n = src.len().min(dst.len());
            for (out, byte) in dst.iter_mut().zip(src.iter()) {
                *out = byte.wrapping_mul(2);
            }
            Ok(n)
        }
    }

    #[test]
    fn a_registered_codec_takes_over_its_method() {
        let mut registry = CodecRegistry::new();
        assert!(registry.is_empty());
        registry.register(Arc::new(DoublingCodec));
        assert_eq!(registry.len(), 1);
        assert!(registry.find(50001).is_some());
        assert!(registry.find(50002).is_none());

        let cx = context(CompressionMethod::Webp);
        let mut dst = [0u8; 4];
        let n = decode_into_with(&[1, 2, 3, 4], &mut dst, &cx, Some(&registry))
            .expect("registered codec");
        assert_eq!(n, 4);
        assert_eq!(dst, [2, 4, 6, 8]);
        // The default encode impl refuses.
        assert!(encode_with(&[1], &cx, CodecLevel::Default, Some(&registry)).is_err());
    }

    #[test]
    fn context_row_geometry() {
        let bits = [4u16];
        let cx = CodecContext::new(CompressionMethod::None, 5, 2, &bits, 1, Endian::Big);
        assert_eq!(cx.row_bytes(), 3);
        assert_eq!(cx.endian, Endian::Big);
        assert_eq!(cx.plane, 0);
        assert_eq!(CodecLevel::default(), CodecLevel::Default);
        assert_eq!(CodecLevel::Level(6), CodecLevel::Level(6));
    }

    #[test]
    fn method_names_cover_the_scheduled_set() {
        assert_eq!(method_name(CompressionMethod::None), "uncompressed");
        assert_eq!(method_name(CompressionMethod::Lzw), "LZW");
        assert_eq!(method_name(CompressionMethod::Zstd), "Zstandard");
        assert_eq!(method_name(CompressionMethod::Webp), "Webp");
    }
}
