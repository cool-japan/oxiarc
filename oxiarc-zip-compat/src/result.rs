//! Error types that can be emitted from this library.

use std::borrow::Cow;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;

use oxiarc_core::error::OxiArcError;

/// Generic result type with ZipError as its error variant.
pub type ZipResult<T> = Result<T, ZipError>;

/// Error type for Zip.
#[derive(Debug)]
#[non_exhaustive]
pub enum ZipError {
    /// i/o error
    Io(io::Error),
    /// invalid Zip archive
    InvalidArchive(Cow<'static, str>),
    /// unsupported Zip archive
    UnsupportedArchive(&'static str),
    /// specified file not found in archive
    FileNotFound,
    /// provided password is incorrect
    InvalidPassword,
    /// The compression method is not supported.
    CompressionMethodNotSupported(u16),
}

impl ZipError {
    /// The text used as an error when a password is required and not
    /// supplied.
    pub const PASSWORD_REQUIRED: &'static str = "Password required to decrypt file";
}

impl Display for ZipError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::InvalidArchive(e) => write!(f, "invalid Zip archive: {e}"),
            Self::UnsupportedArchive(e) => write!(f, "unsupported Zip archive: {e}"),
            Self::FileNotFound => f.write_str("specified file not found in archive"),
            Self::InvalidPassword => f.write_str("provided password is incorrect"),
            Self::CompressionMethodNotSupported(id) => {
                write!(f, "compression method not supported: {id}")
            }
        }
    }
}

impl Error for ZipError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ZipError> for io::Error {
    fn from(err: ZipError) -> io::Error {
        let kind = match &err {
            ZipError::Io(err) => err.kind(),
            ZipError::InvalidArchive(_) => io::ErrorKind::InvalidData,
            ZipError::UnsupportedArchive(_) | ZipError::CompressionMethodNotSupported(_) => {
                io::ErrorKind::Unsupported
            }
            ZipError::FileNotFound => io::ErrorKind::NotFound,
            ZipError::InvalidPassword => io::ErrorKind::InvalidInput,
        };
        io::Error::new(kind, err)
    }
}

impl From<io::Error> for ZipError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<std::string::FromUtf8Error> for ZipError {
    fn from(_: std::string::FromUtf8Error) -> Self {
        ZipError::InvalidArchive(Cow::Borrowed("Invalid UTF-8"))
    }
}

impl From<DateTimeRangeError> for ZipError {
    fn from(_: DateTimeRangeError) -> Self {
        ZipError::InvalidArchive(Cow::Borrowed("Invalid date or time"))
    }
}

/// Map an oxiarc error to the zip-crate variant a caller would expect.
pub(crate) fn from_oxiarc(err: OxiArcError) -> ZipError {
    match err {
        OxiArcError::Io(e) => ZipError::Io(e),
        OxiArcError::UnsupportedMethod { .. } => {
            ZipError::UnsupportedArchive("Compression method not supported")
        }
        other => ZipError::InvalidArchive(Cow::Owned(other.to_string())),
    }
}

/// Error type for time parsing.
#[derive(Debug)]
pub struct DateTimeRangeError;

impl From<std::num::TryFromIntError> for DateTimeRangeError {
    fn from(_value: std::num::TryFromIntError) -> Self {
        DateTimeRangeError
    }
}

impl Display for DateTimeRangeError {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "a date could not be represented within the bounds the MS-DOS date range (1980-2107)"
        )
    }
}

impl Error for DateTimeRangeError {}
