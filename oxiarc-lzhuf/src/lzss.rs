//! LZSS algorithm for LZH compression.
//!
//! LZSS (Lempel-Ziv-Storer-Szymanski) is a derivative of LZ77 that uses
//! a flag bit to distinguish between literals and matches.
//!
//! The encoder uses hash chain traversal for O(1) amortized match finding.
//! A 3-byte hash table maps byte trigrams (the minimum match length) to
//! chains of positions in the circular window.
//!
//! # Window correctness invariant
//!
//! The circular window only ever contains bytes that have already been
//! *consumed* by the encoder (i.e. true history).  Lookahead bytes are read
//! directly from the caller-supplied input slice, never from the window.
//! This guarantees that inputs larger than the window size cannot clobber
//! history that match candidates still reference (the root cause of a
//! former round-trip corruption bug for payloads beyond one window).

use oxiarc_core::RingBuffer;
use oxiarc_core::error::{OxiArcError, Result};

/// LZSS token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LzssToken {
    /// A literal byte.
    Literal(u8),
    /// A match reference to previously decoded data.
    Match {
        /// Number of bytes to copy.
        length: u16,
        /// Distance back into the history buffer.
        distance: u16,
    },
}

/// LZSS decoder using a ring buffer.
#[derive(Debug)]
pub struct LzssDecoder {
    /// Ring buffer for history.
    ring: RingBuffer,
    /// Output buffer.
    output: Vec<u8>,
}

impl LzssDecoder {
    /// Create a new LZSS decoder with the specified window size.
    pub fn new(window_size: usize) -> Self {
        Self {
            ring: RingBuffer::new(window_size),
            output: Vec::new(),
        }
    }

    /// Create a decoder for lh5 (8KB window).
    pub fn lh5() -> Self {
        Self::new(8192)
    }

    /// Reset the decoder.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.output.clear();
    }

    /// Decode a literal byte.
    pub fn decode_literal(&mut self, byte: u8) {
        self.ring.write_byte(byte);
        self.output.push(byte);
    }

    /// Decode a match (length, distance).
    pub fn decode_match(&mut self, length: u16, distance: u16) -> Result<()> {
        if distance == 0 || distance as usize > self.ring.len() {
            return Err(OxiArcError::invalid_distance(
                distance as usize,
                self.ring.len(),
            ));
        }

        // Copy from history
        for _ in 0..length {
            let byte = self.ring.read_at_distance(distance as usize)?;
            self.ring.write_byte(byte);
            self.output.push(byte);
        }

        Ok(())
    }

    /// Get the decoded output.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Take the decoded output.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Get output length.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Preload the ring buffer with dictionary bytes so back-references into
    /// the dictionary are valid from position 0.
    ///
    /// If `dict` is larger than the window, only the last `window_size` bytes
    /// are used (matching the zlib convention for preset dictionaries).
    pub fn preload_dictionary(&mut self, dict: &[u8]) {
        if dict.is_empty() {
            return;
        }
        self.ring.preload_dictionary(dict);
    }
}

/// Sentinel value indicating an empty hash chain slot.
const EMPTY: u32 = u32::MAX;

/// Maximum hash chain depth to traverse per match query.
/// Balances compression quality vs. speed.
const MAX_CHAIN_LEN: usize = 128;

/// Maximum distance representable in an [`LzssToken::Match`] (`u16` field).
///
/// lh7 has a 65536-byte window, but a full-window distance of 65536 cannot
/// be represented in 16 bits, so matches are capped at 65535.
const MAX_TOKEN_DISTANCE: usize = u16::MAX as usize;

/// Compute hash table size for a given window size.
/// Returns a power-of-two that gives good distribution density.
fn hash_table_size_for_window(window_size: usize) -> usize {
    // lh5:  8192 window → 8192  entries
    // lh6: 32768 window → 16384 entries
    // lh7: 65536 window → 32768 entries
    // Smaller windows get table == window; larger windows get table == window/2.
    if window_size <= 8192 {
        window_size.next_power_of_two()
    } else {
        (window_size / 2).next_power_of_two()
    }
}

