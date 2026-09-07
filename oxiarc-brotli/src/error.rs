//! Error types for Brotli operations.

use oxiarc_core::error::OxiArcError;
use std::io;
use thiserror::Error;

/// Error type for Brotli compression/decompression operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BrotliError {
    /// Invalid or corrupted Brotli data.
    #[error("corrupted Brotli data: {0}")]
    CorruptedData(String),
    /// Invalid Huffman code encountered.
    #[error("invalid Huffman code: {0}")]
    InvalidHuffmanCode(String),
    /// Invalid backward reference distance.
    #[error("invalid backward reference distance: {distance} exceeds max {max_distance}")]
    InvalidDistance {
        /// The invalid distance value.
        distance: usize,
        /// Maximum allowed distance at this point.
        max_distance: usize,
    },
    /// Invalid parameter value.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    /// Unexpected end of input.
    #[error("unexpected end of Brotli stream")]
    UnexpectedEof,
    /// Output size exceeded expected limit.
    #[error("output size {0} exceeds limit")]
    OutputTooLarge(usize),
    /// The stream would produce more output than the caller's memory budget.
    ///
    /// Returned only by the bounded entry points
    /// ([`crate::decompress_with_limit`], [`crate::BrotliDecompressor::with_max_output`]).
    /// The check happens *during* decoding — before the over-budget bytes are
    /// produced — so a decompression bomb is rejected without ever being
    /// materialised.
    #[error("memory budget exceeded: budget={budget} bytes, requested={requested} bytes")]
    MemoryBudgetExceeded {
        /// The caller-supplied budget, in bytes.
        budget: usize,
        /// Total output the stream declared it would need, in bytes.
        requested: usize,
    },
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// Invalid window size.
    #[error("invalid window size: {0}")]
    InvalidWindowSize(u32),
    /// The stream declares a sliding window larger than the caller allows.
    ///
    /// Returned by [`crate::BrotliStream::with_max_window`] while reading the
    /// stream header, before any window memory is allocated.
    #[error("declared window of {declared} bytes exceeds the {max}-byte limit")]
    WindowTooLarge {
        /// The window the stream declared, `1 << WBITS`.
        declared: usize,
        /// The caller's ceiling.
        max: usize,
    },
    /// Invalid block type.
    #[error("invalid block type: {0}")]
    InvalidBlockType(u8),
    /// Dictionary reference error.
    #[error("dictionary error: {0}")]
    DictionaryError(String),
    /// Invalid context map.
    #[error("invalid context map: {0}")]
    InvalidContextMap(String),
    /// Invalid prefix code.
    #[error("invalid prefix code: {0}")]
    InvalidPrefixCode(String),
    /// Operation cancelled by the caller.
    #[error("operation cancelled")]
    Cancelled,
}

impl From<BrotliError> for io::Error {
    fn from(err: BrotliError) -> Self {
        match err {
            BrotliError::Io(e) => e,
            // Cancellation is not a data defect, so it keeps the
            // `ErrorKind::Other` classification the streaming adapters have
            // always surfaced; everything else is malformed input.
            BrotliError::Cancelled => io::Error::other(BrotliError::Cancelled.to_string()),
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
            BrotliError::MemoryBudgetExceeded { budget, requested } => {
                OxiArcError::memory_budget_exceeded(budget, requested)
            }
            BrotliError::Cancelled => OxiArcError::Cancelled,
            BrotliError::WindowTooLarge { declared, max } => {
                OxiArcError::memory_budget_exceeded(max, declared)
            }
            other => OxiArcError::corrupted(0, other.to_string()),
        }
    }
}

impl From<OxiArcError> for BrotliError {
    fn from(err: OxiArcError) -> Self {
        match err {
            OxiArcError::Io(e) => BrotliError::Io(e),
            OxiArcError::Cancelled => BrotliError::Cancelled,
            other => BrotliError::CorruptedData(other.to_string()),
        }
    }
}

/// Result type alias for Brotli operations.
pub type BrotliResult<T> = Result<T, BrotliError>;
