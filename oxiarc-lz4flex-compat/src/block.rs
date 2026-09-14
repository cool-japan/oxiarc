//! LZ4 block format: raw blocks with no header, as used by Parquet's
//! `LZ4_RAW` and Hadoop codecs.

use std::error::Error;
use std::fmt;

/// An error representing invalid compressed data.
#[derive(Debug)]
#[non_exhaustive]
pub enum DecompressError {
    /// The provided output is too small.
    OutputTooSmall {
        /// Minimum expected output size.
        expected: usize,
        /// Actual size of output.
        actual: usize,
    },
    /// Literal is out of bounds of the input.
    LiteralOutOfBounds,
    /// Expected another byte, but none found.
    ExpectedAnotherByte,
    /// Deduplication offset out of bounds (not in buffer).
    OffsetOutOfBounds,
    /// A match offset of zero.
    OffsetZero,
}

/// Errors that can happen during compression.
#[derive(Debug)]
#[non_exhaustive]
pub enum CompressError {
    /// The provided output is too small.
    OutputTooSmall,
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecompressError::OutputTooSmall { expected, actual } => write!(
                f,
                "provided output is too small for the decompressed data, actual {actual}, expected \
                 {expected}"
            ),
            DecompressError::LiteralOutOfBounds => {
                f.write_str("literal is out of bounds of the input")
            }
            DecompressError::ExpectedAnotherByte => {
                f.write_str("expected another byte, found none")
            }
            DecompressError::OffsetZero => f.write_str("0 is not a valid match offset"),
            DecompressError::OffsetOutOfBounds => {
                f.write_str("the offset to copy is not contained in the decompressed buffer")
            }
        }
    }
}

impl fmt::Display for CompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompressError::OutputTooSmall => f.write_str(
                "output is too small for the compressed data, use get_maximum_output_size to \
                 reserve enough space",
            ),
        }
    }
}

impl Error for DecompressError {}

impl Error for CompressError {}

/// Returns the maximum output size of the compressed data. Can be used to
/// preallocate capacity on the output vector.
pub const fn get_maximum_output_size(input_len: usize) -> usize {
    16 + 4 + (input_len as u64 * 110 / 100) as usize
}

/// Largest output a single block can decode to given its compressed size
/// (every byte is at most a 255-length continuation).
fn worst_case_expansion(input_len: usize) -> usize {
    input_len.saturating_mul(255).saturating_add(64)
}

/// Classify a decode failure: an output that was merely too small, or
/// corrupt input.
fn classify_failure(input: &[u8], dict: &[u8], actual: usize) -> DecompressError {
    let retry = if dict.is_empty() {
        oxiarc_lz4::decompress_block(input, worst_case_expansion(input.len()))
    } else {
        oxiarc_lz4::decompress_block_dict(input, dict, worst_case_expansion(input.len()))
    };
    match retry {
        Ok(full) if full.len() > actual => DecompressError::OutputTooSmall {
            expected: full.len(),
            actual,
        },
        Ok(_) => DecompressError::LiteralOutOfBounds,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("offset") {
                DecompressError::OffsetOutOfBounds
            } else if msg.contains("truncat") || msg.contains("end of") {
                DecompressError::ExpectedAnotherByte
            } else {
                DecompressError::LiteralOutOfBounds
            }
        }
    }
}

/// Compress all bytes of `input` into `output`. The output must be at least
/// [`get_maximum_output_size`] bytes to be safe. Returns the number of
/// bytes written.
///
/// # Errors
///
/// [`CompressError::OutputTooSmall`] when the block does not fit.
pub fn compress_into(input: &[u8], output: &mut [u8]) -> Result<usize, CompressError> {
    let compressed =
        oxiarc_lz4::compress_block(input).map_err(|_| CompressError::OutputTooSmall)?;
    let dst = output
        .get_mut(..compressed.len())
        .ok_or(CompressError::OutputTooSmall)?;
    dst.copy_from_slice(&compressed);
    Ok(compressed.len())
}

/// Compress all bytes of `input` into `output`, with an external dictionary.
///
/// # Errors
///
/// [`CompressError::OutputTooSmall`] when the block does not fit.
pub fn compress_into_with_dict(
    input: &[u8],
    output: &mut [u8],
    dict_data: &[u8],
) -> Result<usize, CompressError> {
    let compressed = oxiarc_lz4::compress_block_with_dict(input, dict_data, 1)
        .map_err(|_| CompressError::OutputTooSmall)?;
    let dst = output
        .get_mut(..compressed.len())
        .ok_or(CompressError::OutputTooSmall)?;
    dst.copy_from_slice(&compressed);
    Ok(compressed.len())
}

