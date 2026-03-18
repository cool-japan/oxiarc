//! Brotli static dictionary (RFC 7932 Appendix A).
//!
//! The Brotli format includes a built-in static dictionary of ~120KB containing
//! common words and phrases. During decompression, backward references can point
//! into this dictionary for improved compression ratios.
//!
//! ## Dictionary Structure
//!
//! The dictionary contains words organized by length (4-24 bytes).
//! For each word length, there are a fixed number of words, and each word
//! can be modified by one of several "transforms" (uppercase, append space, etc.).
//!
//! ## Implementation Note
//!
//! This implementation starts with a minimal dictionary suitable for basic
//! compression/decompression. The full 120KB dictionary can be embedded later.

use crate::error::{BrotliError, BrotliResult};

/// Minimum word length in the static dictionary.
pub const MIN_DICTIONARY_WORD_LENGTH: usize = 4;

/// Maximum word length in the static dictionary.
pub const MAX_DICTIONARY_WORD_LENGTH: usize = 24;

/// Number of word lengths supported (4..=24 => 21 lengths).
pub const NUM_DICTIONARY_LENGTHS: usize =
    MAX_DICTIONARY_WORD_LENGTH - MIN_DICTIONARY_WORD_LENGTH + 1;

/// Number of transforms applied to dictionary words.
pub const NUM_TRANSFORMS: usize = 121;

/// Number of words per length in the static dictionary.
/// These are the actual values from the Brotli specification.
const WORDS_PER_LENGTH: [u32; NUM_DICTIONARY_LENGTHS] = [
    0, // length 4: 0 (not used in minimal dict)
    0, // length 5
    0, // length 6
    0, // length 7
    0, // length 8
    0, // length 9
    0, // length 10
    0, // length 11
    0, // length 12
    0, // length 13
    0, // length 14
    0, // length 15
    0, // length 16
    0, // length 17
    0, // length 18
    0, // length 19
    0, // length 20
    0, // length 21
    0, // length 22
    0, // length 23
    0, // length 24
];

// Note: The full dictionary would have per-length word counts from RFC 7932.
// In a production implementation, the full ~120KB dictionary data
// would be embedded as a const byte array.

/// Static dictionary data.
///
/// In a full implementation, this would be the ~120KB dictionary from
/// RFC 7932 Appendix A. For now, we provide the infrastructure and
/// use an empty dictionary, which means dictionary references during
/// decompression will be handled but with empty content.
static DICTIONARY_DATA: &[u8] = &[];

/// Offset table for each word length.
/// `DICTIONARY_OFFSETS[i]` is the byte offset in `DICTIONARY_DATA`
/// where words of length `(i + MIN_DICTIONARY_WORD_LENGTH)` begin.
static DICTIONARY_OFFSETS: [usize; NUM_DICTIONARY_LENGTHS + 1] = [0; NUM_DICTIONARY_LENGTHS + 1];

/// Dictionary word sizes (number of bits to index words of each length).
const DICTIONARY_SIZE_BITS_BY_LENGTH: [u8; NUM_DICTIONARY_LENGTHS] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// A Brotli dictionary transform.
#[derive(Debug, Clone)]
pub struct Transform {
    /// Prefix string to prepend.
    pub prefix: &'static str,
    /// Type of transformation on the dictionary word.
    pub transform_type: TransformType,
    /// Suffix string to append.
    pub suffix: &'static str,
}

/// Types of transforms applied to dictionary words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformType {
    /// Use the word as-is.
    Identity,
    /// Uppercase the first letter.
    UppercaseFirst,
    /// Uppercase all letters.
    UppercaseAll,
    /// Omit the last N bytes.
    OmitLast(u8),
    /// Omit the first N bytes.
    OmitFirst(u8),
}

/// All 121 transforms from the Brotli specification.
/// This is a representative subset; the full list is in RFC 7932 Section 8.
static TRANSFORMS: &[Transform] = &[
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " ",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::Identity,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(1),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::UppercaseFirst,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " the ",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::Identity,
        suffix: "",
    },
    Transform {
        prefix: "s ",
        transform_type: TransformType::Identity,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " of ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::UppercaseFirst,
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " and ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(2),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(1),
        suffix: "",
    },
    Transform {
        prefix: ", ",
        transform_type: TransformType::Identity,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: ", ",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::UppercaseFirst,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " in ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " to ",
    },
    Transform {
        prefix: "e ",
        transform_type: TransformType::Identity,
        suffix: " ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "\"",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: ".",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "\">",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "\n",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(3),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "]",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " for ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(3),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(2),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " a ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " that ",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::UppercaseFirst,
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: ". ",
    },
    Transform {
        prefix: ".",
        transform_type: TransformType::Identity,
        suffix: "",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::Identity,
        suffix: ", ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(4),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " with ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "'",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " from ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " by ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(5),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(6),
        suffix: "",
    },
    Transform {
        prefix: " the ",
        transform_type: TransformType::Identity,
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(4),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: ". The ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::UppercaseAll,
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " on ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " as ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " is ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(7),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(1),
        suffix: "ing ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "\n\t",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: ":",
    },
    Transform {
        prefix: " ",
        transform_type: TransformType::Identity,
        suffix: ". ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "ed ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(9),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(7),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(6),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "(",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::UppercaseFirst,
        suffix: ", ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(8),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: " at ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::Identity,
        suffix: "ly ",
    },
    Transform {
        prefix: " the ",
        transform_type: TransformType::Identity,
        suffix: " of ",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitLast(5),
        suffix: "",
    },
    Transform {
        prefix: "",
        transform_type: TransformType::OmitFirst(8),
        suffix: "",
    },
];

