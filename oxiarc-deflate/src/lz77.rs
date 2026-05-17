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
pub(crate) const HASH_SIZE: usize = 32768;

/// Hash mask.
const HASH_MASK: usize = HASH_SIZE - 1;

/// Number of hash chain entries to check.
const MAX_CHAIN_LENGTH: usize = 4096;

/// LZ77 match-finding tuning parameters.
///
/// All fields have per-level defaults that reproduce the existing encoder output
/// bit-for-bit. The heuristics only activate when the caller explicitly sets values
/// smaller than the per-level defaults.
#[derive(Clone, Debug)]
pub struct Lz77Params {
    /// Stop searching when a match of this length or longer is found.
    ///
    /// Range: [`MIN_MATCH`, `MAX_MATCH`]. Setting this low trades compression ratio
    /// for speed; the default at each level reproduces the legacy encoder output.
    pub nice_length: u16,
    /// Maximum hash-chain steps per encoded position.
    ///
    /// Setting `u32::MAX` (or any value ≥ `MAX_CHAIN_LENGTH`) is equivalent to the
    /// uncapped behaviour of the original encoder at level 9.
    pub max_chain: u32,
    /// Once a match of at least this length is found, reduce the remaining chain
    /// budget by 75 % (mirrors zlib's `good_match` heuristic).
    ///
    /// Set to `MAX_MATCH + 1` (the default) to disable the reduction entirely.
    pub good_length: u16,
}

impl Lz77Params {
    /// Return parameters that reproduce the current (legacy) encoder output for
    /// `level` (0–9).
    ///
    /// `good_length` is always set to `MAX_MATCH + 1` (disabled) so the new code
    /// path is dead and byte-identical output is guaranteed.
    pub fn for_level(level: u32) -> Self {
        let (max_chain, _, _, nice_length) = match level.min(9) {
            0 => (0usize, MAX_MATCH + 1, false, MAX_MATCH),
            1 => (4, 4, false, MAX_MATCH),
            2 => (8, 4, false, MAX_MATCH),
            3 => (16, 4, false, MAX_MATCH),
            4 => (32, 4, false, MAX_MATCH),
            5 => (64, 4, true, 64),
            6 => (128, 4, true, 128),
            7 => (256, 3, true, 192),
            8 => (1024, 3, true, 250),
            9 => (MAX_CHAIN_LENGTH, 3, true, MAX_MATCH),
            _ => unreachable!(),
        };
        Self {
            nice_length: nice_length as u16,
            max_chain: max_chain as u32,
            // Never fire by default — keeps output byte-identical.
            good_length: (MAX_MATCH + 1) as u16,
        }
    }
}

impl Default for Lz77Params {
    fn default() -> Self {
        Self::for_level(6)
    }
}

/// Named presets for LZ77 match-finding heuristics.
///
/// Each preset trades speed for compression ratio. `Default` reproduces the
/// level-6 encoder's implicit parameters (same output as `Lz77Params::for_level(6)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lz77Preset {
    /// Fastest search: small nice_length and small max_chain.
    Fast,
    /// Balanced: level-6 defaults (same as `Lz77Params::default()`).
    Default,
    /// Better ratio: larger nice_length and more chain steps.
    Best,
    /// Maximum search: full nice_length and uncapped chain.
    Ultra,
}

