//! Joliet UCS-2 BE filename decoding.
//!
//! ISO 9660 Joliet extensions encode filenames as UCS-2 Big Endian.
//! This module converts them to UTF-8 strings.

/// Decode a UCS-2 Big Endian byte slice into a UTF-8 `String`.
///
/// Trailing NUL characters are trimmed from the result.
pub fn decode_ucs2_be(bytes: &[u8]) -> String {
    let chars: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    String::from_utf16_lossy(&chars)
        .trim_end_matches('\0')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_ucs2_be_basic() {
        // "hello" in UCS-2 BE
        let bytes = b"\x00h\x00e\x00l\x00l\x00o";
        assert_eq!(decode_ucs2_be(bytes), "hello");
    }

    #[test]
    fn test_decode_ucs2_be_with_nul() {
        let bytes = b"\x00h\x00i\x00\x00";
        assert_eq!(decode_ucs2_be(bytes), "hi");
    }

    #[test]
    fn test_decode_ucs2_be_empty() {
        assert_eq!(decode_ucs2_be(b""), "");
    }

    #[test]
    fn test_decode_ucs2_be_odd_length() {
        // Odd byte: chunks_exact truncates the trailing byte
        let bytes = b"\x00h\x00i\x41"; // 'A' as trailing stray byte
        assert_eq!(decode_ucs2_be(bytes), "hi");
    }
}
