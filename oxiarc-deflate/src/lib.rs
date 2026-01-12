//! # OxiArc Deflate
//!
//! Pure Rust implementation of the DEFLATE compression algorithm (RFC 1951).
//!
//! This crate provides compression and decompression of DEFLATE data, which
//! is the basis for ZIP, GZIP, and PNG formats.
//!
//! ## Features
//!
//! - **Decompression**: Full support for all DEFLATE block types
//!   - Stored (uncompressed) blocks
//!   - Fixed Huffman codes
//!   - Dynamic Huffman codes
//! - **Compression**: LZ77 + Huffman encoding
//!   - Multiple compression levels (0-9)
//!   - Fixed Huffman codes
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_deflate::{deflate, inflate};
//!
//! // Compress data
//! let original = b"Hello, World! Hello, World!";
//! let compressed = deflate(original, 6).unwrap();
//!
//! // Decompress data
//! let decompressed = inflate(&compressed).unwrap();
//! assert_eq!(&decompressed, original);
//! ```
//!
//! ## Compression Levels
//!
//! - Level 0: No compression (stored blocks)
//! - Level 1-3: Fast compression
//! - Level 4-6: Balanced (default is 6)
//! - Level 7-9: Best compression (slower)

#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod deflate;
pub mod huffman;
pub mod inflate;
pub mod lz77;
pub mod tables;
pub mod zlib;

// Re-exports
pub use deflate::{Deflater, deflate};
pub use huffman::{HuffmanBuilder, HuffmanTree};
pub use inflate::{Inflater, inflate};
pub use lz77::{Lz77Encoder, Lz77Token};
pub use zlib::{Adler32, ZlibCompressor, ZlibDecompressor, zlib_compress, zlib_decompress};