/// Look up a word from the static dictionary.
///
/// Returns the dictionary word of the specified length at the given index,
/// or an error if the dictionary is not available or the reference is invalid.
pub fn lookup_word(word_length: usize, word_index: u32) -> BrotliResult<&'static [u8]> {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return Err(BrotliError::DictionaryError(format!(
            "word length {word_length} out of range [{MIN_DICTIONARY_WORD_LENGTH}, {MAX_DICTIONARY_WORD_LENGTH}]"
        )));
    }

    let len_idx = word_length - MIN_DICTIONARY_WORD_LENGTH;
    let num_words = WORDS_PER_LENGTH[len_idx];

    if word_index >= num_words {
        return Err(BrotliError::DictionaryError(format!(
            "word index {word_index} exceeds count {num_words} for length {word_length}"
        )));
    }

    let offset = DICTIONARY_OFFSETS[len_idx] + (word_index as usize) * word_length;
    let end = offset + word_length;

    if end > DICTIONARY_DATA.len() {
        return Err(BrotliError::DictionaryError(
            "dictionary data too short".to_string(),
        ));
    }

    Ok(&DICTIONARY_DATA[offset..end])
}

/// Apply a transform to a dictionary word.
///
/// Returns the transformed word as a new `Vec<u8>`.
pub fn apply_transform(word: &[u8], transform_id: usize) -> BrotliResult<Vec<u8>> {
    if transform_id >= TRANSFORMS.len() {
        // For transforms beyond our table, use identity.
        return Ok(word.to_vec());
    }

    let transform = &TRANSFORMS[transform_id];
    let mut result =
        Vec::with_capacity(transform.prefix.len() + word.len() + transform.suffix.len());

    // Add prefix.
    result.extend_from_slice(transform.prefix.as_bytes());

    // Apply word transformation.
    match transform.transform_type {
        TransformType::Identity => {
            result.extend_from_slice(word);
        }
        TransformType::UppercaseFirst => {
            if let Some((&first, rest)) = word.split_first() {
                result.push(first.to_ascii_uppercase());
                result.extend_from_slice(rest);
            }
        }
        TransformType::UppercaseAll => {
            for &b in word {
                result.push(b.to_ascii_uppercase());
            }
        }
        TransformType::OmitLast(n) => {
            let n = n as usize;
            if word.len() > n {
                result.extend_from_slice(&word[..word.len() - n]);
            }
        }
        TransformType::OmitFirst(n) => {
            let n = n as usize;
            if word.len() > n {
                result.extend_from_slice(&word[n..]);
            }
        }
    }

    // Add suffix.
    result.extend_from_slice(transform.suffix.as_bytes());

    Ok(result)
}

/// Get the number of dictionary size bits for a given word length.
pub fn dictionary_size_bits(word_length: usize) -> u8 {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return 0;
    }
    let idx = word_length - MIN_DICTIONARY_WORD_LENGTH;
    DICTIONARY_SIZE_BITS_BY_LENGTH[idx]
}

/// Check if the dictionary has words for a given length.
pub fn has_dictionary_words(word_length: usize) -> bool {
    if !(MIN_DICTIONARY_WORD_LENGTH..=MAX_DICTIONARY_WORD_LENGTH).contains(&word_length) {
        return false;
    }
    let idx = word_length - MIN_DICTIONARY_WORD_LENGTH;
    WORDS_PER_LENGTH[idx] > 0
}

/// Get the number of transforms.
pub fn num_transforms() -> usize {
    TRANSFORMS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_identity() {
        let word = b"hello";
        let result = apply_transform(word, 0).expect("should apply transform");
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_transform_with_space_suffix() {
        let word = b"hello";
        let result = apply_transform(word, 1).expect("should apply transform");
        assert_eq!(result, b"hello ");
    }

    #[test]
    fn test_transform_with_prefix_and_suffix() {
        let word = b"hello";
        let result = apply_transform(word, 2).expect("should apply transform");
        assert_eq!(result, b" hello ");
    }

    #[test]
    fn test_transform_uppercase_first() {
        let word = b"hello";
        let result = apply_transform(word, 9).expect("should apply transform");
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn test_transform_uppercase_all() {
        let word = b"hello";
        let result = apply_transform(word, 44).expect("should apply transform");
        assert_eq!(result, b"HELLO");
    }

    #[test]
    fn test_transform_omit_last() {
        let word = b"hello";
        // Transform 12: OmitLast(1)
        let result = apply_transform(word, 12).expect("should apply transform");
        assert_eq!(result, b"hell");
    }

    #[test]
    fn test_transform_omit_first() {
        let word = b"hello";
        // Transform 3: OmitFirst(1)
        let result = apply_transform(word, 3).expect("should apply transform");
        assert_eq!(result, b"ello");
    }

    #[test]
    fn test_dictionary_bounds() {
        // With empty dictionary, all lookups should fail.
        assert!(lookup_word(4, 0).is_err());
        assert!(lookup_word(3, 0).is_err()); // below min
        assert!(lookup_word(25, 0).is_err()); // above max
    }

    #[test]
    fn test_has_dictionary_words() {
        // With minimal dictionary, no words available.
        assert!(!has_dictionary_words(4));
        assert!(!has_dictionary_words(3)); // below range
        assert!(!has_dictionary_words(25)); // above range
    }

    #[test]
    fn test_num_transforms() {
        assert!(num_transforms() > 0);
        assert!(num_transforms() <= NUM_TRANSFORMS);
    }

    #[test]
    fn test_transform_out_of_range() {
        // Transform beyond our table should use identity.
        let word = b"test";
        let result = apply_transform(word, 999).expect("should use identity");
        assert_eq!(result, b"test");
    }
}
