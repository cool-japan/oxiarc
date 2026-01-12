//! Core traits for compression and archive operations.
//!
//! This module defines the fundamental traits that all compression algorithms
//! and archive handlers must implement.

use crate::entry::Entry;
use crate::error::Result;
use std::io::{Read, Write};

/// Status of a streaming decompression operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompressStatus {
    /// More input is needed to continue decompression.
    NeedsInput,
    /// More output buffer space is needed.
    NeedsOutput,
    /// Decompression is complete.
    Done,
    /// A block boundary was reached (caller may want to check CRC, etc.).
    BlockEnd,
}

/// Status of a streaming compression operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressStatus {
    /// More input data can be accepted.
    NeedsInput,
    /// More output buffer space is needed.
    NeedsOutput,
    /// Compression is complete.
    Done,
}

/// Flush mode for compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlushMode {
    /// No flush - buffer data for best compression.
    #[default]
    None,
    /// Sync flush - emit all pending output.
    Sync,
    /// Full flush - emit and reset encoder state.
    Full,
    /// Finish - complete the stream.
    Finish,
}

/// A streaming decompressor (decoder).
///
/// This trait is implemented by all decompression algorithms (Deflate, LZSS, etc.).
/// It provides a streaming interface that can process data in chunks.
pub trait Decompressor {
    /// Decompress data from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Input compressed data
    /// * `output` - Output buffer for decompressed data
    ///
    /// # Returns
    ///
    /// A tuple of (bytes consumed from input, bytes written to output, status)
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)>;

    /// Reset the decompressor to its initial state.
    fn reset(&mut self);

    /// Check if the decompressor has finished.
    fn is_finished(&self) -> bool;

    /// Decompress all data at once (convenience method).
    fn decompress_all(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut input_pos = 0;
        let mut buffer = vec![0u8; 32768];

        loop {
            let (consumed, produced, status) = self.decompress(&input[input_pos..], &mut buffer)?;

            input_pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput if input_pos >= input.len() => break,
                DecompressStatus::NeedsOutput | DecompressStatus::NeedsInput => continue,
                DecompressStatus::BlockEnd => continue,
            }
        }

        Ok(output)
    }
}

/// A streaming compressor (encoder).
///
/// This trait is implemented by all compression algorithms.
pub trait Compressor {
    /// Compress data from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Input data to compress
    /// * `output` - Output buffer for compressed data
    /// * `flush` - Flush mode
    ///
    /// # Returns
    ///
    /// A tuple of (bytes consumed from input, bytes written to output, status)
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)>;

    /// Reset the compressor to its initial state.
    fn reset(&mut self);

    /// Check if the compressor has finished.
    fn is_finished(&self) -> bool;

    /// Compress all data at once (convenience method).
    fn compress_all(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut input_pos = 0;
        let mut buffer = vec![0u8; 32768];

        // Compress data
        loop {
            let flush = if input_pos >= input.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };

            let (consumed, produced, status) =
                self.compress(&input[input_pos..], &mut buffer, flush)?;

            input_pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            match status {
                CompressStatus::Done => break,
                CompressStatus::NeedsInput if input_pos >= input.len() => {
                    // Final flush
                    let (_, produced, status) =
                        self.compress(&[], &mut buffer, FlushMode::Finish)?;
                    output.extend_from_slice(&buffer[..produced]);
                    if status == CompressStatus::Done {
                        break;
                    }
                }
                _ => continue,
            }
        }

        Ok(output)
    }
}

/// An archive reader that can list and extract entries.
///
/// This trait is implemented by archive format handlers (ZIP, TAR, LZH, etc.).
pub trait ArchiveReader {
    /// Get the list of entries in the archive.
    fn entries(&mut self) -> Result<Vec<Entry>>;

    /// Extract a specific entry by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name/path of the entry to extract
    /// * `writer` - Where to write the extracted data
    ///
    /// # Returns
    ///
    /// The number of bytes written.
    fn extract_by_name<W: Write>(&mut self, name: &str, writer: &mut W) -> Result<u64>;

    /// Extract a specific entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to extract
    /// * `writer` - Where to write the extracted data
    ///
    /// # Returns
    ///
    /// The number of bytes written.
    fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64>;

    /// Get an entry by name.
    fn entry_by_name(&mut self, name: &str) -> Result<Option<Entry>> {
        let entries = self.entries()?;
        Ok(entries.into_iter().find(|e| e.name == name))
    }
}

/// An archive writer that can create archives.
pub trait ArchiveWriter {
    /// Add an entry to the archive.
    ///
    /// # Arguments
    ///
    /// * `entry` - Metadata for the entry
    /// * `data` - The data to write
    fn add_entry<R: Read>(&mut self, entry: &Entry, data: &mut R) -> Result<()>;

    /// Add a file from disk.
    ///
    /// # Arguments
    ///
    /// * `name` - Name/path in the archive
    /// * `path` - Path to the file on disk
    fn add_file(&mut self, name: &str, path: &std::path::Path) -> Result<()>;

    /// Finalize the archive.
    fn finish(&mut self) -> Result<()>;
}

/// Compression level for algorithms that support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionLevel(u8);

impl CompressionLevel {
    /// No compression (store only).
    pub const NONE: Self = Self(0);
    /// Fastest compression.
    pub const FAST: Self = Self(1);
    /// Default compression (balanced).
    pub const DEFAULT: Self = Self(6);
    /// Best compression (slowest).
    pub const BEST: Self = Self(9);

    /// Create a custom compression level (0-9).
    pub fn new(level: u8) -> Self {
        Self(level.min(9))
    }

    /// Get the level value.
    pub fn level(&self) -> u8 {
        self.0
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<u8> for CompressionLevel {
    fn from(level: u8) -> Self {
        Self::new(level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_level() {
        assert_eq!(CompressionLevel::NONE.level(), 0);
        assert_eq!(CompressionLevel::FAST.level(), 1);
        assert_eq!(CompressionLevel::DEFAULT.level(), 6);
        assert_eq!(CompressionLevel::BEST.level(), 9);

        // Test clamping
        assert_eq!(CompressionLevel::new(100).level(), 9);
    }

    #[test]
    fn test_flush_mode_default() {
        assert_eq!(FlushMode::default(), FlushMode::None);
    }
}
