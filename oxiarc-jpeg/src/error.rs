//! Error types for JPEG decoding and encoding.
//!
//! Every fallible operation in this crate returns [`JpegError`]. The type is
//! `#[non_exhaustive]`, so downstream `match` expressions need a wildcard arm.
//!
//! A [`From`] conversion into [`oxiarc_core::OxiArcError`] is provided so that
//! container crates (`oxiarc-tiff`, `oxiarc-archive`) can propagate JPEG
//! failures with `?` without wrapping them by hand.

use oxiarc_core::OxiArcError;
use thiserror::Error;

/// Which family of table a reference pointed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableKind {
    /// Quantisation table (`DQT`, `Tq`).
    Quantisation,
    /// DC Huffman table (`DHT` with `Tc == 0`, `Td`).
    DcHuffman,
    /// AC Huffman table (`DHT` with `Tc == 1`, `Ta`).
    AcHuffman,
    /// Arithmetic conditioning table (`DAC`).
    ArithmeticConditioning,
}

impl core::fmt::Display for TableKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            TableKind::Quantisation => "quantisation",
            TableKind::DcHuffman => "DC Huffman",
            TableKind::AcHuffman => "AC Huffman",
            TableKind::ArithmeticConditioning => "arithmetic conditioning",
        };
        f.write_str(name)
    }
}

/// A JPEG feature this build cannot decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedFeature {
    /// Hierarchical progression (`SOF5`/`6`/`7`/`13`/`14`/`15`, `DHP`, `EXP`).
    ///
    /// No reference encoder produces these frames, so the crate rejects them
    /// with a named error rather than guessing at an untested implementation.
    Hierarchical,
    /// Arithmetic entropy coding (`SOF9`/`10`/`11`).
    ArithmeticCoding,
    /// Sample precision `P` outside the range the coding process allows.
    SamplePrecision(u8),
    /// Component count outside `1..=4`.
    ComponentCount(u8),
    /// A sampling factor pair outside `1..=4`, or one that cannot tile the frame.
    SamplingFactor {
        /// Horizontal sampling factor `Hi`.
        h: u8,
        /// Vertical sampling factor `Vi`.
        v: u8,
    },
    /// An Adobe `APP14` colour transform code this crate does not know.
    ColorTransform(u8),
    /// A lossless predictor selection value outside `1..=7`.
    ///
    /// `Psv == 0` selects the differential (hierarchical) process.
    LosslessPredictor(u8),
}

impl core::fmt::Display for UnsupportedFeature {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UnsupportedFeature::Hierarchical => {
                f.write_str("hierarchical JPEG (SOF5/6/7/13/14/15)")
            }
            UnsupportedFeature::ArithmeticCoding => {
                f.write_str("arithmetic entropy coding (SOF9/10/11)")
            }
            UnsupportedFeature::SamplePrecision(p) => write!(f, "sample precision {p}"),
            UnsupportedFeature::ComponentCount(n) => write!(f, "{n} components"),
            UnsupportedFeature::SamplingFactor { h, v } => {
                write!(f, "sampling factor {h}x{v}")
            }
            UnsupportedFeature::ColorTransform(t) => write!(f, "Adobe colour transform {t}"),
            UnsupportedFeature::LosslessPredictor(p) => {
                write!(f, "lossless predictor selection value {p}")
            }
        }
    }
}

/// Which configured limit a stream tried to exceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// `DecodeLimits::max_width`.
    Width,
    /// `DecodeLimits::max_height`.
    Height,
    /// `DecodeLimits::max_pixels`.
    Pixels,
    /// `DecodeLimits::max_components`.
    Components,
    /// `DecodeLimits::max_scans`.
    Scans,
    /// `DecodeLimits::max_coefficient_bytes`.
    CoefficientMemory,
    /// `DecodeLimits::max_output_bytes`.
    OutputBytes,
    /// `DecodeLimits::max_input_bytes`.
    InputBytes,
}

impl core::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            LimitKind::Width => "image width",
            LimitKind::Height => "image height",
            LimitKind::Pixels => "pixel count",
            LimitKind::Components => "component count",
            LimitKind::Scans => "scan count",
            LimitKind::CoefficientMemory => "coefficient buffer bytes",
            LimitKind::OutputBytes => "output bytes",
            LimitKind::InputBytes => "input bytes",
        };
        f.write_str(name)
    }
}

