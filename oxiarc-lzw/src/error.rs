//! LZW-specific error types.

use thiserror::Error;

/// LZW compression/decompression errors.
#[derive(Debug, Error)]
pub enum LzwError {
    /// Invalid LZW code encountered.
    #[error("Invalid LZW code: {0}")]
    InvalidCode(u16),

    /// Code table is full.
    #[error("Code table full (max {max_codes} codes)")]
    TableFull {
        /// Maximum number of codes allowed.
        max_codes: u16,
    },

    /// Invalid bit width specified.
    #[error("Invalid bit width: {0} (must be 9-12)")]
    InvalidBitWidth(u8),

    /// Unexpected end of data.
    #[error("Unexpected end of data at bit position {position}")]
    UnexpectedEof {
        /// Bit position where EOF occurred.
        position: u64,
    },

    /// Invalid clear code position.
    #[error("Invalid clear code at position {position}")]
    InvalidClearCode {
        /// Bit position of invalid clear code.
        position: u64,
    },

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type for LZW operations.
pub type Result<T> = std::result::Result<T, LzwError>;
