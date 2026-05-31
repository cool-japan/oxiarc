use thiserror::Error;

/// Errors that can occur during AEC/SZIP encoding or decoding.
#[derive(Debug, Error)]
pub enum SzipError {
    /// The compressed input buffer is too short to decode.
    #[error("input too short: need at least {need} bytes, have {have}")]
    InputTooShort { need: usize, have: usize },

    /// An option ID was encountered that is not valid for the given `bpp`.
    #[error("invalid block option ID {id} for bpp={bpp}")]
    InvalidBlockOption { id: u32, bpp: u8 },

    /// A parameter value is out of the allowed range.
    #[error("invalid parameter: {0}")]
    InvalidParam(&'static str),

    /// The decoded byte length does not match the expected output size.
    #[error("output length mismatch: expected {expected} bytes, decoded {actual} bytes")]
    LengthMismatch { expected: usize, actual: usize },

    /// An option mask bit combination that is not supported was encountered.
    #[error("unsupported option mask bits: 0x{mask:02x}")]
    UnsupportedOption { mask: u8 },

    /// Attempt to read past the end of the compressed bit stream.
    #[error("unexpected end of bit stream at bit offset {offset}")]
    UnexpectedEof { offset: usize },
}
