//! Bzip2 file support.
//!
//! This module provides reading and writing of Bzip2 (.bz2) compressed files.
//! Bzip2 is a compression-only format (single file, no archive structure).
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::bzip2::Bzip2Reader;
//! use std::fs::File;
//!
//! let file = File::open("data.bz2").unwrap();
//! let mut reader = Bzip2Reader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_bzip2::{BzDecoder, BzEncoder};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::Read;

/// Bzip2 magic bytes ("BZh").
pub const BZIP2_MAGIC: [u8; 3] = [0x42, 0x5A, 0x68];

/// Bzip2 file reader.
pub struct Bzip2Reader {
    /// Buffered data.
    data: Vec<u8>,
    /// Block size from header (1-9).
    block_size_level: u8,
    /// Optional progress sink forwarded to the underlying BzDecoder.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token forwarded to the underlying BzDecoder.
    cancel: Option<CancellationToken>,
}

impl Bzip2Reader {
    /// Create a new Bzip2 reader.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        // Read all data
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        // Verify magic
        if data.len() < 4 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "file too short for Bzip2".to_string(),
            });
        }

        // Check "BZ" prefix
        if data[0..2] != [0x42, 0x5A] {
            return Err(OxiArcError::invalid_magic([0x42, 0x5A], &data[0..2]));
        }

        // Check 'h' for huffman coding
        if data[2] != 0x68 {
            return Err(OxiArcError::CorruptedData {
                offset: 2,
                message: format!(
                    "invalid bzip2 version byte: expected 0x68 ('h'), found 0x{:02x}",
                    data[2]
                ),
            });
        }

        // Get block size level (ASCII '1'-'9')
        let block_size_char = data[3];
        if !(b'1'..=b'9').contains(&block_size_char) {
            return Err(OxiArcError::CorruptedData {
                offset: 3,
                message: format!(
                    "invalid bzip2 block size: expected '1'-'9', found 0x{:02x}",
                    block_size_char
                ),
            });
        }

        let block_size_level = block_size_char - b'0';

        Ok(Self {
            data,
            block_size_level,
            progress: None,
            cancel: None,
        })
    }

    /// Attach a progress sink forwarded to the underlying BzDecoder.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token forwarded to the underlying BzDecoder.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Get the block size level (1-9).
    pub fn block_size_level(&self) -> u8 {
        self.block_size_level
    }

    /// Get the block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size_level as usize * 100_000
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        // Fast-path when no hooks are attached: keep using the free function
        // so that output/behaviour is unchanged from before this feature.
        if self.progress.is_none() && self.cancel.is_none() {
            return oxiarc_bzip2::decompress(&self.data[..]);
        }

        let mut decoder = BzDecoder::new(&self.data[..])?;
        if let Some(handle) = self.progress.clone() {
            decoder = decoder.with_progress(handle);
        }
        if let Some(token) = self.cancel.clone() {
            decoder = decoder.with_cancel(token);
        }
        let mut output = Vec::new();
        while let Some(block) = decoder.read_block()? {
            output.extend_from_slice(&block);
        }
        Ok(output)
    }
}

/// Bzip2 file writer.
pub struct Bzip2Writer {
    /// Compression level (1-9).
    level: oxiarc_bzip2::CompressionLevel,
    /// Optional progress sink forwarded to the underlying BzEncoder.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token forwarded to the underlying BzEncoder.
    cancel: Option<CancellationToken>,
}

impl Bzip2Writer {
    /// Create a new Bzip2 writer with default compression level (9).
    pub fn new() -> Self {
        Self {
            level: oxiarc_bzip2::CompressionLevel::default(),
            progress: None,
            cancel: None,
        }
    }

    /// Create a new Bzip2 writer with specified compression level.
    pub fn with_level(level: u8) -> Self {
        Self {
            level: oxiarc_bzip2::CompressionLevel::new(level),
            progress: None,
            cancel: None,
        }
    }

    /// Set compression level (1-9).
    pub fn set_level(&mut self, level: u8) {
        self.level = oxiarc_bzip2::CompressionLevel::new(level);
    }

    /// Attach a progress sink forwarded to the underlying BzEncoder.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token forwarded to the underlying BzEncoder.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Compress data.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        // Fast-path when no hooks are attached so output bytes match the
        // free function verbatim.
        if self.progress.is_none() && self.cancel.is_none() {
            return oxiarc_bzip2::compress(data, self.level);
        }

