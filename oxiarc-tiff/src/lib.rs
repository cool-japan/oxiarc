//! Pure Rust TIFF 6.0 / BigTIFF reader and writer for OxiArc.
//!
//! Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
//! archive/compression ecosystem: no C, no FFI, no `unsafe`.
//!
//! # What this crate covers
//!
//! * **Containers** — classic TIFF (32-bit offsets) and BigTIFF (64-bit), in
//!   either byte order, with multi-page IFD chains, SubIFD trees and the
//!   EXIF / GPS / Interoperability sub-IFDs.
//! * **Tags** — the TIFF 6.0 baseline and extensions, the GeoTIFF tags, the
//!   metadata blobs (XMP, ICC, IPTC, Photoshop) and the DNG basics, with
//!   unknown tags *and unknown field types* retained for round-tripping.
//! * **Geometry** — strips and tiles, chunky and planar, 1/2/4/8/12/16/24/32/64
//!   bit samples, `FillOrder` 2, heterogeneous `BitsPerSample`, YCbCr
//!   subsampling.
//! * **Transforms** — predictors 1/2/3 (horizontal differencing with whole-sample
//!   carry propagation in the *file's* byte order, and the floating-point
//!   byte-plane transpose) and the photometric conversions.
//! * **Codecs** — uncompressed and PackBits ship today; every other registered
//!   compression value has a named enum variant and returns a typed
//!   [`UnsupportedError::NotYetAvailable`] error from the dispatch rather than
//!   falling through a wildcard.
//!
//! # Reading
//!
//! ```
//! use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec, Samples};
//! use std::io::Cursor;
//!
//! // Build a 4x2 greyscale image so the example is self-contained.
//! let pixels: Vec<u8> = vec![0, 40, 80, 120, 160, 200, 240, 255];
//! let mut buffer = Cursor::new(Vec::new());
//! let mut encoder = Encoder::new(&mut buffer)?;
//! encoder.write_image(&ImageSpec::new(4, 2, ColorType::Gray(8)), &pixels)?;
//! encoder.finish()?;
//!
//! let mut decoder = Decoder::new(Cursor::new(buffer.into_inner()))?;
//! assert_eq!(decoder.dimensions()?, (4, 2));
//! assert_eq!(decoder.color_type()?, ColorType::Gray(8));
//! match decoder.read_image()? {
//!     Samples::U8(data) => assert_eq!(data, pixels),
//!     other => panic!("unexpected sample type: {:?}", other.sample_type()),
//! }
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```
//!
//! # Interoperability notes
//!
//! * A predictor is only honoured by libtiff for the codecs that install its
//!   predictor hooks (LZW, Deflate, ZSTD, LZMA, LERC). Writing
//!   [`Predictor::Horizontal`] together with [`Compression::None`] or
//!   [`Compression::PackBits`] produces a file this crate reads back exactly
//!   and libtiff misreads; see [`ImageSpec::with_predictor`].
//! * A `YCbCr` image with no `YCbCrSubSampling` tag is read back as 2x2
//!   subsampled, because that is the TIFF 6.0 default. The encoder therefore
//!   always writes tag 530 for a YCbCr page.
//! * `Orientation`, `ImageDescription` (ImageJ, OME) and the GeoTIFF *keys* are
//!   exposed and passed through, never interpreted.
//!
//! # Guarding untrusted input
//!
//! Every allocation size in a TIFF decoder comes from numbers in the file, so
//! [`Limits`] is checked *before* any allocation and the file-level
//! [`OutputBudget`] bounds the total decoded output even though each strip
//! resets its codec.
//!
//! ```
//! use oxiarc_tiff::{Decoder, Limits};
//! use std::io::Cursor;
//!
//! # let bytes = {
//! #     use oxiarc_tiff::{ColorType, Encoder, ImageSpec};
//! #     let mut buffer = Cursor::new(Vec::new());
//! #     let mut encoder = Encoder::new(&mut buffer).expect("encoder");
//! #     encoder
//! #         .write_image(&ImageSpec::new(2, 2, ColorType::Gray(8)), &[0u8, 1, 2, 3])
//! #         .expect("write");
//! #     encoder.finish().expect("finish");
//! #     buffer.into_inner()
//! # };
//! let limits = Limits::default().with_max_image_bytes(2);
//! let mut decoder = Decoder::new(Cursor::new(bytes))?.with_limits(limits);
//! assert!(decoder.read_image().unwrap_err().is_limits());
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

pub mod byteorder;
pub mod colour;
pub mod compression;
pub mod decode;
pub mod error;
pub mod header;
pub mod ifd;
pub mod image;
pub mod limits;
pub mod predictor;
pub mod reader;
pub mod sample;
pub mod tags;
pub mod writer;

pub use byteorder::{Endian, EndianReader, EndianWriter};
pub use compression::{Codec, CodecContext, CodecRegistry};
pub use error::{FormatError, LimitError, Result, TiffError, UnsupportedError, UsageError};
pub use header::{Header, Variant};
pub use ifd::{Directory, Entry, IfdPointer, Rational, SRational, Value, ValueSource};
pub use image::{ChunkGeometry, ChunkType, ColorType, ImageInfo, ImageLayout, Rect};
pub use limits::{Leniency, Limits, OutputBudget, Warning, Warnings};
pub use predictor::{apply_predictor_forward, apply_predictor_reverse};
pub use reader::{Decoder, GeoTags, SubIfdNode};
pub use sample::{SampleType, Samples, f16_bits_to_f32, f32_to_f16_bits};
pub use tags::{
    CompressionMethod, ExtraSamples, FillOrder, Orientation, PhotometricInterpretation,
    PlanarConfiguration, Predictor, ResolutionUnit, SampleFormat, Tag, Type,
};
pub use writer::{
    Compression, DirectoryWriter, Encoder, ImageSpec, ImageWriter, Layout, VariantChoice,
};
