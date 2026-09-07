//! Parsers for the ancillary chunks.
//!
//! Every function here takes a chunk payload and returns the parsed value or a
//! [`crate::DecodingError`]. Ordering, duplication and CRC policy are decided
//! one level up, in [`crate::decoder::stream`]; these functions only care about
//! whether the bytes make sense.

pub mod color;
pub(crate) mod emit;
pub mod misc;

use crate::chunk::ChunkType;
use crate::error::{DecodingError, FormatErrorKind};

/// Read a big-endian `u32` at `offset`, or fail with a malformed-chunk error.
pub(crate) fn be_u32(data: &[u8], offset: usize, kind: ChunkType) -> Result<u32, DecodingError> {
    data.get(offset..offset + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| FormatErrorKind::MalformedChunk { kind }.into())
}

/// Read a big-endian `i32` at `offset`, or fail with a malformed-chunk error.
pub(crate) fn be_i32(data: &[u8], offset: usize, kind: ChunkType) -> Result<i32, DecodingError> {
    be_u32(data, offset, kind).map(|v| v as i32)
}

/// Read a big-endian `u16` at `offset`, or fail with a malformed-chunk error.
pub(crate) fn be_u16(data: &[u8], offset: usize, kind: ChunkType) -> Result<u16, DecodingError> {
    data.get(offset..offset + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or_else(|| FormatErrorKind::MalformedChunk { kind }.into())
}

/// Split at the first null byte, returning `(before, after)`.
pub(crate) fn split_null(data: &[u8], kind: ChunkType) -> Result<(&[u8], &[u8]), DecodingError> {
    match data.iter().position(|b| *b == 0) {
        Some(i) => Ok((&data[..i], &data[i + 1..])),
        None => Err(FormatErrorKind::MalformedChunk { kind }.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk;

    #[test]
    fn integer_readers_bounds_check() {
        let data = [0x00, 0x00, 0x01, 0x02, 0xFF];
        assert_eq!(be_u32(&data, 0, chunk::pHYs).expect("read"), 0x0102);
        assert_eq!(be_u16(&data, 2, chunk::pHYs).expect("read"), 0x0102);
        assert_eq!(
            be_i32(&[0xFF, 0xFF, 0xFF, 0xFF], 0, chunk::oFFs).expect("read"),
            -1
        );
        assert!(be_u32(&data, 2, chunk::pHYs).is_err());
        assert!(be_u16(&data, 4, chunk::pHYs).is_err());
    }

    #[test]
    fn split_null_finds_the_separator() {
        let (a, b) = split_null(b"name\0rest", chunk::sPLT).expect("split");
        assert_eq!(a, b"name");
        assert_eq!(b, b"rest");
        assert!(split_null(b"no separator", chunk::sPLT).is_err());
    }
}
