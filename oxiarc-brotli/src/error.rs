//! Error types for Brotli operations.

use oxiarc_core::error::OxiArcError;
use std::fmt;
use std::io;

/// Error type for Brotli compression/decompression operations.
#[derive(Debug)]
pub enum BrotliError {
    /// Invalid or corrupted Brotli data.
    CorruptedData(String),
    /// Invalid Huffman code encountered.
    InvalidHuffmanCode(String),
    /// Invalid backward reference distance.
    InvalidDistance {
        /// The invalid distance value.
        distance: usize,
        /// Maximum allowed distance at this point.
        max_distance: usize,
    },
    /// Invalid parameter value.
    InvalidParameter(String),
    /// Unexpected end of input.
    UnexpectedEof,
    /// Output size exceeded expected limit.
    OutputTooLarge(usize),
    /// I/O error.
    Io(io::Error),
    /// Invalid window size.
    InvalidWindowSize(u32),
    /// Invalid block type.
    InvalidBlockType(u8),
    /// Dictionary reference error.
    DictionaryError(String),
    /// Invalid context map.
    InvalidContextMap(String),
    /// Invalid prefix code.
    InvalidPrefixCode(String),
}

impl fmt::Display for BrotliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrotliError::CorruptedData(msg) => write!(f, "corrupted Brotli data: {msg}"),
            BrotliError::InvalidHuffmanCode(msg) => write!(f, "invalid Huffman code: {msg}"),
            BrotliError::InvalidDistance {
                distance,
                max_distance,
            } => write!(
                f,
                "invalid backward reference distance: {distance} exceeds max {max_distance}"
            ),
            BrotliError::InvalidParameter(msg) => write!(f, "invalid parameter: {msg}"),
            BrotliError::UnexpectedEof => write!(f, "unexpected end of Brotli stream"),
            BrotliError::OutputTooLarge(size) => {
                write!(f, "output size {size} exceeds limit")
            }
            BrotliError::Io(err) => write!(f, "I/O error: {err}"),
            BrotliError::InvalidWindowSize(size) => write!(f, "invalid window size: {size}"),
            BrotliError::InvalidBlockType(bt) => write!(f, "invalid block type: {bt}"),
            BrotliError::DictionaryError(msg) => write!(f, "dictionary error: {msg}"),
            BrotliError::InvalidContextMap(msg) => write!(f, "invalid context map: {msg}"),
            BrotliError::InvalidPrefixCode(msg) => write!(f, "invalid prefix code: {msg}"),
        }
    }
}

impl std::error::Error for BrotliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BrotliError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for BrotliError {
    fn from(err: io::Error) -> Self {
        BrotliError::Io(err)
    }
}

impl From<BrotliError> for io::Error {
    fn from(err: BrotliError) -> Self {
        match err {
            BrotliError::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

impl From<BrotliError> for OxiArcError {
    fn from(err: BrotliError) -> Self {
        match err {
            BrotliError::Io(e) => OxiArcError::Io(e),
            BrotliError::UnexpectedEof => OxiArcError::unexpected_eof(0),
            BrotliError::InvalidDistance {
                distance,
                max_distance,
            } => OxiArcError::invalid_distance(distance, max_distance),
            BrotliError::InvalidHuffmanCode(msg) => OxiArcError::corrupted(0, msg),
            other => OxiArcError::corrupted(0, other.to_string()),
        }
    }
}

/// Result type alias for Brotli operations.
pub type BrotliResult<T> = Result<T, BrotliError>;
