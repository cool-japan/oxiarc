//! Brotli static dictionary (RFC 7932 Section 8 and Appendix A).
//!
//! The Brotli format includes a built-in static dictionary of 122,784 bytes
//! containing common words and phrases. During decompression, backward
//! references whose distance exceeds the maximum allowed backward distance
//! are treated as references into this dictionary.
//!
//! ## Dictionary Structure
//!
//! The dictionary contains words organized by length (4-24 bytes). The
//! number of words for each length is `1 << NDBITS[length]`, and each word
//! can be modified by one of 121 transforms (Appendix B): a prefix string,
//! one of 21 elementary transforms (identity, UTF-8-aware "ferment"
//! uppercasing, omit-first-k, omit-last-k), and a suffix string.
//!
//! The embedded `dict_data.bin` is the exact Appendix A byte sequence
//! (length 122,784, CRC-32 `0x5136cb04`, verified by a unit test below).

use crate::error::{BrotliError, BrotliResult};

/// Minimum word length in the static dictionary.
pub const MIN_DICTIONARY_WORD_LENGTH: usize = 4;

/// Maximum word length in the static dictionary.
pub const MAX_DICTIONARY_WORD_LENGTH: usize = 24;

/// Number of transforms applied to dictionary words.
pub const NUM_TRANSFORMS: usize = 121;

/// The RFC 7932 Appendix A dictionary bytes (122,784 bytes).
static DICTIONARY_DATA: &[u8] = include_bytes!("dict_data.bin");

/// `NDBITS[length]`: base-2 log of the number of dictionary words for each
/// word length 0..=24 (RFC 7932 Appendix A). Lengths 0-3 have no words.
pub const NDBITS: [u8; 25] = [
    0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5,
];

/// `DOFFSET[length]`: byte offset in the dictionary data where words of the
/// given length begin. Defined by the RFC recursion
/// `DOFFSET[len + 1] = DOFFSET[len] + len * NWORDS[len]`; verified by a test.
pub const DOFFSET: [u32; 25] = [
    0, 0, 0, 0, 0, 4096, 9216, 21504, 35840, 44032, 53248, 63488, 74752, 87040, 93696, 100864,
    104704, 106752, 108928, 113536, 115968, 118528, 119872, 121280, 122016,
];

/// Number of words in the dictionary for the given word length.
pub fn num_words(word_length: usize) -> u32 {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return 0;
    }
    1u32 << NDBITS[word_length]
}

/// Get the number of dictionary index bits for a given word length.
pub fn dictionary_size_bits(word_length: usize) -> u8 {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return 0;
    }
    NDBITS[word_length]
}

/// Check if the dictionary has words for a given length.
pub fn has_dictionary_words(word_length: usize) -> bool {
    num_words(word_length) > 0
}

/// Look up a base word from the static dictionary.
///
/// Returns the dictionary word of the specified length at the given index,
/// or an error if the reference is out of range.
pub fn lookup_word(word_length: usize, word_index: u32) -> BrotliResult<&'static [u8]> {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return Err(BrotliError::DictionaryError(format!(
            "word length {word_length} out of range [{MIN_DICTIONARY_WORD_LENGTH}, {MAX_DICTIONARY_WORD_LENGTH}]"
        )));
    }
    let nwords = num_words(word_length);
    if word_index >= nwords {
        return Err(BrotliError::DictionaryError(format!(
            "word index {word_index} exceeds count {nwords} for length {word_length}"
        )));
    }
    let offset = DOFFSET[word_length] as usize + (word_index as usize) * word_length;
    let end = offset + word_length;
    DICTIONARY_DATA
        .get(offset..end)
        .ok_or_else(|| BrotliError::DictionaryError("dictionary data out of bounds".to_string()))
}

