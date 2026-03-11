//! Async ZIP entry reading support.
//!
//! This module provides async functions for reading ZIP archive entries
//! using Tokio's async I/O primitives, enabling non-blocking archive operations.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-archive = { version = "0.2.2", features = ["async-io"] }
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_archive::async_zip::read_zip_entry_async;
//! use oxiarc_archive::zip::ZipReader;
//! use std::io::Cursor;
//!
//! // Build a ZipReader synchronously from a Vec<u8> or file, then read entries async
//! let cursor = Cursor::new(zip_bytes);
//! let mut reader = ZipReader::new(cursor).unwrap();
//! let entries = reader.entries().to_vec();
//! let entry = &entries[0];
//!
//! let data = read_zip_entry_async(&mut reader, entry).await.unwrap();
//! ```

use oxiarc_core::Entry;
use oxiarc_core::entry::CompressionMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_deflate::inflate::Inflater;
use std::io::SeekFrom;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

use crate::zip::ZipReader;

/// Decompress a DEFLATE-compressed byte slice synchronously into a `Vec<u8>`.
///
/// This is a helper used by the async entry readers. DEFLATE decompression is
/// inherently CPU-bound and processes the entire input at once, so there is no
/// benefit to an async decompressor: we read all compressed bytes first
/// (asynchronously), then decompress them in a single synchronous call.
fn deflate_decompress(compressed: &[u8], size_hint: usize) -> Result<Vec<u8>> {
    let mut inflater = Inflater::new();
    let mut cursor = std::io::Cursor::new(compressed);
    let raw = inflater.inflate_reader(&mut cursor)?;
    let _ = size_hint; // used only for capacity hints in callers
    Ok(raw)
}

/// Read all bytes of a ZIP entry asynchronously, given a raw byte slice of the
/// compressed entry data and its metadata.
///
/// This function accepts the raw (already-seeked-to) compressed bytes and
/// metadata from a `ZipEntry`, decompresses them asynchronously using Tokio,
/// and returns the decompressed result.
///
/// For practical async ZIP reading, use [`read_zip_entry_async`] which takes
/// a `ZipReader` directly.
///
/// # Arguments
///
/// * `compressed_data` - Raw compressed bytes for this entry
/// * `entry` - The entry metadata (method, sizes, etc.)
///
/// # Returns
///
/// The decompressed entry data.
///
/// # Errors
///
/// Returns an error if decompression fails or the compression method is unsupported.
pub async fn decompress_zip_entry_async(compressed_data: &[u8], entry: &Entry) -> Result<Vec<u8>> {
    match entry.method {
        CompressionMethod::Stored => Ok(compressed_data.to_vec()),
        CompressionMethod::Deflate => deflate_decompress(compressed_data, entry.size as usize),
        CompressionMethod::Unknown(method_id) => Err(OxiArcError::unsupported_method(format!(
            "unsupported ZIP compression method: {}",
            method_id
        ))),
        other => Err(OxiArcError::unsupported_method(other.name())),
    }
}

/// Read a ZIP entry's data asynchronously from an async reader.
///
/// This function seeks to the correct position in the async reader,
/// reads the compressed data, and decompresses it asynchronously.
///
/// # Arguments
///
/// * `reader` - An async reader positioned anywhere; this function will seek
///   to the entry's data offset automatically.
/// * `entry` - The entry to read (obtained from [`ZipReader::entries()`]).
///
/// # Returns
///
/// The decompressed entry data as a `Vec<u8>`.
///
/// # Errors
///
/// Returns an error if seeking, reading, or decompression fails.
pub async fn read_zip_entry_async<R>(reader: &mut R, entry: &Entry) -> Result<Vec<u8>>
where
    R: AsyncRead + AsyncSeek + Unpin,
{
    // Seek to the start of the compressed data
    reader.seek(SeekFrom::Start(entry.offset)).await?;

    let compressed_size = entry.compressed_size as usize;

    match entry.method {
        CompressionMethod::Stored => {
            let mut buf = vec![0u8; compressed_size];
            reader.read_exact(&mut buf).await?;
            Ok(buf)
        }
        CompressionMethod::Deflate => {
            // Read compressed bytes asynchronously, then decompress synchronously.
            // DEFLATE decompression processes the whole input at once, so there is
            // no benefit to streaming the decompression step.
            let mut compressed = vec![0u8; compressed_size];
            reader.read_exact(&mut compressed).await?;
            deflate_decompress(&compressed, entry.size as usize)
        }
        CompressionMethod::Unknown(method_id) => Err(OxiArcError::unsupported_method(format!(
            "unsupported ZIP compression method: {}",
            method_id
        ))),
        other => Err(OxiArcError::unsupported_method(other.name())),
    }
}

