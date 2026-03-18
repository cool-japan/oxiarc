//! Error types for Snappy compression/decompression.

use std::fmt;
use std::io;

/// Error type for Snappy operations.
#[derive(Debug)]
pub enum SnappyError {
    /// The input data is too short or truncated.
    UnexpectedEof {
        /// Description of what was expected.
        context: &'static str,
    },
    /// The decompressed length header is invalid or too large.
    InvalidLength {
        /// The decoded length value.
        length: usize,
        /// Maximum allowed length.
        max_length: usize,
    },
    /// An invalid tag byte was encountered during decompression.
    InvalidTag {
        /// The tag byte value.
        tag: u8,
        /// Byte offset in the compressed stream.
        offset: usize,
    },
    /// A copy operation references data before the start of the output.
    InvalidOffset {
        /// The back-reference offset.
        offset: usize,
        /// Current output position.
        position: usize,
    },
    /// The decompressed output does not match the expected length.
    OutputLengthMismatch {
        /// Expected output length from the header.
        expected: usize,
        /// Actual decompressed length.
        actual: usize,
    },
    /// CRC32C checksum mismatch in framed format.
    ChecksumMismatch {
        /// Expected checksum from the frame.
        expected: u32,
        /// Computed checksum from the data.
        computed: u32,
    },
    /// Invalid or unrecognized chunk type in framed format.
    InvalidChunkType {
        /// The chunk type byte.
        chunk_type: u8,
    },
    /// The stream identifier is missing or invalid.
    InvalidStreamIdentifier,
    /// The compressed data is corrupted.
    CorruptedData {
        /// Description of the corruption.
        message: String,
    },
    /// An I/O error occurred.
    Io(io::Error),
}

impl fmt::Display for SnappyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { context } => {
                write!(f, "unexpected end of input: {context}")
            }
            Self::InvalidLength { length, max_length } => {
                write!(f, "invalid decompressed length {length} (max {max_length})")
            }
            Self::InvalidTag { tag, offset } => {
                write!(f, "invalid tag byte {tag:#04x} at offset {offset}")
            }
            Self::InvalidOffset { offset, position } => {
                write!(
                    f,
                    "invalid back-reference offset {offset} at output position {position}"
                )
            }
            Self::OutputLengthMismatch { expected, actual } => {
                write!(
                    f,
                    "output length mismatch: expected {expected}, got {actual}"
                )
            }
            Self::ChecksumMismatch { expected, computed } => {
                write!(
                    f,
                    "CRC32C checksum mismatch: expected {expected:#010x}, computed {computed:#010x}"
                )
            }
            Self::InvalidChunkType { chunk_type } => {
                write!(f, "invalid chunk type: {chunk_type:#04x}")
            }
            Self::InvalidStreamIdentifier => {
                write!(f, "missing or invalid stream identifier")
            }
            Self::CorruptedData { message } => {
                write!(f, "corrupted data: {message}")
            }
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for SnappyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SnappyError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
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
            other => Self::CorruptedData {
                offset: 0,
                message: other.to_string(),
            },
        }
    }
}
