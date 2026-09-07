//! Error types.
//!
//! The four-variant shapes of [`DecodingError`] and [`EncodingError`] mirror
//! the `png` crate so that downstream `match` arms keep compiling. The
//! additive part is [`FormatError::kind`], which exposes the precise reason a
//! file was rejected instead of only its `Display` text.

use std::fmt;
use std::io;

use crate::chunk::ChunkType;
use crate::header::{BitDepth, ColorType};

/// The error type produced by every decoding entry point.
#[derive(Debug)]
pub enum DecodingError {
    /// An error reading from the underlying stream.
    IoError(io::Error),
    /// The input is not a well-formed PNG.
    Format(FormatError),
    /// The API was used incorrectly (wrong buffer size, polled after the end,
    /// and so on).
    Parameter(ParameterError),
    /// A configured [`crate::Limits`] or [`crate::DecodeLimits`] was hit.
    LimitsExceeded,
}

impl fmt::Display for DecodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodingError::IoError(err) => write!(f, "{err}"),
            DecodingError::Format(desc) => write!(f, "{desc}"),
            DecodingError::Parameter(desc) => write!(f, "{desc}"),
            DecodingError::LimitsExceeded => write!(f, "limits are exceeded"),
        }
    }
}

impl std::error::Error for DecodingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodingError::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for DecodingError {
    fn from(err: io::Error) -> DecodingError {
        DecodingError::IoError(err)
    }
}

impl From<DecodingError> for io::Error {
    fn from(err: DecodingError) -> io::Error {
        match err {
            DecodingError::IoError(err) => err,
            err => io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
        }
    }
}

impl From<FormatError> for DecodingError {
    fn from(err: FormatError) -> DecodingError {
        DecodingError::Format(err)
    }
}

impl From<FormatErrorKind> for DecodingError {
    fn from(kind: FormatErrorKind) -> DecodingError {
        DecodingError::Format(FormatError { kind })
    }
}

impl From<ParameterErrorKind> for DecodingError {
    fn from(kind: ParameterErrorKind) -> DecodingError {
        DecodingError::Parameter(ParameterError { kind })
    }
}

impl DecodingError {
    /// The precise format-level reason, when this is a format error.
    #[must_use]
    pub fn format_kind(&self) -> Option<&FormatErrorKind> {
        match self {
            DecodingError::Format(err) => Some(&err.kind),
            _ => None,
        }
    }

    pub(crate) fn unexpected_eof() -> DecodingError {
        DecodingError::IoError(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of PNG stream",
        ))
    }
}

/// A structural problem with the encoded file.
#[derive(Debug)]
pub struct FormatError {
    kind: FormatErrorKind,
}

