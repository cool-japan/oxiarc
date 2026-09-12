//! XZ format support.
//!
//! XZ is a container format for LZMA2 compressed data with integrity checks.
//!
//! ## Implementation
//!
//! The framing implementation lives in [`oxiarc_lzma::xz`] and this module
//! is a thin re-export of it: every public path
//! (`oxiarc_archive::xz::{CheckType, XzReader, XzWriter, compress,
//! decompress}`) is unchanged. It moved there so image codecs — TIFF
//! `Compression = 34925` stores a complete `.xz` stream per strip — can
//! depend on `oxiarc-lzma` alone instead of pulling in all eight archive
//! codecs.
//!
//! Two read-side behaviours changed with the move, both towards what
//! `xz -d` does: a file holding **several concatenated streams** now
//! decodes all of them instead of returning the first stream's bytes as a
//! clean success, and trailing bytes that are neither Stream Padding nor a
//! further stream are reported instead of ignored. See
//! [`oxiarc_lzma::xz`] for the details.
//!
//! ## File Structure
//!
//! - Stream Header (12 bytes): Magic + Flags + CRC32
//! - Blocks: Compressed data blocks
//! - Index: Block size/offset information
//! - Stream Footer (12 bytes): CRC32 + Backward Size + Flags + Magic
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_archive::xz;
//!
//! // Round-trip through the crate's own compressor rather than depending
//! // on an external fixture file.
//! let original = b"Hello, XZ!";
//! let compressed = xz::compress(original, 6)?;
//! let data = xz::decompress(&mut &compressed[..])?;
//! assert_eq!(data, original);
//! # Ok::<(), oxiarc_core::error::OxiArcError>(())
//! ```

// `CheckType` is re-exported because it appears in the public signature of
// [`XzWriter::with_check_type`]; without this the parameter type would be
// reachable but unnameable by downstream crates (rustc's `unnameable_types`).
pub use oxiarc_lzma::xz::{CheckType, XzReader, XzWriter, compress, decompress};