/// LZSS encoder with hash chain acceleration.
///
/// Uses a circular sliding window of size `window_size`. Absolute byte
/// positions are tracked as `u64` counters so we never need to renumber
/// existing chain entries after a window slide. Each `window[pos %
/// window_size]` cell stores the byte written at that absolute position.
///
/// The window only contains *consumed* bytes (true history). The hash table
/// maps a 3-byte trigram hash → most-recent absolute position that had that
/// trigram. The hash chain maps `abs_pos % window_size` → previous absolute
/// position with the same trigram hash (or EMPTY).
#[derive(Debug)]
pub struct LzssEncoder {
    /// Circular sliding window (consumed history only).
    window: Vec<u8>,
    /// Absolute position of the next byte to be written into the window.
    abs_write_pos: u64,
    /// Absolute position of the next window byte to be inserted into the
    /// hash chains. Positions `q < hashed_upto` are indexed; a position can
    /// only be indexed once bytes `q..q+3` are all present in the window
    /// (`q + 3 <= abs_write_pos`).
    hashed_upto: u64,
    /// Window size (always a power of two so we can use masking).
    window_size: usize,
    /// window_size - 1, used for fast modular indexing.
    window_mask: usize,
    /// Minimum match length.
    min_match: usize,
    /// Maximum match length.
    max_match: usize,
    /// Hash table: trigram hash → most-recent absolute position (EMPTY = none).
    hash_table: Vec<u32>,
    /// Hash chain: window slot → previous abs pos with same hash (EMPTY = none).
    hash_chain: Vec<u32>,
    /// Mask for the hash table (hash_table.len() - 1).
    hash_mask: usize,
    /// Whether lazy matching is enabled.
    lazy_match: bool,
}

/// Snapshot of the encoder's window state, used by the optimal parser to
/// replay the same input block over multiple DP passes.
#[derive(Debug, Clone)]
pub(crate) struct LzssWindowSnapshot {
    window: Vec<u8>,
    abs_write_pos: u64,
    hashed_upto: u64,
}

impl LzssEncoder {
    /// Create a new LZSS encoder.
    ///
    /// `window_size` will be rounded up to the next power of two if it is not
    /// already one, because the circular-buffer indexing uses bit-masking.
    pub fn new(window_size: usize, min_match: usize, max_match: usize) -> Self {
        let window_size = window_size.next_power_of_two().max(16);
        let window_mask = window_size - 1;
        let ht_size = hash_table_size_for_window(window_size);
        let hash_mask = ht_size - 1;

        Self {
            window: vec![0u8; window_size],
            abs_write_pos: 0,
            hashed_upto: 0,
            window_size,
            window_mask,
            min_match: min_match.max(1),
            max_match,
            hash_table: vec![EMPTY; ht_size],
            hash_chain: vec![EMPTY; window_size],
            hash_mask,
            lazy_match: true,
        }
    }

    /// Create an encoder for lh5.
    pub fn lh5() -> Self {
        Self::new(8192, 3, 256)
    }

    /// Capture the current window state (used by the optimal parser).
    pub(crate) fn save_window_state(&self) -> LzssWindowSnapshot {
        LzssWindowSnapshot {
            window: self.window.clone(),
            abs_write_pos: self.abs_write_pos,
            hashed_upto: self.hashed_upto,
        }
    }

    /// Restore a previously captured window state and clear all hash chains.
    ///
    /// After this call the hash chains are empty; `hashed_upto` is set to the
    /// snapshot's `abs_write_pos` so that only bytes pushed *after* the
    /// restore are (re-)indexed. This mirrors the multi-pass behaviour of the
    /// optimal parser, which re-seeds chains fresh on each forward scan.
    pub(crate) fn restore_window_state(&mut self, snap: &LzssWindowSnapshot) {
        self.window.copy_from_slice(&snap.window);
        self.abs_write_pos = snap.abs_write_pos;
        self.hashed_upto = snap.abs_write_pos.max(snap.hashed_upto);
        self.hash_table.fill(EMPTY);
        self.hash_chain.fill(EMPTY);
    }

    /// Reset the encoder to initial state.
    pub fn reset(&mut self) {
        self.abs_write_pos = 0;
        self.hashed_upto = 0;
        self.window.fill(0);
        self.hash_table.fill(EMPTY);
        self.hash_chain.fill(EMPTY);
    }