/// The error type for every operation in this crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JpegError {
    /// An I/O error from the underlying reader or writer.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The stream ended in the middle of a structure that needs more bytes.
    #[error("unexpected end of JPEG stream in {context}")]
    UnexpectedEof {
        /// Where the truncation was noticed, e.g. `"SOF segment"`.
        context: &'static str,
    },

    /// A marker was required at this position and a different one was found.
    #[error("expected marker {expected:#06X}, found {found:#06X} at offset {offset}")]
    UnexpectedMarker {
        /// The marker code the parser required (`0xFFxx`).
        expected: u16,
        /// The marker code actually present (`0xFFxx`).
        found: u16,
        /// Byte offset of the offending marker.
        offset: u64,
    },

    /// A segment's contents violate the syntax rules of T.81.
    #[error("malformed {segment} segment at offset {offset}: {reason}")]
    MalformedSegment {
        /// Segment name, e.g. `"DHT"`.
        segment: &'static str,
        /// Byte offset of the segment's marker.
        offset: u64,
        /// Why the segment was rejected.
        reason: &'static str,
    },

    /// The entropy-coded data contains a code the active table cannot decode.
    #[error("invalid Huffman code in scan at MCU {mcu}")]
    InvalidHuffmanCode {
        /// Index of the MCU (or sample, for lossless) being decoded.
        mcu: u64,
    },

    /// An arithmetic-coded scan produced a decision sequence T.81 does not
    /// define: a magnitude beyond `2^15` or a spectral index past `Se`.
    ///
    /// Unlike a Huffman scan, a *truncated* arithmetic scan does not
    /// necessarily reach this error: T.81 D.2.6 lets the decoder read zeros
    /// past the last coded byte, so short data usually decodes to noise. Only
    /// an impossible decision sequence, or a scan that ended before its last
    /// unit, is reported here.
    #[error("invalid arithmetic code in scan at MCU {mcu}")]
    InvalidArithmeticCode {
        /// Index of the MCU (or sample, for lossless) being decoded.
        mcu: u64,
    },

    /// A scan referenced a table slot that no segment ever defined.
    #[error("reference to undefined {kind} table {index}")]
    UndefinedTable {
        /// Which table family the reference names.
        kind: TableKind,
        /// The table slot index.
        index: u8,
    },

    /// The stream uses a feature this build cannot decode.
    #[error("unsupported JPEG feature: {0}")]
    Unsupported(UnsupportedFeature),

    /// A [`crate::DecodeLimits`] entry would have been exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(LimitKind),

    /// A caller-supplied output buffer is smaller than the decoded image.
    #[error("output buffer too small: need {need}, got {got}")]
    BufferTooSmall {
        /// Bytes (or `u16` elements) the image requires.
        need: usize,
        /// Bytes (or `u16` elements) the caller supplied.
        got: usize,
    },

    /// A scan-only abbreviated stream was decoded without a frame header.
    #[error("abbreviated stream has no frame header; call load_tables() first")]
    AbbreviatedWithoutFrame,

    /// The stream carries no `SOS`, so nothing could be decoded.
    #[error("no scan (SOS) found before end of stream")]
    NoScan,

    /// A `SOF` with `Y == 0` was not followed by a `DNL` segment.
    #[error("frame declares height 0 but no DNL segment supplies the real height")]
    MissingDnl,

    /// Eight-bit output was requested for a frame of wider precision.
    #[error("frame has {precision}-bit samples; use the u16 decode entry points")]
    PrecisionMismatch {
        /// The frame's sample precision `P`.
        precision: u8,
    },

    /// The encoder was asked for something it cannot express in a JPEG
    /// datastream, or for a combination of options that contradict each other.
    #[error("invalid encoder setting {parameter}: {reason}")]
    InvalidEncodeParameter {
        /// Which setting is at fault, e.g. `"precision"`.
        parameter: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },
}

impl JpegError {
    /// Convenience constructor for [`JpegError::MalformedSegment`].
    pub(crate) fn malformed(segment: &'static str, offset: usize, reason: &'static str) -> Self {
        JpegError::MalformedSegment {
            segment,
            offset: offset as u64,
            reason,
        }
    }

