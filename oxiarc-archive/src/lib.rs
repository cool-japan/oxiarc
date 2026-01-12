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

pub mod bzip2;
pub mod cab;
pub mod detect;
pub mod gzip;
pub mod lz4;
pub mod lzh;
pub mod sevenz;
pub mod tar;
pub mod xz;
pub mod zip;
pub mod zstd;

// Re-exports
pub use bzip2::{Bzip2Reader, Bzip2Writer};
pub use cab::CabReader;
pub use detect::ArchiveFormat;
pub use gzip::{GzipHeader, GzipReader};
pub use lz4::{Lz4Reader, Lz4Writer};
pub use lzh::{LzhCompressionLevel, LzhHeader, LzhReader, LzhWriter};
pub use sevenz::{SevenZEntry, SevenZReader};
pub use tar::{TarHeader, TarReader, TarWriter};
pub use xz::{XzReader, XzWriter};
pub use zip::{LocalFileHeader, ZipCompressionLevel, ZipReader, ZipWriter};
pub use zstd::{ZstdReader, ZstdWriter};
