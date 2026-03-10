//! LZ77 match finder for Zstandard compression.
//!
//! This module implements the LZ77 parsing phase of Zstandard compression.
//! It uses a hash chain data structure to efficiently find repeated byte
//! sequences (matches) in the input data.
//!
//! # Architecture
//!
//! The match finder uses two tables:
//! - **Hash table**: maps a 4-byte hash to the most recent position with that hash.
//! - **Chain table**: links positions with the same hash, forming a chain of candidates.
//!
//! For low compression levels (1-2), only the hash table is consulted (greedy matching).
//! For higher levels, the chain is followed up to `search_depth` entries, and
//! lazy matching is used to improve compression ratios.
//!
//! # Dictionary Support
//!
//! The match finder supports an optional dictionary (history) buffer. Matches
//! can reference bytes in the dictionary, which is logically prepended before
//! the actual data. Offsets into the dictionary are computed accordingly.

use oxiarc_core::error::Result;

/// A match found by the LZ77 engine.
#[derive(Debug, Clone, Copy)]
pub struct Match {
    /// Offset of the match (distance back from the current position).
    /// An offset of 1 means the match starts at the immediately preceding byte.
    pub offset: usize,
    /// Length of the match in bytes.
    pub length: usize,
}

/// Minimum match length for Zstandard.
pub const MIN_MATCH: usize = 3;

/// Maximum match length we search for (from Zstd match length table maximum).
pub const MAX_MATCH: usize = 65539;

/// Hash seed for the multiply-shift hash function.
const HASH_PRIME: u32 = 0x9E3779B1;

/// Compression level configuration.
///
/// Controls the trade-off between compression speed and ratio.
/// Higher levels use larger hash/chain tables and deeper search,
/// producing better compression at the cost of more CPU time.
#[derive(Debug, Clone)]
pub struct LevelConfig {
    /// Hash log: hash table size = `1 << hash_log`.
    pub hash_log: u8,
    /// Chain log: chain table size = `1 << chain_log`. Set to 0 to disable chaining.
    pub chain_log: u8,
    /// Maximum number of positions checked when following a hash chain.
    pub search_depth: u32,
    /// Whether to use lazy matching (check position P+1 before committing to P).
    pub lazy_matching: bool,
    /// Minimum length improvement required for a lazy match to replace the current.
    pub lazy_min_gain: usize,
    /// Target block size in bytes for block splitting.
    pub target_block_size: usize,
}

impl LevelConfig {
    /// Get configuration for a given compression level (1-22).
    ///
    /// Levels are clamped to the range [1, 22]. Higher levels produce
    /// better compression at the cost of more memory and CPU time.
    pub fn for_level(level: i32) -> Self {
        let level = level.clamp(1, 22);
        match level {
            1 => LevelConfig {
                hash_log: 17,
                chain_log: 0,
                search_depth: 1,
                lazy_matching: false,
                lazy_min_gain: 0,
                target_block_size: 128 * 1024,
            },
            2 => LevelConfig {
                hash_log: 17,
                chain_log: 0,
                search_depth: 2,
                lazy_matching: false,
                lazy_min_gain: 0,
                target_block_size: 128 * 1024,
            },
            3 => LevelConfig {
                hash_log: 17,
                chain_log: 16,
                search_depth: 6,
                lazy_matching: false,
                lazy_min_gain: 0,
                target_block_size: 128 * 1024,
            },
            4..=6 => LevelConfig {
                hash_log: 18,
                chain_log: 17,
                search_depth: (level * 4) as u32,
                lazy_matching: true,
                lazy_min_gain: 1,
                target_block_size: 128 * 1024,
            },
            7..=9 => LevelConfig {
                hash_log: 19,
                chain_log: 18,
                search_depth: (level * 8) as u32,
                lazy_matching: true,
                lazy_min_gain: 1,
                target_block_size: 128 * 1024,
            },
            10..=15 => LevelConfig {
                hash_log: 20,
                chain_log: 19,
                search_depth: (level * 16) as u32,
                lazy_matching: true,
                lazy_min_gain: 2,
                target_block_size: 128 * 1024,
            },
            _ => LevelConfig {
                hash_log: 20,
                chain_log: 20,
                search_depth: (level * 32) as u32,
                lazy_matching: true,
                lazy_min_gain: 3,
                target_block_size: 128 * 1024,
            },
        }
    }
}

