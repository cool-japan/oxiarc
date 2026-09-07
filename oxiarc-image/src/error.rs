//! Error types, shaped after `image` 0.25's [`ImageError`]/[`ImageResult`].
//!
//! The six top-level variants and their names match the real crate exactly,
//! since downstream `match` arms and `.to_string()` output depend on them.
//! The opaque wrapper structs ([`DecodingError`], [`EncodingError`], ...) are
//! trimmed to what this crate actually produces: no `Cicp`/HDR-related kinds
//! (this crate never decodes a colour space that needs them), and every
//! `From` conversion from the three underlying codec crates
//! (`oxiarc_png::DecodingError`, `oxiarc_jpeg::JpegError`,
//! `oxiarc_tiff::TiffError`, and their encode-side counterparts) lives here.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use crate::color::ExtendedColorType;
use crate::format::ImageFormat;

/// Result of a decode or encode.
pub type ImageResult<T> = Result<T, ImageError>;

/// The generic error type for image operations.
///
/// Mirrors `image::ImageError`'s six variants so that `match` arms written
/// against the real crate keep compiling.
#[derive(Debug)]
pub enum ImageError {
    /// The input did not conform to its format's specification, or no
    /// format could be determined.
    Decoding(DecodingError),
    /// The image cannot be encoded with the chosen format.
    Encoding(EncodingError),
    /// An error in the caller's arguments.
    Parameter(ParameterError),
    /// Completing the operation would need more resources than a
    /// configured limit allows.
    Limits(LimitError),
    /// The format, colour type or feature is not one this crate implements.
    Unsupported(UnsupportedError),
    /// An I/O error from the underlying reader or writer.
    IoError(io::Error),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decoding(e) => e.fmt(f),
            Self::Encoding(e) => e.fmt(f),
            Self::Parameter(e) => e.fmt(f),
            Self::Limits(e) => e.fmt(f),
            Self::Unsupported(e) => e.fmt(f),
            Self::IoError(e) => e.fmt(f),
        }
    }
}

impl StdError for ImageError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Decoding(e) => e.source(),
            Self::Encoding(e) => e.source(),
            Self::Parameter(e) => e.source(),
            Self::Limits(e) => e.source(),
            Self::Unsupported(e) => e.source(),
            Self::IoError(e) => e.source(),
        }
    }
}

impl From<io::Error> for ImageError {
    fn from(err: io::Error) -> Self {
        Self::IoError(err)
    }
}

/// A best-effort identification of the image format an error concerns.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImageFormatHint {
    /// The format is known exactly.
    Exact(ImageFormat),
    /// The format is known only by a name (e.g. a MIME type).
    Name(String),
    /// Only a path extension is known.
    PathExtension(std::ffi::OsString),
    /// The format could not be determined at all.
    Unknown,
}

impl fmt::Display for ImageFormatHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(format) => write!(f, "{format:?}"),
            Self::Name(name) => write!(f, "`{name}`"),
            Self::PathExtension(ext) => write!(f, "`.{}`", ext.to_string_lossy()),
            Self::Unknown => write!(f, "`Unknown`"),
        }
    }
}

impl From<ImageFormat> for ImageFormatHint {
    fn from(format: ImageFormat) -> Self {
        Self::Exact(format)
    }
}

/// An error encountered while decoding.
#[derive(Debug)]
pub struct DecodingError {
    format: ImageFormatHint,
    underlying: Option<Box<dyn StdError + Send + Sync>>,
}

impl DecodingError {
    /// Build a decoding error from an underlying error.
    pub fn new(format: ImageFormatHint, err: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self {
            format,
            underlying: Some(err.into()),
        }
    }

    /// Build a decoding error naming only the format.
    #[must_use]
    pub fn from_format_hint(format: ImageFormatHint) -> Self {
        Self {
            format,
            underlying: None,
        }
    }

    /// The format this error concerns.
    #[must_use]
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

impl fmt::Display for DecodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.underlying {
            None => write!(f, "Format error decoding {}", self.format),
            Some(u) => write!(f, "Format error decoding {}: {u}", self.format),
        }
    }
}

impl StdError for DecodingError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.underlying.as_deref().map(|e| e as &(dyn StdError))
    }
}

/// An error encountered while encoding.
#[derive(Debug)]
pub struct EncodingError {
    format: ImageFormatHint,
    underlying: Option<Box<dyn StdError + Send + Sync>>,
}

