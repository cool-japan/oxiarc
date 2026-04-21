//! Snappy file support.
//!
//! This module provides reading and writing of Snappy compressed files
//! using the framed (streaming) format, which includes the stream identifier
//! magic bytes for format detection.
//!
//! Snappy is a speed-oriented compressor with no compression level settings.
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::snappy::SnappyReader;
//! use std::fs::File;
//!
//! let file = File::open("data.sz").unwrap();
//! let mut reader = SnappyReader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::{Read, Write};

/// Snappy framed format stream identifier (magic bytes).
pub const SNAPPY_MAGIC: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

/// Snappy file reader (framed format).
pub struct SnappyReader {
    /// Buffered compressed data.
    data: Vec<u8>,
    /// Optional progress sink forwarded to the underlying framed decoder.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token forwarded to the underlying framed decoder.
    cancel: Option<CancellationToken>,
}

impl SnappyReader {
    /// Create a new Snappy reader.
    ///
    /// Reads all data and validates the stream identifier magic.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        if data.len() < SNAPPY_MAGIC.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "file too short for Snappy framed format".to_string(),
            });
        }

        if data[..SNAPPY_MAGIC.len()] != SNAPPY_MAGIC {
            return Err(OxiArcError::invalid_magic(
                SNAPPY_MAGIC,
                &data[..SNAPPY_MAGIC.len()],
            ));
        }

        Ok(Self {
            data,
            progress: None,
            cancel: None,
        })
    }

    /// Create a new Snappy reader from raw bytes.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.len() < SNAPPY_MAGIC.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "data too short for Snappy framed format".to_string(),
            });
        }

        if data[..SNAPPY_MAGIC.len()] != SNAPPY_MAGIC {
            return Err(OxiArcError::invalid_magic(
                SNAPPY_MAGIC,
                &data[..SNAPPY_MAGIC.len()],
            ));
        }

        Ok(Self {
            data,
            progress: None,
            cancel: None,
        })
    }

    /// Attach a progress sink forwarded to the underlying Snappy decoder.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token forwarded to the underlying Snappy decoder.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Get the compressed size.
    pub fn compressed_size(&self) -> usize {
        self.data.len()
    }

    /// Decompress the entire file using the framed decoder.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        let mut decoder = oxiarc_snappy::FrameDecoder::new(&self.data[..]);
        if let Some(handle) = self.progress.clone() {
            decoder = decoder.with_progress(handle);
        }
        if let Some(token) = self.cancel.clone() {
            decoder = decoder.with_cancel(token);
        }
        let mut output = Vec::new();
        decoder.read_to_end(&mut output)?;
        Ok(output)
    }
}

/// Snappy file writer (framed format).
///
/// Snappy is speed-oriented and has no compression level settings.
/// Quality parameters are accepted but ignored for API compatibility.
pub struct SnappyWriter {
    /// Optional progress sink forwarded to the underlying framed encoder.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token forwarded to the underlying framed encoder.
    cancel: Option<CancellationToken>,
}

impl SnappyWriter {
    /// Create a new Snappy writer.
    pub fn new() -> Self {
        Self {
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink forwarded to the underlying Snappy encoder.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token forwarded to the underlying Snappy encoder.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Compress data to Snappy framed format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        {
            let mut encoder = oxiarc_snappy::FrameEncoder::new(&mut output);
            if let Some(handle) = self.progress.clone() {
                encoder = encoder.with_progress(handle);
            }
            if let Some(token) = self.cancel.clone() {
                encoder = encoder.with_cancel(token);
            }
            encoder.write_all(data)?;
            encoder.finish().map_err(OxiArcError::Io)?;
        }
        Ok(output)
    }
}

impl Default for SnappyWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Snappy framed data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = oxiarc_snappy::FrameDecoder::new(data);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

/// Compress data to Snappy framed format.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let writer = SnappyWriter::new();
    writer.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_snappy_magic() {
        assert_eq!(SNAPPY_MAGIC[0], 0xFF);
        assert_eq!(&SNAPPY_MAGIC[4..], b"sNaPpY");
    }

