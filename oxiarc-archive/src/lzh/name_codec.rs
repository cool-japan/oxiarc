//! Filename encoding helpers for LZH archives.
//!
//! LZH filenames are conventionally stored in Shift_JIS (the format
//! originated on Japanese MS-DOS systems, and legacy tools such as LHA and
//! Lhaplus both write and expect Shift_JIS). The writer therefore encodes
//! entry names as Shift_JIS, and the reader decodes Shift_JIS first with a
//! UTF-8 fallback.

use encoding_rs::SHIFT_JIS;

/// Decode an LZH entry name (Shift_JIS first, per LZH convention).
pub(crate) fn decode_lzh_name(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let (decoded, _, had_errors) = SHIFT_JIS.decode(bytes);
    if !had_errors {
        return decoded.into_owned();
    }
    if let Ok(name) = std::str::from_utf8(bytes) {
        return name.to_string();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Encode an LZH entry name as Shift_JIS.
///
/// Falls back to raw UTF-8 bytes only when the name contains characters
/// with no Shift_JIS mapping.
pub(crate) fn encode_lzh_name(name: &str) -> Vec<u8> {
    let (encoded, _, had_errors) = SHIFT_JIS.encode(name);
    if had_errors {
        name.as_bytes().to_vec()
    } else {
        encoded.into_owned()
    }
}

/// Decode a directory-name extension header (type `0x02`) payload.
///
/// Path components are terminated by `0xFF` per the LHA specification;
/// each component is Shift_JIS. Returns the components joined with `/`.
pub(crate) fn decode_lzh_dirname(bytes: &[u8]) -> String {
    bytes
        .split(|&b| b == 0xFF)
        .filter(|component| !component.is_empty())
        .map(decode_lzh_name)
        .collect::<Vec<_>>()
        .join("/")
}

/// Encode a `/`-separated directory path as a type `0x02` extension payload
/// (each component Shift_JIS encoded and terminated by `0xFF`).
pub(crate) fn encode_lzh_dirname(dir: &str) -> Vec<u8> {
    let mut encoded = Vec::new();
    for component in dir.split('/').filter(|c| !c.is_empty()) {
        encoded.extend_from_slice(&encode_lzh_name(component));
        encoded.push(0xFF);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_jis_roundtrip() {
        let encoded = encode_lzh_name("日本語ファイル.txt");
        // Must actually be Shift_JIS, not UTF-8.
        assert_ne!(encoded, "日本語ファイル.txt".as_bytes());
        assert_eq!(decode_lzh_name(&encoded), "日本語ファイル.txt");
    }

    #[test]
    fn ascii_passthrough() {
        assert_eq!(encode_lzh_name("hello.txt"), b"hello.txt");
        assert_eq!(decode_lzh_name(b"hello.txt"), "hello.txt");
    }

    #[test]
    fn dirname_components() {
        let payload = encode_lzh_dirname("日本語/sub");
        assert_eq!(payload.iter().filter(|&&b| b == 0xFF).count(), 2);
        assert_eq!(decode_lzh_dirname(&payload), "日本語/sub");
    }

    #[test]
    fn unmappable_falls_back_to_utf8() {
        // U+1F980 (crab) has no Shift_JIS mapping.
        let name = "🦀.txt";
        let encoded = encode_lzh_name(name);
        assert_eq!(encoded, name.as_bytes());
    }
}
