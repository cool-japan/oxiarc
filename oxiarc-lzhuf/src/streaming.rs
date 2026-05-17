//! Streaming LZH decompression.
//!
//! This module provides a true streaming decompressor that can handle partial
//! input/output and resume decompression across multiple calls.
//!
//! # Submodules
//!
//! - [`huffman`]: Streaming bit reader and Huffman lookup table primitives.
//! - [`decoder`]: Decoder state machine, `LzhStreamDecoder`, and convenience functions.

pub mod decoder;
pub mod huffman;

// Re-export public items so the crate-level `use streaming::*` API is unchanged.
pub use decoder::{
    DecoderPhase, LzhStreamDecoder, StreamingLzhDecoder, create_streaming_decoder,
    decode_lzh_streaming,
};
pub use huffman::{BitReaderState, StreamingBitReader, StreamingHuffmanTree};
