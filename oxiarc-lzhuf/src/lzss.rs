//! LZSS algorithm for LZH compression.
//!
//! LZSS (Lempel-Ziv-Storer-Szymanski) is a derivative of LZ77 that uses
//! a flag bit to distinguish between literals and matches.
//!
//! The encoder uses hash chain traversal for O(1) amortized match finding,
//! replacing the previous O(n) linear scan. A 3-byte hash table maps byte
//! trigrams to chains of positions in the circular window.

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
}

/// Sentinel value indicating an empty hash chain slot.
const EMPTY: u32 = u32::MAX;

/// Maximum hash chain depth to traverse per match query.
/// Balances compression quality vs. speed.
const MAX_CHAIN_LEN: usize = 128;

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
/// The hash table maps a 3-byte trigram hash → most-recent absolute position
/// that had that trigram. The hash chain maps `abs_pos % window_size` →
/// previous absolute position with the same trigram hash (or EMPTY).
#[derive(Debug)]
pub struct LzssEncoder {
    /// Circular sliding window.
    window: Vec<u8>,
    /// Absolute position of the next byte to be written into the window.
    abs_write_pos: u64,
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
            window_size,
            window_mask,
            min_match,
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

    /// Reset the encoder to initial state.
    pub fn reset(&mut self) {
        self.abs_write_pos = 0;
        self.window.fill(0);
        self.hash_table.fill(EMPTY);
        self.hash_chain.fill(EMPTY);
    }

    // -------------------------------------------------------------------------
    // Hash helpers
    // -------------------------------------------------------------------------

    /// Compute a 3-byte hash, masked to `[0, hash_mask]`.
    #[inline]
    fn hash3(b0: u8, b1: u8, b2: u8, mask: usize) -> usize {
        let h = (b0 as usize).wrapping_mul(506_832_829)
            ^ ((b1 as usize).wrapping_mul(2_654_435_761) << 8)
            ^ ((b2 as usize).wrapping_mul(374_761_393) << 16);
        (h ^ (h >> 13)) & mask
    }

    /// Insert the trigram at absolute position `abs_pos` into the hash chain.
    ///
    /// Reads `window[abs_pos % ws]`, `window[(abs_pos+1) % ws]`, and
    /// `window[(abs_pos+2) % ws]`. Silently returns if fewer than 3 bytes
    /// have been written yet.
    fn update_hash(&mut self, abs_pos: u64) {
        if abs_pos + 3 > self.abs_write_pos {
            return;
        }
        let ws = self.window_size;
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
        self.hash_chain[p0 % ws] = prev;
        self.hash_table[h] = abs_pos_u32;
    }

    // -------------------------------------------------------------------------
    // Match finding
    // -------------------------------------------------------------------------