impl EncodingError {
    /// Build an encoding error from an underlying error.
    pub fn new(format: ImageFormatHint, err: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self {
            format,
            underlying: Some(err.into()),
        }
    }

    /// Build an encoding error naming only the format.
    #[must_use]
    pub fn from_format_hint(format: ImageFormatHint) -> Self {
        Self {
            format,
            underlying: None,
        }
    }

    /// The format this error concerns.
    #[must_use]
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.underlying {
            None => write!(f, "Format error encoding {}", self.format),
            Some(u) => write!(f, "Format error encoding {}: {u}", self.format),
        }
    }
}

impl StdError for EncodingError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.underlying.as_deref().map(|e| e as &(dyn StdError))
    }
}

/// How a caller's argument was malformed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParameterErrorKind {
    /// The supplied dimensions do not match the buffer, or are otherwise
    /// invalid (zero, or too large for the target format).
    DimensionMismatch,
    /// A free-form description of the problem.
    Generic(String),
    /// No more data is available (an iterator-style decoder was exhausted).
    NoMoreData,
}

/// An error encountered in the caller's arguments.
#[derive(Debug)]
pub struct ParameterError {
    kind: ParameterErrorKind,
}

impl ParameterError {
    /// Build a parameter error directly from its kind.
    #[must_use]
    pub fn from_kind(kind: ParameterErrorKind) -> Self {
        Self { kind }
    }

    /// The kind of malformed parameter.
    #[must_use]
    pub fn kind(&self) -> ParameterErrorKind {
        self.kind.clone()
    }
}

impl fmt::Display for ParameterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParameterErrorKind::DimensionMismatch => {
                write!(
                    f,
                    "The image's dimensions are either too small or too large"
                )
            }
            ParameterErrorKind::Generic(message) => {
                write!(f, "The parameter is malformed: {message}")
            }
            ParameterErrorKind::NoMoreData => write!(f, "The end of the image has been reached"),
        }
    }
}

impl StdError for ParameterError {}

/// Which resource limit was exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitErrorKind {
    /// The image's dimensions exceed a configured limit.
    DimensionError,
    /// The operation would have allocated more memory than allowed.
    InsufficientMemory,
}

/// Completing the operation would have required more resources than a
/// configured limit allows.
#[derive(Debug)]
pub struct LimitError {
    kind: LimitErrorKind,
}

impl LimitError {
    /// Build a limit error directly from its kind.
    #[must_use]
    pub fn from_kind(kind: LimitErrorKind) -> Self {
        Self { kind }
    }

    /// The kind of limit that was exceeded.
    #[must_use]
    pub fn kind(&self) -> LimitErrorKind {
        self.kind
    }
}

impl From<LimitErrorKind> for LimitError {
    fn from(kind: LimitErrorKind) -> Self {
        Self { kind }
    }
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            LimitErrorKind::DimensionError => write!(f, "Image size exceeds limit"),
            LimitErrorKind::InsufficientMemory => write!(f, "Memory limit exceeded"),
        }
    }
}

impl StdError for LimitError {}

/// What feature could not be handled.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnsupportedErrorKind {
    /// The colour type cannot be handled by the chosen format's encoder or
    /// decoder.
    Color(ExtendedColorType),
    /// The image format itself is not supported.
    Format(ImageFormatHint),
    /// A feature named by a free-form string.
    GenericFeature(String),
}

/// The requested operation is not implemented, or is disabled.
#[derive(Debug)]
pub struct UnsupportedError {
    format: ImageFormatHint,
    kind: UnsupportedErrorKind,
}

impl UnsupportedError {
    /// Build an unsupported-feature error naming both the format and the
    /// feature.
    #[must_use]
    pub fn from_format_and_kind(format: ImageFormatHint, kind: UnsupportedErrorKind) -> Self {
        Self { format, kind }
    }

    /// The feature that could not be handled.
    #[must_use]
    pub fn kind(&self) -> UnsupportedErrorKind {
        self.kind.clone()
    }

    /// The format this error concerns.
    #[must_use]
    pub fn format_hint(&self) -> ImageFormatHint {
        self.format.clone()
    }
}

impl From<ImageFormatHint> for UnsupportedError {
    fn from(hint: ImageFormatHint) -> Self {
        Self {
            format: hint.clone(),
            kind: UnsupportedErrorKind::Format(hint),
        }
    }
}

