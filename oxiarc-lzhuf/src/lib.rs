//! # OxiArc LZHuf
//!
//! Pure Rust implementation of LZH (LZSS + Huffman) compression.
//!
//! LZH is the compression format used by LHA/LZH archives, which were
//! particularly popular in Japan. This crate provides compression and
//! decompression for the following methods:
//!
//! - **lh0**: Stored (no compression)
//! - **lh4**: 4KB window, static Huffman
//! - **lh5**: 8KB window, static Huffman (most common)
//! - **lh6**: 32KB window, static Huffman
//! - **lh7**: 64KB window, static Huffman
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_lzhuf::{LzhMethod, LzhDecoder, LzhEncoder};
//!
//! // Create an encoder
//! let encoder = LzhEncoder::new(LzhMethod::Lh5);
//!
//! // Create a decoder
//! let decoder = LzhDecoder::new(LzhMethod::Lh5, 1000);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod decode;
pub mod encode;
pub mod huffman;
pub mod lzss;
pub mod methods;

// Re-exports
pub use decode::{LzhDecoder, decode_lzh};
pub use encode::{LzhEncoder, encode_lzh};
pub use huffman::LzhHuffmanTree;
pub use lzss::{LzssDecoder, LzssEncoder, LzssToken};
pub use methods::LzhMethod;
