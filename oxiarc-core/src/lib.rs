//! # OxiArc Core
//!
//! Core components for the OxiArc archive library.
//!
//! This crate provides the fundamental building blocks for archive operations:
//!
//! - [`bitstream`]: LSB-first bit-level I/O for variable-length codes (DEFLATE, etc.)
//! - [`msb_bitstream`]: MSB-first bit-level I/O for canonical LZH/LHA codecs
//! - [`ringbuffer`]: Sliding window buffer for LZ77/LZSS decompression
//! - [`crc`]: CRC-32 and CRC-16 checksums
//! - [`traits`]: Core traits for compression/decompression
//! - [`entry`]: Archive entry metadata
//! - [`error`]: Error types
//! - `async_io`: Async I/O support (requires `async-io` feature)
//! - `mmap`: Memory-mapped file support (requires `mmap` feature)
//!
//! ## Pre-1.0 API notes
//!
//! - [`traits::Compressor`]/[`traits::Decompressor`] model the DEFLATE-family
//!   streaming contract and are implemented by the codecs that fit it
//!   (Deflate, LZH, LZ4); they are an optional convenience, not a
//!   requirement on every codec crate. See the trait docs for details.
//! - The `CompressStatus`/`DecompressStatus`/`FlushMode` enums are
//!   `#[non_exhaustive]` so new variants can be added without a breaking
//!   change.
//! - There is no shared `CompressionLevel` type in core; each codec defines
//!   its own (see `traits` module source for the rationale).
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
//! let bits = reader.read_bits(12).expect("read 12 bits from 2-byte buffer");
//!
//! // Compute CRC-32
//! let crc = Crc32::compute(b"Hello, World!");
//! assert_eq!(crc, 0xEC4AC3D0);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod bitstream;
pub mod cancel;
pub mod crc;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub mod crc_simd;
pub mod entry;
pub mod error;
pub mod msb_bitstream;
pub mod progress;
pub mod ringbuffer;
pub mod sha256;
pub mod traits;

#[cfg(feature = "async-io")]
pub mod async_io;

#[cfg(feature = "mmap")]
pub mod mmap;

// Re-exports for convenience
pub use bitstream::{BitCache, BitReader, BitWriter};
pub use cancel::CancellationToken;
pub use crc::{Crc16, Crc32, Crc64};
pub use entry::{CompressionMethod, Entry, EntryBuilder, EntryType, FileAttributes};
pub use error::{OxiArcError, Result};
pub use msb_bitstream::{MsbBitReader, MsbBitWriter};
pub use progress::{NoopProgress, ProgressHandle, ProgressSink, noop_progress};
pub use ringbuffer::{OutputRingBuffer, RingBuffer, RingSnapshot};
pub use traits::{
    ArchiveReader, ArchiveWriter, CompressStatus, Compressor, DecompressStatus, Decompressor,
    FlushMode,
};

// Optional async-io re-exports
#[cfg(feature = "async-io")]
pub use async_io::{
    AsyncCompressor, AsyncCompressorWrapper, AsyncDecompressor, AsyncDecompressorWrapper,
    StreamingAsyncCompressor, StreamingAsyncDecompressor, compress_concurrent,
    decompress_concurrent,
};

// Optional mmap re-exports
#[cfg(feature = "mmap")]
pub use mmap::{MappedFile, MmapOptions, MmapReader};

/// Prelude module for convenient imports.
///
/// # Why only `oxiarc-core` has a prelude
///
/// This is a deliberate, documented pre-1.0 API decision: `oxiarc-core` is
/// the one crate in the workspace whose public surface is broad enough
/// (bitstream I/O, CRCs, entry metadata, error types, ring buffers, core
/// traits) to benefit from a single glob-importable module. The codec
/// crates (`oxiarc-deflate`, `oxiarc-bzip2`, `oxiarc-lz4`, etc.) and the
/// container crate (`oxiarc-archive`) intentionally do **not** get their
/// own `prelude` modules. Each of those crates already re-exports its
/// small, flat public API directly at the crate root (`oxiarc_bzip2::compress`,
/// `oxiarc_archive::zip::ZipWriter`, and so on), so a `prelude` submodule
/// would just be a redundant, parallel-maintained copy of the crate root
/// with no import ergonomics gained. Do not add per-crate preludes to the 9
/// codec/format crates during the API freeze; if a specific crate's root
/// surface grows large enough to warrant one, that should be a deliberate,
/// separately-reviewed decision for that crate, not a blanket pattern.
pub mod prelude {
    #[cfg(feature = "async-io")]
    pub use crate::async_io::{
        AsyncCompressor, AsyncCompressorWrapper, AsyncDecompressor, AsyncDecompressorWrapper,
    };
    pub use crate::bitstream::{BitCache, BitReader, BitWriter};
    pub use crate::crc::{Crc16, Crc32};
    pub use crate::entry::{CompressionMethod, Entry, EntryBuilder, EntryType};
    pub use crate::error::{OxiArcError, Result};
    #[cfg(feature = "mmap")]
    pub use crate::mmap::{MappedFile, MmapOptions, MmapReader};
    pub use crate::msb_bitstream::{MsbBitReader, MsbBitWriter};
    pub use crate::ringbuffer::{OutputRingBuffer, RingBuffer, RingSnapshot};
    pub use crate::traits::{ArchiveReader, ArchiveWriter, Compressor, Decompressor};
}
