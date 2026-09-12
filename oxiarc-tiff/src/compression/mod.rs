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

#[cfg(feature = "ccitt")]
pub mod ccitt;
#[cfg(feature = "deflate")]
pub mod deflate;
#[cfg(feature = "jpeg")]
pub mod jpeg;
#[cfg(feature = "lzma")]
pub mod lzma;
#[cfg(feature = "lzw")]
pub mod lzw;
pub mod none;
pub mod packbits;
#[cfg(feature = "zstd")]
pub mod zstd;

use std::sync::Arc;

use crate::byteorder::Endian;
use crate::error::{FormatError, Result, TiffError, UnsupportedError};
use crate::limits::Leniency;
use crate::tags::{
    CompressionMethod, FillOrder, PhotometricInterpretation, PlanarConfiguration, T4Options,
    T6Options,
};

#[cfg(any(
    feature = "deflate",
    feature = "ccitt",
    feature = "zstd",
    feature = "lzma"
))]
mod pool;
mod state;

pub use state::CodecState;

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
    /// `YCbCrSubSampling` (530), or `(1, 1)` when the image is not subsampled.
    pub ycbcr_subsampling: (u16, u16),
    /// How often the JPEG encoder writes a restart marker, in **MCU rows**.
    ///
    /// `0` (the default, and libtiff's) writes no `DRI` segment and no
    /// `RSTn` markers at all; `n` is `cjpeg -restart n`, resolved per scan.
    /// Restart markers cost a few bytes per row and let a decoder resynchronise
    /// after a corrupt MCU instead of losing the rest of the chunk.
    ///
    /// Decode ignores it: the interval a stream was coded with is in its own
    /// `DRI` segment.
    pub jpeg_restart_rows: u16,
    /// The abbreviated JPEG table stream from tag 347, if any.
    pub jpeg_tables: Option<&'a [u8]>,
    /// The old-style JPEG (compression 6) parameter tags, if any.
    pub old_jpeg: Option<&'a OldJpegParams>,
    /// The file's byte order.
    pub endian: Endian,
    /// How strictly a codec should treat a stream that disagrees with the
    /// geometry.
    ///
    /// [`Leniency::Strict`] turns every deviation into an error;
    /// [`Leniency::Lenient`] fills the shortfall (CCITT pads the row with the
    /// background colour) and skips checksum verification.
    pub leniency: Leniency,
    /// Per-image codec state: decisions taken once (the LZW code-width rule)
    /// and scratch reused across chunks (the inflate window).
    ///
    /// `None` means "no state available": every codec still works, it just
    /// re-takes its decisions and re-allocates its scratch for each chunk.
    pub state: Option<&'a CodecState>,
    /// The largest scratch buffer a codec may allocate for this chunk, in
    /// bytes — [`crate::Limits::intermediate_buffer_size`], forwarded.
    ///
    /// `dst` bounds a codec's *output*, but not every codec can decode
    /// straight into it: a JPEG strip, for one, carries its own frame
    /// dimensions in the `SOF` marker, and when those disagree with the
    /// chunk's TIFF geometry the frame has to be decoded into scratch and
    /// cropped. That scratch is sized by numbers taken from the compressed
    /// stream, so it needs the same guard every other file-driven allocation
    /// in this crate gets; without one, a strip whose `SOF` claims
    /// 65535x65535 costs gigabytes before a single byte is decoded. Allocate
    /// through [`crate::Limits::checked_alloc`] with this budget.
    ///
    /// The decode pipeline sets it from the reader's [`crate::Limits`];
    /// [`CodecContext::new`] and the encoder use
    /// `Limits::default().intermediate_buffer_size`.
    pub max_scratch_bytes: usize,
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
            ycbcr_subsampling: (1, 1),
            jpeg_restart_rows: 0,
            jpeg_tables: None,
            old_jpeg: None,
            endian,
            leniency: Leniency::Normal,
            state: None,
            max_scratch_bytes: crate::limits::Limits::default().intermediate_buffer_size,
        }
    }

    /// The chunk's coded pixel count, saturating instead of overflowing.
    #[must_use]
    pub fn pixel_count(&self) -> usize {
        self.width.saturating_mul(self.height)
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

/// The old-style JPEG (compression 6) parameter tags, with every referenced
/// table already loaded from the file.
///
/// TIFF 6.0 §22 described JPEG data through tags 512-521 before TTN2 replaced
/// the whole scheme with compression 7. Tags 519/520/521 hold *file offsets*
/// to the quantisation and Huffman tables, so they cannot be resolved from a
/// chunk alone: [`crate::ImageInfo`] loads them while it parses the directory
/// and hands the bytes to the codec through
/// [`CodecContext::old_jpeg`](CodecContext::old_jpeg).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OldJpegParams {
    /// `JPEGProc` (512): 1 = baseline sequential, 14 = lossless.
    pub proc: u16,
    /// `JPEGInterchangeFormat` (513) resolved to its bytes, when the offset
    /// and length were usable.
    pub interchange: Option<Vec<u8>>,
    /// `JPEGRestartInterval` (515).
    pub restart_interval: u16,
    /// `JPEGQTables` (519), one 64-byte table per component, in tag order.
    pub q_tables: Vec<Vec<u8>>,
    /// `JPEGDCTables` (520), one `BITS`+`HUFFVAL` blob per component.
    pub dc_tables: Vec<Vec<u8>>,
    /// `JPEGACTables` (521), one `BITS`+`HUFFVAL` blob per component.
    pub ac_tables: Vec<Vec<u8>>,
    /// `JPEGLosslessPredictors` (517).
    pub lossless_predictors: Vec<u16>,
    /// `JPEGPointTransform` (518).
    pub point_transform: Vec<u16>,
}

