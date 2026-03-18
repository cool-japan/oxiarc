//! Snappy block decompression.
//!
//! Implements the Snappy decompression algorithm, parsing literal and
//! copy operations from the compressed block format.

use crate::error::SnappyError;

/// Maximum allowed decompressed size (256 MiB) to prevent memory exhaustion.
const MAX_DECOMPRESSED_SIZE: usize = 256 * 1024 * 1024;

/// Read the varint-encoded decompressed length from the start of compressed data.
///
/// # Arguments
/// * `input` - The compressed data (must start with the varint-encoded length).
///
/// # Returns
/// A tuple of (decompressed_length, bytes_consumed) on success.
///
/// # Errors
/// Returns `SnappyError::UnexpectedEof` if the input is too short.
/// Returns `SnappyError::InvalidLength` if the decoded length exceeds the maximum.
pub fn decompress_len(input: &[u8]) -> Result<(usize, usize), SnappyError> {
    let (value, consumed) = decode_varint(input)?;
    if value > MAX_DECOMPRESSED_SIZE {
        return Err(SnappyError::InvalidLength {
            length: value,
            max_length: MAX_DECOMPRESSED_SIZE,
        });
    }
    Ok((value, consumed))
}

/// Get the decompressed length from compressed Snappy data.
///
/// This is the public API that returns just the length.
///
/// # Arguments
/// * `input` - The compressed data.
///
/// # Returns
/// The decompressed length on success.
pub fn get_decompress_len(input: &[u8]) -> Result<usize, SnappyError> {
    let (len, _) = decompress_len(input)?;
    Ok(len)
}

/// Decompress Snappy block-format data.
///
/// # Arguments
/// * `input` - The compressed data in Snappy block format.
///
/// # Returns
/// A vector containing the decompressed data.
///
/// # Errors
/// Returns an error if the data is corrupted, truncated, or invalid.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>, SnappyError> {
    if input.is_empty() {
        return Err(SnappyError::UnexpectedEof {
            context: "empty input",
        });
    }

    // Read the decompressed length
    let (decompressed_len, mut pos) = decompress_len(input)?;

    if decompressed_len == 0 {
        return Ok(Vec::new());
    }

    let mut output = Vec::with_capacity(decompressed_len);

    while pos < input.len() && output.len() < decompressed_len {
        let tag = input[pos];
        let tag_type = tag & 0x03;
        pos += 1;

        match tag_type {
            0x00 => {
                // Literal
                pos = decode_literal(input, pos, tag, &mut output, decompressed_len)?;
            }
            0x01 => {
                // Copy with 1-byte offset
                pos = decode_copy1(input, pos, tag, &mut output, decompressed_len)?;
            }
            0x02 => {
                // Copy with 2-byte offset
                pos = decode_copy2(input, pos, tag, &mut output, decompressed_len)?;
            }
            0x03 => {
                // Copy with 4-byte offset
                pos = decode_copy4(input, pos, tag, &mut output, decompressed_len)?;
            }
            _ => {
                // This is unreachable since tag_type is masked to 2 bits,
                // but we handle it for completeness
                return Err(SnappyError::InvalidTag {
                    tag,
                    offset: pos - 1,
                });
            }
        }
    }

    if output.len() != decompressed_len {
        return Err(SnappyError::OutputLengthMismatch {
            expected: decompressed_len,
            actual: output.len(),
        });
    }

    Ok(output)
}

