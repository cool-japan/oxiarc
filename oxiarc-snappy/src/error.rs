//! Error types for Snappy compression/decompression.

use std::io;
use thiserror::Error;

/// Error type for Snappy operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnappyError {
    /// The input data is too short or truncated.
    #[error("unexpected end of input: {context}")]
    UnexpectedEof {
        /// Description of what was expected.
        context: &'static str,
    },
    /// The decompressed length header is invalid or too large.
    #[error("invalid decompressed length {length} (max {max_length})")]
    InvalidLength {
        /// The decoded length value.
        length: usize,
        /// Maximum allowed length.
        max_length: usize,
    },
    /// An invalid tag byte was encountered during decompression.
    #[error("invalid tag byte {tag:#04x} at offset {offset}")]
    InvalidTag {
        /// The tag byte value.
        tag: u8,
        /// Byte offset in the compressed stream.
        offset: usize,
    },
    /// A copy operation references data before the start of the output.
    #[error("invalid back-reference offset {offset} at output position {position}")]
    InvalidOffset {
        /// The back-reference offset.
        offset: usize,
        /// Current output position.
        position: usize,
    },
    /// The decompressed output does not match the expected length.
    #[error("output length mismatch: expected {expected}, got {actual}")]
    OutputLengthMismatch {
        /// Expected output length from the header.
        expected: usize,
        /// Actual decompressed length.
        actual: usize,
    },
    /// CRC32C checksum mismatch in framed format.
    #[error("CRC32C checksum mismatch: expected {expected:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        /// Expected checksum from the frame.
        expected: u32,
        /// Computed checksum from the data.
        computed: u32,
    },
    /// Invalid or unrecognized chunk type in framed format.
    #[error("invalid chunk type: {chunk_type:#04x}")]
    InvalidChunkType {
        /// The chunk type byte.
        chunk_type: u8,
    },
    /// The stream identifier is missing or invalid.
    #[error("missing or invalid stream identifier")]
    InvalidStreamIdentifier,
    /// The compressed data is corrupted.
    #[error("corrupted data: {message}")]
    CorruptedData {
        /// Description of the corruption.
        message: String,
    },
    /// A framed-format chunk's declared (wire) or actual decoded size
    /// exceeds the Snappy framing format's 64 KiB per-chunk maximum.
    ///
    /// Per the framing format spec, any content chunk's *uncompressed*
    /// payload must not exceed 65536 bytes. This is returned both when a
    /// chunk header declares a length that could not possibly decode within
    /// that bound (rejected before any read/decompression is attempted) and
    /// when a compressed chunk actually decodes to more than 65536 bytes
    /// (a decompression-amplification attempt).
    #[error("chunk size {size} exceeds the 64 KiB Snappy framing format maximum ({max})")]
    ChunkTooLarge {
        /// The chunk's declared (wire) or actual decoded size, in bytes.
        size: usize,
        /// The maximum allowed size, in bytes.
        max: usize,
    },
    /// A bounded decode's output exceeded the caller's total-output limit.
    ///
    /// Returned by [`crate::decompress_with_limit`],
    /// [`crate::decompress_frame_with_limit`] and
    /// [`crate::frame::FrameDecoder::with_max_output_size`]. Each framed
    /// chunk is already capped at 65536 bytes, but a stream may contain an
    /// unbounded number of chunks, and a raw block may declare up to the
    /// crate's 256 MiB maximum; this bounds a single decode session to the
    /// caller's budget.
    ///
    /// The limit is enforced against the size each block/chunk *declares*,
    /// so it fires before the over-budget output is decoded or allocated.
    #[error("total decompressed output {produced} bytes exceeds configured maximum {max} bytes")]
    TotalOutputExceeded {
        /// Total bytes the decode would have produced, including the
        /// block/chunk that tripped the limit.
        produced: u64,
        /// The configured maximum total output, in bytes.
        max: u64,
    },
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl From<SnappyError> for io::Error {
    fn from(err: SnappyError) -> Self {
        match err {
            SnappyError::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

impl From<SnappyError> for oxiarc_core::error::OxiArcError {
    fn from(err: SnappyError) -> Self {
        match err {
            SnappyError::Io(e) => Self::Io(e),
            SnappyError::ChecksumMismatch { expected, computed } => {
                Self::CrcMismatch { expected, computed }
            }
            SnappyError::UnexpectedEof { .. } => Self::UnexpectedEof { expected: 0 },
            SnappyError::TotalOutputExceeded { produced, max } => Self::MemoryBudgetExceeded {
                budget: usize::try_from(max).unwrap_or(usize::MAX),
                requested: usize::try_from(produced).unwrap_or(usize::MAX),
            },
            other => Self::CorruptedData {
                offset: 0,
                message: other.to_string(),
            },
        }
    }
}
