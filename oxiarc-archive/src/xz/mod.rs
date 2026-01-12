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
//! ```rust,ignore
//! use oxiarc_archive::xz;
//!
//! // Decompress data
//! let compressed = include_bytes!("data.xz");
//! let data = xz::decompress(&mut &compressed[..]).unwrap();
//! ```

mod header;

pub use header::{XzReader, XzWriter, compress, decompress};