/// LZ77 sequence: a literal run followed by an optional match.
///
/// Each sequence represents a contiguous portion of the input:
/// first `literals.len()` literal bytes, then `match_length` bytes
/// copied from `offset` bytes back in the output history.
#[derive(Debug, Clone)]
pub struct Lz77Sequence {
    /// Literal bytes before the match.
    pub literals: Vec<u8>,
    /// Match offset (distance back from current position). 0 means no match.
    pub offset: usize,
    /// Match length in bytes. 0 means no match (literal-only sequence).
    pub match_length: usize,
}

/// LZ77 match finder using hash chains.
///
/// The match finder maintains a hash table and chain table that are indexed
/// by position in a virtual address space where the dictionary is prepended
/// to the actual data.
pub struct MatchFinder {
    /// Hash table: maps hash value to most recent position with that hash.
    /// Positions are stored as u32 for memory efficiency.
    hash_table: Vec<u32>,
    /// Chain table: for each position, links to the previous position with
    /// the same hash value. Only used when `chain_log > 0`.
    chain_table: Vec<u32>,
    /// Bitmask for hash table indexing.
    hash_mask: u32,
    /// Bitmask for chain table indexing. Zero if chaining is disabled.
    chain_mask: u32,
    /// Configuration for this compression level.
    config: LevelConfig,
}

impl MatchFinder {
    /// Create a new match finder with the given level configuration.
    pub fn new(config: &LevelConfig) -> Self {
        let hash_size = 1usize << config.hash_log;
        let chain_size = if config.chain_log > 0 {
            1usize << config.chain_log
        } else {
            0
        };

        Self {
            hash_table: vec![0u32; hash_size],
            chain_table: vec![0u32; chain_size],
            hash_mask: (hash_size as u32).wrapping_sub(1),
            chain_mask: if chain_size > 0 {
                (chain_size as u32).wrapping_sub(1)
            } else {
                0
            },
            config: config.clone(),
        }
    }

    /// Find LZ77 sequences in the input data.
    ///
    /// The optional `dict` slice acts as history prepended before `data`.
    /// Matches may reference bytes in the dictionary. The returned sequences
    /// cover exactly all bytes of `data` (not the dictionary).
    ///
    /// # Errors
    ///
    /// Returns an error if internal invariants are violated (should not happen
    /// in normal operation).
    pub fn find_sequences(&mut self, data: &[u8], dict: &[u8]) -> Result<Vec<Lz77Sequence>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let dict_len = dict.len();
        // Build a combined view: dict ++ data.
        // Positions in [0..dict_len) refer to dictionary bytes,
        // positions in [dict_len..dict_len+data.len()) refer to data bytes.
        let combined_len = dict_len + data.len();
        let combined = CombinedBuffer::new(dict, data);

        // Seed the hash table with dictionary positions.
        self.seed_dictionary(&combined, dict_len);

        let mut sequences = Vec::new();
        let mut pos = dict_len; // Current position in combined space.
        let mut literal_start = dict_len; // Start of current literal run.

