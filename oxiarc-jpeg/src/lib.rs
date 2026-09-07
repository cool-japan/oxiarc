//! Pure Rust JPEG (ITU-T T.81 / ISO/IEC 10918-1) codec for OxiArc.
//!
//! Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
//! archive and compression ecosystem. No C, no FFI, no `unsafe`.
//!
//! # What is decoded
//!
//! | `SOF` | Process | Status |
//! |---|---|---|
//! | `SOF0` | Baseline sequential DCT, 8-bit | decoded |
//! | `SOF1` | Extended sequential DCT, 8- and 12-bit | decoded |
//! | `SOF2` | Progressive DCT, 8- and 12-bit | decoded |
//! | `SOF3` | Lossless predictive, 2..=16 bit | decoded |
//! | `SOF9`/`10`/`11` | Arithmetic entropy coding, 8- and 12-bit (and 2..=16 lossless) | decoded, feature `arithmetic` |
//! | `SOF5`/`6`/`7`/`13`/`14`/`15` | Hierarchical | [`UnsupportedFeature::Hierarchical`] |
//!
//! Restart markers, `DNL`-resolved heights, one to four components, every
//! sampling factor in `1..=4` (including 4:1:1 and 1x2), `APPn`/`COM`
//! passthrough with `JFIF`, EXIF, XMP, ICC and Adobe `APP14` recognition, and
//! TIFF's abbreviated tables/scan split are all supported, as is legacy
//! OJPEG (TIFF `Compression = 6`) reconstruction — see [`tiff`].
//!
//! # Feature flags
//!
//! | Feature | Default | What it does |
//! |---|---|---|
//! | `arithmetic` | on | The QM coder of T.81 Annex D, in both directions, for `SOF9`, `SOF10` and `SOF11`, with `DAC` conditioning and restart handling. Without it those frames report [`UnsupportedFeature::ArithmeticCoding`]; the `DAC` segment is parsed, stored and re-emitted either way. |
//! | `rayon` | off | Entropy coding across restart intervals, in parallel. The output is byte-identical with and without it. |
//! | `jpeg-oracle` | off | Differential tests against `cjpeg`/`djpeg`/`tiffcp`/Pillow, which self-skip when the tools are absent. |
//!
//! # Arithmetic coding
//!
//! `SOF9` and `SOF10` are held to byte identity with libjpeg in both
//! directions: our decode equals `djpeg -dct int`'s and our encode equals
//! `cjpeg -arithmetic -dct int`'s, `DAC` segments included. `SOF11` has no
//! reference implementation anywhere — libjpeg-turbo refuses `-lossless`
//! with `-arithmetic` and cannot decode the process either — so it follows
//! T.81 Annex H.1.2.3 (the two-dimensional statistical model) and is gated by
//! exact round-tripping instead.
//!
//! One property differs from Huffman scans and is worth knowing: T.81 D.2.6
//! lets an arithmetic decoder read zeros past the last coded byte, so a
//! *truncated* arithmetic scan decodes to noise rather than to an error.
//! Only an impossible decision sequence
//! ([`JpegError::InvalidArithmeticCode`]) or a missing restart marker is
//! reported. libjpeg behaves the same way.
//!
//! # What is encoded
//!
//! [`Encoder`] writes `SOF0`, `SOF1` (8- and 12-bit), `SOF2` and `SOF3`, with
//! quality-scaled or caller-supplied quantisation tables, standard Annex K.3
//! or generated Huffman tables, box or smoothed chroma decimation at every
//! exact sampling ratio, restart intervals, `JFIF`/Adobe/EXIF/XMP/ICC/`COM`
//! metadata, and TIFF's abbreviated tables-only and scan-only halves. See
//! [`EncodeOptions`].
//!
//! # Accuracy
//!
//! Both halves are held to byte identity with libjpeg-turbo, not to a
//! tolerance. Encoded output matches `cjpeg -dct int` byte for byte across
//! every subsampling ratio, quality, restart spelling, `-optimize`,
//! `-smooth`, `-progressive` (default and custom scan scripts),
//! `-precision 12` and `-lossless`, and the `JPEGTables` blob
//! [`table_set`] builds matches libtiff's for the same quality.
//!
//! The inverse DCT is libjpeg's `jpeg_idct_islow` reproduced exactly, the
//! chroma upsamplers are libjpeg's three fancy kernels with its asymmetric
//! rounding, and the YCbCr to RGB conversion is its fixed-point form. Decoded
//! 8-bit output is therefore intended to be **byte-identical** to
//! `djpeg -dct int`, which is what the `jpeg-oracle` test suite asserts. The
//! single deliberate deviation is that out-of-range reconstructions are
//! clamped rather than run through libjpeg's wrapping range-limit table; see
//! the `idct` module documentation.
//!
//! # What this crate does not do
//!
//! It codes JPEG and hands metadata back verbatim. It does not interpret
//! EXIF, apply ICC profiles, honour orientation, resize or otherwise process
//! images.
//!
//! # Examples
//!
//! Decode a complete datastream:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::{ColorSpace, Decoder};
//!
//! let bytes: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1;
//! let mut decoder = Decoder::new(bytes);
//! let info = decoder.read_info()?;
//! assert_eq!((info.width, info.height), (1, 1));
//! assert_eq!(info.output_color_space, ColorSpace::Luma);
//!
//! let pixels = decoder.decode()?;
//! assert_eq!(pixels.len(), decoder.output_buffer_size().unwrap_or(0));
//! # Ok(())
//! # }
//! ```
//!
//! Decode a TIFF `Compression = 7` strip against its `JPEGTables` tag, with no
//! buffer concatenation:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::{DecodeOptions, TableSet, decode_abbreviated_into};
//!
//! let tables = TableSet::parse(&oxiarc_jpeg::sample::GRAY_1X1_TABLES)?;
//! let strip: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1_SCAN;
//!
//! let mut out = [0u8; 1];
//! let info = decode_abbreviated_into(
//!     Some(&tables),
//!     strip,
//!     &DecodeOptions::raw(),
//!     &mut out,
//! )?;
//! assert_eq!(info.num_components, 1);
//! # Ok(())
//! # }
//! ```
//!
//! Encode with the arithmetic coder and read it back (the whole example is
//! compiled away when the `arithmetic` feature is off):
//!
//! ```
//! # #[cfg(feature = "arithmetic")]
//! # fn run() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::{Decoder, EncodeOptions, EntropyCoding, InputColor,
//!                   encode_to_vec_with_options};
//!
//! let pixels = vec![70u8; 32 * 16 * 3];
//! let options = EncodeOptions {
//!     quality: 90,
//!     entropy: EntropyCoding::Arithmetic,
//!     ..Default::default()
//! };
//! let jpeg = encode_to_vec_with_options(&pixels, 32, 16, InputColor::Rgb, &options)?;
//! // SOF9: extended sequential, arithmetic.
//! assert!(jpeg.windows(2).any(|w| w == [0xFF, 0xC9]));
//!
//! let info = Decoder::new(&jpeg[..]).read_info()?;
//! assert_eq!(info.entropy, EntropyCoding::Arithmetic);
//! let decoded = Decoder::new(&jpeg[..]).decode()?;
//! assert!(decoded.iter().all(|&v| v.abs_diff(70) <= 3));
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "arithmetic"))]
//! # fn run() -> Result<(), oxiarc_jpeg::JpegError> { Ok(()) }
//! # run().expect("arithmetic round trip");
//! ```
//!
//! Encode one, and read back what was written:
//!
//! ```
//! # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
//! use oxiarc_jpeg::{Decoder, InputColor, Subsampling, EncodeOptions,
//!                   encode_to_vec_with_options};
//!
//! let pixels = vec![90u8; 16 * 16 * 3];
//! let options = EncodeOptions {
//!     quality: 92,
//!     subsampling: Subsampling::S444,
//!     ..Default::default()
//! };
//! let jpeg = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options)?;
//!
//! let decoded = Decoder::new(&jpeg[..]).decode()?;
//! assert_eq!(decoded.len(), 16 * 16 * 3);
//! assert!(decoded.iter().all(|&v| v.abs_diff(90) <= 2));
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

