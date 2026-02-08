//! LZ77 compression for DEFLATE.
//!
//! This module implements the LZ77 algorithm as used in DEFLATE compression.
//! LZ77 finds repeated sequences in the input and replaces them with
//! back-references (length, distance pairs).
//!
//! # Algorithm
//!
//! The algorithm maintains a sliding window of recently seen data (32KB for DEFLATE).
//! For each position, it searches for the longest match in the window and either:
//! - Emits a literal byte if no good match is found
//! - Emits a (length, distance) pair if a match of 3+ bytes is found

/// Maximum window size for DEFLATE (32KB).
pub const WINDOW_SIZE: usize = 32768;

/// Minimum match length.
pub const MIN_MATCH: usize = 3;

/// Maximum match length.
pub const MAX_MATCH: usize = 258;

/// Size of the hash table (power of 2).
const HASH_SIZE: usize = 32768;

/// Hash mask.
const HASH_MASK: usize = HASH_SIZE - 1;

/// Number of hash chain entries to check.
const MAX_CHAIN_LENGTH: usize = 4096;

/// A token produced by LZ77 compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lz77Token {
    /// A literal byte.
    Literal(u8),
    /// A back-reference to previously seen data.
    Match {
        /// Number of bytes to copy (3-258).
        length: u16,
        /// Distance back into the window (1-32768).
        distance: u16,
    },
}

/// LZ77 encoder for DEFLATE compression.
#[derive(Debug)]
pub struct Lz77Encoder {
    /// Sliding window buffer.
    window: Vec<u8>,
    /// Current position in the window.
    window_pos: usize,
    /// Hash table: maps hash -> position in window.
    hash_table: Vec<u16>,
    /// Hash chain: previous position with same hash.
    hash_chain: Vec<u16>,
    /// Compression level (affects chain search depth).
    max_chain: usize,
    /// Minimum match length to accept.
    min_match: usize,
    /// Enable lazy matching.
    lazy_match: bool,
}

impl Lz77Encoder {
    /// Create a new LZ77 encoder with default settings.
    pub fn new() -> Self {
        Self::with_level(6)
    }

    /// Create a new LZ77 encoder with the specified compression level (0-9).
    pub fn with_level(level: u8) -> Self {
        let level = level.min(9);

        // Adjust parameters based on level
        let (max_chain, min_match, lazy_match) = match level {
            0 => (0, MAX_MATCH + 1, false), // Store only
            1 => (4, 4, false),
            2 => (8, 4, false),
            3 => (16, 4, false),
            4 => (32, 4, false),
            5 => (64, 4, true),
            6 => (128, 4, true),
            7 => (256, 3, true),
            8 => (1024, 3, true),
            9 => (MAX_CHAIN_LENGTH, 3, true),
            _ => unreachable!(),
        };

        Self {
            window: vec![0; WINDOW_SIZE * 2],
            window_pos: 0,
            hash_table: vec![0; HASH_SIZE],
            hash_chain: vec![0; WINDOW_SIZE],
            max_chain,
            min_match,
            lazy_match,
        }
    }

    /// Reset the encoder state.
    pub fn reset(&mut self) {
        self.window_pos = 0;
        self.hash_table.fill(0);
        self.hash_chain.fill(0);
    }

    /// Set a preset dictionary for improved compression.
    ///
    /// The dictionary is preloaded into the sliding window, allowing
    /// matches to reference dictionary content from the start of compression.
    /// This can significantly improve compression for data that shares
    /// patterns with the dictionary (e.g., compressing similar files).
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// The Adler-32 checksum of the dictionary (used for identification).
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        // Reset state first
        self.reset();

        // Use only last WINDOW_SIZE bytes if dictionary is larger
        let dict_to_use = if dictionary.len() > WINDOW_SIZE {
            &dictionary[dictionary.len() - WINDOW_SIZE..]
        } else {
            dictionary
        };

        // Copy dictionary to window
        self.window[..dict_to_use.len()].copy_from_slice(dict_to_use);
        self.window_pos = dict_to_use.len();

        // Build hash table for dictionary content
        // We need at least 3 bytes to form a hash
        if dict_to_use.len() >= MIN_MATCH {
            for pos in 0..dict_to_use.len().saturating_sub(MIN_MATCH - 1) {
                let h = Self::hash(dict_to_use[pos], dict_to_use[pos + 1], dict_to_use[pos + 2]);
                let prev = self.hash_table[h];
                self.hash_chain[pos & (WINDOW_SIZE - 1)] = prev;
                self.hash_table[h] = pos as u16;
            }
        }

