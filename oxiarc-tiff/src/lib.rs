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
//! * **Codecs** — uncompressed (1), PackBits (32773), LZW (5), Deflate (8 and
//!   32946), CCITT RLE (2), Group 3 (3), Group 4 (4) and word-aligned RLE
//!   (32771) by default, plus ZSTD (50000), LZMA (34925) and JPEG (7, with
//!   best-effort old-style JPEG 6) behind cargo features. Every one of them
//!   encodes as well as decodes. A registered value this crate implements but
//!   whose feature is off reports [`UnsupportedError::FeatureNotCompiled`],
//!   and one it does not implement reports [`UnsupportedError::Compression`];
//!   nothing falls through a wildcard, and out-of-tree codecs plug in through
//!   [`Codec`] and a [`CodecRegistry`] (an out-of-tree codec can also be
//!   selected for **encoding**, via [`writer::Compression::Registered`]).
//! * **Optional features** — `compat` (a `tiff`-0.11.3-shaped facade, see
//!   "Migrating from `tiff`" below), `rayon` (parallel strip/tile decode and
//!   encode, byte-identical to the serial path) and `mmap`
//!   (`Decoder::from_path`, a memory-mapped reader). All three are named as
//!   plain text here, not doc links: each is only compiled when its feature
//!   is on, and this paragraph is not.
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
//! * [`FillOrder`] 2 reverses the bits of every byte of the **compressed**
//!   chunk, at *every* bit depth — not only for sub-byte samples, and not
//!   after decompression. That is where libtiff does it (`TIFFFillStrip` on
//!   read, `TIFFFlushData1` on write), verified against libtiff 4.7.1 at 1, 8,
//!   16, 32 and 64 bits in strips and tiles: a `tiffcp -c packbits -f lsb2msb`
//!   strip has its PackBits control bytes reversed too, so decoding it without
//!   reversing first yields the wrong *length*. The CCITT codecs are the
//!   exception and consume the tag themselves
//!   ([`compression::handles_fill_order`]). [`Decoder::read_chunk_raw`]
//!   deliberately does not apply the reversal.
//! * The CCITT codes are **photometric-agnostic**: measured against libtiff
//!   4.7.1, `tiffcp -c g3` and `-c g4` write byte-identical strips for a
//!   `MinIsWhite` and a `MinIsBlack` page holding the same bits, so a coded
//!   *white* run is a run of zero bits whatever tag 262 says. Group 3/4
//!   uncompressed mode (T.4 §4.2.1.3.2) is decoded unconditionally and
//!   written only when [`ImageSpec::with_ccitt_uncompressed`] asks for it,
//!   because libtiff 4.7.1 cannot read it back.
//! * A JPEG page's `SOF` sampling factors win over `YCbCrSubSampling` (TTN2);
//!   a disagreement is an error only under [`Leniency::Strict`]. TIFF JPEG
//!   carries no `JFIF` and no `Adobe` marker, so colour stays in
//!   `PhotometricInterpretation` and a `Separated` page is never inverted.
//! * [`Compression::Deflate`] writes tag value 8 (the Adobe registration that
//!   libtiff, GDAL and `tifffile` write); 32946 is read identically.
//! * `Orientation`, `ImageDescription` (ImageJ, OME) and the GeoTIFF *keys* are
//!   exposed and passed through, never interpreted.
//!
//! # Migrating from `tiff` / `image`
//!
//! Enable the `compat` feature for a mechanical, mostly-mechanical migration
//! off the `tiff` crate (0.11.3-shaped) or, transitively, `image`'s `tiff`
//! codec (the sibling [`oxiarc-image`](https://docs.rs/oxiarc-image) crate is
//! the facade for `image` itself; `compat` is the layer under it for TIFF
//! specifically):
//!
//! ```text
//! - use tiff::decoder::{Decoder, DecodingResult};
//! - use tiff::{ColorType, TiffError};
//! - use tiff::tags::Tag;
//! + use oxiarc_tiff::compat::decoder::{Decoder, DecodingResult};
//! + use oxiarc_tiff::compat::{ColorType, TiffError};
//! + use oxiarc_tiff::compat::tags::Tag;
//! ```
//!
//! Three shapes are frozen there because downstream code (`image` 0.25.10's
//! own `codecs/tiff.rs` included) matches them exhaustively with no wildcard
//! arm: `compat::decoder::DecodingResult` (exactly the eleven upstream
//! variants; `F16` carries real `half::f16` values, unlike this crate's own
//! [`Samples::F16`], which stays raw `u16` bits so the native API needs no
//! `half` dependency), `compat::ColorType` (all ten upstream variants) and
//! `compat::TiffError` (exactly six variants). `tests/compat_api.rs`
//! reproduces the calls `image` 0.25.10 makes against this module — the
//! `Limits` setup, `dimensions`, `colortype`,
//! `find_tag_unsigned_vec::<u16>(SampleFormat)`, `read_image_to_buffer`, the
//! ICC and orientation tag reads, both exhaustive `TiffError` match sites and
//! the encoder path — so a shape break fails a compile or a test in this
//! crate, never downstream. It pins the *shape*; it is not a claim that
//! `image` itself builds against this module unmodified (one of the
//! documented deviations, the owned `to_bytes` in place of upstream's
//! borrowed `as_bytes`, makes that impossible under
//! `#![forbid(unsafe_code)]`), which is what the `oxiarc-image` facade is
//! for. See the `compat` module's own docs (`cargo doc --features compat`)
//! for the full contract, including the deliberate, documented deviations
//! from upstream (never a silent behaviour change). This paragraph names it
//! as plain text, not a doc link, because it is only compiled when the
//! `compat` feature is on, and this paragraph is not.
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
#[cfg(feature = "compat")]
pub mod compat;
pub mod compression;
pub mod decode;
pub mod error;
pub mod header;
pub mod ifd;
pub mod image;
pub mod limits;
#[cfg(feature = "mmap")]
pub mod mmap;
pub mod predictor;
#[cfg(feature = "rayon")]
pub mod rayon_support;
pub mod reader;
pub mod sample;
pub mod tags;
pub mod writer;

pub use byteorder::{Endian, EndianReader, EndianWriter};
pub use compression::{Codec, CodecContext, CodecRegistry, CodecState, OldJpegParams};
pub use error::{FormatError, LimitError, Result, TiffError, UnsupportedError, UsageError};
pub use header::{Header, Variant};
pub use ifd::{Directory, Entry, IfdPointer, Rational, SRational, Value, ValueSource};
pub use image::{ChunkGeometry, ChunkType, ColorType, ImageInfo, ImageLayout, Rect};
pub use limits::{Leniency, Limits, OutputBudget, Warning, Warnings};
#[cfg(feature = "mmap")]
pub use mmap::MmapDecoder;
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