/// Read a ZIP entry asynchronously from a `ZipReader`.
///
/// This is the primary high-level API for async ZIP entry reading.
/// It extracts the compressed data using the synchronous `ZipReader`'s
/// `extract_raw` mechanism and then decompresses it asynchronously.
///
/// # Arguments
///
/// * `zip_reader` - A mutable reference to an initialized `ZipReader`
/// * `entry` - The entry to read (cloned from `zip_reader.entries()`)
///
/// # Returns
///
/// The decompressed entry data.
///
/// # Errors
///
/// Returns an error if reading or decompression fails.
pub async fn read_zip_entry_from_reader_async<R>(
    zip_reader: &mut ZipReader<R>,
    entry: &Entry,
) -> Result<Vec<u8>>
where
    R: std::io::Read + std::io::Seek,
{
    // Extract raw compressed data via the synchronous reader
    let raw = zip_reader.extract_raw(entry)?;
    decompress_zip_entry_async(&raw, entry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::{ZipReader, ZipWriter};
    use std::io::Cursor;

    /// Helper: build a ZIP in memory with multiple entries.
    fn build_test_zip() -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer
                .add_file("hello.txt", b"Hello, async world!")
                .expect("add_file failed");
            let repeated = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(100);
            writer
                .add_file("repeat.txt", repeated.as_slice())
                .expect("add_file failed");
            writer.finish().expect("finish failed");
        }
        output
    }

    #[tokio::test]
    async fn test_async_zip_read_stored() {
        let zip_bytes = build_test_zip();

        let cursor = Cursor::new(zip_bytes.clone());
        let mut reader = ZipReader::new(cursor).expect("ZipReader::new failed");
        let entries = reader.entries().to_vec();

        let entry = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt not found");

        // Use decompress_zip_entry_async with pre-read raw data
        let raw = reader.extract_raw(entry).expect("extract_raw failed");
        let data = decompress_zip_entry_async(&raw, entry)
            .await
            .expect("async decompress failed");

        assert_eq!(data, b"Hello, async world!");
    }

    #[tokio::test]
    async fn test_async_zip_read_deflated() {
        let zip_bytes = build_test_zip();

        let cursor = Cursor::new(zip_bytes.clone());
        let mut reader = ZipReader::new(cursor).expect("ZipReader::new failed");
        let entries = reader.entries().to_vec();

        let entry = entries
            .iter()
            .find(|e| e.name == "repeat.txt")
            .expect("repeat.txt not found");

        let raw = reader.extract_raw(entry).expect("extract_raw failed");
        let data = decompress_zip_entry_async(&raw, entry)
            .await
            .expect("async decompress failed");

        let expected: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(100).to_vec();
        assert_eq!(data, expected);
    }

    #[tokio::test]
    async fn test_read_zip_entry_from_reader_async() {
        let zip_bytes = build_test_zip();

        let cursor = Cursor::new(zip_bytes.clone());
        let mut reader = ZipReader::new(cursor).expect("ZipReader::new failed");
        let entries = reader.entries().to_vec();

        for entry in &entries {
            let data = read_zip_entry_from_reader_async(&mut reader, entry)
                .await
                .expect("read_zip_entry_from_reader_async failed");
            // Just verify we got non-zero data for non-empty entries
            if !entry.is_dir() && entry.size > 0 {
                assert!(!data.is_empty(), "entry {} returned empty data", entry.name);
            }
        }
    }

    #[tokio::test]
    async fn test_read_zip_entry_async_with_cursor() {
        // Test read_zip_entry_async using a tokio-wrapped std::io::Cursor
        // (tokio::io::Cursor is compatible with AsyncRead + AsyncSeek)
        let zip_bytes = build_test_zip();

        // Get entry metadata via sync reader
        let sync_cursor = Cursor::new(zip_bytes.clone());
        let reader = ZipReader::new(sync_cursor).expect("ZipReader::new failed");
        let entries = reader.entries().to_vec();

        // Use a tokio-wrapped cursor for async reads
        let mut async_cursor = tokio::io::BufReader::new(Cursor::new(zip_bytes.clone()));

        for entry in &entries {
            if entry.is_dir() {
                continue;
            }
            let data = read_zip_entry_async(&mut async_cursor, entry)
                .await
                .expect("read_zip_entry_async failed");
            if entry.size > 0 {
                assert!(!data.is_empty(), "entry {} returned empty data", entry.name);
            }
        }
    }
}