        while pos < combined_len {
            // Need at least MIN_MATCH bytes remaining to find a match.
            if pos + MIN_MATCH > combined_len {
                break;
            }

            let best_match = self.find_best_match(&combined, pos, dict_len);

            // Lazy matching: if we found a match at pos, check if pos+1 gives a better one.
            let (final_match, advance_one) = if self.config.lazy_matching {
                if let Some(m1) = best_match {
                    if pos + 1 + MIN_MATCH <= combined_len {
                        // Temporarily insert pos into hash/chain so pos+1 can reference it.
                        self.insert_position(&combined, pos);
                        let m2 = self.find_best_match(&combined, pos + 1, dict_len);
                        if let Some(m2) = m2 {
                            if m2.length > m1.length + self.config.lazy_min_gain {
                                // Lazy match is better: emit literal at pos, use m2 at pos+1.
                                (Some(m2), true)
                            } else {
                                (Some(m1), false)
                            }
                        } else {
                            (Some(m1), false)
                        }
                    } else {
                        (Some(m1), false)
                    }
                } else {
                    (None, false)
                }
            } else {
                (best_match, false)
            };

            if advance_one {
                // Emit literal for current position, then advance to pos+1 for the match.
                pos += 1;
            }

            if let Some(m) = final_match {
                // Collect literals from literal_start to pos.
                let literals: Vec<u8> = (literal_start..pos).map(|p| combined.get(p)).collect();

                // Convert combined-space offset to data-relative offset.
                // The match offset is already a distance back from pos.
                sequences.push(Lz77Sequence {
                    literals,
                    offset: m.offset,
                    match_length: m.length,
                });

                // Insert all positions covered by the match into hash/chain
                // (so future matches can reference them).
                let match_end = (pos + m.length).min(combined_len);
                // Insert the match start position if not already inserted by lazy matching.
                if !advance_one {
                    self.insert_position(&combined, pos);
                }
                // Insert intermediate positions (skip first, already inserted).
                for insert_pos in (pos + 1)..match_end {
                    if insert_pos + MIN_MATCH <= combined_len {
                        self.insert_position(&combined, insert_pos);
                    }
                }

                pos = match_end;
                literal_start = pos;
            } else {
                // No match found: insert position and advance.
                self.insert_position(&combined, pos);
                pos += 1;
            }
        }

        // Remaining literals at the end of the data.
        if literal_start < combined_len {
            let literals: Vec<u8> = (literal_start..combined_len)
                .map(|p| combined.get(p))
                .collect();

            sequences.push(Lz77Sequence {
                literals,
                offset: 0,
                match_length: 0,
            });
        }

