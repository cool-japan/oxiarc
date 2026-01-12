//! GZIP format support (RFC 1952).
//!
//! GZIP is a file format for single-file compression using DEFLATE.
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_archive::gzip;
//!
//! // Compress data
//! let data = b"Hello, World!";
//! let compressed = gzip::compress(data, 6).unwrap();
//!
//! // Decompress data
//! let mut reader = std::io::Cursor::new(compressed);
//! let decompressed = gzip::decompress(&mut reader).unwrap();
//! assert_eq!(decompressed, data);
//! ```

mod header;

pub use header::{GzipHeader, GzipReader, GzipWriter, compress, compress_with_filename};

use oxiarc_core::error::Result;
use std::io::Read;

/// Decompress a GZIP file.
pub fn decompress<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut gzip_reader = GzipReader::new(reader)?;
    gzip_reader.decompress()
}