/// The 21 elementary transforms of RFC 7932 Section 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformType {
    /// Use the word as-is.
    Identity,
    /// UTF-8-aware uppercase of the first character ("FermentFirst").
    FermentFirst,
    /// UTF-8-aware uppercase of every character ("FermentAll").
    FermentAll,
    /// Omit the first N bytes (1..=9).
    OmitFirst(u8),
    /// Omit the last N bytes (1..=9).
    OmitLast(u8),
}

impl TransformType {
    /// The single-byte identifier used by the Appendix B serialization
    /// (0 = Identity, 1 = FermentFirst, 2 = FermentAll, 3..=11 =
    /// OmitFirst1..9, 12..=20 = OmitLast1..9).
    #[cfg(test)]
    fn appendix_b_id(self) -> u8 {
        match self {
            TransformType::Identity => 0,
            TransformType::FermentFirst => 1,
            TransformType::FermentAll => 2,
            TransformType::OmitFirst(n) => 2 + n,
            TransformType::OmitLast(n) => 11 + n,
        }
    }
}

/// A Brotli dictionary word transform: `prefix + T(word) + suffix`.
#[derive(Debug, Clone, Copy)]
pub struct Transform {
    /// Byte string prepended to the transformed word.
    pub prefix: &'static [u8],
    /// The elementary transform applied to the base word.
    pub transform_type: TransformType,
    /// Byte string appended to the transformed word.
    pub suffix: &'static [u8],
}

/// Shorthand constructor used by the static table below.
const fn t(prefix: &'static [u8], tt: TransformType, suffix: &'static [u8]) -> Transform {
    Transform {
        prefix,
        transform_type: tt,
        suffix,
    }
}

use TransformType::{FermentAll, FermentFirst, Identity, OmitFirst, OmitLast};