    /// Find the longest match for the bytes starting at `lookahead`.
    ///
    /// `cur_abs` is the absolute position of `lookahead[0]` in the stream.
    ///
    /// Returns `(best_length, best_distance)` where both are 0 if no match of
    /// at least `min_match` bytes was found.
    fn find_match(&self, cur_abs: u64, lookahead: &[u8]) -> (usize, usize) {
        if lookahead.len() < self.min_match {
            return (0, 0);
        }

        let max_len = lookahead.len().min(self.max_match);
        let ws = self.window_size;
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
            if dist == 0 || dist > ws {
                // Position is outside the valid window; stop traversal.
                break;
            }

            // Quick-reject: the byte at offset `best_len` in the candidate
            // must equal `lookahead[best_len]` before we do a full compare.
            let quick_idx = (match_abs as usize + best_len) & wm;
            if self.window[quick_idx] != lookahead[best_len] {
                // Advance chain.
                let chain_slot = (match_abs as usize) & wm;
                match_abs = self.hash_chain[chain_slot];
                continue;
            }

            // Full match comparison – support overlapping copies (dist < match_len).
            let mut len = 0usize;
            while len < max_len {
                let src_byte = if dist <= len {
                    // Overlapping: the source wraps into the already-copied
                    // region, so the pattern repeats with period `dist`.
                    lookahead[len % dist]
                } else {
                    let src_idx = (match_abs as usize + len) & wm;
                    self.window[src_idx]
                };
                if src_byte != lookahead[len] {
                    break;
                }
                len += 1;
            }

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

    // -------------------------------------------------------------------------
    // Window management
    // -------------------------------------------------------------------------

    /// Write a single byte into the circular window at `abs_pos` and advance
    /// the absolute write cursor.
    #[inline]
    fn push_byte(&mut self, byte: u8) {
        let slot = (self.abs_write_pos as usize) & self.window_mask;
        self.window[slot] = byte;
        self.abs_write_pos += 1;
    }

    // -------------------------------------------------------------------------
    // Encoding
    // -------------------------------------------------------------------------

    /// Encode `data` and return a list of LZSS tokens.
    pub fn encode(&mut self, data: &[u8]) -> Vec<LzssToken> {
        let mut tokens = Vec::with_capacity(data.len());

        // Stage 1: write all data bytes into the circular window so that
        // look-ahead byte reads are always valid during find_match.
        let data_start_abs = self.abs_write_pos;
        for &byte in data {
            self.push_byte(byte);
        }

        // Stage 2: walk through the data, maintaining the hash chain for the
        // prefix already "consumed" by the encoder, and finding matches for
        // the current lookahead.
        let data_len = data.len();
        let mut pos = 0usize; // position within `data`

        while pos < data_len {
            let cur_abs = data_start_abs + pos as u64;

            // Build a lookahead slice from the circular window.
            // Because the window is circular and data_len can exceed window_size,
            // we cap the lookahead at max_match bytes (and actual remaining).
            let lookahead_len = (data_len - pos).min(self.max_match);
            let mut lookahead_buf = Vec::with_capacity(lookahead_len);
            for i in 0..lookahead_len {
                let slot = (cur_abs as usize + i) & self.window_mask;
                lookahead_buf.push(self.window[slot]);
            }
            let lookahead = &lookahead_buf;

            // Search BEFORE inserting cur_abs so that hash_table[h] points to
            // a strictly earlier position (no self-match with dist == 0).
            let (len, dist) = self.find_match(cur_abs, lookahead);

            // Now insert cur_abs into the hash chain (it becomes the new head).
            self.update_hash(cur_abs);

            if len >= self.min_match && self.lazy_match && pos + 1 < data_len {
                // Lazy match: check if position pos+1 gives a strictly longer match.
                let next_abs = cur_abs + 1;

                let next_lookahead_len = (data_len - pos - 1).min(self.max_match);
                let mut next_lookahead_buf = Vec::with_capacity(next_lookahead_len);
                for i in 0..next_lookahead_len {
                    let slot = (next_abs as usize + i) & self.window_mask;
                    next_lookahead_buf.push(self.window[slot]);
                }
                let next_lookahead = &next_lookahead_buf;

                // Search at next_abs; at this point hash contains positions ≤ cur_abs.
                let (next_len, next_dist) = self.find_match(next_abs, next_lookahead);

                if next_len > len {
                    // Emit literal at pos, then use the longer match at pos+1.
                    let lit_slot = cur_abs as usize & self.window_mask;
                    tokens.push(LzssToken::Literal(self.window[lit_slot]));
                    pos += 1;

                    // Insert next_abs into hash and emit the better match.
                    self.update_hash(next_abs);

                    if next_len >= self.min_match && next_dist > 0 {
                        tokens.push(LzssToken::Match {
                            length: next_len as u16,
                            distance: next_dist as u16,
                        });
                        // Update hash for each position skipped during the match
                        // (positions next_abs+1 .. next_abs+next_len-1).
                        for skip in 1..next_len {
                            self.update_hash(next_abs + skip as u64);
                        }
                        pos += next_len;
                    } else {
                        // The next match was not good after all – emit literal.
                        let lit_slot2 = next_abs as usize & self.window_mask;
                        tokens.push(LzssToken::Literal(self.window[lit_slot2]));
                        pos += 1;
                    }
                    continue;
                }
                // Original match was at least as good; fall through.
            }

            if len >= self.min_match && dist > 0 {
                tokens.push(LzssToken::Match {
                    length: len as u16,
                    distance: dist as u16,
                });
                // Update hash for each position within the match span
                // (positions cur_abs+1 .. cur_abs+len-1; cur_abs was handled above).
                for skip in 1..len {
                    self.update_hash(cur_abs + skip as u64);
                }
                pos += len;
            } else {
                // Emit literal.
                let lit_slot = cur_abs as usize & self.window_mask;
                tokens.push(LzssToken::Literal(self.window[lit_slot]));
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
        decoder.decode_match(2, 2).unwrap();

        assert_eq!(decoder.output(), b"ABAB");
    }

    #[test]
    fn test_lzss_decoder_overlapping_match() {
        let mut decoder = LzssDecoder::new(1024);

        // Write "A"
        decoder.decode_literal(b'A');

        // Match: copy 5 bytes from distance 1 -> "AAAAA"
        decoder.decode_match(5, 1).unwrap();

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
                LzssToken::Match { length, distance } => {
                    decoder.decode_match(length, distance).unwrap()
                }
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
                    decoder.decode_match(*length, *distance).unwrap();
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
        // "AAAAAAAAA..." – forces overlapping copies (dist=1, len > dist).
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