impl fmt::Display for UnsupportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            UnsupportedErrorKind::Format(ImageFormatHint::Unknown) => {
                write!(f, "The image format could not be determined")
            }
            UnsupportedErrorKind::Format(hint @ ImageFormatHint::PathExtension(_)) => {
                write!(
                    f,
                    "The file extension {hint} was not recognized as an image format"
                )
            }
            UnsupportedErrorKind::Format(hint) => {
                write!(f, "The image format {hint} is not supported")
            }
            UnsupportedErrorKind::Color(color) => write!(
                f,
                "The encoder or decoder for {} does not support the color type `{:?}`",
                self.format, color
            ),
            UnsupportedErrorKind::GenericFeature(message) => write!(f, "{message}"),
        }
    }
}

impl StdError for UnsupportedError {}

/// Build an [`ImageError::Unsupported`] naming a format and a colour type.
pub(crate) fn unsupported_color(format: ImageFormat, color: ExtendedColorType) -> ImageError {
    ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        format.into(),
        UnsupportedErrorKind::Color(color),
    ))
}

// --- Conversions from the three codec crates' own error types ---------

impl From<oxiarc_png::DecodingError> for ImageError {
    fn from(err: oxiarc_png::DecodingError) -> Self {
        if let oxiarc_png::DecodingError::IoError(io_err) = err {
            return Self::IoError(io_err);
        }
        if matches!(err, oxiarc_png::DecodingError::LimitsExceeded) {
            return Self::Limits(LimitErrorKind::InsufficientMemory.into());
        }
        Self::Decoding(DecodingError::new(ImageFormat::Png.into(), err))
    }
}

impl From<oxiarc_png::EncodingError> for ImageError {
    fn from(err: oxiarc_png::EncodingError) -> Self {
        if let oxiarc_png::EncodingError::IoError(io_err) = err {
            return Self::IoError(io_err);
        }
        Self::Encoding(EncodingError::new(ImageFormat::Png.into(), err))
    }
}

impl From<oxiarc_jpeg::JpegError> for ImageError {
    fn from(err: oxiarc_jpeg::JpegError) -> Self {
        if let oxiarc_jpeg::JpegError::Io(io_err) = err {
            return Self::IoError(io_err);
        }
        // oxiarc-jpeg has one error enum for both directions; whether a given
        // variant is "decoding" or "encoding" is not knowable from the value
        // alone, so callers that need the distinction use `Decoding`/
        // `Encoding` at the call site (see `codecs::jpeg`) and this blanket
        // conversion — used by the free functions that decode only — always
        // reports `Decoding`.
        Self::Decoding(DecodingError::new(ImageFormat::Jpeg.into(), err))
    }
}

impl From<oxiarc_tiff::TiffError> for ImageError {
    fn from(err: oxiarc_tiff::TiffError) -> Self {
        match err {
            oxiarc_tiff::TiffError::Io(io_err) => Self::IoError(io_err),
            oxiarc_tiff::TiffError::Limits(_) => {
                Self::Limits(LimitErrorKind::InsufficientMemory.into())
            }
            other => Self::Decoding(DecodingError::new(ImageFormat::Tiff.into(), other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_texts_are_stable_and_non_empty() {
        let err = ImageError::Unsupported(UnsupportedError::from_format_and_kind(
            ImageFormatHint::Unknown,
            UnsupportedErrorKind::Format(ImageFormatHint::Unknown),
        ));
        assert_eq!(err.to_string(), "The image format could not be determined");
    }

    #[test]
    fn io_error_round_trips() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "gone");
        let err: ImageError = io_err.into();
        assert!(matches!(err, ImageError::IoError(_)));
    }

    #[test]
    fn png_limits_exceeded_maps_to_limit_error() {
        let err: ImageError = oxiarc_png::DecodingError::LimitsExceeded.into();
        assert!(matches!(err, ImageError::Limits(_)));
    }

    #[test]
    fn unsupported_color_names_both_format_and_color() {
        let err = unsupported_color(ImageFormat::Png, ExtendedColorType::L1);
        let text = err.to_string();
        assert!(text.contains("Png"));
        assert!(text.contains("L1"));
    }

    #[test]
    fn parameter_and_limit_kinds_are_queryable() {
        let p = ParameterError::from_kind(ParameterErrorKind::DimensionMismatch);
        assert_eq!(p.kind(), ParameterErrorKind::DimensionMismatch);
        let l = LimitError::from_kind(LimitErrorKind::DimensionError);
        assert_eq!(l.kind(), LimitErrorKind::DimensionError);
    }
}
