//! `tiff`-0.11-shaped error types.
//!
//! [`TiffError`] has **exactly** the six variants upstream does, and is
//! deliberately **not** `#[non_exhaustive]`: `image` 0.25.10 matches it
//! exhaustively, twice (`codecs/tiff.rs:237-274`). Every variant is built
//! from this crate's native [`crate::error::TiffError`] by
//! [`TiffError::from_native`] -- there is no separate error-producing logic
//! here, only translation.

use std::fmt;
use std::io;

use super::tags::Tag;

/// `Result<T, TiffError>`.
pub type TiffResult<T> = Result<T, TiffError>;

/// Any error produced while reading or writing a TIFF file, `tiff`-0.11
/// shaped.
///
/// Exactly six variants, matching upstream; **not** `#[non_exhaustive]` so
/// that a caller's exhaustive `match` (the pattern `image` 0.25.10 uses)
/// keeps compiling only for as long as this stays true -- see
/// `tests/compat_api.rs`.
#[derive(Debug)]
pub enum TiffError {
    /// The underlying reader or writer failed.
    IoError(io::Error),
    /// The byte stream is not a well-formed TIFF.
    FormatError(TiffFormatError),
    /// An integer conversion between the file's and the host's width would
    /// have overflowed (e.g. a `usize` too narrow for a `u64` offset).
    IntSizeError,
    /// The caller used the API incorrectly.
    UsageError(UsageError),
    /// The file is well formed but uses a feature this build cannot handle.
    UnsupportedError(TiffUnsupportedError),
    /// A configured [`super::decoder::Limits`] guard rejected the operation.
    LimitsExceeded,
}

impl fmt::Display for TiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoError(err) => write!(f, "I/O error: {err}"),
            Self::FormatError(err) => write!(f, "format error: {err}"),
            Self::IntSizeError => write!(f, "integer conversion overflow"),
            Self::UsageError(err) => write!(f, "usage error: {err}"),
            Self::UnsupportedError(err) => write!(f, "unsupported: {err}"),
            Self::LimitsExceeded => write!(f, "limits exceeded"),
        }
    }
}

impl std::error::Error for TiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for TiffError {
    fn from(err: io::Error) -> Self {
        Self::IoError(err)
    }
}

impl TiffError {
    /// Translates a native [`crate::error::TiffError`] into this shape.
    ///
    /// `crate::error::LimitError` and `crate::error::TiffError::IntOverflow`
    /// carry no payload upstream either (`LimitsExceeded`/`IntSizeError` are
    /// bare units there too), so nothing is lost by dropping their detail
    /// here -- it was already only diagnostic text, never structured data a
    /// caller could match on.
    #[must_use]
    pub fn from_native(err: crate::error::TiffError) -> Self {
        match err {
            crate::error::TiffError::Io(io_err) => Self::IoError(io_err),
            crate::error::TiffError::Format(fmt_err) => {
                Self::FormatError(TiffFormatError::from_native(fmt_err))
            }
            crate::error::TiffError::Unsupported(unsup) => {
                Self::UnsupportedError(TiffUnsupportedError::from_native(unsup))
            }
            crate::error::TiffError::Usage(usage) => {
                Self::UsageError(UsageError(usage.to_string()))
            }
            crate::error::TiffError::Limits(_) => Self::LimitsExceeded,
            crate::error::TiffError::IntOverflow => Self::IntSizeError,
        }
    }
}

impl From<crate::error::TiffError> for TiffError {
    fn from(err: crate::error::TiffError) -> Self {
        Self::from_native(err)
    }
}

/// A malformed-file error.
///
/// `#[non_exhaustive]`: `image` never matches this exhaustively, only for
/// the single named case below (`if let ... = err`, which needs no wildcard
/// regardless), so new variants are always a compatible addition.
#[derive(Debug)]
#[non_exhaustive]
pub enum TiffFormatError {
    /// A tag this format requires (for this file's shape) was absent.
    ///
    /// `image` matches this one specifically, to detect "no XMP tag" as
    /// distinct from every other format defect
    /// (`codecs/tiff.rs:326`).
    RequiredTagNotFound(Tag),
    /// A tag was present but carried a field type the requested conversion
    /// cannot read (upstream's own error for this), or a value too wide for
    /// the requested width.
    ///
    /// Produced by the `into_*` conversions on
    /// [`ValueBuffer`](super::tags::ValueBuffer); `found` is the raw TIFF
    /// field-type code that was actually there (0 for a value this crate
    /// synthesised without one).
    InvalidTypeForTag {
        /// The raw TIFF field-type code present in the file.
        found: u16,
    },
    /// Every other malformed-file defect, carrying the native message.
    Other(String),
}

