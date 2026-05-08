//! Dictionary support for Zstandard compression.
//!
//! Dictionaries improve compression of small data by providing a pre-trained
//! context of common patterns. A dictionary is built from a collection of
//! representative samples and can then be supplied to both the encoder and
//! decoder to achieve significantly better compression ratios on small
//! payloads.
//!
//! # Overview
//!
//! The [`ZstdDict`] struct wraps raw dictionary bytes and computes a unique
//! dictionary ID (lower 32 bits of XXH64 with seed 0). The
//! [`train_dictionary`] function creates a dictionary from sample data using
//! a frequency-weighted n-gram extraction approach.
//!
//! # Example
//!
//! ```rust,no_run
//! use oxiarc_zstd::dict::{ZstdDict, train_dictionary};
//!
//! let samples: Vec<&[u8]> = vec![
//!     b"common prefix data A",
//!     b"common prefix data B",
//!     b"common prefix data C",
//! ];
//! let dict = train_dictionary(&samples, 4096).expect("dictionary training failed");
//! assert!(dict.len() > 0);
//! ```

use crate::xxhash::xxhash64_with_seed;
use oxiarc_core::error::{OxiArcError, Result};
use std::collections::HashMap;

/// Maximum dictionary size (1 MB).
pub const MAX_DICT_SIZE: usize = 1024 * 1024;

/// Minimum n-gram window size for dictionary training.
const MIN_NGRAM: usize = 4;

/// Maximum n-gram window size for dictionary training.
const MAX_NGRAM: usize = 16;

/// Minimum occurrence count for an n-gram to be considered useful.
const MIN_FREQUENCY: usize = 2;

/// A Zstandard dictionary.
///
/// Wraps raw dictionary data and provides the associated dictionary ID
/// computed as the lower 32 bits of XXH64 with seed 0.
#[derive(Debug, Clone)]
pub struct ZstdDict {
    /// Raw dictionary bytes.
    data: Vec<u8>,
    /// Dictionary ID (lower 32 bits of XXH64 with seed 0).
    id: u32,
}

impl ZstdDict {
    /// Create a dictionary from raw data.
    ///
    /// # Errors
    ///
    /// Returns an error if `data` exceeds [`MAX_DICT_SIZE`].
    pub fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() > MAX_DICT_SIZE {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: format!(
                    "dictionary too large: {} bytes exceeds maximum {} bytes",
                    data.len(),
                    MAX_DICT_SIZE
                ),
            });
        }
        let id = xxhash64_with_seed(&data, 0) as u32;
        Ok(Self { data, id })
    }

    /// Get the dictionary ID.
    ///
    /// The ID is the lower 32 bits of the XXH64 hash of the dictionary
    /// content with seed 0.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get a reference to the raw dictionary data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get the dictionary length in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Consume the dictionary and return the underlying byte vector.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Train a dictionary from sample data.