impl FormatError {
    /// The precise reason the file was rejected.
    #[must_use]
    pub fn kind(&self) -> &FormatErrorKind {
        &self.kind
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for FormatError {}

/// Every distinct way a PNG stream can be malformed.
///
/// New variants are added in minor releases, hence `#[non_exhaustive]`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatErrorKind {
    /// The 8-byte PNG signature was missing or wrong.
    #[error("invalid PNG signature")]
    InvalidSignature,
    /// A chunk's stored CRC did not match the computed one.
    #[error("CRC error: expected {crc_val:#010x}, computed {crc_sum:#010x} for chunk {chunk}")]
    CrcMismatch {
        /// The CRC stored in the file.
        crc_val: u32,
        /// The CRC computed over type + data.
        crc_sum: u32,
        /// The chunk the mismatch was found in.
        chunk: ChunkType,
    },
    /// A chunk declared a length outside the range the specification allows for
    /// that chunk type.
    #[error("chunk {kind} has an invalid length")]
    ChunkLengthWrong {
        /// The offending chunk type.
        kind: ChunkType,
    },
    /// A chunk declared a length above the 2^31-1 specification cap.
    #[error("chunk length {len} exceeds the 2^31-1 limit")]
    ChunkTooLong {
        /// The declared length.
        len: u32,
    },
    /// A chunk type contained bytes outside `A-Za-z`.
    #[error("chunk type is not four ASCII letters")]
    InvalidChunkType,
    /// The reserved bit (bit 5 of the third byte) was set in a chunk type.
    #[error("reserved bit is set in chunk type {kind}")]
    ReservedBitSet {
        /// The offending chunk type.
        kind: ChunkType,
    },
    /// Width or height was zero, or above 2^31-1.
    #[error("invalid image dimensions")]
    InvalidDimensions,
    /// The bit depth byte was not 1, 2, 4, 8 or 16.
    #[error("invalid bit depth {depth}")]
    InvalidBitDepth {
        /// The rejected byte.
        depth: u8,
    },
    /// The colour type byte was not 0, 2, 3, 4 or 6.
    #[error("invalid colour type {color_type}")]
    InvalidColorType {
        /// The rejected byte.
        color_type: u8,
    },
    /// The colour type and bit depth pair is forbidden by Table 11.1.
    #[error("invalid colour type {color_type:?} for bit depth {bit_depth:?}")]
    InvalidColorBitDepth {
        /// The colour type.
        color_type: ColorType,
        /// The bit depth.
        bit_depth: BitDepth,
    },
    /// `IHDR` named a compression method other than 0.
    #[error("unknown compression method {method}")]
    UnknownCompressionMethod {
        /// The rejected byte.
        method: u8,
    },
    /// `IHDR` named a filter method other than 0.
    #[error("unknown filter method {method}")]
    UnknownFilterMethod {
        /// The rejected byte.
        method: u8,
    },
    /// `IHDR` named an interlace method other than 0 or 1.
    #[error("unknown interlace method {method}")]
    UnknownInterlaceMethod {
        /// The rejected byte.
        method: u8,
    },
    /// A scanline's filter byte was not in `0..=4`.
    #[error("invalid filter type {code} on a scanline")]
    InvalidRowFilter {
        /// The rejected byte.
        code: u8,
    },
    /// A `tRNS` chunk appeared on a colour type that already has alpha.
    #[error("tRNS chunk found on a colour type that already carries alpha")]
    ColorWithBadTrns,
    /// A palette image had no `PLTE` chunk.
    #[error("indexed image is missing its PLTE chunk")]
    PaletteRequired,
    /// A `PLTE` or `tRNS` chunk was shorter than required.
    #[error("palette-like chunk is too short: {len} bytes, expected at least {expected}")]
    ShortPalette {
        /// The required length.
        expected: usize,
        /// The observed length.
        len: usize,
    },
    /// A palette index was greater than or equal to the number of entries and
    /// strict palette checking was requested.
    #[error("palette index {index} is out of range for {entries} entries")]
    PaletteIndexOutOfRange {
        /// The offending index.
        index: u16,
        /// The number of entries the palette has.
        entries: usize,
    },
    /// An `sBIT` chunk had a length that does not match the colour type.
    #[error("sBIT chunk has an invalid size")]
    InvalidSbitChunkSize,
    /// An `sBIT` value was zero or greater than the sample depth.
    #[error("sBIT value is out of range")]
    InvalidSbit,
    /// The image data stream ended before the image was complete.
    #[error("not enough image data")]
    NoMoreImageData,
    /// The image data stream produced more bytes than the header allows.
    #[error("extra image data after the end of the image")]
    ExtraImageData,
    /// No `IDAT` chunk was present at all.
    #[error("missing image data")]
    MissingImageData,
    /// A non-`IDAT` chunk was found in the middle of the `IDAT` run.
    #[error("chunk {kind} restarts the image data sequence")]
    UnexpectedRestartOfDataChunkSequence {
        /// The chunk that interrupted the run.
        kind: ChunkType,
    },
    /// A chunk appeared before `IHDR`.
    #[error("chunk {kind} appeared before IHDR")]
    ChunkBeforeIhdr {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// A chunk that must precede `PLTE` appeared after it.
    #[error("chunk {kind} must appear before PLTE")]
    AfterPlte {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// A chunk that must precede `IDAT` appeared after it.
    #[error("chunk {kind} must appear before IDAT")]
    AfterIdat {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// A chunk that must follow `PLTE` appeared before it.
    #[error("chunk {kind} must appear after PLTE")]
    BeforePlte {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// A chunk that must sit between `PLTE` and `IDAT` did not.
    #[error("chunk {kind} must appear between PLTE and IDAT")]
    OutsidePlteIdat {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// A chunk that may appear at most once appeared twice.
    #[error("duplicate chunk {kind}")]
    DuplicateChunk {
        /// The offending chunk.
        kind: ChunkType,
    },
    /// An unknown chunk had its critical bit set.
    #[error("unrecognized critical chunk {type_str}")]
    UnrecognizedCriticalChunk {
        /// The offending chunk.
        type_str: ChunkType,
    },
    /// Both `sRGB` and `iCCP` were present, which the specification forbids.
    #[error("sRGB and iCCP chunks are mutually exclusive")]
    SrgbAndIccp,
    /// The file ended without an `IEND` chunk.
    #[error("missing IEND chunk")]
    MissingIend,
    /// Bytes followed the `IEND` chunk and strict mode was requested.
    #[error("trailing data after IEND")]
    TrailingData,
    /// The `IDAT` zlib header named a compression method other than DEFLATE.
    #[error("zlib compression method {cm} is not DEFLATE")]
    InvalidZlibCompressionMethod {
        /// The `CM` nibble.
        cm: u8,
    },
    /// The `IDAT` zlib header declared a window larger than 32768 bytes.
    #[error("zlib window size field {cinfo} exceeds the PNG limit of 7")]
    InvalidZlibWindowSize {
        /// The `CINFO` nibble.
        cinfo: u8,
    },
    /// The `IDAT` zlib header set `FDICT`, which PNG forbids.
    #[error("zlib preset dictionaries are forbidden in PNG image data")]
    ZlibPresetDictionaryForbidden,
    /// The `IDAT` zlib header's check bits were wrong.
    #[error("zlib header check failed")]
    InvalidZlibHeaderCheck,
    /// The compressed stream was corrupt.
    #[error("corrupt DEFLATE stream: {err}")]
    CorruptFlateStream {
        /// The message from the inflater.
        err: String,
    },
    /// An `acTL` chunk declared zero frames.
    #[error("animation declares zero frames")]
    ZeroFrames,
    /// An `fcTL`/`fdAT` sequence number was not the expected one.
    #[error("APNG sequence number {present} out of order, expected {expected}")]
    ApngOrder {
        /// The sequence number found.
        present: u32,
        /// The sequence number required.
        expected: u32,
    },
    /// An `fdAT` chunk appeared before any `fcTL`.
    #[error("fdAT chunk without a preceding fcTL")]
    MissingFctl,
    /// An `fdAT` chunk was shorter than its four-byte sequence number.
    #[error("fdAT chunk is shorter than four bytes")]
    FdatShorterThanFourBytes,
    /// A frame's rectangle did not fit inside the canvas.
    #[error("APNG sub-frame is out of bounds")]
    BadSubFrameBounds,
    /// The file is an Apple `CgBI` PNG and strict mode was requested.
    #[error("Apple CgBI PNG is rejected in strict mode")]
    CgbiUnsupported,
    /// Text-chunk content violated the specification's encoding rules.
    #[error("bad text encoding: {0}")]
    BadTextEncoding(&'static str),
    /// A keyword was empty, longer than 79 bytes, outside Latin-1, or had
    /// leading, trailing or consecutive spaces.
    #[error("invalid text keyword")]
    InvalidTextKeyword,
    /// A chunk's payload was structurally malformed.
    #[error("malformed {kind} chunk")]
    MalformedChunk {
        /// The offending chunk type.
        kind: ChunkType,
    },
}

impl From<FormatErrorKind> for FormatError {
    fn from(kind: FormatErrorKind) -> FormatError {
        FormatError { kind }
    }
}

/// Misuse of the decoding API rather than a problem with the file.
#[derive(Debug)]
pub struct ParameterError {
    kind: ParameterErrorKind,
}

impl ParameterError {
    /// The precise kind of misuse.
    #[must_use]
    pub fn kind(&self) -> &ParameterErrorKind {
        &self.kind
    }
}

impl fmt::Display for ParameterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for ParameterError {}

impl From<ParameterErrorKind> for ParameterError {
    fn from(kind: ParameterErrorKind) -> ParameterError {
        ParameterError { kind }
    }
}

/// Ways in which the decoding API can be misused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParameterErrorKind {
    /// The output buffer handed to `next_frame` was the wrong size.
    #[error("output buffer has {actual} bytes, expected at least {expected}")]
    ImageBufferSize {
        /// The required size.
        expected: usize,
        /// The size supplied.
        actual: usize,
    },
    /// The decoder was polled after it had already produced the last row.
    #[error("polled after the end of the image")]
    PolledAfterEndOfImage,
    /// The decoder was polled after a fatal error had been latched.
    #[error("polled after a fatal error")]
    PolledAfterFatalError,
    /// A frame was requested from a file that is not an animation.
    #[error("the image is not an animation")]
    NotAnimated,
}

/// The error type produced by every encoding entry point.
#[derive(Debug)]
pub enum EncodingError {
    /// An error writing to the underlying stream.
    IoError(io::Error),
    /// The requested output cannot be represented as a PNG.
    Format(EncodingFormatError),
    /// The API was used incorrectly.
    Parameter(ParameterError),
    /// A configured limit was hit.
    LimitsExceeded,
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodingError::IoError(err) => write!(f, "{err}"),
            EncodingError::Format(desc) => write!(f, "{desc}"),
            EncodingError::Parameter(desc) => write!(f, "{desc}"),
            EncodingError::LimitsExceeded => write!(f, "limits are exceeded"),
        }
    }
}

impl std::error::Error for EncodingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EncodingError::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for EncodingError {
    fn from(err: io::Error) -> EncodingError {
        EncodingError::IoError(err)
    }
}

impl From<EncodingError> for io::Error {
    fn from(err: EncodingError) -> io::Error {
        match err {
            EncodingError::IoError(err) => err,
            err => io::Error::other(err.to_string()),
        }
    }
}

impl From<EncodingFormatErrorKind> for EncodingError {
    fn from(kind: EncodingFormatErrorKind) -> EncodingError {
        EncodingError::Format(EncodingFormatError { kind })
    }
}

impl From<ParameterErrorKind> for EncodingError {
    fn from(kind: ParameterErrorKind) -> EncodingError {
        EncodingError::Parameter(ParameterError { kind })
    }
}

/// A structural problem with the image the caller asked to encode.
#[derive(Debug)]
pub struct EncodingFormatError {
    kind: EncodingFormatErrorKind,
}

impl EncodingFormatError {
    /// The precise reason the request was rejected.
    #[must_use]
    pub fn kind(&self) -> &EncodingFormatErrorKind {
        &self.kind
    }
}

impl fmt::Display for EncodingFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for EncodingFormatError {}

/// Every distinct way an encoding request can be invalid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EncodingFormatErrorKind {
    /// The image width was zero.
    #[error("image width is zero")]
    ZeroWidth,
    /// The image height was zero.
    #[error("image height is zero")]
    ZeroHeight,
    /// The colour type and bit depth pair is forbidden by Table 11.1.
    #[error("invalid colour type and bit depth combination")]
    InvalidColorCombination,
    /// An indexed image was requested without a palette.
    #[error("indexed image requires a palette")]
    NoPalette,
    /// More image data was written than the frame holds.
    #[error("wrote {0} bytes too many")]
    WrittenTooMuch(usize),
    /// Animation controls were used on a non-animated encoder.
    #[error("the encoder is not animated")]
    NotAnimated,
    /// A frame rectangle was outside the canvas.
    #[error("frame is out of bounds")]
    OutOfBounds,
    /// All declared frames have already been written.
    #[error("all frames have been written")]
    EndReached,
    /// An animation declared zero frames.
    #[error("animation declares zero frames")]
    ZeroFrames,
    /// The encoder was finished before all frames were written.
    #[error("{0} frames are still missing")]
    MissingFrames(u32),
    /// A row was left partially written.
    #[error("{0} bytes of image data are missing")]
    MissingData(usize),
    /// The supplied buffer did not match the frame size.
    #[error("image buffer has {actual} bytes, expected {expected}")]
    ImageBufferSize {
        /// The required size.
        expected: usize,
        /// The size supplied.
        actual: usize,
    },
    /// Text could not be encoded in the requested character set.
    #[error("bad text encoding: {0}")]
    BadTextEncoding(&'static str),
    /// The encoder cannot recover from a previous failure.
    #[error("the encoder is in an unrecoverable state")]
    Unrecoverable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_error_maps_to_io_error() {
        let err: io::Error = DecodingError::from(FormatErrorKind::InvalidSignature).into();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let io_err = io::Error::new(io::ErrorKind::UnexpectedEof, "x");
        let err: io::Error = DecodingError::from(io_err).into();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn format_kind_is_reachable_from_the_public_error() {
        let err = DecodingError::from(FormatErrorKind::ChunkTooLong { len: 1 << 31 });
        assert!(matches!(
            err.format_kind(),
            Some(FormatErrorKind::ChunkTooLong { len }) if *len == 1 << 31
        ));
        assert!(err.to_string().contains("2^31-1"));
    }

    #[test]
    fn parameter_and_encoding_errors_display() {
        let err = DecodingError::from(ParameterErrorKind::ImageBufferSize {
            expected: 10,
            actual: 3,
        });
        assert!(err.to_string().contains("expected at least 10"));
        let err = EncodingError::from(EncodingFormatErrorKind::ZeroWidth);
        assert!(err.to_string().contains("width is zero"));
        let err = EncodingError::from(ParameterErrorKind::PolledAfterEndOfImage);
        assert!(err.to_string().contains("polled"));
    }
}