#[cfg(feature = "arithmetic")]
mod arith;
mod color;
mod decoder;
mod downsample;
mod encoder;
mod error;
mod fdct;
mod frame;
mod huffman;
mod idct;
mod limits;
mod marker;
mod metadata;
mod parser;
mod quant;
mod tableset;
mod upsample;

pub mod compat;
pub mod sample;
pub mod tables;
pub mod tiff;

pub use color::ColorSpace;
pub use decoder::{
    ComponentInfo, DecodeOptions, Decoder, ImageInfo, PixelFormat, Upsampling, decode_abbreviated,
    decode_abbreviated_into, decode_abbreviated_into_u16,
};
pub use downsample::Downsampling;
pub use encoder::{
    ComponentIds, Density, EncodeOptions, EncodeProcess, Encoder, InputColor, MarkerPolicy,
    QuantTableSource, RestartInterval, ScanSpec, Subsampling, encode_to_vec,
    encode_to_vec_with_options, encode_u16_to_vec_with_options, table_set,
};
pub use error::{JpegError, LimitKind, TableKind, UnsupportedFeature};
pub use frame::{
    ArithmeticConditioning, CodingProcess, Component, EntropyCoding, FrameHeader, ScanHeader,
};
pub use huffman::HuffmanTable;
pub use limits::DecodeLimits;
pub use marker::Marker;
pub use metadata::{AdobeHeader, AppSegment, JfifHeader};
pub use quant::{QuantTable, natural_to_zigzag, quality_scaling_factor};
pub use tableset::{TableSet, TablesMode};
