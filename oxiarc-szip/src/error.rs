//! Error types returned by the AEC/SZIP encoder and decoder.

use thiserror::Error;

/// Errors that can occur during AEC/SZIP encoding or decoding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SzipError {
    /// The input buffer holds fewer elements than the operation requires
    /// (e.g. an encode sample buffer shorter than `SzipParams::samples`).
    #[error("input too short: need at least {need} elements, have {have}")]
    InputTooShort {
        /// Minimum number of elements required to proceed.
        need: usize,
        /// Number of elements actually available in the input buffer.
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

    /// The decoded data length does not match the expected output size
    /// (e.g. a zero-block run claims more samples than the reference sample
    /// interval can hold).
    #[error("output length mismatch: expected at most {expected} samples, stream encodes {actual}")]
    LengthMismatch {
        /// Number of samples the current segment can hold based on `SzipParams`.
        expected: usize,
        /// Number of samples the corrupt stream claims for the segment.
        actual: usize,
    },

    /// An option mask bit combination that is not supported was encountered.
    ///
    /// Reserved for future option-mask parsing (e.g. HDF5 SZIP filter
    /// masks); currently not produced by this crate.
    #[error("unsupported option mask bits: 0x{mask:02x}")]
    UnsupportedOption {
        /// The unsupported option mask bits that were read.
        mask: u8,
    },

    /// A sample value passed to the encoder exceeds the maximum value
    /// representable in `bits_per_pixel` bits.
    #[error("sample {index} out of range: value {value} exceeds max {max}")]
    SampleOutOfRange {
        /// Index of the offending sample in the input buffer.
        index: usize,
        /// The out-of-range sample value.
        value: u64,
        /// Maximum representable value, `(1 << bits_per_pixel) - 1`.
        max: u64,
    },

    /// Attempt to read past the end of the compressed bit stream.
    #[error("unexpected end of bit stream at bit offset {offset}")]
    UnexpectedEof {
        /// The bit offset at which the read past the end of the stream was attempted.
        offset: usize,
    },
}
