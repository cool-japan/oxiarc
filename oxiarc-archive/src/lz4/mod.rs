//! LZ4 file support.
//!
//! This module provides reading and writing of LZ4 compressed files.
//! LZ4 is a compression-only format (single file, no archive structure).
//! Supports the official LZ4 frame format (RFC).
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::lz4::Lz4Reader;
//! use std::fs::File;
//!
//! let file = File::open("data.lz4").unwrap();
//! let mut reader = Lz4Reader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_lz4::{compress, decompress};
use std::io::{Read, Write};

/// LZ4 frame magic number.
const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];

/// LZ4 file reader.
///
/// Supports the official LZ4 frame format with header checksum,
/// content size, and content checksum.
///
/// Progress/cancellation is emitted by the wrapper itself (the underlying
/// `oxiarc-lz4` crate does not yet expose builder hooks). Granularity is
/// therefore one-shot: a single `on_progress` call after decompression
/// completes, followed by `on_finish`.
pub struct Lz4Reader<R: Read> {
    reader: R,
    header: Vec<u8>,
    content_size: Option<u64>,
    /// Optional progress sink (wrapper-emitted, one-shot).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before decompression.
    cancel: Option<CancellationToken>,
}

impl<R: Read> Lz4Reader<R> {
    /// Create a new LZ4 reader.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read and verify magic
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;

        if magic != LZ4_MAGIC {
            return Err(OxiArcError::invalid_magic(LZ4_MAGIC, magic));
        }

        // Read FLG and BD bytes
        let mut flg_bd = [0u8; 2];
        reader.read_exact(&mut flg_bd)?;

        let flg = flg_bd[0];
        let has_content_size = (flg & 0x08) != 0;

        // Build header
        let mut header = Vec::with_capacity(15);
        header.extend_from_slice(&magic);
        header.extend_from_slice(&flg_bd);

        // Read content size if present
        let content_size = if has_content_size {
            let mut size_bytes = [0u8; 8];
            reader.read_exact(&mut size_bytes)?;
            header.extend_from_slice(&size_bytes);
            Some(u64::from_le_bytes(size_bytes))
        } else {
            None
        };

        // Read header checksum
        let mut hc = [0u8; 1];
        reader.read_exact(&mut hc)?;
        header.extend_from_slice(&hc);

        Ok(Self {
            reader,
            header,
            content_size,
            progress: None,
            cancel: None,
        })
    }

    /// Attach a progress sink. Emitted once after decompression completes.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before decompression begins.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Get the original (uncompressed) size from the header.
    ///
    /// Returns None if content size was not included in the frame header.
    pub fn original_size(&self) -> Option<u64> {
        self.content_size
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Read remaining data
        let mut compressed = Vec::new();
        self.reader.read_to_end(&mut compressed)?;

        // Reconstruct full frame
        let mut frame = self.header.clone();
        frame.extend_from_slice(&compressed);

        // Determine max output size
        let max_output = self.content_size.unwrap_or(64 * 1024 * 1024) as usize;

        // Decompress
        let output = decompress(&frame, max_output * 2)?;

        if let Some(ref handle) = self.progress {
            let total = output.len() as u64;
            handle.on_progress(total, Some(total));
            handle.on_finish();
        }

        Ok(output)
    }
}

/// LZ4 file writer.
///
/// Progress/cancellation is emitted by the wrapper itself (the underlying
/// `oxiarc-lz4` crate does not yet expose builder hooks). Granularity is
/// therefore one-shot per `write_compressed` call: a single `on_progress`
/// after a block is written, plus `on_finish` when the writer is dropped via
/// [`Lz4Writer::into_inner`].
pub struct Lz4Writer<W: Write> {
    writer: W,
    /// Optional progress sink (wrapper-emitted).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before each compressed write.
    cancel: Option<CancellationToken>,
    /// Cumulative uncompressed bytes successfully written so far.
    bytes_processed: u64,
}

impl<W: Write> Lz4Writer<W> {
    /// Create a new LZ4 writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            progress: None,
            cancel: None,
            bytes_processed: 0,
        }
    }

    /// Attach a progress sink. Notified once per successful
    /// [`Lz4Writer::write_compressed`] call with the cumulative uncompressed
    /// byte count.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked at the start of each
    /// [`Lz4Writer::write_compressed`] call.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Compress and write data.
    pub fn write_compressed(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }
        let compressed = compress(data)?;
        self.writer.write_all(&compressed)?;
        self.bytes_processed = self.bytes_processed.saturating_add(data.len() as u64);
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }
        Ok(())
    }

    /// Get the inner writer, notifying the progress sink that the stream is
    /// complete.
    pub fn into_inner(self) -> W {
        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }
        self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_lz4_roundtrip() {
        let data = b"Hello, LZ4! This is a test of the LZ4 compression format.";

        // Compress
        let mut output = Vec::new();
        {
            let mut writer = Lz4Writer::new(&mut output);
            writer.write_compressed(data).unwrap();
        }

        // Decompress
        let cursor = Cursor::new(&output);
        let mut reader = Lz4Reader::new(cursor).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_lz4_original_size() {
        let data = b"Test data for size check";
        let compressed = compress(data).unwrap();

        let cursor = Cursor::new(&compressed);
        let reader = Lz4Reader::new(cursor).unwrap();

        // Content size is now optional in the frame format
        assert_eq!(reader.original_size(), Some(data.len() as u64));
    }

    #[test]
    fn test_lz4_invalid_magic() {
        let bad_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(&bad_data);
        let result = Lz4Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_lz4_repeated_pattern() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

        let mut output = Vec::new();
        {
            let mut writer = Lz4Writer::new(&mut output);
            writer.write_compressed(data).unwrap();
        }

        // Verify compression happened
        assert!(output.len() < data.len());

        let cursor = Cursor::new(&output);
        let mut reader = Lz4Reader::new(cursor).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_lz4_progress_forwarding() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            finish_count: AtomicU64,
            last_processed: AtomicU64,
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
            finish_count: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        let data = vec![0x42u8; 64 * 1024];
        let mut output = Vec::new();
        {
            let mut writer = Lz4Writer::new(&mut output).with_progress(handle);
            writer
                .write_compressed(&data)
                .expect("lz4 write_compressed should succeed");
            let _ = writer.into_inner();
        }

        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64
        );

        // Round-trip sanity.
        let cursor = Cursor::new(&output);
        let mut reader = Lz4Reader::new(cursor).expect("reader should construct");
        let decompressed = reader.decompress().expect("decompress should succeed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_lz4_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;

        let token = CancellationToken::new();
        token.cancel();
        let mut output = Vec::new();
        let mut writer = Lz4Writer::new(&mut output).with_cancel(token);
        let result = writer.write_compressed(b"some data");
        assert!(matches!(result, Err(OxiArcError::Cancelled)));
    }
}