impl Lz77Preset {
    /// Convert the preset to [`Lz77Params`].
    pub fn params(self) -> Lz77Params {
        match self {
            Lz77Preset::Fast => Lz77Params {
                nice_length: 16,
                max_chain: 8,
                good_length: (MAX_MATCH + 1) as u16,
            },
            Lz77Preset::Default => Lz77Params::for_level(6),
            Lz77Preset::Best => Lz77Params {
                nice_length: 128,
                max_chain: 256,
                good_length: (MAX_MATCH + 1) as u16,
            },
            Lz77Preset::Ultra => Lz77Params {
                nice_length: MAX_MATCH as u16,
                // Use MAX_CHAIN_LENGTH (4096) as the practical upper bound.
                // u32::MAX would work but causes very long encode times on
                // repetitive input since every position in the window is visited.
                max_chain: MAX_CHAIN_LENGTH as u32,
                good_length: (MAX_MATCH + 1) as u16,
            },
        }
    }
}

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
    /// Minimum match length to accept from the hash chain search.
    min_match: usize,
    /// Enable lazy matching.
    lazy_match: bool,
    /// Nice match length: stop searching when match reaches this length.
    nice_length: usize,
    /// Minimum useful match length: matches shorter than this are emitted as literals.
    /// Default equals `min_match` from the level table (3 or 4).
    min_useful_match: usize,
    /// Good-length threshold for chain-budget reduction.
    ///
    /// When `best_len >= good_length`, the remaining chain budget is reduced to 25 %
    /// of what is left (mirrors zlib's `good_match` heuristic). Defaults to
    /// `MAX_MATCH + 1` (disabled).
    good_length: usize,
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
        // Tuple: (max_chain, min_match, lazy_match, nice_length)
        let (max_chain, min_match, lazy_match, nice_length) = match level {
            0 => (0, MAX_MATCH + 1, false, MAX_MATCH), // Store only
            1 => (4, 4, false, MAX_MATCH),
            2 => (8, 4, false, MAX_MATCH),
            3 => (16, 4, false, MAX_MATCH),
            4 => (32, 4, false, MAX_MATCH),
            5 => (64, 4, true, 64),
            6 => (128, 4, true, 128),
            7 => (256, 3, true, 192),
            8 => (1024, 3, true, 250),
            9 => (MAX_CHAIN_LENGTH, 3, true, MAX_MATCH),
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
            nice_length,
            min_useful_match: min_match,
            // Disabled by default — no change to legacy output.
            good_length: MAX_MATCH + 1,
        }
    }

    /// Create a new LZ77 encoder at `level` using caller-supplied, pre-zeroed
    /// buffers from a pool.
    ///
    /// The buffers must have exactly the required lengths:
    /// - `window`: `WINDOW_SIZE * 2` bytes (all zeroed)
    /// - `hash_table`: `HASH_SIZE` u16 entries (all zeroed)
    /// - `hash_chain`: `WINDOW_SIZE` u16 entries (all zeroed)
    ///
    /// This constructor is `pub(crate)` and intended for use by
    /// [`DeflatePool`](crate::pool::DeflatePool).
    pub(crate) fn with_level_and_buffers(
        level: u8,
        window: Vec<u8>,
        hash_table: Vec<u16>,
        hash_chain: Vec<u16>,
    ) -> Self {
        let level = level.min(9);

        let (max_chain, min_match, lazy_match, nice_length) = match level {
            0 => (0, MAX_MATCH + 1, false, MAX_MATCH),
            1 => (4, 4, false, MAX_MATCH),
            2 => (8, 4, false, MAX_MATCH),
            3 => (16, 4, false, MAX_MATCH),
            4 => (32, 4, false, MAX_MATCH),
            5 => (64, 4, true, 64),
            6 => (128, 4, true, 128),
            7 => (256, 3, true, 192),
            8 => (1024, 3, true, 250),
            9 => (MAX_CHAIN_LENGTH, 3, true, MAX_MATCH),
            _ => unreachable!(),
        };

        Self {
            window,
            window_pos: 0,
            hash_table,
            hash_chain,
            max_chain,
            min_match,
            lazy_match,
            nice_length,
            min_useful_match: min_match,
            good_length: MAX_MATCH + 1,
        }
    }

    /// Consume the encoder and return the underlying buffers for pool recycling.
    ///
    /// Returns `(window, hash_table, hash_chain)`.  This is `pub(crate)` and
    /// intended to be called after encoding completes when a pool is in use.
    pub(crate) fn into_buffers(self) -> (Vec<u8>, Vec<u16>, Vec<u16>) {
        (self.window, self.hash_table, self.hash_chain)
    }

    /// Override the nice match length (stops searching for longer matches once a match
    /// of this length is found). Default is level-dependent (64–258).
    /// Clamped to `[MIN_MATCH, MAX_MATCH]`.
    pub fn with_nice_length(mut self, nice: usize) -> Self {
        self.nice_length = nice.clamp(MIN_MATCH, MAX_MATCH);
        self
    }

    /// Set the minimum useful match length. Matches shorter than this are treated as
    /// literals even if the hash chain finds them. Clamped to `[MIN_MATCH, MAX_MATCH]`.
    pub fn with_min_match_length(mut self, min_match: usize) -> Self {
        self.min_useful_match = min_match.clamp(MIN_MATCH, MAX_MATCH);
        self
    }

    /// Override the maximum hash-chain walk length.
    ///
    /// Larger values find better matches but cost more CPU time.
    /// Use `usize::MAX` (or a value ≥ `MAX_CHAIN_LENGTH`) for uncapped search.
    pub fn with_max_chain(mut self, max_chain: usize) -> Self {
        self.max_chain = max_chain;
        self
    }

    /// Set the good-length threshold for chain-budget reduction.
    ///
    /// Once the best match found so far is at least `good_length` bytes long,
    /// the remaining chain budget is cut to 25 % of what is left. This mirrors
    /// the zlib `good_match` heuristic and speeds up encoding when a "good
    /// enough" match is found early.
    ///
    /// Set to `MAX_MATCH + 1` (or any value > `MAX_MATCH`) to disable entirely.
    /// Clamped to `[MIN_MATCH, MAX_MATCH + 1]`.
    pub fn with_good_length(mut self, good_length: usize) -> Self {
        self.good_length = good_length.clamp(MIN_MATCH, MAX_MATCH + 1);
        self
    }

    /// Apply [`Lz77Params`] to this encoder, overriding `nice_length`,
    /// `max_chain`, and `good_length` in one call.
    pub fn with_lz77_params(self, params: &Lz77Params) -> Self {
        self.with_nice_length(params.nice_length as usize)
            .with_max_chain(params.max_chain as usize)
            .with_good_length(params.good_length as usize)
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
        // We need at least 4 bytes to form the hash (MIN_MATCH stays 3; 4th byte reduces collisions)
        let len = dict_to_use.len();
        if len >= 4 {
            for pos in 0..len.saturating_sub(3) {
                // Guard: pos+3 < len is guaranteed by the loop bound above
                let h = Self::hash(
                    dict_to_use[pos],
                    dict_to_use[pos + 1],
                    dict_to_use[pos + 2],
                    dict_to_use[pos + 3],
                );
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

    /// Compute hash for 4 bytes using improved mixing for better distribution.
    ///
    /// Uses 4 bytes to reduce hash collisions, though MIN_MATCH remains 3.
    #[inline(always)]
    fn hash(b0: u8, b1: u8, b2: u8, b3: u8) -> usize {
        // Improved hash with better avalanche properties
        // Multiply and rotate for better distribution
        let h = ((b0 as usize).wrapping_mul(506832829))
            ^ ((b1 as usize).wrapping_mul(2654435761) << 8)
            ^ ((b2 as usize).wrapping_mul(374761393) << 16)
            ^ ((b3 as usize).wrapping_mul(1000000007) << 24);
        (h ^ (h >> 15)) & HASH_MASK
    }

    /// Expose a mutable view of the window buffer for bulk loading.
    pub(crate) fn window_as_slice_mut(&mut self) -> &mut [u8] {
        &mut self.window
    }

    /// Reset only the hash table and chain (not the window content or window_pos).
    pub(crate) fn reset_hash(&mut self) {
        self.hash_table.fill(0);
        self.hash_chain.fill(0);
    }

    /// Update hash table with current position.
    fn update_hash(&mut self, pos: usize) {
        if pos + 3 < self.window.len() {
            let h = Self::hash(
                self.window[pos],
                self.window[pos + 1],
                self.window[pos + 2],
                self.window[pos + 3],
            );
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

        // Need at least 4 bytes for the hash; fall back gracefully when near end.
        let h = if pos + 3 < self.window.len() {
            Self::hash(
                self.window[pos],
                self.window[pos + 1],
                self.window[pos + 2],
                self.window[pos + 3],
            )
        } else if pos + 2 < self.window.len() {
            Self::hash(
                self.window[pos],
                self.window[pos + 1],
                self.window[pos + 2],
                0,
            )
        } else {
            return None;
        };

        let mut match_pos = self.hash_table[h] as usize;
        let mut best_len = self.min_match - 1;
        let mut best_dist = 0usize;

        let min_pos = pos.saturating_sub(WINDOW_SIZE);
        let mut chain_len = 0;
        let max_check = max_len.min(MAX_MATCH);
        // Effective chain cap — may be tightened by the good_length heuristic.
        let mut effective_max_chain = self.max_chain;
        // Ensure the good_length heuristic fires at most once per call.
        let mut good_length_applied = false;

        while match_pos >= min_pos && match_pos < pos && chain_len < effective_max_chain {
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

                            // Early exit if we found a great match or reached nice_length
                            if len >= max_len || len >= MAX_MATCH || best_len >= self.nice_length {
                                break;
                            }

                            // good_length heuristic: if the match is "good enough",
                            // cut remaining chain budget to 25 % (mirrors zlib good_match).
                            // Only applied once to avoid re-triggering on each improvement.
                            if !good_length_applied && best_len >= self.good_length {
                                let remaining = effective_max_chain.saturating_sub(chain_len);
                                effective_max_chain = chain_len + remaining / 4;
                                good_length_applied = true;
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
                    // Reject matches shorter than the configured minimum useful length.
                    if (length as usize) < self.min_useful_match {
                        tokens.push(Lz77Token::Literal(self.window[pos]));
                        self.update_hash(pos);
                        pos += 1;
                        continue;
                    }

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

    /// Update the hash table for a single absolute window position `wpos`.
    ///
    /// The optimal parser pre-fills the window with the entire input, then calls
    /// this for each position (0..data_len) in order before `find_all_matches`.
    /// This keeps the hash chain consistent with what `find_match` expects.
    pub(crate) fn update_hash_single(&mut self, wpos: usize) {
        if wpos + 3 < self.window.len() {
            let h = Self::hash(
                self.window[wpos],
                self.window[wpos + 1],
                self.window[wpos + 2],
                self.window[wpos + 3],
            );
            let prev = self.hash_table[h];
            self.hash_chain[wpos & (WINDOW_SIZE - 1)] = prev;
            self.hash_table[h] = wpos as u16;
        }
    }

    /// Return all distinct useful `(length, distance)` matches at absolute window
    /// position `wpos`, where `data_end = wpos + remaining` bounds the match length.
    ///
    /// "Useful" means each returned entry is strictly longer than the previous one
    /// (the list is sorted by ascending length).  The caller must have called
    /// `update_hash_single(w)` for every `w` in `0..wpos` before invoking this so
    /// that the hash chain contains the necessary history.
    ///
    /// Returns an empty Vec when `remaining < MIN_MATCH` or no match is found.
    pub(crate) fn find_all_matches(&self, wpos: usize, remaining: usize) -> Vec<(u16, u16)> {
        if remaining < MIN_MATCH {
            return Vec::new();
        }
        if wpos >= self.window.len() {
            return Vec::new();
        }

        let h = if wpos + 3 < self.window.len() {
            Self::hash(
                self.window[wpos],
                self.window[wpos + 1],
                self.window[wpos + 2],
                self.window[wpos + 3],
            )
        } else if wpos + 2 < self.window.len() {
            Self::hash(
                self.window[wpos],
                self.window[wpos + 1],
                self.window[wpos + 2],
                0,
            )
        } else {
            return Vec::new();
        };

        let mut results: Vec<(u16, u16)> = Vec::new();
        let mut best_len = MIN_MATCH - 1;

        let mut match_wpos = self.hash_table[h] as usize;
        let min_wpos = wpos.saturating_sub(WINDOW_SIZE);
        let max_check = remaining.min(MAX_MATCH);
        let mut chain_len = 0;

        while match_wpos >= min_wpos && match_wpos < wpos && chain_len < MAX_CHAIN_LENGTH {
            let dist = wpos - match_wpos;
            if dist > 0 && dist <= WINDOW_SIZE {
                let mpos_at_best = match_wpos + best_len;
                let wpos_at_best = wpos + best_len;

                let quick_ok = wpos_at_best < self.window.len()
                    && mpos_at_best < self.window.len()
                    && self.window[mpos_at_best] == self.window[wpos_at_best];

                if quick_ok && self.window[match_wpos] == self.window[wpos] {
                    let mut len = 1usize;

                    if len < max_check && self.window[match_wpos + 1] == self.window[wpos + 1] {
                        len = 2;
                        if len < max_check && self.window[match_wpos + 2] == self.window[wpos + 2] {
                            len = 3;
                            while len < max_check
                                && self.window[match_wpos + len] == self.window[wpos + len]
                            {
                                len += 1;
                            }
                        }
                    }

                    if len > best_len {
                        best_len = len;
                        results.push((len as u16, dist as u16));
                        if len >= MAX_MATCH {
                            break;
                        }
                    }
                }
            }

            match_wpos = self.hash_chain[match_wpos & (WINDOW_SIZE - 1)] as usize;
            chain_len += 1;
        }

        results
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
        let h1 = Lz77Encoder::hash(b'a', b'b', b'c', b'd');
        let h2 = Lz77Encoder::hash(b'a', b'b', b'c', b'd');
        assert_eq!(h1, h2);

        // Different input should (usually) give different hash
        let h3 = Lz77Encoder::hash(b'x', b'y', b'z', b'w');
        // Note: collisions are possible, so we don't assert inequality
        let _ = h3;
    }

    #[test]
    fn test_with_nice_length_builder() {
        // Builder compiles; roundtrip via manual token decode verifies correctness.
        let input: Vec<u8> = (0u8..=127).cycle().take(2000).collect();
        let mut encoder = Lz77Encoder::with_level(6).with_nice_length(32);
        let tokens = encoder.compress(&input);

        // Reconstruct and verify
        let mut output = Vec::new();
        for token in &tokens {
            match token {
                Lz77Token::Literal(b) => output.push(*b),
                Lz77Token::Match { length, distance } => {
                    for _ in 0..(*length as usize) {
                        let pos = output.len() - *distance as usize;
                        output.push(output[pos]);
                    }
                }
            }
        }
        assert_eq!(output, input, "roundtrip failed with nice_length=32");
    }

    #[test]
    fn test_nice_length_affects_compression_speed() {
        // With a very small nice_length the encoder stops searching early,
        // producing more tokens in less chain-walk time.  We verify the
        // roundtrip still holds; an exact timing assertion is fragile, so we
        // just confirm both variants produce valid output.
        let input: Vec<u8> = b"abcdefgh".iter().cycle().take(50_000).copied().collect();

        let mut enc_fast = Lz77Encoder::with_level(9).with_nice_length(8);
        let tokens_fast = enc_fast.compress(&input);

        let mut enc_full = Lz77Encoder::with_level(9);
        let tokens_full = enc_full.compress(&input);

        // Both must roundtrip.
        for (label, tokens) in [
            ("fast", tokens_fast.as_slice()),
            ("full", tokens_full.as_slice()),
        ] {
            let mut out = Vec::with_capacity(input.len());
            for token in tokens {
                match token {
                    Lz77Token::Literal(b) => out.push(*b),
                    Lz77Token::Match { length, distance } => {
                        for _ in 0..*length {
                            let pos = out.len() - *distance as usize;
                            out.push(out[pos]);
                        }
                    }
                }
            }
            assert_eq!(out, input, "roundtrip failed for {} encoder", label);
        }

        // The small nice_length encoder is expected to produce more tokens
        // (shorter matches, less aggressive compression).
        assert!(
            tokens_fast.len() >= tokens_full.len(),
            "fast encoder ({} tokens) should produce >= tokens than full ({} tokens)",
            tokens_fast.len(),
            tokens_full.len()
        );
    }

    #[test]
    fn test_with_min_match_length_builder() {
        let input: Vec<u8> = (0u8..=255).cycle().take(3000).collect();
        let mut encoder = Lz77Encoder::new().with_min_match_length(4);
        let tokens = encoder.compress(&input);

        // Reconstruct and verify roundtrip.
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
        assert_eq!(output, input, "roundtrip failed with min_match_length=4");
    }

    #[test]
    fn test_min_match_length_skip_short_matches() {
        // With min_match=6, short 3-5 byte runs must be emitted as literals.
        // Using pseudo-random data to avoid long natural matches.
        // When we suppress short matches (min_useful_match=6), the encoder
        // is forced to emit literals for positions that would otherwise use a
        // short back-reference, so the token stream is strictly larger than
        // the default (min_useful_match=3/4).
        let mut input = Vec::with_capacity(10_000);
        let mut state: u32 = 0xDEAD_BEEF;
        for _ in 0..10_000 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            input.push((state >> 24) as u8);
        }

        let tokens_default = Lz77Encoder::compress_all(&input, 6);
        let tokens_min6 = {
            let mut enc = Lz77Encoder::with_level(6).with_min_match_length(6);
            enc.compress(&input)
        };

        // With min_match=6 all emitted Match tokens must have length >= 6.
        for token in &tokens_min6 {
            if let Lz77Token::Match { length, .. } = token {
                assert!(
                    *length >= 6,
                    "min_useful_match=6 produced a match of length {}",
                    length
                );
            }
        }

        // Suppressing short matches produces >= as many tokens as the default encoder.
        assert!(
            tokens_min6.len() >= tokens_default.len(),
            "min6 tokens ({}) should be >= default tokens ({})",
            tokens_min6.len(),
            tokens_default.len()
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Lz77Params / Lz77Preset tests
    // ─────────────────────────────────────────────────────────────────────────

    /// Helper: decode LZ77 token stream back to raw bytes.
    fn decode_tokens(tokens: &[Lz77Token]) -> Vec<u8> {
        let mut out = Vec::new();
        for token in tokens {
            match token {
                Lz77Token::Literal(b) => out.push(*b),
                Lz77Token::Match { length, distance } => {
                    for _ in 0..*length {
                        let pos = out.len() - *distance as usize;
                        out.push(out[pos]);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn test_lz77_params_for_level_defaults() {
        // Lz77Params::for_level must match the hard-coded per-level table.
        // Verify nice_length and max_chain for the levels that have non-trivial values.
        let table: [(u32, usize, usize); 10] = [
            (0, 0, MAX_MATCH),
            (1, 4, MAX_MATCH),
            (2, 8, MAX_MATCH),
            (3, 16, MAX_MATCH),
            (4, 32, MAX_MATCH),
            (5, 64, 64),
            (6, 128, 128),
            (7, 256, 192),
            (8, 1024, 250),
            (9, MAX_CHAIN_LENGTH, MAX_MATCH),
        ];
        for (level, expected_max_chain, expected_nice) in table {
            let p = Lz77Params::for_level(level);
            assert_eq!(
                p.max_chain as usize, expected_max_chain,
                "max_chain mismatch at level {}",
                level
            );
            assert_eq!(
                p.nice_length as usize, expected_nice,
                "nice_length mismatch at level {}",
                level
            );
            // good_length must be disabled (> MAX_MATCH) so the heuristic never fires.
            assert!(
                p.good_length as usize > MAX_MATCH,
                "good_length should be disabled (> MAX_MATCH) at level {}; got {}",
                level,
                p.good_length
            );
        }
    }

    #[test]
    fn test_lz77_params_max_nice_length_is_byte_identical() {
        // Encoding with explicit Lz77Params that match for_level(6) must produce
        // exactly the same token stream as the default level-6 encoder.
        let input: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(32_768)
            .copied()
            .collect();

        let tokens_default = Lz77Encoder::compress_all(&input, 6);

        let params = Lz77Params::for_level(6);
        let tokens_explicit = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&params);
            enc.compress(&input)
        };

        assert_eq!(
            tokens_default, tokens_explicit,
            "explicit Lz77Params::for_level(6) must produce byte-identical token stream"
        );
    }

    #[test]
    fn test_lz77_params_small_nice_length_roundtrip() {
        // A small nice_length still produces valid (decompressible) output.
        let input: Vec<u8> = (0u8..=127).cycle().take(32_768).collect();
        let params = Lz77Params {
            nice_length: MIN_MATCH as u16,
            max_chain: 128,
            good_length: (MAX_MATCH + 1) as u16,
        };
        let tokens = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&params);
            enc.compress(&input)
        };
        assert_eq!(
            decode_tokens(&tokens),
            input,
            "roundtrip failed with nice_length=MIN_MATCH"
        );
    }

    #[test]
    fn test_lz77_preset_ultra_roundtrip() {
        // Use small input so the uncapped (u32::MAX) chain doesn't time out.
        let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz"
            .iter()
            .cycle()
            .take(2_048)
            .copied()
            .collect();
        let params = Lz77Preset::Ultra.params();
        let tokens = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&params);
            enc.compress(&input)
        };
        assert_eq!(
            decode_tokens(&tokens),
            input,
            "Lz77Preset::Ultra roundtrip failed"
        );
    }

    #[test]
    fn test_lz77_preset_fast_roundtrip_and_ratio() {
        // Fast and Ultra must both roundtrip correctly.
        // Fast is expected to produce >= tokens (worse or equal ratio) vs Ultra on
        // moderately compressible, varied input.
        // Keep input small enough that Ultra doesn't time out.
        let input: Vec<u8> = b"abcdefgh".iter().cycle().take(4_096).copied().collect();

        let fast_params = Lz77Preset::Fast.params();
        let ultra_params = Lz77Preset::Ultra.params();

        let tokens_fast = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&fast_params);
            enc.compress(&input)
        };
        let tokens_ultra = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&ultra_params);
            enc.compress(&input)
        };

        // Both must decode correctly.
        assert_eq!(
            decode_tokens(&tokens_fast),
            input,
            "Fast preset roundtrip failed"
        );
        assert_eq!(
            decode_tokens(&tokens_ultra),
            input,
            "Ultra preset roundtrip failed"
        );

        // Fast uses a small chain so may miss longer matches → more tokens.
        assert!(
            tokens_fast.len() >= tokens_ultra.len(),
            "Fast ({} tokens) should produce >= tokens than Ultra ({} tokens)",
            tokens_fast.len(),
            tokens_ultra.len()
        );
    }

    #[test]
    fn test_with_max_chain_builder() {
        // Reducing max_chain must still produce valid output.
        let input: Vec<u8> = b"hello world "
            .iter()
            .cycle()
            .take(10_000)
            .copied()
            .collect();
        let mut enc = Lz77Encoder::with_level(6).with_max_chain(4);
        let tokens = enc.compress(&input);
        assert_eq!(
            decode_tokens(&tokens),
            input,
            "with_max_chain(4) roundtrip failed"
        );
    }

    #[test]
    fn test_with_good_length_builder() {
        // Enabling good_length heuristic must still produce valid output.
        let input: Vec<u8> = b"abcdefgh".iter().cycle().take(32_768).copied().collect();
        let mut enc = Lz77Encoder::with_level(6).with_good_length(32);
        let tokens = enc.compress(&input);
        assert_eq!(
            decode_tokens(&tokens),
            input,
            "with_good_length(32) roundtrip failed"
        );
    }

    #[test]
    fn test_good_length_default_is_disabled() {
        // good_length default (MAX_MATCH+1) must not change the token stream vs default encoder.
        let input: Vec<u8> = b"the quick brown fox "
            .iter()
            .cycle()
            .take(32_768)
            .copied()
            .collect();
        let tokens_default = Lz77Encoder::compress_all(&input, 6);
        let params_default = Lz77Params::for_level(6);
        let tokens_params = {
            let mut enc = Lz77Encoder::with_level(6).with_lz77_params(&params_default);
            enc.compress(&input)
        };
        assert_eq!(
            tokens_default, tokens_params,
            "default good_length must not change token stream"
        );
    }
}
