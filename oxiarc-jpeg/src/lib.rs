//! Pure Rust JPEG (ITU-T T.81 / ISO/IEC 10918-1) decoder for OxiArc.
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
//! | `SOF9`/`10`/`11` | Arithmetic entropy coding | [`UnsupportedFeature::ArithmeticCoding`] |
//! | `SOF5`/`6`/`7`/`13`/`14`/`15` | Hierarchical | [`UnsupportedFeature::Hierarchical`] |
//!
//! Restart markers, `DNL`-resolved heights, one to four components, every
//! sampling factor in `1..=4` (including 4:1:1 and 1x2), `APPn`/`COM`
//! passthrough with `JFIF`, EXIF, XMP, ICC and Adobe `APP14` recognition, and
//! TIFF's abbreviated tables/scan split are all supported.
//!
//! Arithmetic coding is parsed as far as the `DAC` segment — the conditioning
//! tables are read, stored and re-emitted — so that the entropy decoder can be
//! added without any change to the parser or to this crate's API.
//!
//! # Accuracy
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
//! It decodes JPEG and hands metadata back verbatim. It does not interpret
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

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

mod color;
mod decoder;
mod error;
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

pub mod sample;
pub mod tables;
pub mod tiff;

pub use color::ColorSpace;
pub use decoder::{
    ComponentInfo, DecodeOptions, Decoder, ImageInfo, PixelFormat, Upsampling, decode_abbreviated,
    decode_abbreviated_into, decode_abbreviated_into_u16,
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