        // Calculate Adler-32 checksum for dictionary identification
        Self::adler32(dictionary)
    }

    /// Calculate Adler-32 checksum (for dictionary identification).
    fn adler32(data: &[u8]) -> u32 {
        const MOD_ADLER: u32 = 65521;
        const NMAX: usize = 5552;

        let mut a: u32 = 1;
        let mut b: u32 = 0;

        let mut remaining = data;

        // Process in chunks to avoid overflow
        while remaining.len() >= NMAX {
            let (chunk, rest) = remaining.split_at(NMAX);
            remaining = rest;

            for &byte in chunk {
                a += byte as u32;
                b += a;
            }

            a %= MOD_ADLER;
            b %= MOD_ADLER;
        }

        // Process remaining bytes
        for &byte in remaining {
            a += byte as u32;
            b += a;
        }

        ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
    }

    /// Check if dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.window_pos > 0
    }

    /// Get the current dictionary size (how much of window is pre-filled).
    pub fn dictionary_size(&self) -> usize {
        self.window_pos
    }

    /// Compute hash for 3 bytes using improved mixing for better distribution.
    #[inline(always)]
    fn hash(b0: u8, b1: u8, b2: u8) -> usize {
        // Improved hash with better avalanche properties
        // Multiply and rotate for better distribution
        let h = ((b0 as usize).wrapping_mul(506832829))
            ^ ((b1 as usize).wrapping_mul(2654435761) << 8)
            ^ ((b2 as usize).wrapping_mul(374761393) << 16);
        (h ^ (h >> 15)) & HASH_MASK
    }

    /// Update hash table with current position.
    fn update_hash(&mut self, pos: usize) {
        if pos + 2 < self.window.len() {
            let h = Self::hash(self.window[pos], self.window[pos + 1], self.window[pos + 2]);
            let prev = self.hash_table[h];
            self.hash_chain[pos & (WINDOW_SIZE - 1)] = prev;
            self.hash_table[h] = pos as u16;
        }
    }

    /// Find the longest match at the current position with optimized matching.
    fn find_match(&self, pos: usize, max_len: usize) -> Option<(u16, u16)> {
        if pos < MIN_MATCH || max_len < self.min_match {
            return None;
        }

        let h = Self::hash(self.window[pos], self.window[pos + 1], self.window[pos + 2]);

        let mut match_pos = self.hash_table[h] as usize;
        let mut best_len = self.min_match - 1;
        let mut best_dist = 0usize;

        let min_pos = pos.saturating_sub(WINDOW_SIZE);
        let mut chain_len = 0;
        let max_check = max_len.min(MAX_MATCH);

        while match_pos >= min_pos && match_pos < pos && chain_len < self.max_chain {
            let dist = pos - match_pos;

            if dist > 0 && dist <= WINDOW_SIZE {
                // Optimization: Check position at best_len first (likely to fail early)
                // This saves us from scanning matches that can't possibly be better
                if self.window[match_pos + best_len] == self.window[pos + best_len] {
                    // Also check first byte (another quick rejection test)
                    if self.window[match_pos] == self.window[pos] {
                        // Count matching bytes with optimized loop
                        let mut len = 0;

                        // Unrolled comparison for first few bytes
                        if len < max_check && self.window[match_pos] == self.window[pos] {
                            len = 1;
                            if len < max_check && self.window[match_pos + 1] == self.window[pos + 1]
                            {
                                len = 2;
                                if len < max_check
                                    && self.window[match_pos + 2] == self.window[pos + 2]
                                {
                                    len = 3;

                                    // Continue with regular loop for remaining bytes
                                    while len < max_check
                                        && self.window[match_pos + len] == self.window[pos + len]
                                    {
                                        len += 1;
                                    }
                                }
                            }
                        }

                        if len > best_len {
                            best_len = len;
                            best_dist = dist;

                            // Early exit if we found a great match
                            if len >= max_len || len >= MAX_MATCH {
                                break;
                            }
                        }
                    }
                }
            }

            // Follow hash chain
            match_pos = self.hash_chain[match_pos & (WINDOW_SIZE - 1)] as usize;
            chain_len += 1;
        }

        if best_len >= self.min_match && best_len >= MIN_MATCH {
            Some((best_len as u16, best_dist as u16))
        } else {
            None
        }
    }

    /// Compress input data to LZ77 tokens.
    pub fn compress(&mut self, input: &[u8]) -> Vec<Lz77Token> {
        let mut tokens = Vec::with_capacity(input.len());
        let mut input_pos = 0;

        while input_pos < input.len() {
            // Determine how much data we can process in this iteration
            let space_in_window = self.window.len().saturating_sub(self.window_pos);
            let chunk_size = space_in_window.min(input.len() - input_pos);

            // Copy chunk to window
            let start = self.window_pos;
            self.window[start..start + chunk_size]
                .copy_from_slice(&input[input_pos..input_pos + chunk_size]);

            let end = start + chunk_size;
            let mut pos = start;

            // Process this chunk
            while pos < end {
                let remaining = end - pos;

                // Find best match
                let match_result = self.find_match(pos, remaining);

                if let Some((length, distance)) = match_result {
                    // Check for lazy matching
                    let mut use_match = true;

                    if self.lazy_match && pos + 1 < end {
                        // Check if next position has a better match
                        self.update_hash(pos);
                        if let Some((next_len, _)) = self.find_match(pos + 1, remaining - 1) {
                            if next_len > length + 1 {
                                // Better to emit literal and use next match
                                use_match = false;
                            }
                        }
                    }

                    if use_match {
                        tokens.push(Lz77Token::Match { length, distance });

                        // Update hash for all positions in match
                        for i in 0..length as usize {
                            self.update_hash(pos + i);
                        }

                        pos += length as usize;
                        continue;
                    }
                }

                // Emit literal
                tokens.push(Lz77Token::Literal(self.window[pos]));
                self.update_hash(pos);
                pos += 1;
            }

            // Update window position
            self.window_pos = end;
            input_pos += chunk_size;

            // If window is getting full, slide it before next iteration
            if self.window_pos >= WINDOW_SIZE + WINDOW_SIZE / 2 {
                self.slide_window();
            }
        }

        tokens
    }

    /// Slide the window to make room for more data.
    fn slide_window(&mut self) {
        // Copy second half to first half
        let slide_amount = WINDOW_SIZE;

        self.window.copy_within(slide_amount..self.window_pos, 0);
        self.window_pos -= slide_amount;

        // Adjust hash table entries
        for entry in &mut self.hash_table {
            if *entry >= slide_amount as u16 {
                *entry -= slide_amount as u16;
            } else {
                *entry = 0;
            }
        }

        // Adjust hash chain entries
        for entry in &mut self.hash_chain {
            if *entry >= slide_amount as u16 {
                *entry -= slide_amount as u16;
            } else {
                *entry = 0;
            }
        }
    }

    /// Compress all data at once (convenience method).
    pub fn compress_all(input: &[u8], level: u8) -> Vec<Lz77Token> {
        let mut encoder = Self::with_level(level);
        encoder.compress(input)
    }
}