/// Decode a literal element.
///
/// The tag byte's upper 6 bits encode the literal length:
/// - 0..=59: literal length is (value + 1)
/// - 60: one extra byte follows with the length
/// - 61: two extra bytes (little-endian)
/// - 62: three extra bytes (little-endian)
/// - 63: four extra bytes (little-endian)
fn decode_literal(
    input: &[u8],
    mut pos: usize,
    tag: u8,
    output: &mut Vec<u8>,
    max_len: usize,
) -> Result<usize, SnappyError> {
    let tag_upper = (tag >> 2) as usize;

    let literal_len = if tag_upper < 60 {
        tag_upper + 1
    } else {
        let extra_bytes = tag_upper - 59;
        if pos + extra_bytes > input.len() {
            return Err(SnappyError::UnexpectedEof {
                context: "literal length bytes",
            });
        }
        let mut len: usize = 0;
        for i in 0..extra_bytes {
            len |= (input[pos + i] as usize) << (i * 8);
        }
        pos += extra_bytes;
        len + 1
    };

    // Validate that we won't exceed the expected output length
    if output.len() + literal_len > max_len {
        return Err(SnappyError::CorruptedData {
            message: format!(
                "literal of length {} would exceed expected output length {}",
                literal_len, max_len
            ),
        });
    }

    if pos + literal_len > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "literal data",
        });
    }

    output.extend_from_slice(&input[pos..pos + literal_len]);
    Ok(pos + literal_len)
}

/// Decode a copy-1 element (1-byte offset, short copy).
///
/// Format: tag byte has OOOOO LLL 01
///   - L = (length - 4), 3 bits -> length 4..11
///   - O = offset, 11 bits (3 from tag, 8 from next byte)
fn decode_copy1(
    input: &[u8],
    pos: usize,
    tag: u8,
    output: &mut Vec<u8>,
    max_len: usize,
) -> Result<usize, SnappyError> {
    if pos >= input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "copy-1 offset byte",
        });
    }

    let length = ((tag >> 2) & 0x07) as usize + 4;
    let offset_hi = ((tag >> 5) & 0x07) as usize;
    let offset_lo = input[pos] as usize;
    let offset = (offset_hi << 8) | offset_lo;

    copy_from_output(output, offset, length, max_len)?;
    Ok(pos + 1)
}

/// Decode a copy-2 element (2-byte offset).
///
/// Format: tag byte has LLLLLL 10
///   - L = (length - 1), 6 bits -> length 1..64
///   - Offset is a 16-bit little-endian value in the next 2 bytes
fn decode_copy2(
    input: &[u8],
    pos: usize,
    tag: u8,
    output: &mut Vec<u8>,
    max_len: usize,
) -> Result<usize, SnappyError> {
    if pos + 2 > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "copy-2 offset bytes",
        });
    }

    let length = ((tag >> 2) & 0x3F) as usize + 1;
    let offset = (input[pos] as usize) | ((input[pos + 1] as usize) << 8);

    copy_from_output(output, offset, length, max_len)?;
    Ok(pos + 2)
}

/// Decode a copy-4 element (4-byte offset).
///
/// Format: tag byte has LLLLLL 11
///   - L = (length - 1), 6 bits -> length 1..64
///   - Offset is a 32-bit little-endian value in the next 4 bytes
fn decode_copy4(
    input: &[u8],
    pos: usize,
    tag: u8,
    output: &mut Vec<u8>,
    max_len: usize,
) -> Result<usize, SnappyError> {
    if pos + 4 > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "copy-4 offset bytes",
        });
    }

    let length = ((tag >> 2) & 0x3F) as usize + 1;
    let offset = (input[pos] as usize)
        | ((input[pos + 1] as usize) << 8)
        | ((input[pos + 2] as usize) << 16)
        | ((input[pos + 3] as usize) << 24);

    copy_from_output(output, offset, length, max_len)?;
    Ok(pos + 4)
}