    #[test]
    fn test_snappy_roundtrip() {
        let original = b"Hello, Snappy world! This is a test of the Snappy compression format.";

        // Compress
        let compressed = compress(original).expect("compression should succeed");
        assert!(!compressed.is_empty());

        // Verify magic is present
        assert_eq!(&compressed[..SNAPPY_MAGIC.len()], &SNAPPY_MAGIC);

        // Decompress via reader
        let cursor = Cursor::new(&compressed);
        let mut reader = SnappyReader::new(cursor).expect("reader creation should succeed");

        assert!(reader.compressed_size() > 0);

        let decompressed = reader.decompress().expect("decompression should succeed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_snappy_empty_data() {
        let empty: &[u8] = b"";
        let compressed = compress(empty).expect("compressing empty should succeed");
        let decompressed = decompress(&compressed).expect("decompressing empty should succeed");
        assert_eq!(decompressed, empty);
    }

    #[test]
    fn test_snappy_invalid_data() {
        let bad_data = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let cursor = Cursor::new(&bad_data);
        let result = SnappyReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_snappy_reader_too_short() {
        let short: &[u8] = &[0xFF, 0x06];
        let cursor = Cursor::new(short);
        let result = SnappyReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_snappy_magic_detection() {
        use crate::detect::ArchiveFormat;
        let magic = SNAPPY_MAGIC;
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Snappy);
    }

    #[test]
    fn test_snappy_writer_default() {
        let _writer = SnappyWriter::default();
    }

    #[test]
    fn test_snappy_from_bytes() {
        let original = b"Test from_bytes constructor";
        let compressed = compress(original).expect("compression should succeed");

        let mut reader = SnappyReader::from_bytes(compressed).expect("from_bytes should succeed");
        let decompressed = reader.decompress().expect("decompression should succeed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_snappy_large_data() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let compressed = compress(&data).expect("large data compression should succeed");
        let decompressed =
            decompress(&compressed).expect("large data decompression should succeed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_snappy_repeated_data() {
        let data = vec![b'A'; 10_000];
        let compressed = compress(&data).expect("repeated data compression should succeed");
        // Repeated data should compress well
        assert!(compressed.len() < data.len());
        let decompressed =
            decompress(&compressed).expect("repeated data decompression should succeed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_snappy_progress_forwarding() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            last_processed: AtomicU64,
        }
        impl ProgressSink for CountingSink {
            fn on_progress(&self, processed: u64, _total: Option<u64>) {
                self.progress_count.fetch_add(1, Ordering::SeqCst);
                self.last_processed.store(processed, Ordering::SeqCst);
            }
            fn on_entry(&self, _name: &str, _index: u64) {}
            fn on_finish(&self) {}
        }

        let sink = Arc::new(CountingSink {
            progress_count: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        // 64 KiB of data to produce at least one chunk in the framed encoder.
        let data = vec![0x42u8; 64 * 1024];
        let writer = SnappyWriter::new().with_progress(handle);
        let compressed = writer
            .compress(&data)
            .expect("snappy compression with progress should succeed");
        // Check that we saw at least one on_progress call and round-tripped.
        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert!(sink.last_processed.load(Ordering::SeqCst) > 0);

        let roundtrip = decompress(&compressed).expect("roundtrip decompress should succeed");
        assert_eq!(roundtrip, data);
    }

    #[test]
    fn test_snappy_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;

        let token = CancellationToken::new();
        token.cancel();
        let writer = SnappyWriter::new().with_cancel(token);

        // A non-empty write triggers the encoder's internal flush which checks
        // the cancellation token.
        let data = vec![0x55u8; 64 * 1024];
        let result = writer.compress(&data);
        assert!(result.is_err(), "cancelled snappy compress must fail");
    }
}
