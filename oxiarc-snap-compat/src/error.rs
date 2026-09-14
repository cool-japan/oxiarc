//! snap's error type, with conversions from oxiarc's.

use std::fmt;
use std::io;

use oxiarc_snappy::SnappyError;

/// A convenient type alias for `Result<T, snap::Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// `IntoInnerError` occurs when consuming an encoder fails.
///
/// Consuming the encoder causes a flush to happen. If the flush fails, then
/// this error is returned, which contains both the original encoder and the
/// error that occurred.
#[derive(Debug)]
pub struct IntoInnerError<W> {
    wtr: W,
    err: io::Error,
}

impl<W> IntoInnerError<W> {
    pub(crate) fn new(wtr: W, err: io::Error) -> IntoInnerError<W> {
        IntoInnerError { wtr, err }
    }

    /// Returns the error which caused the call to `into_inner` to fail.
    pub fn error(&self) -> &io::Error {
        &self.err
    }

    /// Returns the error which caused the call to `into_inner` to fail.
    pub fn into_error(self) -> io::Error {
        self.err
    }

    /// Returns the underlying writer which generated the error.
    pub fn into_inner(self) -> W {
        self.wtr
    }
}

impl<W: std::any::Any> std::error::Error for IntoInnerError<W> where W: fmt::Debug {}

impl<W> fmt::Display for IntoInnerError<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.err.fmt(f)
    }
}

/// Error describes all the possible errors that may occur during Snappy
/// compression or decompression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// This error occurs when the given input is too big.
    TooBig {
        /// The size of the given input.
        given: u64,
        /// The maximum allowed size of an input buffer.
        max: u64,
    },
    /// This error occurs when the given buffer is too small to contain the
    /// maximum possible compressed bytes or the total number of decompressed
    /// bytes.
    BufferTooSmall {
        /// The size of the given output buffer.
        given: u64,
        /// The minimum size of the output buffer.
        min: u64,
    },
    /// This error occurs when trying to decompress a zero length buffer.
    Empty,
    /// This error occurs when an invalid header is found during
    /// decompression.
    Header,
    /// This error occurs when there is a mismatch between the number of
    /// decompressed bytes reported in the header and the number of actual
    /// decompressed bytes.
    HeaderMismatch {
        /// The total number of decompressed bytes expected (i.e., the header
        /// value).
        expected_len: u64,
        /// The total number of actual decompressed bytes.
        got_len: u64,
    },
    /// This error occurs during decompression when there was a problem
    /// reading a literal.
    Literal {
        /// The expected length of the literal.
        len: u64,
        /// The number of remaining bytes in the compressed bytes.
        src_len: u64,
        /// The number of remaining slots in the decompression buffer.
        dst_len: u64,
    },
    /// This error occurs during decompression when there was a problem
    /// reading a copy.
    CopyRead {
        /// The expected length of the copy (as encoded in the compressed
        /// bytes).
        len: u64,
        /// The number of remaining bytes in the compressed bytes.
        src_len: u64,
    },
    /// This error occurs during decompression when there was a problem
    /// writing a copy to the decompression buffer.
    CopyWrite {
        /// The length of the copy (i.e., the total number of bytes to be
        /// produced by this copy in the decompression buffer).
        len: u64,
        /// The number of remaining bytes in the decompression buffer.
        dst_len: u64,
    },
    /// This error occurs during decompression when an invalid copy offset
    /// is found.
    Offset {
        /// The offset that was read.
        offset: u64,
        /// The current position in the decompression buffer.
        dst_pos: u64,
    },
    /// This error occurs when a stream header chunk type was expected but
    /// got a different chunk type.
    StreamHeader {
        /// The chunk type byte that was read.
        byte: u8,
    },
    /// This error occurs when the magic stream headers bytes do not match
    /// what is expected.
    StreamHeaderMismatch {
        /// The bytes that were read.
        bytes: Vec<u8>,
    },
    /// This error occurs when an unsupported chunk type is seen.
    UnsupportedChunkType {
        /// The chunk type byte that was read.
        byte: u8,
    },
    /// This error occurs when trying to read a chunk with an unexpected or
    /// incorrect length when reading a snappy frame formatted stream.
    UnsupportedChunkLength {
        /// The length of the chunk encountered.
        len: u64,
        /// True when this error occured while reading the stream header.
        header: bool,
    },
    /// This error occurs when a checksum validity check fails.
    Checksum {
        /// The expected checksum read from the stream.
        expected: u32,
        /// The computed checksum.
        got: u32,
    },
}