/// All 121 word transformations from RFC 7932 Appendix B, in order.
///
/// A unit test checks this table against the Appendix B serialization
/// length (648 bytes) and CRC-32 (`0x3d965f81`).
pub static TRANSFORMS: [Transform; NUM_TRANSFORMS] = [
    t(b"", Identity, b""),              // 0
    t(b"", Identity, b" "),             // 1
    t(b" ", Identity, b" "),            // 2
    t(b"", OmitFirst(1), b""),          // 3
    t(b"", FermentFirst, b" "),         // 4
    t(b"", Identity, b" the "),         // 5
    t(b" ", Identity, b""),             // 6
    t(b"s ", Identity, b" "),           // 7
    t(b"", Identity, b" of "),          // 8
    t(b"", FermentFirst, b""),          // 9
    t(b"", Identity, b" and "),         // 10
    t(b"", OmitFirst(2), b""),          // 11
    t(b"", OmitLast(1), b""),           // 12
    t(b", ", Identity, b" "),           // 13
    t(b"", Identity, b", "),            // 14
    t(b" ", FermentFirst, b" "),        // 15
    t(b"", Identity, b" in "),          // 16
    t(b"", Identity, b" to "),          // 17
    t(b"e ", Identity, b" "),           // 18
    t(b"", Identity, b"\""),            // 19
    t(b"", Identity, b"."),             // 20
    t(b"", Identity, b"\">"),           // 21
    t(b"", Identity, b"\n"),            // 22
    t(b"", OmitLast(3), b""),           // 23
    t(b"", Identity, b"]"),             // 24
    t(b"", Identity, b" for "),         // 25
    t(b"", OmitFirst(3), b""),          // 26
    t(b"", OmitLast(2), b""),           // 27
    t(b"", Identity, b" a "),           // 28
    t(b"", Identity, b" that "),        // 29
    t(b" ", FermentFirst, b""),         // 30
    t(b"", Identity, b". "),            // 31
    t(b".", Identity, b""),             // 32
    t(b" ", Identity, b", "),           // 33
    t(b"", OmitFirst(4), b""),          // 34
    t(b"", Identity, b" with "),        // 35
    t(b"", Identity, b"'"),             // 36
    t(b"", Identity, b" from "),        // 37
    t(b"", Identity, b" by "),          // 38
    t(b"", OmitFirst(5), b""),          // 39
    t(b"", OmitFirst(6), b""),          // 40
    t(b" the ", Identity, b""),         // 41
    t(b"", OmitLast(4), b""),           // 42
    t(b"", Identity, b". The "),        // 43
    t(b"", FermentAll, b""),            // 44
    t(b"", Identity, b" on "),          // 45
    t(b"", Identity, b" as "),          // 46
    t(b"", Identity, b" is "),          // 47
    t(b"", OmitLast(7), b""),           // 48
    t(b"", OmitLast(1), b"ing "),       // 49
    t(b"", Identity, b"\n\t"),          // 50
    t(b"", Identity, b":"),             // 51
    t(b" ", Identity, b". "),           // 52
    t(b"", Identity, b"ed "),           // 53
    t(b"", OmitFirst(9), b""),          // 54
    t(b"", OmitFirst(7), b""),          // 55
    t(b"", OmitLast(6), b""),           // 56
    t(b"", Identity, b"("),             // 57
    t(b"", FermentFirst, b", "),        // 58
    t(b"", OmitLast(8), b""),           // 59
    t(b"", Identity, b" at "),          // 60
    t(b"", Identity, b"ly "),           // 61
    t(b" the ", Identity, b" of "),     // 62
    t(b"", OmitLast(5), b""),           // 63
    t(b"", OmitLast(9), b""),           // 64
    t(b" ", FermentFirst, b", "),       // 65
    t(b"", FermentFirst, b"\""),        // 66
    t(b".", Identity, b"("),            // 67
    t(b"", FermentAll, b" "),           // 68
    t(b"", FermentFirst, b"\">"),       // 69
    t(b"", Identity, b"=\""),           // 70
    t(b" ", Identity, b"."),            // 71
    t(b".com/", Identity, b""),         // 72
    t(b" the ", Identity, b" of the "), // 73
    t(b"", FermentFirst, b"'"),         // 74
    t(b"", Identity, b". This "),       // 75
    t(b"", Identity, b","),             // 76
    t(b".", Identity, b" "),            // 77
    t(b"", FermentFirst, b"("),         // 78
    t(b"", FermentFirst, b"."),         // 79
    t(b"", Identity, b" not "),         // 80
    t(b" ", Identity, b"=\""),          // 81
    t(b"", Identity, b"er "),           // 82
    t(b" ", FermentAll, b" "),          // 83
    t(b"", Identity, b"al "),           // 84
    t(b" ", FermentAll, b""),           // 85
    t(b"", Identity, b"='"),            // 86
    t(b"", FermentAll, b"\""),          // 87
    t(b"", FermentFirst, b". "),        // 88
    t(b" ", Identity, b"("),            // 89
    t(b"", Identity, b"ful "),          // 90
    t(b" ", FermentFirst, b". "),       // 91
    t(b"", Identity, b"ive "),          // 92
    t(b"", Identity, b"less "),         // 93
    t(b"", FermentAll, b"'"),           // 94
    t(b"", Identity, b"est "),          // 95
    t(b" ", FermentFirst, b"."),        // 96
    t(b"", FermentAll, b"\">"),         // 97
    t(b" ", Identity, b"='"),           // 98
    t(b"", FermentFirst, b","),         // 99
    t(b"", Identity, b"ize "),          // 100
    t(b"", FermentAll, b"."),           // 101
    t(b"\xc2\xa0", Identity, b""),      // 102
    t(b" ", Identity, b","),            // 103
    t(b"", FermentFirst, b"=\""),       // 104
    t(b"", FermentAll, b"=\""),         // 105
    t(b"", Identity, b"ous "),          // 106
    t(b"", FermentAll, b", "),          // 107
    t(b"", FermentFirst, b"='"),        // 108
    t(b" ", FermentFirst, b","),        // 109
    t(b" ", FermentAll, b"=\""),        // 110
    t(b" ", FermentAll, b", "),         // 111
    t(b"", FermentAll, b","),           // 112
    t(b"", FermentAll, b"("),           // 113
    t(b"", FermentAll, b". "),          // 114
    t(b" ", FermentAll, b"."),          // 115
    t(b"", FermentAll, b"='"),          // 116
    t(b" ", FermentAll, b". "),         // 117
    t(b" ", FermentFirst, b"=\""),      // 118
    t(b" ", FermentAll, b"='"),         // 119
    t(b" ", FermentFirst, b"='"),       // 120
];