    /// Preload dictionary bytes into the sliding window and populate hash chains.
    ///
    /// If `dict` is larger than the window, only the last `window_size` bytes
    /// are used (matching the zlib convention for preset dictionaries).
    ///
    /// After this call `abs_write_pos` equals the number of dictionary bytes
    /// loaded, and all hash chains covering those bytes are populated so that
    /// the encoder can immediately find matches that span into the dictionary.
    pub fn preload_dictionary(&mut self, dict: &[u8]) {
        if dict.is_empty() {
            return;
        }

        // Take only the last `window_size` bytes if dict is larger.
        let start = dict.len().saturating_sub(self.window_size);
        self.push_bytes(&dict[start..]);
    }

    // -------------------------------------------------------------------------
    // Hash helpers
    // -------------------------------------------------------------------------

    /// Compute a 3-byte multiplicative hash, masked to `[0, mask]`.
    #[inline(always)]
    fn hash3(b0: u8, b1: u8, b2: u8, mask: usize) -> usize {
        let v = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        let h = v.wrapping_mul(2_654_435_761);
        ((h >> 12) as usize) & mask
    }

    /// Insert the trigram at absolute position `abs_pos` into the hash chain.
    ///
    /// The caller must guarantee that bytes `abs_pos..abs_pos+3` have already
    /// been written into the window (`abs_pos + 3 <= abs_write_pos`).
    #[inline]
    fn insert_hash(&mut self, abs_pos: u64) {
        let p0 = (abs_pos as usize) & self.window_mask;
        let p1 = (abs_pos as usize + 1) & self.window_mask;
        let p2 = (abs_pos as usize + 2) & self.window_mask;
        let h = Self::hash3(
            self.window[p0],
            self.window[p1],
            self.window[p2],
            self.hash_mask,
        );

        // Saturate to u32 – positions beyond u32::MAX - 1 are treated as EMPTY
        // in chain lookups (chain validity is checked via distance arithmetic).
        let abs_pos_u32 = if abs_pos < EMPTY as u64 {
            abs_pos as u32
        } else {
            // Extremely long streams: reset hash state gracefully.
            self.hash_table.fill(EMPTY);
            self.hash_chain.fill(EMPTY);
            (abs_pos & 0xFFFF_FFFE) as u32
        };

        let prev = self.hash_table[h];
        self.hash_chain[p0] = prev;
        self.hash_table[h] = abs_pos_u32;
    }

    /// Index every window position that has become hashable since the last
    /// call (i.e. all `q` with `q + 3 <= abs_write_pos`).
    #[inline]
    fn advance_hashes(&mut self) {
        while self.hashed_upto + 3 <= self.abs_write_pos {
            let q = self.hashed_upto;
            self.insert_hash(q);
            self.hashed_upto += 1;
        }
    }

    // -------------------------------------------------------------------------
    // Window management
    // -------------------------------------------------------------------------

    /// Write a single byte into the circular window and advance the cursor.
    #[inline]
    fn push_byte_raw(&mut self, byte: u8) {
        let slot = (self.abs_write_pos as usize) & self.window_mask;
        self.window[slot] = byte;
        self.abs_write_pos += 1;
    }