/// Compress all bytes of `input`.
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = vec![0; get_maximum_output_size(input.len())];
    match compress_into(input, &mut out) {
        Ok(n) => {
            out.truncate(n);
            out
        }
        // The bound always fits a block; fall back to the encoder's own
        // allocation should it ever not.
        Err(_) => oxiarc_lz4::compress_block(input).unwrap_or_default(),
    }
}

/// Compress all bytes of `input`, with an external dictionary.
pub fn compress_with_dict(input: &[u8], ext_dict: &[u8]) -> Vec<u8> {
    oxiarc_lz4::compress_block_with_dict(input, ext_dict, 1).unwrap_or_default()
}

/// Compress all bytes of `input` into a vector, prepending the uncompressed
/// size as a little-endian `u32`.
pub fn compress_prepend_size(input: &[u8]) -> Vec<u8> {
    let mut out = (input.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&compress(input));
    out
}

/// Like [`compress_prepend_size`], with an external dictionary.
pub fn compress_prepend_size_with_dict(input: &[u8], ext_dict: &[u8]) -> Vec<u8> {
    let mut out = (input.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&compress_with_dict(input, ext_dict));
    out
}

/// Decompress all bytes of `input` into `output`. `output` should be
/// preallocated with a size of the uncompressed data. Returns the number of
/// bytes written.
///
/// # Errors
///
/// [`DecompressError::OutputTooSmall`] or a corruption variant.
pub fn decompress_into(input: &[u8], output: &mut [u8]) -> Result<usize, DecompressError> {
    decompress_into_with_dict(input, output, &[])
}

/// Decompress all bytes of `input` into `output`, with an external
/// dictionary.
///
/// # Errors
///
/// See [`decompress_into`].
pub fn decompress_into_with_dict(
    input: &[u8],
    output: &mut [u8],
    ext_dict: &[u8],
) -> Result<usize, DecompressError> {
    if input.is_empty() {
        return Err(DecompressError::ExpectedAnotherByte);
    }
    let decoded = if ext_dict.is_empty() {
        oxiarc_lz4::decompress_block(input, output.len())
    } else {
        oxiarc_lz4::decompress_block_dict(input, ext_dict, output.len())
    };
    match decoded {
        Ok(bytes) => {
            output[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
        Err(_) => Err(classify_failure(input, ext_dict, output.len())),
    }
}

/// Decompress all bytes of `input` into a new vec. `min_uncompressed_size`
/// must be at least the uncompressed size.
///
/// # Errors
///
/// See [`decompress_into`].
pub fn decompress(input: &[u8], min_uncompressed_size: usize) -> Result<Vec<u8>, DecompressError> {
    let mut out = vec![0; min_uncompressed_size];
    let n = decompress_into(input, &mut out)?;
    out.truncate(n);
    Ok(out)
}

/// Decompress all bytes of `input` into a new vec, with an external
/// dictionary.
///
/// # Errors
///
/// See [`decompress_into`].
pub fn decompress_with_dict(
    input: &[u8],
    min_uncompressed_size: usize,
    ext_dict: &[u8],
) -> Result<Vec<u8>, DecompressError> {
    let mut out = vec![0; min_uncompressed_size];
    let n = decompress_into_with_dict(input, &mut out, ext_dict)?;
    out.truncate(n);
    Ok(out)
}

/// Read the size prefix written by [`compress_prepend_size`].
///
/// # Errors
///
/// [`DecompressError::ExpectedAnotherByte`] for inputs under 4 bytes.
pub fn uncompressed_size(input: &[u8]) -> Result<(usize, &[u8]), DecompressError> {
    let size = input.get(..4).ok_or(DecompressError::ExpectedAnotherByte)?;
    let uncompressed_size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize;
    Ok((uncompressed_size, &input[4..]))
}

/// Decompress all bytes of `input` into a new vec. The first 4 bytes are
/// the uncompressed size in little endian.
///
/// # Errors
///
/// See [`decompress_into`].
pub fn decompress_size_prepended(input: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let (size, rest) = uncompressed_size(input)?;
    decompress(rest, size)
}

/// Like [`decompress_size_prepended`], with an external dictionary.
///
/// # Errors
///
/// See [`decompress_into`].
pub fn decompress_size_prepended_with_dict(
    input: &[u8],
    ext_dict: &[u8],
) -> Result<Vec<u8>, DecompressError> {
    let (size, rest) = uncompressed_size(input)?;
    decompress_with_dict(rest, size, ext_dict)
}