/// UTF-8-aware uppercasing step of RFC 7932 Section 8 ("Ferment").
///
/// Modifies the character starting at `pos` in place and returns the number
/// of bytes consumed (1 for ASCII/invalid, 2 or 3 for multi-byte starts).
fn ferment(word: &mut [u8], pos: usize) -> usize {
    if word[pos] < 192 {
        if word[pos].is_ascii_lowercase() {
            word[pos] ^= 32;
        }
        1
    } else if word[pos] < 224 {
        if pos + 1 < word.len() {
            word[pos + 1] ^= 32;
        }
        2
    } else {
        if pos + 2 < word.len() {
            word[pos + 2] ^= 5;
        }
        3
    }
}

/// Apply transform `transform_id` to a base dictionary word, appending the
/// result to `out`. Returns the number of bytes appended.
pub fn apply_transform_to(
    word: &[u8],
    transform_id: usize,
    out: &mut Vec<u8>,
) -> BrotliResult<usize> {
    let transform = TRANSFORMS.get(transform_id).ok_or_else(|| {
        BrotliError::DictionaryError(format!("invalid transform id {transform_id}"))
    })?;

    let start = out.len();
    out.extend_from_slice(transform.prefix);

    match transform.transform_type {
        Identity => out.extend_from_slice(word),
        FermentFirst => {
            let body_start = out.len();
            out.extend_from_slice(word);
            let body = &mut out[body_start..];
            if !body.is_empty() {
                ferment(body, 0);
            }
        }
        FermentAll => {
            let body_start = out.len();
            out.extend_from_slice(word);
            let body = &mut out[body_start..];
            let mut i = 0;
            while i < body.len() {
                i += ferment(body, i);
            }
        }
        OmitFirst(n) => {
            let n = n as usize;
            if word.len() > n {
                out.extend_from_slice(&word[n..]);
            }
        }
        OmitLast(n) => {
            let n = n as usize;
            if word.len() > n {
                out.extend_from_slice(&word[..word.len() - n]);
            }
        }
    }

    out.extend_from_slice(transform.suffix);
    Ok(out.len() - start)
}

/// Apply a transform to a dictionary word, returning a new `Vec<u8>`.
pub fn apply_transform(word: &[u8], transform_id: usize) -> BrotliResult<Vec<u8>> {
    let mut out = Vec::with_capacity(word.len() + 16);
    apply_transform_to(word, transform_id, &mut out)?;
    Ok(out)
}