    /// Convenience constructor for [`JpegError::UnexpectedEof`].
    pub(crate) fn eof(context: &'static str) -> Self {
        JpegError::UnexpectedEof { context }
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, JpegError>;

impl From<JpegError> for OxiArcError {
    fn from(err: JpegError) -> Self {
        match err {
            JpegError::Io(e) => OxiArcError::Io(e),
            JpegError::UnexpectedEof { context } => OxiArcError::CorruptedData {
                offset: 0,
                message: format!("unexpected end of JPEG stream in {context}"),
            },
            JpegError::UnexpectedMarker {
                expected,
                found,
                offset,
            } => OxiArcError::CorruptedData {
                offset,
                message: format!("expected JPEG marker {expected:#06X}, found {found:#06X}"),
            },
            JpegError::MalformedSegment {
                segment,
                offset,
                reason,
            } => OxiArcError::InvalidHeader {
                message: format!("malformed JPEG {segment} segment at {offset}: {reason}"),
            },
            JpegError::InvalidHuffmanCode { mcu } | JpegError::InvalidArithmeticCode { mcu } => {
                OxiArcError::InvalidHuffmanCode { bit_position: mcu }
            }
            JpegError::UndefinedTable { kind, index } => OxiArcError::InvalidHeader {
                message: format!("JPEG scan references undefined {kind} table {index}"),
            },
            JpegError::Unsupported(feature) => OxiArcError::UnsupportedMethod {
                method: format!("JPEG: {feature}"),
            },
            JpegError::LimitExceeded(kind) => OxiArcError::InvalidHeader {
                message: format!("JPEG decode limit exceeded: {kind}"),
            },
            JpegError::BufferTooSmall { need, got } => OxiArcError::BufferTooSmall {
                needed: need,
                available: got,
            },
            JpegError::AbbreviatedWithoutFrame => OxiArcError::InvalidHeader {
                message: "abbreviated JPEG stream has no frame header".to_string(),
            },
            JpegError::NoScan => OxiArcError::InvalidHeader {
                message: "JPEG stream has no scan (SOS)".to_string(),
            },
            JpegError::MissingDnl => OxiArcError::InvalidHeader {
                message: "JPEG frame height is 0 and no DNL segment follows".to_string(),
            },
            JpegError::PrecisionMismatch { precision } => OxiArcError::InvalidHeader {
                message: format!("JPEG frame has {precision}-bit samples"),
            },
            JpegError::InvalidEncodeParameter { parameter, reason } => OxiArcError::InvalidHeader {
                message: format!("invalid JPEG encoder setting {parameter}: {reason}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_name_the_feature() {
        let err = JpegError::Unsupported(UnsupportedFeature::Hierarchical);
        assert!(err.to_string().contains("SOF5"));
        let err = JpegError::Unsupported(UnsupportedFeature::ArithmeticCoding);
        assert!(err.to_string().contains("arithmetic"));
    }

    #[test]
    fn limit_kind_display_is_human_readable() {
        assert_eq!(LimitKind::Pixels.to_string(), "pixel count");
        assert_eq!(LimitKind::InputBytes.to_string(), "input bytes");
    }

    #[test]
    fn table_kind_display_is_human_readable() {
        assert_eq!(TableKind::DcHuffman.to_string(), "DC Huffman");
        assert_eq!(
            TableKind::ArithmeticConditioning.to_string(),
            "arithmetic conditioning"
        );
    }

    #[test]
    fn converts_into_oxiarc_error() {
        let err: OxiArcError = JpegError::Unsupported(UnsupportedFeature::Hierarchical).into();
        assert!(matches!(err, OxiArcError::UnsupportedMethod { .. }));

        let err: OxiArcError = JpegError::BufferTooSmall { need: 10, got: 4 }.into();
        assert!(matches!(
            err,
            OxiArcError::BufferTooSmall {
                needed: 10,
                available: 4
            }
        ));

        let err: OxiArcError = JpegError::LimitExceeded(LimitKind::Pixels).into();
        assert!(matches!(err, OxiArcError::InvalidHeader { .. }));
    }
}
