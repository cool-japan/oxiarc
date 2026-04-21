//! # OxiArc Archive
//!
//! Archive container format support for OxiArc.
//!
//! This crate provides reading and writing of various archive formats:
//!
//! - **ZIP**: The ubiquitous archive format
//! - **GZIP**: Single-file compression using DEFLATE
//! - **TAR**: Unix tape archive format
//! - **LZH**: Japanese archive format with LZSS+Huffman compression
//! - **XZ**: LZMA2 compressed files with integrity checks
//! - **7z**: 7-Zip archive format with LZMA/LZMA2 compression
//! - **LZ4**: Fast compression format
//! - **Zstandard**: Modern fast compression format
//! - **Bzip2**: Block-sorting compression format
//! - **CAB**: Microsoft Cabinet archive format
//!
//! ## Example
//!
//! ```rust,no_run
//! use oxiarc_archive::detect::ArchiveFormat;
//! use oxiarc_archive::zip::ZipReader;
//! use std::fs::File;
//!
//! // Detect format
//! let mut file = File::open("archive.zip").unwrap();
//! let (format, _) = ArchiveFormat::detect(&mut file).unwrap();
//! println!("Format: {}", format);
//! ```
//!
//! ## Format Detection
//!
//! Use [`detect::ArchiveFormat`] to automatically detect the format of an
//! archive based on its magic bytes.

#![warn(missing_docs)]
#![warn(clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod brotli;
pub mod bzip2;
pub mod cab;
pub mod detect;
pub mod gzip;
pub mod lenient;
pub mod lz4;
pub mod lzh;
pub mod sevenz;
pub mod snappy;
pub mod tar;
pub mod xz;
pub mod zip;
pub mod zstd;

#[cfg(feature = "async-io")]
pub mod async_lzh;

#[cfg(feature = "async-io")]
pub mod async_tar;

#[cfg(feature = "async-io")]
pub mod async_zip;

// Re-exports
pub use brotli::{BrotliReader, BrotliWriter};
pub use bzip2::{Bzip2Reader, Bzip2Writer};
pub use cab::CabReader;
pub use detect::ArchiveFormat;
pub use gzip::{GzipHeader, GzipReader};
pub use lenient::{LenientWarning, LenientWarningKind};
pub use lz4::{Lz4Reader, Lz4Writer};
pub use lzh::{
    LzhCompressionLevel, LzhHeader, LzhReader, LzhStreamEntry, LzhStreamReader, LzhWriter,
};
pub use sevenz::{SevenZEntry, SevenZReader};
pub use snappy::{SnappyReader, SnappyWriter};
pub use tar::{TarHeader, TarReader, TarStreamEntry, TarStreamReader, TarWriter};
pub use xz::{XzReader, XzWriter};
pub use zip::{
    LocalFileHeader, ZipCompressionLevel, ZipReader, ZipStreamEntry, ZipStreamEntryMeta,
    ZipStreamReader, ZipWriter,
};
pub use zstd::{ZstdReader, ZstdWriter};

// Optional feature re-exports

/// Memory-mapped ZIP reader convenience function.
/// Open a ZIP archive via memory-mapped I/O.
#[cfg(feature = "mmap")]
pub use zip::open_zip_mmap;

/// Memory-mapped TAR reader convenience function.
/// Open a TAR archive via memory-mapped I/O.
#[cfg(feature = "mmap")]
pub use tar::open_tar_mmap;

/// Memory-mapped LZH reader convenience function.
/// Open a LZH archive via memory-mapped I/O.
#[cfg(feature = "mmap")]
pub use lzh::open_lzh_mmap;

/// Re-export MmapReader when the mmap feature is enabled.
#[cfg(feature = "mmap")]
pub use oxiarc_core::mmap::{MmapOptions, MmapReader};

/// Async TAR entry reading functions.
#[cfg(feature = "async-io")]
pub use async_tar::{read_tar_entries_async, read_tar_entry_async};

/// Async LZH entry reading functions.
#[cfg(feature = "async-io")]
pub use async_lzh::{read_lzh_entries_async, read_lzh_entry_async};

/// Async ZIP entry reading functions.
#[cfg(feature = "async-io")]
pub use async_zip::{
    decompress_zip_entry_async, read_zip_entry_async, read_zip_entry_from_reader_async,
};
