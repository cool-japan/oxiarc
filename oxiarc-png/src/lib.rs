//! Pure Rust PNG and APNG codec, built on [`oxiarc_deflate`].
//!
//! `oxiarc-png` reads ISO/IEC 15948 (W3C PNG 3rd Edition) images with no C, no
//! `unsafe`, and no dependency outside the OxiArc workspace. The decoding
//! surface is deliberately shaped like the `png` crate 0.18 — same type names,
//! same method names, same `Decoded` events — with a few documented
//! differences (see
//! [compatibility](crate#compatibility-with-the-png-crate) below). The encoder
//! half and the `png`-substitution compatibility test suite land alongside it.
//!
//! # Decoding a whole image
//!
//! ```
//! # use oxiarc_png::chunk::{self, write_chunk, SIGNATURE};
//! # let mut png = SIGNATURE.to_vec();
//! # let mut ihdr = [0u8; 13];
//! # ihdr[3] = 2; ihdr[7] = 2; ihdr[8] = 8; ihdr[9] = 0;
//! # write_chunk(&mut png, chunk::IHDR, &ihdr).expect("write");
//! # let idat = oxiarc_deflate::zlib_compress(&[0u8, 1, 2, 0, 3, 4], 6).expect("z");
//! # write_chunk(&mut png, chunk::IDAT, &idat).expect("write");
//! # write_chunk(&mut png, chunk::IEND, &[]).expect("write");
//! let image = oxiarc_png::decode(&png).expect("decode");
//! assert_eq!((image.width, image.height), (2, 2));
//! assert_eq!(image.data, vec![1, 2, 3, 4]);
//! assert_eq!(image.to_rgba8().expect("rgba").len(), 2 * 2 * 4);
//! ```
//!
//! # Decoding row by row
//!
//! ```no_run
//! use std::fs::File;
//! let decoder = oxiarc_png::Decoder::new(File::open("image.png")?);
//! let mut reader = decoder.read_info()?;
//! while let Some(row) = reader.next_row()? {
//!     let _pixels: &[u8] = row.data();
//! }
//! # Ok::<(), oxiarc_png::DecodingError>(())
//! ```
//!
//! # Safety against hostile input
//!
//! [`decode`] applies [`DecodeLimits::max_alloc_bytes`] to the frame buffer it
//! allocates before it allocates it, so no header can talk this crate into a
//! multi-gigabyte `Vec`. The row-by-row path allocates only two scanlines and
//! is deliberately exempt, so large images can still be streamed.
//!
//! The number of raw bytes a PNG's image data may produce is fixed by `IHDR`
//! alone — for interlaced images, the sum over the seven Adam7 passes that
//! have a non-zero extent. The decoder computes that number before it inflates
//! anything and never offers the decompressor more output space than the
//! remainder of the current scanline, so no compression ratio can make it
//! allocate more than the image header describes. Everything else that scales
//! with attacker-controlled input — text chunks, ICC profiles, retained
//! unknown chunks, animation frames — is bounded by [`DecodeLimits`].
//!
//! # Compatibility with the `png` crate
//!
//! | Area | `png` 0.18 | `oxiarc-png` |
//! |---|---|---|
//! | `Decoder<R>` bound | `R: BufRead + Seek` | `R: Read` (strictly wider) |
//! | Unknown ancillary chunks | discarded | retained in [`Info::unknown_chunks`] |
//! | `tIME`/`sPLT`/`hIST`/`oFFs`/`sCAL`/`pCAL`/`sTER` | not parsed | parsed into [`Info`] |
//! | Apple `CgBI` files | confusing zlib error | decoded, flagged in [`Info::cgbi`] |
//! | Adler-32 | ignored by default | ignored by default (same) |
//! | Ancillary CRC failures | chunk dropped | chunk dropped (same) |
//! | Palette index out of range | opaque black | opaque black, or an error under [`DecodeOptions::set_strict`] |
//! | Whole-image `decode` | not provided | provided, and bounded by [`DecodeLimits::max_alloc_bytes`] |
//!
//! [`Info::unknown_chunks`]: crate::info::Info::unknown_chunks
//! [`Info::cgbi`]: crate::info::Info::cgbi
//!
//! # Migrating from the `png` crate
//!
//! ```toml
//! [dependencies]
//! png = { package = "oxiarc-png", version = "0.4" }
//! ```
//!
//! Leave every `use png::...` and call site alone: the table above lists
//! what differs, and `tests/compat_surface.rs` in this crate's repository
//! reproduces (and runs, not merely type-checks) every real-world call
//! site this crate was designed against — [`Decoder`]/[`Reader`] and
//! [`Encoder`]/[`Writer`] alike — under exactly this `png` alias, so a
//! signature change here fails that test before it can break a downstream
//! `Cargo.toml` swap. [`Writer`]'s default `IDAT`/`fdAT` chunk size (64
//! KiB, configurable via [`Encoder::set_idat_chunk_size`]) is smaller than
//! `png` 0.18's own default (effectively one chunk per frame, since it
//! splits only past `u32::MAX >> 1` bytes) — chosen for
//! streaming-friendliness, and immaterial to decoded pixel content either
//! way.
//!
//! [`Encoder::set_idat_chunk_size`]: crate::encoder::Encoder::set_idat_chunk_size

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