        let block_size = self.level.block_size();
        let output = Vec::new();
        let mut encoder = BzEncoder::new(output, self.level)?;
        if let Some(handle) = self.progress.clone() {
            encoder = encoder.with_progress(handle);
        }
        if let Some(token) = self.cancel.clone() {
            encoder = encoder.with_cancel(token);
        }

        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + block_size).min(data.len());
            encoder.write_block(&data[offset..end])?;
            offset = end;
        }
        encoder.finish()
    }
}

impl Default for Bzip2Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Bzip2 data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_bzip2::decompress(data)
}

/// Decompress from a reader.
pub fn decompress_reader<R: Read>(reader: R) -> Result<Vec<u8>> {
    oxiarc_bzip2::decompress(reader)
}

/// Compress data with default level.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_bzip2::compress(data, oxiarc_bzip2::CompressionLevel::default())
}

/// Compress data with specified level.
pub fn compress_with_level(data: &[u8], level: u8) -> Result<Vec<u8>> {
    oxiarc_bzip2::compress(data, oxiarc_bzip2::CompressionLevel::new(level))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_bzip2_magic() {
        assert_eq!(BZIP2_MAGIC, [0x42, 0x5A, 0x68]); // "BZh"
    }

    #[test]
    fn test_bzip2_invalid_magic() {
        let bad_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(&bad_data);
        let result = Bzip2Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_bzip2_too_short() {
        let short_data = [0x42, 0x5A];
        let cursor = Cursor::new(&short_data);
        let result = Bzip2Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_bzip2_roundtrip() {
        let original = b"Hello, Bzip2 world!";

        // Compress
        let compressed = compress(original).expect("compress");
        assert!(!compressed.is_empty());

        // Decompress via reader
        let cursor = Cursor::new(&compressed);
        let mut reader = Bzip2Reader::new(cursor).expect("Bzip2Reader::new");

        assert_eq!(reader.block_size_level(), 9); // Default level
        assert_eq!(reader.block_size(), 900_000);

        let decompressed = reader.decompress().expect("decompress");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_bzip2_writer_levels() {
        let data = b"test data for compression";

        for level in 1..=9 {
            let writer = Bzip2Writer::with_level(level);
            let compressed = writer.compress(data).expect("compress at level");

            // Verify can decompress
            let decompressed = decompress(&compressed).expect("decompress at level");
            assert_eq!(decompressed, data.as_slice());
        }
    }

    #[test]
    fn test_bzip2_empty() {
        let empty: &[u8] = b"";
        let compressed = compress(empty).expect("compress empty");
        let decompressed = decompress(&compressed).expect("decompress empty");
        assert_eq!(decompressed, empty);
    }

    #[test]
    fn test_bzip2_progress_forwarding() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            last_processed: AtomicU64,
            finish_count: AtomicU64,
        }
        impl ProgressSink for CountingSink {
            fn on_progress(&self, processed: u64, _total: Option<u64>) {
                self.progress_count.fetch_add(1, Ordering::SeqCst);
                self.last_processed.store(processed, Ordering::SeqCst);
            }
            fn on_entry(&self, _name: &str, _index: u64) {}
            fn on_finish(&self) {
                self.finish_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountingSink {
            progress_count: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
            finish_count: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        // 64 KiB fits in a single 100 KiB block (level 1), so we get at
        // least one progress notification + one finish.
        let data = vec![0x42u8; 64 * 1024];
        let writer = Bzip2Writer::with_level(1).with_progress(handle);
        let compressed = writer
            .compress(&data)
            .expect("bzip2 compression with progress should succeed");

        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert!(sink.last_processed.load(Ordering::SeqCst) > 0);

        // Round-trip sanity.
        let roundtrip = decompress(&compressed).expect("roundtrip should succeed");
        assert_eq!(roundtrip, data);
    }

    #[test]
    fn test_bzip2_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;

        let token = CancellationToken::new();
        token.cancel();
        let data = vec![0x55u8; 1024];
        let writer = Bzip2Writer::with_level(1).with_cancel(token);
        let result = writer.compress(&data);
        assert!(result.is_err(), "cancelled bzip2 compress must fail");
    }
}