/// Copy `length` bytes from position `output.len() - offset` within the output.
///
/// This handles overlapping copies correctly (e.g., for run-length encoding
/// where offset < length).
fn copy_from_output(
    output: &mut Vec<u8>,
    offset: usize,
    length: usize,
    max_len: usize,
) -> Result<(), SnappyError> {
    if offset == 0 {
        return Err(SnappyError::InvalidOffset {
            offset,
            position: output.len(),
        });
    }

    if offset > output.len() {
        return Err(SnappyError::InvalidOffset {
            offset,
            position: output.len(),
        });
    }

    if output.len() + length > max_len {
        return Err(SnappyError::CorruptedData {
            message: format!(
                "copy of length {} would exceed expected output length {}",
                length, max_len
            ),
        });
    }

    let start = output.len() - offset;

    // Handle overlapping copies: when offset < length, bytes are repeated
    if offset >= length {
        // Non-overlapping: we can copy the slice directly
        // We need to copy from existing data, so extend from a slice
        output.reserve(length);
        let src_end = start + length;
        // Safe because we checked offset <= output.len()
        // We need to avoid borrow issues by copying byte-by-byte when extending
        // Actually, since we're appending and the source is before current end,
        // we can use a temporary copy for non-overlapping case
        let chunk: Vec<u8> = output[start..src_end].to_vec();
        output.extend_from_slice(&chunk);
    } else {
        // Overlapping copy: copy byte by byte
        output.reserve(length);
        for i in 0..length {
            let byte = output[start + (i % offset)];
            output.push(byte);
        }
    }

    Ok(())
}

/// Decode a varint from the input.
///
/// Returns (value, bytes_consumed).
fn decode_varint(input: &[u8]) -> Result<(usize, usize), SnappyError> {
    let mut value: usize = 0;
    let mut shift = 0u32;

    for (i, &byte) in input.iter().enumerate() {
        if shift >= 35 {
            return Err(SnappyError::CorruptedData {
                message: "varint too long".to_string(),
            });
        }

        let low_bits = (byte & 0x7F) as usize;
        value |= low_bits << shift;

        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }

        shift += 7;
    }

    Err(SnappyError::UnexpectedEof { context: "varint" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_varint() {
        let (val, consumed) = decode_varint(&[0]).expect("should decode");
        assert_eq!(val, 0);
        assert_eq!(consumed, 1);

        let (val, consumed) = decode_varint(&[127]).expect("should decode");
        assert_eq!(val, 127);
        assert_eq!(consumed, 1);

        let (val, consumed) = decode_varint(&[0x80, 0x01]).expect("should decode");
        assert_eq!(val, 128);
        assert_eq!(consumed, 2);

        let (val, consumed) = decode_varint(&[0xAC, 0x02]).expect("should decode");
        assert_eq!(val, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_decode_varint_truncated() {
        let result = decode_varint(&[0x80]);
        assert!(result.is_err());
    }

    #[test]
    fn test_copy_from_output_basic() {
        let mut output = vec![1, 2, 3, 4, 5];
        copy_from_output(&mut output, 3, 3, 100).expect("should succeed");
        assert_eq!(output, vec![1, 2, 3, 4, 5, 3, 4, 5]);
    }

    #[test]
    fn test_copy_from_output_overlapping() {
        let mut output = vec![1, 2, 3];
        // Copy with offset 1, length 5 -> should repeat the last byte
        copy_from_output(&mut output, 1, 5, 100).expect("should succeed");
        assert_eq!(output, vec![1, 2, 3, 3, 3, 3, 3, 3]);
    }

    #[test]
    fn test_copy_from_output_offset_two_repeat() {
        let mut output = vec![0xAB, 0xCD];
        // Copy with offset 2, length 6 -> should repeat AB CD pattern
        copy_from_output(&mut output, 2, 6, 100).expect("should succeed");
        assert_eq!(output, vec![0xAB, 0xCD, 0xAB, 0xCD, 0xAB, 0xCD, 0xAB, 0xCD]);
    }

    #[test]
    fn test_copy_from_output_invalid_offset() {
        let mut output = vec![1, 2, 3];
        let result = copy_from_output(&mut output, 0, 1, 100);
        assert!(result.is_err());

        let result = copy_from_output(&mut output, 10, 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_decompress_empty_input() {
        let result = decompress(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decompress_zero_length() {
        let result = decompress(&[0]).expect("should decompress");
        assert!(result.is_empty());
    }

    #[test]
    fn test_get_decompress_len() {
        // Compressed data starting with varint(300)
        let len = get_decompress_len(&[0xAC, 0x02, 0xFF]).expect("should decode");
        assert_eq!(len, 300);
    }
}
