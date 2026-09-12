//! LZW-specific error types.

use thiserror::Error;

/// LZW compression/decompression errors.
#[derive(Debug, Error)]
#[non_exhaustive]
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
    #[error("Invalid bit width: {0} (must be 9-16)")]
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

    /// The stream does not start with the UNIX `compress` magic `1F 9D`
    /// (see [`crate::z`]).
    #[error("not a .Z stream: expected magic 1F 9D, found {magic:02X?}")]
    ZInvalidMagic {
        /// The first two bytes that were found instead.
        magic: [u8; 2],
    },

    /// A `.Z` stream ended before its three-byte header was complete.
    #[error(".Z stream is shorter than its 3-byte header ({len} bytes)")]
    ZTruncatedHeader {
        /// Number of bytes actually available.
        len: usize,
    },

    /// A `.Z` header declares a maximum code width outside 9-16.
    #[error("unsupported .Z code width: {0} bits (must be 9-16)")]
    ZUnsupportedMaxBits(u8),

    /// Decoding produced more output than the caller's limit allows.
    ///
    /// Raised *during* decoding, as soon as the limit is crossed, so a
    /// decompression bomb never fully expands in memory.
    #[error("decompressed output exceeds the {limit}-byte limit")]
    OutputLimitExceeded {
        /// The limit that was crossed, in bytes.
        limit: usize,
    },

    /// The caller-supplied output buffer is too small for the decoded
    /// stream.
    ///
    /// Unlike [`LzwError::OutputLimitExceeded`] this is a sizing mistake
    /// rather than a security bound; the total decoded size is not reported
    /// because decoding stops as soon as the buffer overflows.
    #[error("output buffer of {available} bytes is too small for the decoded stream")]
    BufferTooSmall {
        /// Capacity of the buffer that was supplied.
        available: usize,
    },

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type for LZW operations.
///
/// Note: this is a crate-local alias distinct from `oxiarc_core::error::Result`.
/// It intentionally shadows the core alias within this crate so that internal
/// LZW code can use the shorter `Result<T>` spelling without repeatedly writing
/// `std::result::Result<T, LzwError>`. Public APIs that need to surface a core
/// `OxiArcError` should convert explicitly (e.g. via `.map_err(Into::into)`)
/// rather than introducing another per-crate alias.
pub type Result<T> = std::result::Result<T, LzwError>;