impl Default for Lz77Encoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literals_only() {
        let input = b"abcdefgh";
        let tokens = Lz77Encoder::compress_all(input, 6);

        // All should be literals (no repeats)
        assert!(tokens.iter().all(|t| matches!(t, Lz77Token::Literal(_))));
        assert_eq!(tokens.len(), 8);
    }

    #[test]
    fn test_simple_match() {
        let input = b"abcabcabc";
        let tokens = Lz77Encoder::compress_all(input, 6);

        // Should find matches
        let has_match = tokens.iter().any(|t| matches!(t, Lz77Token::Match { .. }));
        assert!(has_match, "Should find at least one match");
    }

    #[test]
    fn test_repeated_char() {
        let input = b"aaaaaaaaaa";
        let tokens = Lz77Encoder::compress_all(input, 6);

        // Should compress well
        let total_output: usize = tokens
            .iter()
            .map(|t| match t {
                Lz77Token::Literal(_) => 1,
                Lz77Token::Match { length, .. } => *length as usize,
            })
            .sum();

        assert_eq!(total_output, 10);
        assert!(tokens.len() < 10, "Should compress repeated chars");
    }

    #[test]
    fn test_decode_matches() {
        let input = b"Hello, Hello, Hello!";
        let tokens = Lz77Encoder::compress_all(input, 6);

        // Reconstruct original
        let mut output = Vec::new();
        for token in &tokens {
            match token {
                Lz77Token::Literal(b) => output.push(*b),
                Lz77Token::Match { length, distance } => {
                    for _ in 0..*length {
                        let pos = output.len() - *distance as usize;
                        output.push(output[pos]);
                    }
                }
            }
        }

        assert_eq!(output, input);
    }

    #[test]
    fn test_level_0_store() {
        let input = b"test data test data";
        let tokens = Lz77Encoder::compress_all(input, 0);

        // Level 0 should be all literals
        assert!(tokens.iter().all(|t| matches!(t, Lz77Token::Literal(_))));
    }

    #[test]
    fn test_hash() {
        // Hash should be consistent
        let h1 = Lz77Encoder::hash(b'a', b'b', b'c');
        let h2 = Lz77Encoder::hash(b'a', b'b', b'c');
        assert_eq!(h1, h2);

        // Different input should (usually) give different hash
        let h3 = Lz77Encoder::hash(b'x', b'y', b'z');
        // Note: collisions are possible, so we don't assert inequality
        let _ = h3;
    }
}