/// Get the number of transforms (always 121 per RFC 7932).
pub fn num_transforms() -> usize {
    NUM_TRANSFORMS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-32 as defined in RFC 7932 Appendix C.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            let mut c = (crc ^ byte as u32) & 0xFF;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            crc = c ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    /// The embedded dictionary must be the exact Appendix A byte sequence.
    #[test]
    fn test_dictionary_data_matches_rfc() {
        assert_eq!(DICTIONARY_DATA.len(), 122_784, "dictionary length");
        assert_eq!(crc32(DICTIONARY_DATA), 0x5136_cb04, "dictionary CRC-32");
        assert_eq!(&DICTIONARY_DATA[..16], b"timedownlifeleft");
    }

    /// DOFFSET must satisfy the RFC recursion from NDBITS, and the total
    /// size must equal the dictionary length.
    #[test]
    fn test_doffset_recursion() {
        let mut offset = 0u32;
        for len in 0..25usize {
            assert_eq!(DOFFSET[len], offset, "DOFFSET[{len}]");
            let nwords = if len >= 4 { 1u32 << NDBITS[len] } else { 0 };
            offset += (len as u32) * nwords;
        }
        assert_eq!(offset as usize, DICTIONARY_DATA.len(), "DICTSIZE");
    }

    /// The transform table must serialize to the Appendix B check values:
    /// prefix + NUL + id byte + suffix + NUL per transform, 648 bytes total,
    /// CRC-32 0x3d965f81.
    #[test]
    fn test_transforms_match_rfc_crc32() {
        let mut serialized = Vec::new();
        for tr in TRANSFORMS.iter() {
            serialized.extend_from_slice(tr.prefix);
            serialized.push(0);
            serialized.push(tr.transform_type.appendix_b_id());
            serialized.extend_from_slice(tr.suffix);
            serialized.push(0);
        }
        assert_eq!(serialized.len(), 648, "Appendix B serialization length");
        assert_eq!(crc32(&serialized), 0x3d96_5f81, "Appendix B CRC-32");
    }

    #[test]
    fn test_lookup_first_words() {
        assert_eq!(lookup_word(4, 0).ok(), Some(&b"time"[..]));
        assert_eq!(lookup_word(4, 1).ok(), Some(&b"down"[..]));
        assert_eq!(lookup_word(4, 2).ok(), Some(&b"life"[..]));
        // Out-of-range lookups must fail.
        assert!(lookup_word(3, 0).is_err());
        assert!(lookup_word(25, 0).is_err());
        assert!(lookup_word(4, num_words(4)).is_err());
    }

    #[test]
    fn test_transform_identity_and_suffixes() {
        assert_eq!(apply_transform(b"hello", 0).ok(), Some(b"hello".to_vec()));
        assert_eq!(apply_transform(b"hello", 1).ok(), Some(b"hello ".to_vec()));
        assert_eq!(apply_transform(b"hello", 2).ok(), Some(b" hello ".to_vec()));
        assert_eq!(apply_transform(b"hello", 3).ok(), Some(b"ello".to_vec()));
        assert_eq!(apply_transform(b"hello", 9).ok(), Some(b"Hello".to_vec()));
        assert_eq!(apply_transform(b"hello", 44).ok(), Some(b"HELLO".to_vec()));
        assert_eq!(apply_transform(b"hello", 12).ok(), Some(b"hell".to_vec()));
        assert!(apply_transform(b"hello", 121).is_err());
    }

    #[test]
    fn test_ferment_utf8_awareness() {
        // 2-byte UTF-8 sequence: 0xC3 0xA9 = 'é' -> XOR 32 on the second
        // byte -> 0xC3 0x89 = 'É'.
        let word = [0xC3u8, 0xA9, b'x'];
        let out = apply_transform(&word, 44).expect("ferment all");
        assert_eq!(out, vec![0xC3, 0x89, b'X']);
        // 3-byte sequence: XOR 5 on the third byte, then continue.
        let word3 = [0xE3u8, 0x81, 0x82];
        let out3 = apply_transform(&word3, 9).expect("ferment first");
        assert_eq!(out3, vec![0xE3, 0x81, 0x87]);
    }

    #[test]
    fn test_omit_transforms_bounds() {
        // OmitFirstK / OmitLastK with k >= len yield the empty string.
        assert_eq!(apply_transform(b"abc", 34).ok(), Some(b"".to_vec())); // OmitFirst4
        assert_eq!(apply_transform(b"abc", 42).ok(), Some(b"".to_vec())); // OmitLast4
    }

    #[test]
    fn test_num_words_table() {
        assert_eq!(num_words(4), 1024);
        assert_eq!(num_words(6), 2048);
        assert_eq!(num_words(24), 32);
        assert_eq!(num_words(3), 0);
        assert_eq!(num_words(25), 0);
    }
}
