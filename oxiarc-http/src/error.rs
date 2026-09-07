//! [`HttpCodingError`]: the crate's error type.

use std::fmt;

use thiserror::Error;

use crate::coding::ContentCoding;

/// Errors from HTTP content-coding parsing, negotiation and encoding.
///
/// `#[non_exhaustive]`: a future coding or limit kind must not be a breaking
/// change for callers who already `match` on this type.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpCodingError {
    /// A content coding this build cannot encode or decode.
    ///
    /// Carries the token as received (not case-normalized beyond
    /// [`ContentCoding::parse`]'s own lowercasing of unknown tokens) for
    /// diagnostics, and whether the coding is "known, but not compiled into
    /// this build" versus "unknown to this crate entirely", so a caller can
    /// distinguish "rebuild with `--features brotli`" from "the peer sent
    /// nonsense".
    #[error(
        "unsupported content coding {token:?}{}",
        match reason {
            UnsupportedReason::FeatureDisabled(f) =>
                format!(" (rebuild oxiarc-http with the `{f}` feature)"),
            UnsupportedReason::Unknown => String::new(),
        }
    )]
    UnsupportedCoding {
        /// The coding token as received.
        token: String,
        /// Why this build cannot handle it.
        reason: UnsupportedReason,
    },

    /// A configured [`DecodeLimits`](crate::DecodeLimits) bound was exceeded.
    #[error("{kind} exceeds configured limit {limit}")]
    LimitExceeded {
        /// The configured threshold that was crossed (bytes, a ratio, or a
        /// coding count, depending on `kind` — always representable as an
        /// `f64` for display purposes).
        limit: f64,
        /// Which limit was exceeded, and the value that tripped it.
        kind: LimitKind,
    },

    /// The underlying `oxiarc-*` codec crate reported an error while
    /// encoding or decoding this coding's data (bad checksum, bad framing,
    /// an invalid parameter, or any other codec-level failure).
    #[error("{coding} codec error: {source}")]
    Corrupt {
        /// The coding whose codec reported the error.
        coding: ContentCoding,
        /// The underlying error, boxed so this crate does not need every
        /// optional codec dependency's error type in its own public API.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// Bytes remained after a complete compressed stream and the active
    /// trailing-data policy rejects that.
    #[error("{count} trailing byte(s) after the end of the {coding} stream")]
    TrailingGarbage {
        /// The coding whose stream ended with unexpected trailing bytes.
        coding: ContentCoding,
        /// How many trailing bytes were seen (bounded by however much the
        /// caller had already buffered; not necessarily the true total).
        count: usize,
    },

    /// A header field value could not be parsed, or exceeded a bound meant
    /// to stop a hostile value from costing unbounded work (RFC 9110
    /// §5.6.1.2's "not so much that they could be used as a denial-of-service
    /// mechanism" caveat on tolerating empty list elements).
    #[error("malformed {field} header: {message}")]
    HeaderSyntax {
        /// The header field name, e.g. `"Accept-Encoding"`.
        field: &'static str,
        /// What was wrong.
        message: String,
    },

    /// [`ContentCoding::Dcz`] (or, once supported, [`ContentCoding::Dcb`])
    /// was requested without supplying the shared dictionary it requires.
    #[error("{coding} requires a shared dictionary, but none was supplied")]
    MissingDictionary {
        /// The coding that needed a dictionary.
        coding: ContentCoding,
    },
}

/// Why [`ContentCoding::is_decodable`]/[`is_encodable`](ContentCoding::is_encodable)
/// reported `false` for a coding named in
/// [`HttpCodingError::UnsupportedCoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedReason {
    /// The coding is implemented, but its Cargo feature is off.
    FeatureDisabled(&'static str),
    /// This crate has no implementation for the coding at all (an unknown
    /// token, or a coding permanently blocked on upstream work — see
    /// [`ContentCoding::Compress`] and [`ContentCoding::Dcb`]).
    Unknown,
}

impl fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FeatureDisabled(feature) => write!(f, "feature `{feature}` disabled"),
            Self::Unknown => f.write_str("no implementation in this build"),
        }
    }
}

/// Which configured limit in [`DecodeLimits`](crate::DecodeLimits) was
/// exceeded, together with the value that tripped it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum LimitKind {
    /// [`DecodeLimits::max_output`](crate::DecodeLimits::max_output): total
    /// decoded bytes produced so far.
    Output {
        /// Bytes produced so far (at least the limit).
        produced: u64,
    },
    /// [`DecodeLimits::max_ratio`](crate::DecodeLimits::max_ratio): the
    /// output:input expansion ratio.
    Ratio {
        /// Compressed bytes consumed so far.
        input: u64,
        /// Decoded bytes produced so far.
        output: u64,
    },
    /// [`DecodeLimits::max_codings`](crate::DecodeLimits::max_codings):
    /// number of chained `Content-Encoding` tokens.
    Codings {
        /// Codings counted so far (more than the limit).
        count: usize,
    },
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output { produced } => write!(f, "decoded output ({produced} bytes)"),
            Self::Ratio { input, output } => {
                let ratio = *output as f64 / (*input as f64).max(1.0);
                write!(
                    f,
                    "expansion ratio ({ratio:.1}x, {output} bytes from {input})"
                )
            }
            Self::Codings { count } => write!(f, "chained coding count ({count})"),
        }
    }
}

/// Result alias for this crate's fallible operations.
pub type Result<T> = std::result::Result<T, HttpCodingError>;

impl From<HttpCodingError> for std::io::Error {
    fn from(e: HttpCodingError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_coding_mentions_feature_when_known() {
        let err = HttpCodingError::UnsupportedCoding {
            token: "br".to_string(),
            reason: UnsupportedReason::FeatureDisabled("brotli"),
        };
        let msg = err.to_string();
        assert!(msg.contains("br"));
        assert!(msg.contains("brotli"));
    }

    #[test]
    fn unsupported_coding_omits_feature_hint_when_unknown() {
        let err = HttpCodingError::UnsupportedCoding {
            token: "shrink-o-matic".to_string(),
            reason: UnsupportedReason::Unknown,
        };
        let msg = err.to_string();
        assert!(msg.contains("shrink-o-matic"));
        assert!(!msg.contains("rebuild"));
    }

    #[test]
    fn limit_exceeded_displays_kind_and_limit() {
        let err = HttpCodingError::LimitExceeded {
            limit: 1024.0,
            kind: LimitKind::Output { produced: 2048 },
        };
        let msg = err.to_string();
        assert!(msg.contains("1024"));
        assert!(msg.contains("2048"));
    }

    #[test]
    fn corrupt_error_has_source() {
        use std::error::Error as _;
        let inner: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("boom"));
        let err = HttpCodingError::Corrupt {
            coding: ContentCoding::Gzip,
            source: inner,
        };
        assert!(err.source().is_some());
        assert!(err.to_string().contains("gzip"));
    }

    #[test]
    fn io_conversion_never_panics() {
        let err = HttpCodingError::TrailingGarbage {
            coding: ContentCoding::Zstd,
            count: 3,
        };
        let io_err: std::io::Error = err.into();
        assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_dictionary_message() {
        let err = HttpCodingError::MissingDictionary {
            coding: ContentCoding::Dcz,
        };
        assert!(err.to_string().contains("dcz"));
        assert!(err.to_string().contains("dictionary"));
    }
}