///
/// This implements a simplified dictionary training algorithm:
///
/// 1. Extract n-grams (substrings of length `MIN_NGRAM`..=`MAX_NGRAM`)
///    from every sample.
/// 2. Score each n-gram by *frequency x length* (its estimated compression
///    impact).
/// 3. Filter n-grams that appear fewer than `MIN_FREQUENCY` times.
/// 4. Sort by descending score and greedily concatenate into the dictionary,
///    skipping n-grams that are substrings of already-included entries.
///
/// If no common n-grams are found (e.g. the samples are too short or too
/// dissimilar), the dictionary falls back to concatenating raw sample data.
///
/// The resulting dictionary is capped at `dict_size` bytes (and never exceeds
/// [`MAX_DICT_SIZE`]).
///
/// # Errors
///
/// Returns an error if `samples` is empty.
pub fn train_dictionary(samples: &[&[u8]], dict_size: usize) -> Result<ZstdDict> {
    if samples.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "no samples provided for dictionary training".to_string(),
        });
    }

    let dict_size = dict_size.min(MAX_DICT_SIZE);

    // Step 1: Collect n-gram frequencies across all samples.
    let mut ngram_counts: HashMap<Vec<u8>, usize> = HashMap::new();

    for sample in samples {
        let max_window = MAX_NGRAM.min(sample.len());
        if max_window < MIN_NGRAM {
            continue; // sample is too short for any n-gram
        }

        for window_size in MIN_NGRAM..=max_window {
            for window in sample.windows(window_size) {
                *ngram_counts.entry(window.to_vec()).or_insert(0) += 1;
            }
        }
    }

    // Step 2: Score by impact = frequency * length, keep only those appearing
    // at least MIN_FREQUENCY times.
    let mut scored: Vec<(Vec<u8>, usize)> = ngram_counts
        .into_iter()
        .filter(|(_, count)| *count >= MIN_FREQUENCY)
        .map(|(ngram, count)| {
            let score = count * ngram.len();
            (ngram, score)
        })
        .collect();

    // Sort by descending score, breaking ties by preferring longer n-grams.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.len().cmp(&a.0.len())));

    // Step 3: Build dictionary by greedily concatenating top n-grams.
    let mut dict_data = Vec::with_capacity(dict_size);
    let mut included_ngrams: Vec<Vec<u8>> = Vec::new();

    for (ngram, _score) in &scored {
        if dict_data.len() + ngram.len() > dict_size {
            // If the dictionary is almost full, try shorter n-grams.
            if dict_data.len() >= dict_size {
                break;
            }
            continue;
        }

        // Skip if this n-gram is a substring of an already-included entry.
        let is_substring = included_ngrams
            .iter()
            .any(|included| included.windows(ngram.len()).any(|w| w == ngram.as_slice()));
        if is_substring {
            continue;
        }

        dict_data.extend_from_slice(ngram);
        included_ngrams.push(ngram.clone());
    }

    // Step 4: If no common patterns were found, fall back to raw sample data.
    if dict_data.is_empty() {
        for sample in samples {
            let remaining = dict_size.saturating_sub(dict_data.len());
            if remaining == 0 {
                break;
            }
            let take = remaining.min(sample.len());
            dict_data.extend_from_slice(&sample[..take]);
        }
    }

    ZstdDict::new(dict_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dict_new_basic() {
        let data = b"hello world".to_vec();
        let dict = ZstdDict::new(data.clone()).expect("compression/encoding failed");
        assert_eq!(dict.data(), data.as_slice());
        assert_eq!(dict.len(), data.len());
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_dict_new_empty() {
        let dict = ZstdDict::new(Vec::new()).expect("compression/encoding failed");
        assert!(dict.is_empty());
        assert_eq!(dict.len(), 0);
    }

    #[test]
    fn test_dict_too_large() {
        let data = vec![0u8; MAX_DICT_SIZE + 1];
        let result = ZstdDict::new(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_dict_id_deterministic() {
        let data = b"test dictionary".to_vec();
        let dict1 = ZstdDict::new(data.clone()).expect("compression/encoding failed");
        let dict2 = ZstdDict::new(data).expect("compression/encoding failed");
        assert_eq!(dict1.id(), dict2.id());
    }

    #[test]
    fn test_dict_id_differs_for_different_data() {
        let dict_a = ZstdDict::new(b"data A".to_vec()).expect("compression/encoding failed");
        let dict_b = ZstdDict::new(b"data B".to_vec()).expect("compression/encoding failed");
        // Not strictly guaranteed but extremely likely for different inputs.
        assert_ne!(dict_a.id(), dict_b.id());
    }

    #[test]
    fn test_dict_into_data() {
        let data = b"round-trip".to_vec();
        let dict = ZstdDict::new(data.clone()).expect("compression/encoding failed");
        assert_eq!(dict.into_data(), data);
    }

    #[test]
    fn test_train_dictionary_no_samples() {
        let result = train_dictionary(&[], 4096);
        assert!(result.is_err());
    }

    #[test]
    fn test_train_dictionary_basic() {
        let samples: Vec<&[u8]> = vec![
            b"the quick brown fox jumps",
            b"the quick brown dog runs",
            b"the quick brown cat sleeps",
        ];
        let dict = train_dictionary(&samples, 256).expect("compression/encoding failed");
        assert!(!dict.is_empty());
        assert!(dict.len() <= 256);
    }

    #[test]
    fn test_train_dictionary_respects_size_limit() {
        let samples: Vec<&[u8]> = vec![
            b"AAAA BBBB CCCC DDDD EEEE",
            b"AAAA BBBB CCCC DDDD FFFF",
            b"AAAA BBBB CCCC DDDD GGGG",
        ];
        let dict = train_dictionary(&samples, 32).expect("compression/encoding failed");
        assert!(dict.len() <= 32);
    }

    #[test]
    fn test_train_dictionary_short_samples() {
        // Samples shorter than MIN_NGRAM should fall back to raw data.
        let samples: Vec<&[u8]> = vec![b"AB", b"CD", b"EF"];
        let dict = train_dictionary(&samples, 64).expect("compression/encoding failed");
        // Should contain raw sample data.
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_train_dictionary_identical_samples() {
        let sample = b"identical content repeated";
        let samples: Vec<&[u8]> = vec![sample, sample, sample];
        let dict = train_dictionary(&samples, 256).expect("compression/encoding failed");
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_train_dictionary_caps_at_max_dict_size() {
        let samples: Vec<&[u8]> = vec![b"data"];
        let dict =
            train_dictionary(&samples, MAX_DICT_SIZE + 100).expect("compression/encoding failed");
        assert!(dict.len() <= MAX_DICT_SIZE);
    }

    #[test]
    fn test_train_dictionary_common_prefix() {
        let samples: Vec<&[u8]> = vec![
            b"prefix_alpha_suffix",
            b"prefix_beta_suffix",
            b"prefix_gamma_suffix",
        ];
        let dict = train_dictionary(&samples, 1024).expect("compression/encoding failed");

        // The common substrings "prefix_" and "_suffix" should be present.
        let dict_str = String::from_utf8_lossy(dict.data());
        // At least one of the common patterns should appear.
        let has_common = dict_str.contains("prefix_") || dict_str.contains("_suffix");
        assert!(
            has_common,
            "dictionary should contain common substrings: {:?}",
            dict_str
        );
    }
}