        Ok(sequences)
    }

    /// Compute the hash of 4 bytes at the given position.
    ///
    /// Uses a multiply-shift scheme for fast, reasonably distributed hashes.
    fn hash(&self, combined: &CombinedBuffer<'_>, pos: usize) -> u32 {
        let b0 = combined.get(pos) as u32;
        let b1 = combined.get(pos + 1) as u32;
        let b2 = combined.get(pos + 2) as u32;
        let b3 = combined.get(pos + 3) as u32;
        let val = b0 | (b1 << 8) | (b2 << 16) | (b3 << 24);
        let h = val.wrapping_mul(HASH_PRIME);
        (h >> (32 - self.config.hash_log)) & self.hash_mask
    }

    /// Insert a position into the hash table (and chain table if enabled).
    fn insert_position(&mut self, combined: &CombinedBuffer<'_>, pos: usize) {
        if pos + 4 > combined.len() {
            return;
        }

        let h = self.hash(combined, pos) as usize;
        let prev = self.hash_table[h];
        self.hash_table[h] = pos as u32;

        if !self.chain_table.is_empty() {
            let chain_idx = (pos as u32) & self.chain_mask;
            self.chain_table[chain_idx as usize] = prev;
        }
    }

    /// Seed the hash table with dictionary positions so matches can reference the dict.
    fn seed_dictionary(&mut self, combined: &CombinedBuffer<'_>, dict_len: usize) {
        if dict_len < 4 {
            return;
        }
        for pos in 0..=(dict_len.saturating_sub(4)) {
            self.insert_position(combined, pos);
        }
    }

    /// Find the best match at a given position by consulting the hash table
    /// and optionally following the hash chain.
    fn find_best_match(
        &self,
        combined: &CombinedBuffer<'_>,
        pos: usize,
        dict_len: usize,
    ) -> Option<Match> {
        if pos + 4 > combined.len() {
            return None;
        }

        let h = self.hash(combined, pos) as usize;
        let mut candidate = self.hash_table[h] as usize;

        // The candidate must be strictly before pos.
        if candidate >= pos {
            return None;
        }

        let max_distance = if pos >= dict_len {
            // In data region: can reference back through data and into dictionary.
            pos
        } else {
            // In dictionary region (during seeding): no matches.
            return None;
        };

        let mut best: Option<Match> = None;
        let mut steps = 0u32;
        let max_steps = self.config.search_depth;

        loop {
            if steps >= max_steps {
                break;
            }
            if candidate >= pos {
                break;
            }

            let distance = pos - candidate;
            if distance > max_distance || distance == 0 {
                break;
            }

            // Compare bytes at candidate and pos to determine match length.
            let match_len = self.compute_match_length(combined, candidate, pos);

            if match_len >= MIN_MATCH {
                let is_better = match best {
                    Some(ref b) => {
                        match_len > b.length || (match_len == b.length && distance < b.offset)
                    }
                    None => true,
                };
                if is_better {
                    let clamped_len = match_len.min(MAX_MATCH);
                    best = Some(Match {
                        offset: distance,
                        length: clamped_len,
                    });
                    // If we found a very long match, stop searching.
                    if clamped_len >= 128 {
                        break;
                    }
                }
            }

            steps += 1;

            // Follow the chain to the next candidate with the same hash.
            if self.chain_table.is_empty() {
                break;
            }
            let chain_idx = (candidate as u32) & self.chain_mask;
            let next_candidate = self.chain_table[chain_idx as usize] as usize;
            if next_candidate >= candidate || next_candidate == 0 {
                // Chain is broken or loops back: stop.
                break;
            }
            candidate = next_candidate;
        }

        best
    }

    /// Compute the number of matching bytes starting at positions `a` and `b`.
    fn compute_match_length(&self, combined: &CombinedBuffer<'_>, a: usize, b: usize) -> usize {
        let max_len = (combined.len() - b).min(MAX_MATCH);
        let mut len = 0usize;

        // Fast path: compare 8 bytes at a time when possible.
        while len + 8 <= max_len {
            let va = combined.get_u64(a + len);
            let vb = combined.get_u64(b + len);
            if va != vb {
                // Find the first differing byte within this 8-byte chunk.
                let diff = va ^ vb;
                len += (diff.trailing_zeros() / 8) as usize;
                return len;
            }
            len += 8;
        }

        // Byte-by-byte for the remaining tail.
        while len < max_len {
            if combined.get(a + len) != combined.get(b + len) {
                break;
            }
            len += 1;
        }

        len
    }

    /// Reset the match finder for a new block.
    ///
    /// Clears the hash and chain tables so no stale positions from previous
    /// blocks are referenced.
    pub fn reset(&mut self) {
        for entry in self.hash_table.iter_mut() {
            *entry = 0;
        }
        for entry in self.chain_table.iter_mut() {
            *entry = 0;
        }
    }
}

/// Combined view of dictionary + data as a single logical byte array.
///
/// Avoids copying by dispatching reads to the appropriate slice based
/// on the position.
struct CombinedBuffer<'a> {
    /// Dictionary (history) bytes, logically at positions [0..dict.len()).
    dict: &'a [u8],
    /// Actual data bytes, logically at positions [dict.len()..dict.len()+data.len()).
    data: &'a [u8],
    /// Total logical length.
    total_len: usize,
}

impl<'a> CombinedBuffer<'a> {
    /// Create a new combined buffer from dict and data slices.
    fn new(dict: &'a [u8], data: &'a [u8]) -> Self {
        Self {
            dict,
            data,
            total_len: dict.len() + data.len(),
        }
    }

    /// Total length of the combined buffer.
    fn len(&self) -> usize {
        self.total_len
    }

    /// Get a single byte at the given logical position.
    fn get(&self, pos: usize) -> u8 {
        if pos < self.dict.len() {
            self.dict[pos]
        } else {
            self.data[pos - self.dict.len()]
        }
    }

