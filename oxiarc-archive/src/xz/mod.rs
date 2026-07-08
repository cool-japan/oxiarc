//! XZ format support.
//!
//! XZ is a container format for LZMA2 compressed data with integrity checks.
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

mod header;
pub(crate) mod sha256;

pub use header::{XzReader, XzWriter, compress, decompress};
