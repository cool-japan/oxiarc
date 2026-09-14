//! Decompression: one-shot helpers, the `core` push interface and the
//! zlib-style `stream` interface.

use std::error::Error;
use std::fmt;

pub mod core;
pub mod stream;

use self::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_IGNORE_ADLER32, TINFL_FLAG_PARSE_ZLIB_HEADER,
    TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use self::core::{DecompressorOxide, decompress};

/// Return status codes.
#[repr(i8)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TINFLStatus {
    /// More input data was expected, but the caller indicated that there
    /// was no more data (`TINFL_FLAG_HAS_MORE_INPUT` unset).
    FailedCannotMakeProgress = -4,
    /// Invalid arguments (e.g. a wrapping buffer that is not a power of
    /// two).
    BadParam = -3,
    /// The zlib Adler-32 trailer did not match the output.
    Adler32Mismatch = -2,
    /// The stream is corrupt.
    Failed = -1,
    /// Decompression completed successfully.
    Done = 0,
    /// The decompressor needs more input.
    NeedsMoreInput = 1,
    /// The output buffer is full; more output is pending.
    HasMoreOutput = 2,
}

impl TINFLStatus {
    /// Convert from the numeric code.
    pub fn from_i32(value: i32) -> Option<TINFLStatus> {
        use self::TINFLStatus::*;
        match value {
            -4 => Some(FailedCannotMakeProgress),
            -3 => Some(BadParam),
            -2 => Some(Adler32Mismatch),
            -1 => Some(Failed),
            0 => Some(Done),
            1 => Some(NeedsMoreInput),
            2 => Some(HasMoreOutput),
            _ => None,
        }
    }
}

/// Struct return when decompress_to_vec functions fail.
#[derive(Debug)]
pub struct DecompressError {
    /// Decompressor status on failure. See [`TINFLStatus`] for details.
    pub status: TINFLStatus,
    /// The currently decompressed data if any.
    pub output: Vec<u8>,
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.status {
            TINFLStatus::FailedCannotMakeProgress => "Truncated input stream",
            TINFLStatus::BadParam => "Invalid output buffer size",
            TINFLStatus::Adler32Mismatch => "Adler32 checksum mismatch",
            TINFLStatus::Failed => "Invalid input data",
            TINFLStatus::Done => "",
            TINFLStatus::NeedsMoreInput => "Truncated input stream",
            TINFLStatus::HasMoreOutput => "Output size exceeded the specified limit",
        })
    }
}

impl Error for DecompressError {}

fn decompress_error(status: TINFLStatus, output: Vec<u8>) -> Result<Vec<u8>, DecompressError> {
    Err(DecompressError { status, output })
}

/// Decompress the deflate-encoded data in `input` to a vector.
///
/// # Errors
///
/// A [`DecompressError`] carrying the status and whatever was decoded.
pub fn decompress_to_vec(input: &[u8]) -> Result<Vec<u8>, DecompressError> {
    decompress_to_vec_inner(input, 0, usize::MAX)
}

/// Decompress the deflate-encoded data (with a zlib wrapper) in `input` to a
/// vector.
///
/// # Errors
///
/// See [`decompress_to_vec`].
pub fn decompress_to_vec_zlib(input: &[u8]) -> Result<Vec<u8>, DecompressError> {
    decompress_to_vec_inner(input, TINFL_FLAG_PARSE_ZLIB_HEADER, usize::MAX)
}

/// Decompress the deflate-encoded data in `input` to a vector, failing with
/// [`TINFLStatus::HasMoreOutput`] once the output would exceed `max_size`.
///
/// # Errors
///
/// See [`decompress_to_vec`].
pub fn decompress_to_vec_with_limit(
    input: &[u8],
    max_size: usize,
) -> Result<Vec<u8>, DecompressError> {
    decompress_to_vec_inner(input, 0, max_size)
}

/// Like [`decompress_to_vec_with_limit`] for zlib-wrapped input.
///
/// # Errors
///
/// See [`decompress_to_vec`].
pub fn decompress_to_vec_zlib_with_limit(
    input: &[u8],
    max_size: usize,
) -> Result<Vec<u8>, DecompressError> {
    decompress_to_vec_inner(input, TINFL_FLAG_PARSE_ZLIB_HEADER, max_size)
}