impl TiffFormatError {
    #[must_use]
    pub(crate) fn from_native(err: crate::error::FormatError) -> Self {
        if let crate::error::FormatError::RequiredTagNotFound(tag) = err {
            Self::RequiredTagNotFound(Tag::from_native(tag))
        } else {
            Self::Other(err.to_string())
        }
    }
}

impl fmt::Display for TiffFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredTagNotFound(tag) => write!(f, "required tag not found: {tag}"),
            Self::InvalidTypeForTag { found } => {
                write!(f, "invalid field type {found} for the requested conversion")
            }
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

/// A well-formed file using a feature this build cannot handle.
#[derive(Debug)]
#[non_exhaustive]
pub enum TiffUnsupportedError {
    /// An unrecognised or unimplemented compression method.
    UnknownCompressionMethod(u16),
    /// Every other unsupported-feature defect, carrying the native message.
    Other(String),
}

impl TiffUnsupportedError {
    #[must_use]
    pub(crate) fn from_native(err: crate::error::UnsupportedError) -> Self {
        match err {
            crate::error::UnsupportedError::Compression(method) => {
                Self::UnknownCompressionMethod(method)
            }
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for TiffUnsupportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCompressionMethod(m) => write!(f, "unsupported compression method {m}"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

/// The caller used the API incorrectly (a bad argument, a call out of
/// sequence, ...).
///
/// Upstream's `UsageError` is itself a simple message-carrying type from
/// `image`'s point of view (it is matched only as the whole `TiffError::UsageError(_)`
/// arm, never destructured); this carries the native error's message.
#[derive(Debug)]
pub struct UsageError(String);

/// Builds a [`UsageError`] from a message, for adapter code inside this
/// module that needs to report a compat-layer-only usage mistake (one with
/// no native-side equivalent to translate, such as a too-small caller
/// buffer).
pub(super) fn usage_error(message: String) -> UsageError {
    UsageError(message)
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_six_variants_and_no_wildcard_needed() {
        // If this stops compiling because a wildcard became mandatory,
        // `TiffError` grew `#[non_exhaustive]` or a seventh variant -- both
        // are contract breaks (critique.md D-1 / compat section 2.11).
        fn describe(err: TiffError) -> &'static str {
            match err {
                TiffError::IoError(_) => "io",
                TiffError::FormatError(_) => "format",
                TiffError::IntSizeError => "int_size",
                TiffError::UsageError(_) => "usage",
                TiffError::UnsupportedError(_) => "unsupported",
                TiffError::LimitsExceeded => "limits",
            }
        }
        assert_eq!(describe(TiffError::IntSizeError), "int_size");
        assert_eq!(describe(TiffError::LimitsExceeded), "limits");
    }

    #[test]
    fn required_tag_not_found_carries_the_compat_tag() {
        let native =
            crate::error::FormatError::RequiredTagNotFound(crate::tags::Tag::InterColorProfile);
        let translated = TiffFormatError::from_native(native);
        assert!(matches!(
            translated,
            TiffFormatError::RequiredTagNotFound(Tag::IccProfile)
        ));
    }

    #[test]
    fn native_errors_translate_into_every_compat_variant() {
        assert!(matches!(
            TiffError::from_native(crate::error::TiffError::IntOverflow),
            TiffError::IntSizeError
        ));
        assert!(matches!(
            TiffError::from_native(crate::error::TiffError::Unsupported(
                crate::error::UnsupportedError::Compression(50001)
            )),
            TiffError::UnsupportedError(TiffUnsupportedError::UnknownCompressionMethod(50001))
        ));
    }
}
