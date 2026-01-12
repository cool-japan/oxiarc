//! # OxiArc Core
//!
//! Core components for the OxiArc archive library.
//!
//! This crate provides the fundamental building blocks for archive operations:
//!
//! - [`bitstream`]: Bit-level I/O for variable-length codes (Huffman, etc.)
//! - [`ringbuffer`]: Sliding window buffer for LZ77/LZSS decompression
//! - [`crc`]: CRC-32 and CRC-16 checksums
//! - [`traits`]: Core traits for compression/decompression
//! - [`entry`]: Archive entry metadata
//! - [`error`]: Error types
//!
//! ## Architecture
//!
//! OxiArc is designed as a layered protocol stack:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ L4: Unified API                                         │
//! │     Archiver trait, CLI, WASM bindings                 │
//! ├─────────────────────────────────────────────────────────┤
//! │ L3: Container                                           │
//! │     ZIP, TAR, GZIP, LZH header/container parsing       │
//! ├─────────────────────────────────────────────────────────┤
//! │ L2: Codec                                               │
//! │     Deflate (LZ77+Huffman), LZSS+Huffman (LZH), LZMA   │
//! ├─────────────────────────────────────────────────────────┤
//! │ L1: BitStream (this crate)                              │
//! │     BitReader/BitWriter, RingBuffer, CRC               │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_core::bitstream::{BitReader, BitWriter};
//! use oxiarc_core::crc::Crc32;
//! use std::io::Cursor;
//!
//! // Read bits from data
//! let data = vec![0xAB, 0xCD];
//! let mut reader = BitReader::new(Cursor::new(data));
//! let bits = reader.read_bits(12).unwrap();
//!
//! // Compute CRC-32
//! let crc = Crc32::compute(b"Hello, World!");
//! assert_eq!(crc, 0xEC4AC3D0);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod bitstream;
pub mod crc;
pub mod entry;
pub mod error;
pub mod ringbuffer;
pub mod traits;

// Re-exports for convenience
pub use bitstream::{BitReader, BitWriter};
pub use crc::{Crc16, Crc32, Crc64};
pub use entry::{CompressionMethod, Entry, EntryType, FileAttributes};
pub use error::{OxiArcError, Result};
pub use ringbuffer::{OutputRingBuffer, RingBuffer};
pub use traits::{
    ArchiveReader, ArchiveWriter, CompressStatus, CompressionLevel, Compressor, DecompressStatus,
    Decompressor, FlushMode,
};

/// Prelude module for convenient imports.
pub mod prelude {
    pub use crate::bitstream::{BitReader, BitWriter};
    pub use crate::crc::{Crc16, Crc32};
    pub use crate::entry::{CompressionMethod, Entry, EntryType};
    pub use crate::error::{OxiArcError, Result};
    pub use crate::ringbuffer::{OutputRingBuffer, RingBuffer};
    pub use crate::traits::{
        ArchiveReader, ArchiveWriter, CompressionLevel, Compressor, Decompressor,
    };
}
