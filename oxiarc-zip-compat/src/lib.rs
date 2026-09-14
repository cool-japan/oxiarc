//! # oxiarc-zip-compat
//!
//! A Pure Rust implementation of the [`zip`](https://docs.rs/zip) crate's
//! public API (the 6.x - 8.x line) over [`oxiarc_archive`]'s ZIP reader and
//! writer, covering what `candle-core` (npy/npz, PyTorch `.pth`) and
//! `ndarray-npy` use:
//!
//! * [`ZipArchive`]: `new`, `len`, `file_names`, `by_index`, `by_name`,
//!   `index_for_name`, `by_index_raw`, `extract`, `into_inner`;
//! * [`read::ZipFile`]: `name`, `size`, `compression`, `is_dir`, ... and
//!   `Read` (stored/deflated entries stream from the underlying reader
//!   with CRC-32 verification);
//! * [`ZipWriter`]: `new`, `start_file`, `Write`, `add_directory`,
//!   `finish`, finalized on drop;
//! * [`write::FileOptions`] / [`write::SimpleFileOptions`],
//!   [`CompressionMethod`], [`DateTime`], [`result::ZipError`].
//!
//! ```
//! use std::io::{Cursor, Read, Write};
//! use oxiarc_zip_compat::{CompressionMethod, ZipArchive, ZipWriter};
//! use oxiarc_zip_compat::write::SimpleFileOptions;
//!
//! let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
//! let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
//! zip.start_file("a.npy", options)?;
//! zip.write_all(b"array bytes")?;
//! let bytes = zip.finish()?.into_inner();
//!
//! let mut archive = ZipArchive::new(Cursor::new(bytes))?;
//! let mut text = String::new();
//! archive.by_name("a.npy")?.read_to_string(&mut text)?;
//! assert_eq!(text, "array bytes");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Differences from zip
//!
//! * Writing supports Stored, Deflated and LZMA; other methods return
//!   [`result::ZipError::UnsupportedArchive`]. Encryption is not supported
//!   in either direction.
//! * Each entry is buffered in memory until the next `start_file` /
//!   `finish` (so sizes and CRC go in the local header); reading streams.
//! * Deflated entries that would not shrink are stored instead.
//! * Archive comments, file comments and extra fields are not written.

#![warn(missing_docs)]

pub mod read;
pub mod result;
mod types;
pub mod write;

pub use crate::read::{HasZipMetadata, ZipArchive, ZipReadOptions};
pub use crate::types::{CompressionMethod, DateTime, SUPPORTED_COMPRESSION_METHODS, System};
pub use crate::write::ZipWriter;

/// The number of bytes above which Zip64 is used (`u32::MAX`).
pub const ZIP64_BYTES_THR: u64 = u32::MAX as u64;
/// The number of entries above which Zip64 is used.
pub const ZIP64_ENTRY_THR: usize = u16::MAX as usize;
