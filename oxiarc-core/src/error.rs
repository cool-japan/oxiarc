//! Error types for OxiArc operations.
//!
//! This module provides a comprehensive error type that covers all possible
//! error conditions in archive operations, including I/O errors, format
//! validation errors, and decompression errors.

use std::io;
use thiserror::Error;

/// The main error type for OxiArc operations.
#[derive(Debug, Error)]
pub enum OxiArcError {
    /// I/O error from underlying reader/writer.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Invalid magic number in archive header.
    #[error("Invalid magic number: expected {expected:02x?}, found {found:02x?}")]
    InvalidMagic {
        /// Expected magic bytes.
        expected: Vec<u8>,
        /// Actual magic bytes found.
        found: Vec<u8>,
    },

    /// Unsupported compression method.
    #[error("Unsupported compression method: {method}")]
    UnsupportedMethod {
        /// The compression method identifier.
        method: String,
    },

    /// CRC checksum mismatch.
    #[error("CRC mismatch: expected {expected:#x}, computed {computed:#x}")]
    CrcMismatch {
        /// Expected CRC value from archive.
        expected: u32,
        /// Computed CRC value from data.
        computed: u32,
    },

    /// Invalid Huffman code encountered during decompression.
    #[error("Invalid Huffman code at bit position {bit_position}")]
    InvalidHuffmanCode {
        /// Bit position where the invalid code was found.
        bit_position: u64,
    },

    /// Corrupted data in archive.
    #[error("Corrupted data at offset {offset}: {message}")]
    CorruptedData {
        /// Byte offset where corruption was detected.
        offset: u64,
        /// Description of the corruption.
        message: String,
    },

    /// Invalid header format.
    #[error("Invalid header: {message}")]
    InvalidHeader {
        /// Description of the header error.
        message: String,
    },

    /// Unexpected end of file.
    #[error("Unexpected end of file: expected {expected} more bytes")]
    UnexpectedEof {
        /// Number of bytes that were expected but not available.
        expected: usize,
    },

    /// Buffer too small for operation.
    #[error("Buffer too small: need {needed} bytes, have {available}")]
    BufferTooSmall {
        /// Number of bytes needed.
        needed: usize,
        /// Number of bytes available.
        available: usize,
    },

    /// Invalid distance in LZ77/LZSS back-reference.
    #[error("Invalid back-reference distance: {distance} exceeds history size {history_size}")]
    InvalidDistance {
        /// The invalid distance value.
        distance: usize,
        /// Current history buffer size.
        history_size: usize,
    },

    /// Path traversal attack detected (e.g., "../" in filename).
    #[error("Path traversal detected in entry: {path}")]
    PathTraversal {
        /// The suspicious path.
        path: String,
    },

    /// Archive appears to be a zip bomb.
    #[error("Potential zip bomb: compression ratio {ratio:.1}x exceeds threshold {threshold}x")]
    ZipBomb {
        /// Detected compression ratio.
        ratio: f64,
        /// Maximum allowed ratio.
        threshold: f64,
    },

    /// Entry not found in archive.
    #[error("Entry not found: {name}")]
    EntryNotFound {
        /// Name of the missing entry.
        name: String,
    },

    /// Encoding error (e.g., invalid Shift_JIS).
    #[error("Encoding error: {message}")]
    EncodingError {
        /// Description of the encoding error.
        message: String,
    },
}

/// Result type alias for OxiArc operations.
pub type Result<T> = std::result::Result<T, OxiArcError>;

impl OxiArcError {
    /// Create an invalid magic error.
    pub fn invalid_magic(expected: impl Into<Vec<u8>>, found: impl Into<Vec<u8>>) -> Self {
        Self::InvalidMagic {
            expected: expected.into(),
            found: found.into(),
        }
    }

    /// Create an unsupported method error.
    pub fn unsupported_method(method: impl Into<String>) -> Self {
        Self::UnsupportedMethod {
            method: method.into(),
        }
    }

    /// Create a CRC mismatch error.
    pub fn crc_mismatch(expected: u32, computed: u32) -> Self {
        Self::CrcMismatch { expected, computed }
    }

    /// Create an invalid Huffman code error.
    pub fn invalid_huffman(bit_position: u64) -> Self {
        Self::InvalidHuffmanCode { bit_position }
    }

    /// Create a corrupted data error.
    pub fn corrupted(offset: u64, message: impl Into<String>) -> Self {
        Self::CorruptedData {
            offset,
            message: message.into(),
        }
    }

    /// Create an invalid header error.
    pub fn invalid_header(message: impl Into<String>) -> Self {
        Self::InvalidHeader {
            message: message.into(),
        }
    }

    /// Create an unexpected EOF error.
    pub fn unexpected_eof(expected: usize) -> Self {
        Self::UnexpectedEof { expected }
    }

    /// Create a buffer too small error.
    pub fn buffer_too_small(needed: usize, available: usize) -> Self {
        Self::BufferTooSmall { needed, available }
    }

    /// Create an invalid distance error.
    pub fn invalid_distance(distance: usize, history_size: usize) -> Self {
        Self::InvalidDistance {
            distance,
            history_size,
        }
    }

    /// Create a path traversal error.
    pub fn path_traversal(path: impl Into<String>) -> Self {
        Self::PathTraversal { path: path.into() }
    }

    /// Create a zip bomb error.
    pub fn zip_bomb(ratio: f64, threshold: f64) -> Self {
        Self::ZipBomb { ratio, threshold }
    }

    /// Create an entry not found error.
    pub fn entry_not_found(name: impl Into<String>) -> Self {
        Self::EntryNotFound { name: name.into() }
    }

    /// Create an encoding error.
    pub fn encoding_error(message: impl Into<String>) -> Self {
        Self::EncodingError {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = OxiArcError::invalid_magic(vec![0x50, 0x4B], vec![0x1F, 0x8B]);
        assert!(err.to_string().contains("Invalid magic"));

        let err = OxiArcError::crc_mismatch(0x12345678, 0xDEADBEEF);
        assert!(err.to_string().contains("CRC mismatch"));

        let err = OxiArcError::unsupported_method("-lh9-");
        assert!(err.to_string().contains("-lh9-"));
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: OxiArcError = io_err.into();
        assert!(matches!(err, OxiArcError::Io(_)));
    }
}
