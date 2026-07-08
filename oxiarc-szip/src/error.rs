//! Error types returned by the AEC/SZIP encoder and decoder.

use thiserror::Error;

/// Errors that can occur during AEC/SZIP encoding or decoding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SzipError {
    /// The compressed input buffer is too short to decode.
    #[error("input too short: need at least {need} bytes, have {have}")]
    InputTooShort {
        /// Minimum number of bytes required to proceed.
        need: usize,
        /// Number of bytes actually available in the input buffer.
        have: usize,
    },

    /// An option ID was encountered that is not valid for the given `bpp`.
    #[error("invalid block option ID {id} for bpp={bpp}")]
    InvalidBlockOption {
        /// The option ID that was read from the bit stream.
        id: u32,
        /// The `bits_per_pixel` value the option ID was validated against.
        bpp: u8,
    },

    /// A parameter value is out of the allowed range.
    #[error("invalid parameter: {0}")]
    InvalidParam(&'static str),

    /// The decoded byte length does not match the expected output size.
    #[error("output length mismatch: expected {expected} bytes, decoded {actual} bytes")]
    LengthMismatch {
        /// Number of bytes expected based on `SzipParams`.
        expected: usize,
        /// Number of bytes actually produced by the decoder.
        actual: usize,
    },

    /// An option mask bit combination that is not supported was encountered.
    #[error("unsupported option mask bits: 0x{mask:02x}")]
    UnsupportedOption {
        /// The unsupported option mask bits that were read.
        mask: u8,
    },

    /// Attempt to read past the end of the compressed bit stream.
    #[error("unexpected end of bit stream at bit offset {offset}")]
    UnexpectedEof {
        /// The bit offset at which the read past the end of the stream was attempted.
        offset: usize,
    },
}