    /// Read 8 bytes as a little-endian u64 starting at the given position.
    ///
    /// If the read spans the dict/data boundary or exceeds the buffer,
    /// falls back to byte-by-byte assembly.
    fn get_u64(&self, pos: usize) -> u64 {
        let dict_len = self.dict.len();

        // Fast path: entirely within one slice.
        if pos + 8 <= dict_len {
            // Entirely in dict.
            let slice = &self.dict[pos..pos + 8];
            return u64::from_le_bytes([
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ]);
        }

        let data_pos = pos.wrapping_sub(dict_len);
        if pos >= dict_len && data_pos + 8 <= self.data.len() {
            // Entirely in data.
            let slice = &self.data[data_pos..data_pos + 8];
            return u64::from_le_bytes([
                slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
            ]);
        }

        // Slow path: spans boundary or near end.
        let mut bytes = [0u8; 8];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let p = pos + i;
            if p < self.total_len {
                *byte = self.get(p);
            }
        }
        u64::from_le_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_level_config_clamping() {
        let c = LevelConfig::for_level(-5);
        assert_eq!(c.hash_log, 17); // Same as level 1

        let c = LevelConfig::for_level(100);
        assert_eq!(c.hash_log, 20); // Same as level 22
    }

    #[test]
    fn test_level_config_ranges() {
        for level in 1..=22 {
            let c = LevelConfig::for_level(level);
            assert!(c.hash_log >= 17);
            assert!(c.hash_log <= 20);
            assert!(c.search_depth >= 1);
            assert!(c.target_block_size > 0);
        }
    }

    #[test]
    fn test_find_sequences_empty() {
        let config = LevelConfig::for_level(1);
        let mut finder = MatchFinder::new(&config);
        let seqs = finder.find_sequences(&[], &[]).expect("should succeed");
        assert!(seqs.is_empty());
    }

    #[test]
    fn test_find_sequences_no_matches() {
        let config = LevelConfig::for_level(1);
        let mut finder = MatchFinder::new(&config);
        // Data with no repeated patterns.
        let data: Vec<u8> = (0..64).collect();
        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        // All sequences should be literal-only (no matches in non-repeating data of this size).
        let total_literals: usize = seqs.iter().map(|s| s.literals.len()).sum();
        let total_match_bytes: usize = seqs.iter().map(|s| s.match_length).sum();
        assert_eq!(total_literals + total_match_bytes, data.len());
    }