/// How hard an encoder should work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecLevel {
    /// The codec's own default effort.
    #[default]
    Default,
    /// A numeric effort level, interpreted per codec.
    Level(i32),
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

/// Whether the codec consumes `FillOrder` (tag 266) itself.
///
/// libtiff reverses the bits of every byte of the raw chunk when `FillOrder`
/// is 2 — on read before the codec, on write after it — for every codec except
/// the CCITT family, whose `Fax3SetupState` sets `TIFF_NOBITREV` because the
/// fax decoder applies the tag while consuming the bit stream. The chunk
/// pipeline therefore skips [`crate::sample::apply_fill_order`] for exactly
/// these methods and passes `fill_order` down in the [`CodecContext`] instead.
///
/// ```
/// use oxiarc_tiff::compression::handles_fill_order;
/// use oxiarc_tiff::CompressionMethod;
///
/// assert!(handles_fill_order(CompressionMethod::CcittFax4));
/// assert!(!handles_fill_order(CompressionMethod::PackBits));
/// ```
#[must_use]
pub const fn handles_fill_order(method: CompressionMethod) -> bool {
    matches!(
        method,
        CompressionMethod::CcittRle
            | CompressionMethod::CcittFax3
            | CompressionMethod::CcittFax4
            | CompressionMethod::CcittRleWord
    )
}

/// Whether the codec resolves `YCbCrSubSampling` itself.
///
/// The JPEG codecs do: a JPEG stream carries its own per-component sampling
/// factors (which TIFF 6.0 TTN2 makes authoritative over tag 530), and the
/// decoder upsamples chroma while it renders, so what comes back is
/// full-resolution interleaved components rather than TIFF's subsampling
/// units. Every other codec hands back units, which the chunk pipeline
/// expands with [`crate::colour::expand_ycbcr_subsampling`].
///
/// ```
/// use oxiarc_tiff::compression::expands_subsampling;
/// use oxiarc_tiff::CompressionMethod;
///
/// assert!(expands_subsampling(CompressionMethod::Jpeg));
/// assert!(!expands_subsampling(CompressionMethod::None));
/// ```
#[must_use]
pub const fn expands_subsampling(method: CompressionMethod) -> bool {
    matches!(method, CompressionMethod::Jpeg | CompressionMethod::OldJpeg)
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
        #[cfg(feature = "lzw")]
        CompressionMethod::Lzw => lzw::decode_into(src, dst, cx),
        #[cfg(feature = "deflate")]
        CompressionMethod::AdobeDeflate8 | CompressionMethod::Deflate => {
            deflate::decode_into(src, dst, cx)
        }
        #[cfg(feature = "zstd")]
        CompressionMethod::Zstd => zstd::decode_into(src, dst, cx),
        #[cfg(feature = "lzma")]
        CompressionMethod::Lzma => lzma::decode_into(src, dst, cx),
        #[cfg(feature = "jpeg")]
        CompressionMethod::Jpeg => jpeg::decode_into(src, dst, cx),
        #[cfg(feature = "jpeg")]
        CompressionMethod::OldJpeg => jpeg::decode_old_into(src, dst, cx),
        #[cfg(feature = "ccitt")]
        CompressionMethod::CcittRle
        | CompressionMethod::CcittFax3
        | CompressionMethod::CcittFax4
        | CompressionMethod::CcittRleWord => ccitt::decode_into(src, dst, cx),
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
    // Not every build has a codec that reads the effort level.
    let _ = &level;
    if let Some(registry) = registry {
        if let Some(codec) = registry.find(cx.compression.to_u16()) {
            return codec.encode(src, cx);
        }
    }
    match cx.compression {
        CompressionMethod::None => Ok(src.to_vec()),
        CompressionMethod::PackBits => Ok(packbits::encode(src, cx.row_bytes())),
        #[cfg(feature = "lzw")]
        CompressionMethod::Lzw => lzw::encode(src),
        #[cfg(feature = "deflate")]
        CompressionMethod::AdobeDeflate8 | CompressionMethod::Deflate => {
            deflate::encode(src, level)
        }
        #[cfg(feature = "zstd")]
        CompressionMethod::Zstd => zstd::encode(src, level),
        #[cfg(feature = "lzma")]
        CompressionMethod::Lzma => lzma::encode(src, level),
        #[cfg(feature = "jpeg")]
        CompressionMethod::Jpeg => jpeg::encode(src, cx, level),
        #[cfg(feature = "ccitt")]
        CompressionMethod::CcittRle
        | CompressionMethod::CcittFax3
        | CompressionMethod::CcittFax4
        | CompressionMethod::CcittRleWord => ccitt::encode(src, cx),
        other => Err(unavailable(other)),
    }
}

