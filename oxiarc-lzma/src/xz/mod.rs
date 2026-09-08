//! XZ container format (`.xz`) — stream framing around LZMA2.
//!
//! XZ is the container format that wraps LZMA2-compressed data with
//! integrity checks. This module owns the whole framing layer:
//!
//! - Stream Header (12 bytes): magic + stream flags + CRC-32
//! - Blocks: block header (filter list, optional sizes, CRC-32), LZMA2
//!   payload, block padding, and the per-block check
//! - Index: one record (unpadded size, uncompressed size) per block, CRC-32
//! - Stream Footer (12 bytes): CRC-32 + backward size + stream flags + magic
//!
//! A `.xz` *file* is **one or more** such streams, optionally separated and
//! terminated by Stream Padding (runs of null bytes whose total length is a
//! multiple of four) — which is what `cat a.xz b.xz` and parallel
//! compressors produce. [`decompress`](crate::xz::decompress) decodes every stream and
//! concatenates the results, matching `xz -d`; trailing bytes that are
//! neither padding nor a further stream are an error rather than a silent
//! short read.
//!
//! All four check types the format defines are verified on read:
//! `None`, `CRC-32`, `CRC-64/ECMA-182` and `SHA-256` (a dependency-free
//! FIPS 180-4 implementation lives in [`oxiarc_core::sha256`], shared with
//! any other crate that needs it — `oxiarc-http`'s RFC 9842 dictionary-hash
//! matching, for one). Because libtiff writes `LZMA_CHECK_NONE`, the reader
//! also cross-checks the two header fields that carry redundancy on that
//! path: a block's declared Uncompressed Size must match what it decoded
//! to, and the index's record count must match the number of blocks the
//! stream actually contained.
//!
//! # Where this module is used
//!
//! `oxiarc-archive` re-exports this module unchanged as
//! `oxiarc_archive::xz`, so archive-level callers see no difference. Image
//! codecs that need `.xz` without pulling in eight archive codecs — TIFF
//! `Compression = 34925` writes **a complete `.xz` stream per strip/tile**,
//! typically with `LZMA_CHECK_NONE` — depend on `oxiarc-lzma` alone and use
//! [`decompress_into`](crate::xz::decompress_into) /
//! [`decompress_with_limit`](crate::xz::decompress_with_limit) /
//! [`XzDecoder`](crate::xz::XzDecoder).
//!
//! # Bounded decoding
//!
//! [`decompress`](crate::xz::decompress) is unbounded (it grows a `Vec` to
//! whatever the stream produces). Untrusted input should use
//! [`decompress_into`](crate::xz::decompress_into) (decode into a
//! caller-sized buffer) or
//! [`decompress_with_limit`](crate::xz::decompress_with_limit) (decode with an
//! explicit output cap); both enforce the limit *during* decoding, chunk by
//! chunk, so a decompression bomb is rejected before it is materialised.
//!
//! # Decoding many streams: reuse [`XzDecoder`](crate::xz::XzDecoder)
//!
//! [`decompress_into`](crate::xz::decompress_into) is a thin one-shot wrapper:
//! every call builds a
//! fresh LZMA2 window (dictionary) from nothing. That is the right choice
//! for a handful of streams, but a TIFF `Compression = 34925` image can
//! carry thousands of strips, each its own complete `.xz` stream — and
//! rebuilding a multi-megabyte dictionary buffer (and repeatedly growing it
//! one `Vec::push` at a time) thousands of times over is pure waste when
//! every strip uses the same dictionary size.
//! [`XzDecoder`](crate::xz::XzDecoder) keeps that
//! buffer allocated across calls: construct one, feed it every strip in
//! turn, and only a genuine change in dictionary size forces a fresh
//! allocation.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_lzma::xz;
//!
//! let original = b"Hello, XZ!";
//! let compressed = xz::compress(original, 6)?;
//!
//! // Unbounded, from any `Read`:
//! let data = xz::decompress(&mut &compressed[..])?;
//! assert_eq!(data, original);
//!
//! // A concatenation of streams decodes as one, the way `xz -d` reads it:
//! let mut two = compressed.clone();
//! two.extend_from_slice(&compressed);
//! assert_eq!(xz::decompress(&mut &two[..])?, b"Hello, XZ!Hello, XZ!");
//!
//! // Bounded, straight into a caller-supplied buffer (the TIFF strip shape):
//! let mut strip = vec![0u8; original.len()];
//! let written = xz::decompress_into(&compressed, &mut strip)?;
//! assert_eq!(&strip[..written], original);
//!
//! // Bounded, growable:
//! let capped = xz::decompress_with_limit(&compressed, 1024)?;
//! assert_eq!(capped, original);
//!
//! // Reused across many streams (the TIFF strip-loop shape): one decoder,
//! // fed one complete `.xz` stream per call.
//! let mut decoder = xz::XzDecoder::new();
//! let mut strip = vec![0u8; original.len()];
//! for _ in 0..3 {
//!     let written = decoder.decompress_into(&compressed, &mut strip)?;
//!     assert_eq!(&strip[..written], original);
//! }
//! # Ok::<(), oxiarc_core::error::OxiArcError>(())
//! ```

mod decoder;
mod filters;
mod header;
mod writer;

// `CheckType` is re-exported because it appears in the public signature of
// [`XzWriter::with_check_type`]; without this the parameter type would be
// reachable but unnameable by downstream crates (rustc's `unnameable_types`).
pub use decoder::XzDecoder;
pub use header::{
    CheckType, XzReader, compress, decompress, decompress_into, decompress_with_limit,
};
pub use writer::XzWriter;