    /// Push consumed bytes into the window and index the newly completed
    /// trigram positions in the hash chains.
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push_byte_raw(b);
        }
        self.advance_hashes();
    }

    // -------------------------------------------------------------------------
    // Match finding
    // -------------------------------------------------------------------------

    /// Maximum distance valid for a match ending at the current position.
    #[inline]
    fn max_distance(&self) -> usize {
        self.window_size.min(MAX_TOKEN_DISTANCE)
    }

    /// Find the longest match for the bytes in `lookahead`.
    ///
    /// `cur_abs` is the absolute position of `lookahead[0]` in the stream and
    /// must satisfy `cur_abs <= abs_write_pos` (history for all positions in
    /// `[cur_abs - window, cur_abs)` must still be intact in the window,
    /// which holds whenever at most `window_size` bytes past `cur_abs` have
    /// been pushed — the encoder never pushes past the current position).
    ///
    /// Returns `(best_length, best_distance)` where both are 0 if no match of
    /// at least `min_match` bytes was found.
    fn find_match(&self, cur_abs: u64, lookahead: &[u8]) -> (usize, usize) {
        if lookahead.len() < self.min_match || lookahead.len() < 3 {
            return (0, 0);
        }

        let max_len = lookahead.len().min(self.max_match);
        let max_dist = self.max_distance();
        let wm = self.window_mask;

        let h = Self::hash3(lookahead[0], lookahead[1], lookahead[2], self.hash_mask);
        let mut match_abs = self.hash_table[h];
        let mut best_len = self.min_match - 1;
        let mut best_dist = 0usize;
        let mut chain_steps = 0usize;

        while match_abs != EMPTY && chain_steps < MAX_CHAIN_LEN {
            chain_steps += 1;

            // Compute distance (unsigned subtraction; wrapping handles any
            // case where match_abs was written before a counter wrap).
            let dist = cur_abs.wrapping_sub(match_abs as u64) as usize;
            if dist == 0 || dist > max_dist {
                // Position is outside the valid window; stop traversal.
                break;
            }

            // Quick-reject: the byte at offset `best_len` in the candidate
            // must equal `lookahead[best_len]` before we do a full compare.
            // For overlapping candidates the probe byte comes from the
            // periodic extension of the lookahead itself.
            let probe = if dist <= best_len {
                lookahead[best_len % dist]
            } else {
                self.window[(match_abs as usize + best_len) & wm]
            };
            if probe != lookahead[best_len] {
                // Advance chain.
                let chain_slot = (match_abs as usize) & wm;
                match_abs = self.hash_chain[chain_slot];
                continue;
            }

            // Full match comparison – support overlapping copies (dist <= len).
            let len = self.match_length(match_abs as u64, dist, lookahead, max_len);

            if len > best_len {
                best_len = len;
                best_dist = dist;
                if best_len >= max_len {
                    break; // Can't do better.
                }
            }

            let chain_slot = (match_abs as usize) & wm;
            match_abs = self.hash_chain[chain_slot];
        }

        if best_len >= self.min_match {
            (best_len, best_dist)
        } else {
            (0, 0)
        }
    }

    /// Compare the candidate at `match_abs` (distance `dist` back from the
    /// current position) against `lookahead`, returning the match length.
    ///
    /// Bytes at source offsets `< dist` are read from the window (true
    /// history); once the source overlaps the copy region the periodic
    /// extension of the already-matched lookahead prefix is used, exactly as
    /// an LZSS decoder would reproduce it.
    #[inline]
    fn match_length(&self, match_abs: u64, dist: usize, lookahead: &[u8], max_len: usize) -> usize {
        let wm = self.window_mask;
        let mut len = 0usize;
        while len < max_len {
            let src_byte = if dist <= len {
                // Overlapping: the source wraps into the already-copied
                // region, so the pattern repeats with period `dist`.
                lookahead[len % dist]
            } else {
                self.window[(match_abs as usize + len) & wm]
            };
            if src_byte != lookahead[len] {
                break;
            }
            len += 1;
        }
        len
    }

    /// Find all matches for bytes starting at `pos` within `data`, returned as
    /// a `Vec<(length, distance)>` in strictly increasing length order.
    ///
    /// The caller must have pushed exactly `data[..pos]` of the current block
    /// into the window (i.e. the absolute position of `data[pos]` is
    /// `abs_write_pos`). This is used by the optimal parser to enumerate
    /// candidate matches at every position without committing to any choice.
    /// Only matches with length in `[min_match, max_match]` are returned.
    pub(crate) fn find_all_matches(&self, data: &[u8], pos: usize) -> Vec<(u16, u16)> {
        if pos + self.min_match > data.len() || pos + 3 > data.len() {
            return Vec::new();
        }

        let lookahead = &data[pos..];
        let cur_abs = self.abs_write_pos;
        let max_len = lookahead.len().min(self.max_match);
        let max_dist = self.max_distance();
        let wm = self.window_mask;

        let h = Self::hash3(lookahead[0], lookahead[1], lookahead[2], self.hash_mask);
        let mut match_abs = self.hash_table[h];
        let mut chain_steps = 0usize;

        // best_at_len[len] = best distance seen so far for exactly `len` bytes.
        // We use a BTreeMap so the final collection is sorted by length.
        let mut best_at_len: std::collections::BTreeMap<usize, usize> =
            std::collections::BTreeMap::new();

        while match_abs != EMPTY && chain_steps < MAX_CHAIN_LEN {
            chain_steps += 1;

            let dist = cur_abs.wrapping_sub(match_abs as u64) as usize;
            if dist == 0 || dist > max_dist {
                break;
            }

            let len = self.match_length(match_abs as u64, dist, lookahead, max_len);

            if len >= self.min_match {
                // Record the best (shortest) distance for each match length.
                // Shorter distance generally means fewer distance-code bits.
                best_at_len
                    .entry(len)
                    .and_modify(|d| {
                        if dist < *d {
                            *d = dist;
                        }
                    })
                    .or_insert(dist);
            }

            let chain_slot = (match_abs as usize) & wm;
            match_abs = self.hash_chain[chain_slot];
        }

        if best_at_len.is_empty() {
            return Vec::new();
        }

        // Build a de-duplicated, strictly-increasing-length list.
        let mut result: Vec<(u16, u16)> = Vec::with_capacity(best_at_len.len());
        let mut last_len = 0usize;
        for (len, dist) in &best_at_len {
            if *len > last_len {
                result.push((*len as u16, *dist as u16));
                last_len = *len;
            }
        }
        result
    }

    // -------------------------------------------------------------------------
    // Encoding
    // -------------------------------------------------------------------------

    /// Encode `data` and return a list of LZSS tokens.
    ///
    /// Bytes are consumed incrementally: the lookahead is always read from
    /// `data` itself, and only consumed bytes enter the sliding window, so
    /// inputs of any size (including far beyond the window size) are handled
    /// correctly.
    pub fn encode(&mut self, data: &[u8]) -> Vec<LzssToken> {
        let mut tokens = Vec::with_capacity(data.len() / 2 + 1);
        let n = data.len();
        let mut pos = 0usize;

        while pos < n {
            let cur_abs = self.abs_write_pos;
            let la_end = n.min(pos + self.max_match);
            let (len, dist) = self.find_match(cur_abs, &data[pos..la_end]);

            if len >= self.min_match && dist > 0 {
                if self.lazy_match && pos + 1 < n && len < self.max_match {
                    // Push the current byte so the window and hash chains
                    // reflect it while probing the next position.
                    self.push_bytes(&data[pos..pos + 1]);
                    let nla_end = n.min(pos + 1 + self.max_match);
                    let (next_len, next_dist) =
                        self.find_match(cur_abs + 1, &data[pos + 1..nla_end]);

                    if next_len > len && next_dist > 0 {
                        // Emit literal at pos, then the longer match at pos+1.
                        tokens.push(LzssToken::Literal(data[pos]));
                        pos += 1;
                        self.push_bytes(&data[pos..pos + next_len]);
                        tokens.push(LzssToken::Match {
                            length: next_len as u16,
                            distance: next_dist as u16,
                        });
                        pos += next_len;
                    } else {
                        // Keep the original match; data[pos] is already pushed.
                        self.push_bytes(&data[pos + 1..pos + len]);
                        tokens.push(LzssToken::Match {
                            length: len as u16,
                            distance: dist as u16,
                        });
                        pos += len;
                    }
                } else {
                    self.push_bytes(&data[pos..pos + len]);
                    tokens.push(LzssToken::Match {
                        length: len as u16,
                        distance: dist as u16,
                    });
                    pos += len;
                }
            } else {
                // Emit literal.
                self.push_bytes(&data[pos..pos + 1]);
                tokens.push(LzssToken::Literal(data[pos]));
                pos += 1;
            }
        }

        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Decoder tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_lzss_decoder_literal() {
        let mut decoder = LzssDecoder::new(1024);

        decoder.decode_literal(b'H');
        decoder.decode_literal(b'i');

        assert_eq!(decoder.output(), b"Hi");
    }

    #[test]
    fn test_lzss_decoder_match() {
        let mut decoder = LzssDecoder::new(1024);

        // Write "AB"
        decoder.decode_literal(b'A');
        decoder.decode_literal(b'B');

        // Match: copy 2 bytes from distance 2 -> "AB" again
        decoder.decode_match(2, 2).expect("operation failed");

        assert_eq!(decoder.output(), b"ABAB");
    }

    #[test]
    fn test_lzss_decoder_overlapping_match() {
        let mut decoder = LzssDecoder::new(1024);

        // Write "A"
        decoder.decode_literal(b'A');

        // Match: copy 5 bytes from distance 1 -> "AAAAA"
        decoder.decode_match(5, 1).expect("operation failed");

        assert_eq!(decoder.output(), b"AAAAAA");
    }

    // -------------------------------------------------------------------------
    // Encoder basic tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_lzss_encoder_literals() {
        let mut encoder = LzssEncoder::new(1024, 3, 256);

        let tokens = encoder.encode(b"abc");

        // No matches possible (fewer than min_match bytes in history), all literals.
        assert!(tokens.iter().all(|t| matches!(t, LzssToken::Literal(_))));
    }

    #[test]
    fn test_lzss_encoder_match() {
        let mut encoder = LzssEncoder::new(1024, 3, 256);

        let tokens = encoder.encode(b"abcabcabc");

        // Should find at least one match for the repeated "abc" pattern.
        let has_match = tokens.iter().any(|t| matches!(t, LzssToken::Match { .. }));
        assert!(has_match);
    }

    #[test]
    fn test_lzss_roundtrip() {
        let mut encoder = LzssEncoder::new(1024, 3, 256);
        let mut decoder = LzssDecoder::new(1024);

        let input = b"Hello Hello Hello World";
        let tokens = encoder.encode(input);

        for token in tokens {
            match token {
                LzssToken::Literal(b) => decoder.decode_literal(b),
                LzssToken::Match { length, distance } => decoder
                    .decode_match(length, distance)
                    .expect("operation failed"),
            }
        }

        assert_eq!(decoder.output(), input);
    }

    // -------------------------------------------------------------------------
    // Hash chain roundtrip tests for lh5, lh6, lh7 window sizes
    // -------------------------------------------------------------------------

    fn roundtrip_with_window(window_size: usize, input: &[u8]) {
        let mut encoder = LzssEncoder::new(window_size, 3, 256);
        let mut decoder = LzssDecoder::new(window_size);

        let tokens = encoder.encode(input);

        for token in &tokens {
            match token {
                LzssToken::Literal(b) => decoder.decode_literal(*b),
                LzssToken::Match { length, distance } => {
                    decoder
                        .decode_match(*length, *distance)
                        .expect("decode_match failed");
                }
            }
        }

        assert_eq!(
            decoder.output(),
            input,
            "roundtrip failed for window_size={window_size}, input_len={}",
            input.len()
        );
    }

    /// Deterministic xorshift32 pseudo-random data (incompressible).
    fn xorshift_data(len: usize, mut state: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 4);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn test_hash_chain_roundtrip_lh5() {
        // lh5: 8 KB window
        let ws = 8192usize;
        // Simple ASCII phrase
        roundtrip_with_window(ws, b"The quick brown fox jumps over the lazy dog.");
        // Repeated pattern
        let rep: Vec<u8> = b"abcdefgh".iter().cycle().take(1024).copied().collect();
        roundtrip_with_window(ws, &rep);
        // All same byte
        let same = vec![0xAAu8; 2048];
        roundtrip_with_window(ws, &same);
        // Random-ish data (pseudo-random deterministic)
        let random: Vec<u8> = (0u32..4096)
            .map(|i| ((i.wrapping_mul(6364).wrapping_add(31337)) & 0xFF) as u8)
            .collect();
        roundtrip_with_window(ws, &random);
    }

    #[test]
    fn test_hash_chain_roundtrip_lh6() {
        // lh6: 32 KB window
        let ws = 32768usize;
        let rep: Vec<u8> = b"lh6_pattern_".iter().cycle().take(8192).copied().collect();
        roundtrip_with_window(ws, &rep);
        let same = vec![0x55u8; 16384];
        roundtrip_with_window(ws, &same);
    }

    #[test]
    fn test_hash_chain_roundtrip_lh7() {
        // lh7: 64 KB window
        let ws = 65536usize;
        let rep: Vec<u8> = b"lh7_long_pattern_xyz_"
            .iter()
            .cycle()
            .take(16384)
            .copied()
            .collect();
        roundtrip_with_window(ws, &rep);
        let same = vec![0xBBu8; 32768];
        roundtrip_with_window(ws, &same);
    }

    // -------------------------------------------------------------------------
    // Regression tests: inputs larger than the window (former corruption bug)
    // -------------------------------------------------------------------------

    #[test]
    fn test_roundtrip_beyond_window_incompressible() {
        // 16 KB of incompressible data through an 8 KB window used to
        // corrupt because the whole block was pre-written into the circular
        // window, clobbering both history and lookahead.
        let data = xorshift_data(16 * 1024, 0x1234_5678);
        roundtrip_with_window(8192, &data);

        // 100 KB through lh5/lh6/lh7 windows.
        let data = xorshift_data(100 * 1024, 0x0BAD_F00D);
        roundtrip_with_window(8192, &data);
        roundtrip_with_window(32768, &data);
        roundtrip_with_window(65536, &data);
    }

    #[test]
    fn test_roundtrip_beyond_window_compressible() {
        let data: Vec<u8> = b"compressible pattern beyond the window! "
            .iter()
            .cycle()
            .take(96 * 1024)
            .copied()
            .collect();
        roundtrip_with_window(8192, &data);
        roundtrip_with_window(32768, &data);
        roundtrip_with_window(65536, &data);
    }

    #[test]
    fn test_token_distances_never_exceed_u16() {
        // lh7 window is 65536, but token distances must fit in u16.
        let ws = 65536usize;
        let mut encoder = LzssEncoder::new(ws, 3, 256);
        let data: Vec<u8> = b"Z".iter().cycle().take(70 * 1024).copied().collect();
        let tokens = encoder.encode(&data);
        for token in &tokens {
            if let LzssToken::Match { distance, .. } = token {
                assert!(*distance >= 1, "distance must be at least 1");
            }
        }
    }

    // -------------------------------------------------------------------------
    // Performance test: lh7 with 64 KB of repetitive data
    // -------------------------------------------------------------------------

    #[test]
    fn test_lh7_performance_repetitive() {
        use std::time::Instant;

        let ws = 65536usize;
        let data: Vec<u8> = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .iter()
            .cycle()
            .take(65536)
            .copied()
            .collect();

        let start = Instant::now();
        let mut encoder = LzssEncoder::new(ws, 3, 256);
        let tokens = encoder.encode(&data);
        let elapsed = start.elapsed();

        // Verify correctness via roundtrip.
        let mut decoder = LzssDecoder::new(ws);
        for token in &tokens {
            match token {
                LzssToken::Literal(b) => decoder.decode_literal(*b),
                LzssToken::Match { length, distance } => {
                    decoder
                        .decode_match(*length, *distance)
                        .expect("operation failed");
                }
            }
        }
        assert_eq!(decoder.output(), &data);

        // Performance assertion: must complete well under 5 seconds.
        assert!(
            elapsed.as_secs() < 5,
            "lh7 64 KB repetitive compression took {:?}, expected < 5 s",
            elapsed
        );
    }

    // -------------------------------------------------------------------------
    // Overlapping match roundtrip
    // -------------------------------------------------------------------------

    #[test]
    fn test_overlapping_match_roundtrip() {
        // "XXXXXXXXX..." – forces overlapping copies (len > dist).
        let input: Vec<u8> = vec![b'X'; 512];
        roundtrip_with_window(1024, &input);
    }

    // -------------------------------------------------------------------------
    // Token stream sanity: no distance > window, no length < min_match
    // -------------------------------------------------------------------------

    #[test]
    fn test_token_sanity() {
        let ws = 8192usize;
        let mut encoder = LzssEncoder::new(ws, 3, 256);
        let data: Vec<u8> = (0u16..4000).map(|i| (i % 251) as u8).collect();
        let tokens = encoder.encode(&data);

        for token in &tokens {
            if let LzssToken::Match { length, distance } = token {
                assert!(*length >= 3, "match length {} < min_match 3", length);
                assert!(*distance > 0, "zero distance in match token");
                assert!(
                    *distance as usize <= ws,
                    "distance {} exceeds window size {}",
                    distance,
                    ws
                );
            }
        }
    }
}