/// The error a codec whose cargo feature is off produces.
///
/// Kept separate from [`unavailable`] so the message points at the fix (turn
/// the feature on) rather than at the calendar.
fn feature_missing(feature: &'static str) -> TiffError {
    TiffError::Unsupported(UnsupportedError::FeatureNotCompiled { feature })
}

/// Wraps a codec crate's error as a TIFF stream defect.
///
/// The codec crates report their own error types; a TIFF caller wants to know
/// *which compression* failed and what it said, which is exactly
/// [`FormatError::Codec`]. Going through `Display` keeps this crate from
/// depending on `oxiarc-core` just to name an error type.
#[cfg_attr(
    not(any(
        feature = "lzw",
        feature = "deflate",
        feature = "zstd",
        feature = "lzma",
        feature = "jpeg",
        feature = "ccitt"
    )),
    allow(dead_code)
)]
pub(crate) fn codec_error(method: CompressionMethod, error: impl core::fmt::Display) -> TiffError {
    TiffError::Format(FormatError::Codec {
        method: method.to_u16(),
        message: error.to_string(),
    })
}

/// The error an unknown compression value produces.
///
/// A method this crate implements never reaches here: the dispatch has an arm
/// for it either way, and the feature-off arm reports
/// [`UnsupportedError::FeatureNotCompiled`] with the feature to turn on. What
/// is left is the registered-but-not-implemented set (WebP, JPEG XL, LERC,
/// JBIG, NeXT, ThunderScan, the IT8 and Pixar values) and genuinely unknown
/// numbers, all of which are [`UnsupportedError::Compression`] — the invitation
/// to register a [`Codec`] for them.
fn unavailable(method: CompressionMethod) -> TiffError {
    match method.cargo_feature() {
        Some(feature) => feature_missing(feature),
        None => TiffError::Unsupported(UnsupportedError::Compression(method.to_u16())),
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
    fn every_in_crate_codec_is_wired_up_or_names_its_feature() {
        // Nothing this crate implements may answer `NotYetAvailable` any
        // more: either the codec runs, or the error names the cargo feature
        // to turn on.
        for method in [
            CompressionMethod::CcittRle,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
            CompressionMethod::CcittRleWord,
            CompressionMethod::Lzw,
            CompressionMethod::OldJpeg,
            CompressionMethod::Jpeg,
            CompressionMethod::AdobeDeflate8,
            CompressionMethod::Deflate,
            CompressionMethod::Lzma,
            CompressionMethod::Zstd,
        ] {
            let bilevel = [1u16];
            let mut cx = context(method);
            if matches!(
                method,
                CompressionMethod::CcittRle
                    | CompressionMethod::CcittFax3
                    | CompressionMethod::CcittFax4
                    | CompressionMethod::CcittRleWord
            ) {
                // The fax codes are defined for single-channel bilevel data
                // only, so give them a geometry they accept.
                cx.bits_per_sample = &bilevel;
                cx.width = 32;
                cx.height = 1;
            }
            let mut dst = [0u8; 4];
            let result = decode_into(&[0; 4], &mut dst, &cx);
            let expected_feature = method.cargo_feature();
            match result {
                Ok(_) => assert!(method.is_available(), "{method} decoded while disabled"),
                Err(TiffError::Unsupported(UnsupportedError::FeatureNotCompiled { feature })) => {
                    assert!(
                        !method.is_available(),
                        "{method} reported its feature while on"
                    );
                    assert_eq!(Some(feature), expected_feature, "{method}");
                }
                Err(TiffError::Format(_)) => {
                    // Four zero bytes are not a valid stream for any of these
                    // codecs; a stream-level complaint is the right answer.
                    assert!(method.is_available(), "{method} parsed while disabled");
                }
                Err(TiffError::Unsupported(UnsupportedError::OldJpeg(_))) => {
                    assert_eq!(method, CompressionMethod::OldJpeg);
                }
                Err(other) => panic!("{method} produced {other}"),
            }
        }
    }

    #[test]
    fn no_codec_reports_not_yet_available() {
        for value in 0u16..=1024 {
            let method = CompressionMethod::from_u16(value);
            let cx = context(method);
            let mut dst = [0u8; 4];
            if let Err(err) = decode_into(&[0; 4], &mut dst, &cx) {
                assert!(
                    !matches!(
                        err,
                        TiffError::Unsupported(UnsupportedError::NotYetAvailable { .. })
                    ),
                    "compression {value} still answers NotYetAvailable"
                );
            }
        }
    }

    #[test]
    fn genuinely_unknown_methods_report_the_number() {
        for method in [
            CompressionMethod::Webp,
            CompressionMethod::JpegXl,
            CompressionMethod::Jbig,
            CompressionMethod::Next,
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