impl From<Error> for io::Error {
    fn from(err: Error) -> io::Error {
        io::Error::other(err)
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::TooBig { given, max } => write!(
                f,
                "snappy: input buffer (size = {given}) is larger than allowed (size = {max})"
            ),
            Error::BufferTooSmall { given, min } => write!(
                f,
                "snappy: output buffer (size = {given}) is smaller than required (size = {min})"
            ),
            Error::Empty => write!(f, "snappy: corrupt input (empty)"),
            Error::Header => write!(f, "snappy: corrupt input (invalid header)"),
            Error::HeaderMismatch {
                expected_len,
                got_len,
            } => write!(
                f,
                "snappy: corrupt input (header mismatch; expected {expected_len} decompressed \
                 bytes but got {got_len})"
            ),
            Error::Literal {
                len,
                src_len,
                dst_len,
            } => write!(
                f,
                "snappy: corrupt input (expected literal read of length {len}; remaining src: \
                 {src_len}; remaining dst: {dst_len})"
            ),
            Error::CopyRead { len, src_len } => write!(
                f,
                "snappy: corrupt input (expected copy read of length {len}; remaining src: \
                 {src_len})"
            ),
            Error::CopyWrite { len, dst_len } => write!(
                f,
                "snappy: corrupt input (expected copy write of length {len}; remaining dst: \
                 {dst_len})"
            ),
            Error::Offset { offset, dst_pos } => write!(
                f,
                "snappy: corrupt input (expected valid offset but got offset {offset}; dst \
                 position: {dst_pos})"
            ),
            Error::StreamHeader { byte } => write!(
                f,
                "snappy: corrupt input (expected stream header but got unexpected chunk type \
                 byte {byte})"
            ),
            Error::StreamHeaderMismatch { ref bytes } => write!(
                f,
                "snappy: corrupt input (expected sNaPpY stream header but got {})",
                escape(bytes)
            ),
            Error::UnsupportedChunkType { byte } => {
                write!(f, "snappy: corrupt input (unsupported chunk type: {byte})")
            }
            Error::UnsupportedChunkLength { len, header: false } => {
                write!(f, "snappy: corrupt input (unsupported chunk length: {len})")
            }
            Error::UnsupportedChunkLength { len, header: true } => write!(
                f,
                "snappy: corrupt input (invalid stream header length: {len})"
            ),
            Error::Checksum { expected, got } => write!(
                f,
                "snappy: corrupt input (bad checksum; expected: {expected}, got: {got})"
            ),
        }
    }
}

fn escape(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|&b| std::ascii::escape_default(b))
        .map(char::from)
        .collect()
}

/// Map an oxiarc-snappy error onto the closest snap variant.
pub(crate) fn from_snappy(err: SnappyError, dst_len: usize) -> Error {
    match err {
        SnappyError::InvalidLength { length, max_length } => {
            if length > max_length {
                Error::TooBig {
                    given: length as u64,
                    max: max_length as u64,
                }
            } else {
                Error::Header
            }
        }
        SnappyError::UnexpectedEof { .. } => Error::Literal {
            len: 0,
            src_len: 0,
            dst_len: dst_len as u64,
        },
        SnappyError::InvalidTag { .. } => Error::Header,
        SnappyError::InvalidOffset { offset, position } => Error::Offset {
            offset: offset as u64,
            dst_pos: position as u64,
        },
        SnappyError::OutputLengthMismatch { expected, actual } => Error::HeaderMismatch {
            expected_len: expected as u64,
            got_len: actual as u64,
        },
        SnappyError::ChecksumMismatch { expected, computed } => Error::Checksum {
            expected,
            got: computed,
        },
        SnappyError::InvalidChunkType { chunk_type } => {
            Error::UnsupportedChunkType { byte: chunk_type }
        }
        SnappyError::ChunkTooLarge { size, .. } => Error::UnsupportedChunkLength {
            len: size as u64,
            header: false,
        },
        _ => Error::Header,
    }
}