    #[test]
    fn test_find_sequences_with_repeats() {
        let config = LevelConfig::for_level(3);
        let mut finder = MatchFinder::new(&config);
        // Data with obvious repeats.
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend_from_slice(b"ABCDEFGHIJ");
        }
        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        // Verify that sequences cover the entire input.
        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());

        // There should be at least one match found in repeated data.
        let has_match = seqs.iter().any(|s| s.match_length > 0);
        assert!(has_match, "Expected at least one match in repeated data");
    }

    #[test]
    fn test_find_sequences_all_same_byte() {
        let config = LevelConfig::for_level(1);
        let mut finder = MatchFinder::new(&config);
        let data = vec![0xAAu8; 1000];
        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());

        // Should find matches (lots of repeated bytes).
        let total_match_bytes: usize = seqs.iter().map(|s| s.match_length).sum();
        assert!(
            total_match_bytes > 0,
            "Expected matches in all-same-byte data"
        );
    }

    #[test]
    fn test_find_sequences_with_dictionary() {
        let config = LevelConfig::for_level(3);
        let mut finder = MatchFinder::new(&config);
        let dict = b"Hello, World! This is a dictionary.";
        let data = b"Hello, World! This is actual data.";
        let seqs = finder
            .find_sequences(data.as_slice(), dict.as_slice())
            .expect("should succeed");

        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());

        // The data starts identically to the dict, so there should be a match.
        let has_match = seqs.iter().any(|s| s.match_length > 0);
        assert!(has_match, "Expected match referencing dictionary");
    }

    #[test]
    fn test_find_sequences_lazy_matching() {
        let config = LevelConfig::for_level(5); // Lazy matching enabled
        let mut finder = MatchFinder::new(&config);
        // Construct data where lazy matching helps:
        // "XABC...ABC..." where matching at pos 1 gives a longer match.
        let mut data = Vec::new();
        data.push(b'X');
        data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
        data.push(b'Y');
        data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());
    }

    #[test]
    fn test_match_finder_reset() {
        let config = LevelConfig::for_level(1);
        let mut finder = MatchFinder::new(&config);

        let data = vec![0xBBu8; 100];
        let _ = finder.find_sequences(&data, &[]).expect("should succeed");

        finder.reset();

        // After reset, hash table should be zeroed.
        assert!(finder.hash_table.iter().all(|&v| v == 0));
    }

    #[test]
    fn test_find_sequences_short_data() {
        let config = LevelConfig::for_level(1);
        let mut finder = MatchFinder::new(&config);

        // Data shorter than MIN_MATCH: all literals, no matches possible.
        let data = b"AB";
        let seqs = finder
            .find_sequences(data.as_slice(), &[])
            .expect("should succeed");
        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());
        assert!(seqs.iter().all(|s| s.match_length == 0));
    }

    #[test]
    fn test_find_sequences_exact_min_match() {
        let config = LevelConfig::for_level(3);
        let mut finder = MatchFinder::new(&config);

        // Create data where only a 3-byte match is possible.
        let mut data = Vec::new();
        data.extend_from_slice(b"ABCXYZ");
        data.extend_from_slice(b"ABCQRS");
        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
        assert_eq!(total_bytes, data.len());
    }

    #[test]
    fn test_combined_buffer_get() {
        let dict = b"DICT";
        let data = b"DATA";
        let combined = CombinedBuffer::new(dict.as_slice(), data.as_slice());

        assert_eq!(combined.len(), 8);
        assert_eq!(combined.get(0), b'D');
        assert_eq!(combined.get(3), b'T');
        assert_eq!(combined.get(4), b'D');
        assert_eq!(combined.get(7), b'A');
    }

    #[test]
    fn test_combined_buffer_get_u64() {
        let dict = b"ABCD";
        let data = b"EFGHIJKL";
        let combined = CombinedBuffer::new(dict.as_slice(), data.as_slice());

        // Read crossing dict/data boundary.
        let val = combined.get_u64(2);
        let expected = u64::from_le_bytes([b'C', b'D', b'E', b'F', b'G', b'H', b'I', b'J']);
        assert_eq!(val, expected);

        // Read entirely in data.
        let val = combined.get_u64(4);
        let expected = u64::from_le_bytes([b'E', b'F', b'G', b'H', b'I', b'J', b'K', b'L']);
        assert_eq!(val, expected);
    }

    #[test]
    fn test_all_levels_produce_valid_output() {
        let data = b"The quick brown fox jumps over the lazy dog. The quick brown fox.";
        for level in 1..=22 {
            let config = LevelConfig::for_level(level);
            let mut finder = MatchFinder::new(&config);
            let seqs = finder
                .find_sequences(data.as_slice(), &[])
                .expect("should succeed");

            let total_bytes: usize = seqs.iter().map(|s| s.literals.len() + s.match_length).sum();
            assert_eq!(
                total_bytes,
                data.len(),
                "Level {} produced incorrect coverage",
                level
            );
        }
    }

    #[test]
    fn test_match_offset_validity() {
        let config = LevelConfig::for_level(3);
        let mut finder = MatchFinder::new(&config);

        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(b"REPEATED_PATTERN_");
        }

        let seqs = finder.find_sequences(&data, &[]).expect("should succeed");

        // All match offsets should be > 0 and <= current position.
        let mut pos = 0usize;
        for seq in &seqs {
            pos += seq.literals.len();
            if seq.match_length > 0 {
                assert!(seq.offset > 0, "Match offset must be positive");
                assert!(
                    seq.offset <= pos,
                    "Match offset {} exceeds position {}",
                    seq.offset,
                    pos
                );
            }
            pos += seq.match_length;
        }
    }
}