fn decompress_to_vec_inner(
    input: &[u8],
    flags: u32,
    max_output_size: usize,
) -> Result<Vec<u8>, DecompressError> {
    let flags = flags | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    let initial = input
        .len()
        .saturating_mul(2)
        .max(64)
        .min(max_output_size.max(1));
    let mut ret: Vec<u8> = vec![0; initial];
    let mut decomp = DecompressorOxide::new();
    let mut in_pos = 0;
    let mut out_pos = 0;
    loop {
        let (status, in_consumed, out_consumed) =
            decompress(&mut decomp, &input[in_pos..], &mut ret, out_pos, flags);
        in_pos += in_consumed;
        out_pos += out_consumed;
        match status {
            TINFLStatus::Done => {
                ret.truncate(out_pos);
                return Ok(ret);
            }
            TINFLStatus::HasMoreOutput => {
                if ret.len() >= max_output_size {
                    ret.truncate(out_pos);
                    return decompress_error(TINFLStatus::HasMoreOutput, ret);
                }
                let new_len = ret.len().saturating_mul(2).min(max_output_size);
                ret.resize(new_len, 0);
            }
            other => {
                ret.truncate(out_pos);
                return decompress_error(other, ret);
            }
        }
    }
}

/// Decompress one or more source slices from an iterator into the output
/// slice.
///
/// * On success, returns the number of bytes that were written.
/// * On failure, returns the failure status code.
///
/// # Errors
///
/// The final [`TINFLStatus`] when the stream did not complete.
pub fn decompress_slice_iter_to_slice<'out, 'inp>(
    out: &'out mut [u8],
    it: impl Iterator<Item = &'inp [u8]>,
    zlib_header: bool,
    ignore_adler32: bool,
) -> Result<usize, TINFLStatus> {
    let mut it = it.peekable();
    let mut r = DecompressorOxide::new();
    let mut out_pos = 0;
    while let Some(in_buf) = it.next() {
        let has_more = it.peek().is_some();
        let flags = if zlib_header {
            TINFL_FLAG_PARSE_ZLIB_HEADER
        } else {
            0
        } | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
            | if ignore_adler32 {
                TINFL_FLAG_IGNORE_ADLER32
            } else {
                0
            }
            | if has_more {
                TINFL_FLAG_HAS_MORE_INPUT
            } else {
                0
            };
        let (status, _input_read, bytes_written) = decompress(&mut r, in_buf, out, out_pos, flags);
        out_pos += bytes_written;
        match status {
            TINFLStatus::NeedsMoreInput => continue,
            TINFLStatus::Done => return Ok(out_pos),
            e => return Err(e),
        }
    }
    Err(TINFLStatus::FailedCannotMakeProgress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_vec_roundtrip_and_limit() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 97) as u8).collect();
        let raw = crate::deflate::compress_to_vec(&data, 10);
        assert_eq!(decompress_to_vec(&raw).ok(), Some(data.clone()));
        let z = crate::deflate::compress_to_vec_zlib(&data, 1);
        assert_eq!(decompress_to_vec_zlib(&z).ok(), Some(data.clone()));
        let err = decompress_to_vec_with_limit(&raw, 1000).err();
        assert_eq!(err.map(|e| e.status), Some(TINFLStatus::HasMoreOutput));
        assert_eq!(
            decompress_to_vec_zlib_with_limit(&z, data.len()).ok(),
            Some(data)
        );
    }

    #[test]
    fn corrupt_and_truncated() {
        let err = decompress_to_vec(&[0xff, 0xff, 0xff]).err();
        assert_eq!(err.map(|e| e.status), Some(TINFLStatus::Failed));
        let raw = crate::deflate::compress_to_vec(b"hello hello hello hello", 6);
        let err = decompress_to_vec(&raw[..raw.len() - 2]).err();
        assert_eq!(
            err.map(|e| e.status),
            Some(TINFLStatus::FailedCannotMakeProgress)
        );
    }

    #[test]
    fn slice_iter() {
        let data = b"split across several input slices, split across several".to_vec();
        let z = crate::deflate::compress_to_vec_zlib(&data, 6);
        let pieces: Vec<&[u8]> = z.chunks(5).collect();
        let mut out = vec![0u8; 100];
        let n = decompress_slice_iter_to_slice(&mut out, pieces.into_iter(), true, false);
        assert_eq!(n, Ok(data.len()));
        assert_eq!(&out[..data.len()], &data[..]);
    }
}