pub mod ancillary;
pub mod apng;
pub mod chunk;
pub mod common;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod filter;
pub mod header;
pub mod image;
pub mod info;
pub mod interlace;
pub mod limits;
pub mod text_metadata;
pub mod v017;
pub mod zlib;

pub use apng::{ApngDecoder, ApngEncoder, ComposedFrame, Subframe};
pub use common::*;
pub use decoder::{
    DecodeOptions, Decoded, Decoder, InterlaceInfo, InterlacedRow, OutputInfo, Reader, Row,
    StreamingDecoder, UnfilterBuf, UnfilterRegion,
};
pub use encoder::{Encoder, StreamWriter, Writer};
pub use error::{
    DecodingError, EncodingError, EncodingFormatError, EncodingFormatErrorKind, FormatError,
    FormatErrorKind, ParameterError, ParameterErrorKind,
};
pub use filter::{Filter, RowFilter};
pub use header::{BitDepth, BytesPerPixel, ColorType, Ihdr, Interlace};
pub use image::Image;
pub use info::Info;
pub use interlace::{
    Adam7Info, Adam7Variant, expand_pass as expand_interlaced_row,
    expand_pass_splat as splat_interlaced_row,
};
pub use limits::{DecodeLimits, Limits};

use std::io::Read;

/// True when `data` starts with the eight-byte PNG signature.
///
/// ```
/// assert!(oxiarc_png::is_png(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
/// assert!(!oxiarc_png::is_png(b"GIF89a"));
/// ```
#[must_use]
pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(&chunk::SIGNATURE)
}

/// Decode a complete PNG from memory, in its own colour type and bit depth.
///
/// For an APNG this decodes the default image, which is the first frame when
/// an `fcTL` chunk precedes `IDAT` and a still image otherwise.
///
/// # Errors
///
/// Any structural problem with the file, or a limit being exceeded.
pub fn decode(data: &[u8]) -> Result<Image, DecodingError> {
    decode_with(data, DecodeOptions::default(), Transformations::IDENTITY)
}

/// Decode a complete PNG from memory into 8-bit RGBA.
///
/// A convenience wrapper that requests `EXPAND | ALPHA | STRIP_16` and returns
/// the packed pixels.
///
/// # Errors
///
/// Any structural problem with the file, or a limit being exceeded.
pub fn decode_rgba8(data: &[u8]) -> Result<Image, DecodingError> {
    decode_with(
        data,
        DecodeOptions::default(),
        Transformations::EXPAND | Transformations::ALPHA | Transformations::STRIP_16,
    )
}

/// Decode a complete PNG from memory with explicit options.
///
/// # Errors
///
/// Any structural problem with the file, or a limit being exceeded.
pub fn decode_with(
    data: &[u8],
    options: DecodeOptions,
    transform: Transformations,
) -> Result<Image, DecodingError> {
    decode_reader_with(data, options, transform)
}

/// Decode a complete PNG from any reader with explicit options.
///
/// # Errors
///
/// Any structural problem with the file, an I/O error, or a limit being
/// exceeded.
pub fn decode_reader_with<R: Read>(
    reader: R,
    options: DecodeOptions,
    transform: Transformations,
) -> Result<Image, DecodingError> {
    let mut decoder = Decoder::new_with_options(reader, options);
    decoder.set_transformations(transform);
    let mut reader = decoder.read_info()?;
    // This is the one place the crate allocates a whole frame on the caller's
    // behalf, so the allocation limit is applied here rather than left to the
    // caller as `png` 0.18 does.
    let size = reader.checked_output_buffer_size()?;
    let mut data = vec![0u8; size];
    let output = reader.next_frame(&mut data)?;
    let _ = reader.finish();
    let info = reader.info().clone();
    Ok(Image {
        width: output.width,
        height: output.height,
        color_type: output.color_type,
        bit_depth: output.bit_depth,
        data,
        info,
    })
}

/// Decode a complete PNG from any reader.
///
/// # Errors
///
/// Any structural problem with the file, an I/O error, or a limit being
/// exceeded.
pub fn decode_reader<R: Read>(reader: R) -> Result<Image, DecodingError> {
    decode_reader_with(reader, DecodeOptions::default(), Transformations::IDENTITY)
}

/// Read only the header and the metadata that precedes the image data.
///
/// No image data is decompressed, so this is cheap even for very large files.
///
/// # Errors
///
/// Any structural problem before the image data.
pub fn peek_info(data: &[u8]) -> Result<Info<'static>, DecodingError> {
    let mut decoder = Decoder::new(data);
    Ok(decoder.read_header_info()?.clone())
}
