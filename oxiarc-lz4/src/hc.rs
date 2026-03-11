//! LZ4-HC (High Compression) mode.
//!
//! LZ4-HC trades compression speed for better compression ratios.
//! It uses a more aggressive match finding strategy with:
//! - Larger hash table
//! - Chain table for multiple matches at same hash position
//! - Better match selection (longest match rather than first)
//! - Compression levels 1-12

use oxiarc_core::error::Result;

/// LZ4-HC compression level (1-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HcLevel(u8);

impl HcLevel {
    /// Minimum HC compression level.
    pub const MIN: Self = Self(1);
    /// Default HC compression level.
    pub const DEFAULT: Self = Self(9);
    /// Maximum HC compression level.
    pub const MAX: Self = Self(12);

    /// Create a new compression level.
    ///
    /// Returns None if level is outside 1-12 range.
    pub fn new(level: u8) -> Option<Self> {
        if (1..=12).contains(&level) {
            Some(Self(level))
        } else {
            None
        }
    }

    /// Get the level value.
    pub fn level(self) -> u8 {
        self.0
    }

    /// Get maximum number of match attempts for this level.
    fn max_attempts(self) -> usize {
        // Higher levels try more matches
        match self.0 {
            1..=3 => 64,
            4..=6 => 256,
            7..=9 => 1024,
            10..=11 => 4096,
            12 => 16384,
            _ => 256,
        }
    }
}

impl Default for HcLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Minimum match length for LZ4.
const MIN_MATCH: usize = 4;

/// Maximum match length for LZ4.
const MAX_MATCH: usize = 65535 + MIN_MATCH;

/// Maximum match offset (16-bit).
const MAX_OFFSET: usize = 65535;

/// Hash table size (must be power of 2).
const HASH_SIZE: usize = 1 << 16; // 64K entries

/// Chain table size.
const CHAIN_SIZE: usize = 1 << 16; // 64K entries

/// Maximum search depth for optimal match.
const OPTIMAL_SEARCH_DEPTH: usize = 64;

/// LZ4-HC encoder.
pub struct HcEncoder {
    level: HcLevel,
    hash_table: Vec<u32>,
    chain_table: Vec<u32>,
}

impl HcEncoder {
    /// Create a new HC encoder with default compression level.
    pub fn new() -> Self {
        Self::with_level(HcLevel::default())
    }

    /// Create a new HC encoder with specific compression level.
    pub fn with_level(level: HcLevel) -> Self {
        Self {
            level,
            hash_table: vec![0; HASH_SIZE],
            chain_table: vec![0; CHAIN_SIZE],
        }
    }

    /// Hash 4 bytes for position lookup.
    #[inline]
    fn hash4(data: &[u8], pos: usize) -> usize {
        if pos + 4 > data.len() {
            return 0;
        }
        let val = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        // FNV-like hash with better distribution
        ((val.wrapping_mul(2654435761)) >> 16) as usize & (HASH_SIZE - 1)
    }

    /// Find the best match at current position.
    fn find_best_match(&self, input: &[u8], pos: usize) -> Option<(usize, usize)> {
        if pos + MIN_MATCH > input.len() {
            return None;
        }

        let h = Self::hash4(input, pos);
        let mut match_pos = self.hash_table[h] as usize;

        let mut best_len = MIN_MATCH - 1;
        let mut best_offset = 0;

        let max_attempts = self.level.max_attempts();
        let mut attempts = 0;

        while match_pos > 0 && attempts < max_attempts {
            let offset = pos - match_pos;
            if offset > MAX_OFFSET || offset == 0 {
                break;
            }

            // Quick reject: check first and last bytes of best match
            if best_len >= MIN_MATCH && input.get(match_pos + best_len) != input.get(pos + best_len)
            {
                match_pos = self.chain_table[match_pos & (CHAIN_SIZE - 1)] as usize;
                attempts += 1;
                continue;
            }

            // Compare from the beginning
            let max_len = (input.len() - pos).min(MAX_MATCH);
            let mut len = 0;

            while len < max_len && input.get(match_pos + len) == input.get(pos + len) {
                len += 1;
            }

            if len > best_len {
                best_len = len;
                best_offset = offset;

                // Early exit for very long matches
                if len >= 128 {
                    break;
                }
            }

            // Follow chain
            match_pos = self.chain_table[match_pos & (CHAIN_SIZE - 1)] as usize;
            attempts += 1;
        }

        if best_len >= MIN_MATCH && best_offset > 0 {
            Some((best_offset, best_len))
        } else {
            None
        }
    }

    /// Insert position into hash table and chain.
    #[inline]
    fn insert_position(&mut self, pos: usize, input: &[u8]) {
        if pos + 4 > input.len() {
            return;
        }

        let h = Self::hash4(input, pos);
        let prev = self.hash_table[h];
        self.chain_table[pos & (CHAIN_SIZE - 1)] = prev;
        self.hash_table[h] = pos as u32;
    }

    /// Compress data using LZ4-HC.
    pub fn compress(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        // Clear tables
        self.hash_table.fill(0);
        self.chain_table.fill(0);

        let mut output = Vec::with_capacity(input.len());
        let mut pos = 0;
        let mut anchor = 0;

        // Insert initial positions
        if input.len() >= 4 {
            self.insert_position(0, input);
        }

        let end = input.len().saturating_sub(5);

        while pos < end {
            // Try to find a match
            if let Some((offset, match_len)) = self.find_best_match(input, pos) {
                // Emit literals before match
                let literal_len = pos - anchor;
                emit_sequence(&mut output, input, anchor, literal_len, offset, match_len);

                // Insert positions for match area
                for i in 1..match_len {
                    if pos + i < input.len() {
                        self.insert_position(pos + i, input);
                    }
                }

                pos += match_len;
                anchor = pos;

                if pos < input.len() {
                    self.insert_position(pos, input);
                }
            } else {
                self.insert_position(pos, input);
                pos += 1;
            }
        }

        // Emit remaining literals
        let remaining = input.len() - anchor;
        if remaining > 0 {
            emit_last_literals(&mut output, input, anchor, remaining);
        }

        Ok(output)
    }

    /// Compress with optimal parsing (level 12).
    /// This uses a more expensive algorithm that considers multiple match choices.
    pub fn compress_optimal(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        // For level 12, use backward-looking optimal parsing
        if self.level.level() >= 12 {
            return self.compress_optimal_internal(input);
        }

        // Otherwise use regular HC compression
        self.compress(input)
    }

    /// Compress data using LZ4-HC with dictionary support.
    ///
    /// Uses the virtual-buffer strategy: the dictionary is logically prepended
    /// to the input, allowing matches to reference bytes in the dictionary.
    /// This provides genuine HC-quality compression (chain-based longest-match
    /// search) against dictionary content.
    ///
    /// If the dictionary is empty, delegates to the regular [`compress`][Self::compress]
    /// method.
    pub fn compress_with_dict(
        &mut self,
        input: &[u8],
        dict: &crate::dict::Lz4Dict,
    ) -> Result<Vec<u8>> {
        if dict.is_empty() {
            return self.compress(input);
        }
        let mut encoder = HcDictEncoder::new(input, dict.data(), self.level);
        encoder.compress()
    }

    fn compress_optimal_internal(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // Clear tables
        self.hash_table.fill(0);
        self.chain_table.fill(0);

        // Build matches for all positions
        let mut matches: Vec<Vec<(usize, usize)>> = vec![Vec::new(); input.len()];
        let search_limit = input.len().saturating_sub(MIN_MATCH);

        for (pos, match_slot) in matches.iter_mut().enumerate().take(search_limit) {
            self.insert_position(pos, input);

            // Find all good matches at this position
            let mut found_matches = Vec::new();
            let h = Self::hash4(input, pos);
            let mut match_pos = self.hash_table[h] as usize;
            let mut attempts = 0;

            while match_pos > 0 && match_pos < pos && attempts < OPTIMAL_SEARCH_DEPTH {
                let offset = pos - match_pos;
                if offset > MAX_OFFSET {
                    break;
                }

                let max_len = (input.len() - pos).min(MAX_MATCH);
                let mut len = 0;

                while len < max_len && input.get(match_pos + len) == input.get(pos + len) {
                    len += 1;
                }

                if len >= MIN_MATCH {
                    found_matches.push((offset, len));
                }

                match_pos = self.chain_table[match_pos & (CHAIN_SIZE - 1)] as usize;
                attempts += 1;
            }

            *match_slot = found_matches;
        }

        // Dynamic programming to find optimal sequence
        // cost[i] = minimum bits to encode first i bytes
        let n = input.len();
        let mut cost = vec![usize::MAX / 2; n + 1];
        let mut prev = vec![(0usize, 0usize, 0usize); n + 1]; // (from, offset, match_len)

        cost[0] = 0;

        for i in 0..n {
            if cost[i] >= usize::MAX / 2 {
                continue;
            }

            // Option 1: Emit literal
            let lit_cost = cost[i] + 8; // 8 bits per literal byte (rough estimate)
            if lit_cost < cost[i + 1] {
                cost[i + 1] = lit_cost;
                prev[i + 1] = (i, 0, 0);
            }

            // Option 2: Emit match
            for &(offset, match_len) in &matches[i] {
                let end_pos = i + match_len;
                if end_pos > n {
                    continue;
                }

                // Cost of encoding a match (rough estimate)
                let match_cost = cost[i] + 24 + if match_len > 18 { 8 } else { 0 };

                if match_cost < cost[end_pos] {
                    cost[end_pos] = match_cost;
                    prev[end_pos] = (i, offset, match_len);
                }
            }
        }

        // Reconstruct the optimal sequence
        let mut sequence = Vec::new();
        let mut pos = n;

        while pos > 0 {
            let (from, offset, match_len) = prev[pos];
            if match_len > 0 {
                sequence.push((from, offset, match_len));
            }
            pos = from;
        }

        sequence.reverse();

        // Encode the sequence
        let mut output = Vec::with_capacity(input.len());
        let mut anchor = 0;

        for (match_start, offset, match_len) in sequence {
            let literal_len = match_start - anchor;
            emit_sequence(&mut output, input, anchor, literal_len, offset, match_len);
            anchor = match_start + match_len;
        }

        // Emit remaining literals
        let remaining = n - anchor;
        if remaining > 0 {
            emit_last_literals(&mut output, input, anchor, remaining);
        }

        Ok(output)
    }
}

impl Default for HcEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HC dictionary support — virtual-buffer strategy
// ---------------------------------------------------------------------------

/// HC encoder with dictionary support.
///
/// The virtual buffer model: dictionary occupies virtual positions `0..dict_len`,
/// input occupies virtual positions `dict_len..dict_len+input_len`.
/// Hash and chain tables are indexed by `(virtual_pos + 1)` — we add 1 so that
/// virtual position 0 (first byte of dictionary) is distinguishable from the
/// "empty slot" sentinel value of 0 stored in the tables.
struct HcDictEncoder<'a> {
    level: HcLevel,
    hash_table: Vec<u32>,
    chain_table: Vec<u32>,
    input: &'a [u8],
    dict: &'a [u8],
    dict_len: usize,
}

impl<'a> HcDictEncoder<'a> {
    /// Create a new HC dict encoder and pre-populate tables with dictionary positions.
    fn new(input: &'a [u8], dict: &'a [u8], level: HcLevel) -> Self {
        let dict_len = dict.len();
        let mut hash_table = vec![0u32; HASH_SIZE];
        let mut chain_table = vec![0u32; CHAIN_SIZE];

        // Pre-populate hash/chain tables with dictionary positions.
        // We store `(virtual_pos + 1)` to avoid collision with the sentinel 0.
        if dict_len >= MIN_MATCH {
            for virt in 0..=(dict_len.saturating_sub(MIN_MATCH)) {
                let h = Self::hash4_slice(dict, virt);
                let prev = hash_table[h];
                chain_table[(virt + 1) & (CHAIN_SIZE - 1)] = prev;
                hash_table[h] = (virt + 1) as u32;
            }
        }

        Self {
            level,
            hash_table,
            chain_table,
            input,
            dict,
            dict_len,
        }
    }

    /// Hash 4 bytes starting at `pos` within `data`.
    #[inline]
    fn hash4_slice(data: &[u8], pos: usize) -> usize {
        if pos + 4 > data.len() {
            return 0;
        }
        let val = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        ((val.wrapping_mul(2654435761)) >> 16) as usize & (HASH_SIZE - 1)
    }

    /// Hash 4 bytes of the input at `input_pos`.
    #[inline]
    fn hash4_input(&self, input_pos: usize) -> usize {
        Self::hash4_slice(self.input, input_pos)
    }

    /// Get a byte from the virtual buffer (dict then input).
    #[inline]
    fn virt_byte(&self, virt: usize) -> Option<u8> {
        if virt < self.dict_len {
            self.dict.get(virt).copied()
        } else {
            self.input.get(virt - self.dict_len).copied()
        }
    }

    /// Insert an input position into hash/chain tables.
    ///
    /// `input_pos` is the position in `self.input`; its virtual position is
    /// `input_pos + self.dict_len`.
    #[inline]
    fn insert_input_pos(&mut self, input_pos: usize) {
        if input_pos + 4 > self.input.len() {
            return;
        }
        let virt = input_pos + self.dict_len;
        let h = self.hash4_input(input_pos);
        let prev = self.hash_table[h];
        self.chain_table[(virt + 1) & (CHAIN_SIZE - 1)] = prev;
        self.hash_table[h] = (virt + 1) as u32;
    }

    /// Find the best match at `input_pos`, considering both dictionary and input.
    ///
    /// Returns `(best_length, best_offset)` or `(0, 0)` if no match of
    /// length ≥ `MIN_MATCH` is found.
    fn find_best_match_with_dict(&self, input_pos: usize) -> (usize, usize) {
        if input_pos + MIN_MATCH > self.input.len() {
            return (0, 0);
        }

        let h = self.hash4_input(input_pos);
        // stored value is (virt + 1); 0 means empty
        let mut slot = self.hash_table[h] as usize;

        let virt_pos = input_pos + self.dict_len;
        let mut best_len = MIN_MATCH - 1;
        let mut best_offset = 0usize;
        let max_attempts = self.level.max_attempts();
        let mut attempts = 0;

        while slot > 0 && attempts < max_attempts {
            // Recover the actual virtual position
            let match_virt = slot - 1;
            let offset = virt_pos.saturating_sub(match_virt);

            if offset > MAX_OFFSET || offset == 0 {
                break;
            }

            // Quick reject using best_len byte
            let skip = best_len >= MIN_MATCH && {
                let a = self.virt_byte(match_virt + best_len);
                let b = self.input.get(input_pos + best_len).copied();
                a != b
            };

            if !skip {
                // Measure match length in the virtual buffer vs. input
                let max_len = (self.input.len() - input_pos).min(MAX_MATCH);
                let mut len = 0;
                while len < max_len {
                    match (
                        self.virt_byte(match_virt + len),
                        self.input.get(input_pos + len).copied(),
                    ) {
                        (Some(a), Some(b)) if a == b => len += 1,
                        _ => break,
                    }
                }

                if len > best_len {
                    best_len = len;
                    best_offset = offset;
                    if len >= 128 {
                        break;
                    }
                }
            }

            // Follow chain
            slot = self.chain_table[(match_virt + 1) & (CHAIN_SIZE - 1)] as usize;
            attempts += 1;
        }

        if best_len >= MIN_MATCH && best_offset > 0 {
            (best_len, best_offset)
        } else {
            (0, 0)
        }
    }

    /// Compress input using HC with dictionary via the virtual-buffer strategy.
    fn compress(&mut self) -> crate::Result<Vec<u8>> {
        let input = self.input;

        if input.is_empty() {
            return Ok(Vec::new());
        }

        let mut output = Vec::with_capacity(input.len());
        let mut pos = 0;
        let mut anchor = 0;

        // Insert initial position
        if input.len() >= 4 {
            self.insert_input_pos(0);
        }

        let end = input.len().saturating_sub(5);

        while pos < end {
            let (match_len, offset) = self.find_best_match_with_dict(pos);

            if match_len >= MIN_MATCH && offset > 0 {
                // Emit literals before this match
                let literal_len = pos - anchor;
                emit_sequence(&mut output, input, anchor, literal_len, offset, match_len);

                // Insert positions inside the matched region
                for i in 1..match_len {
                    if pos + i < input.len() {
                        self.insert_input_pos(pos + i);
                    }
                }

                pos += match_len;
                anchor = pos;

                if pos < input.len() {
                    self.insert_input_pos(pos);
                }
            } else {
                self.insert_input_pos(pos);
                pos += 1;
            }
        }

        // Emit remaining literals
        let remaining = input.len() - anchor;
        if remaining > 0 {
            emit_last_literals(&mut output, input, anchor, remaining);
        }

        Ok(output)
    }
}

/// Emit a sequence (literals + match) to output.
fn emit_sequence(
    output: &mut Vec<u8>,
    input: &[u8],
    literal_start: usize,
    literal_len: usize,
    offset: usize,
    match_len: usize,
) {
    // Token: upper 4 bits = literal length, lower 4 bits = match length - 4
    let lit_token = if literal_len >= 15 { 15 } else { literal_len };
    let match_token = {
        let ml = match_len.saturating_sub(MIN_MATCH);
        if ml >= 15 { 15 } else { ml }
    };

    let token = ((lit_token << 4) | match_token) as u8;
    output.push(token);

    // Extended literal length
    if literal_len >= 15 {
        let mut remaining = literal_len - 15;
        while remaining >= 255 {
            output.push(255);
            remaining -= 255;
        }
        output.push(remaining as u8);
    }

    // Literals
    output.extend_from_slice(&input[literal_start..literal_start + literal_len]);

    // Match offset: 2 bytes little-endian
    output.push(offset as u8);
    output.push((offset >> 8) as u8);

    // Extended match length
    if match_len >= MIN_MATCH + 15 {
        let mut remaining = match_len - MIN_MATCH - 15;
        while remaining >= 255 {
            output.push(255);
            remaining -= 255;
        }
        output.push(remaining as u8);
    }
}

/// Emit last literals (no match at the end).
fn emit_last_literals(
    output: &mut Vec<u8>,
    input: &[u8],
    literal_start: usize,
    literal_len: usize,
) {
    // Token with match length = 0
    let lit_token = if literal_len >= 15 { 15 } else { literal_len };
    let token = (lit_token << 4) as u8;
    output.push(token);

    // Extended literal length
    if literal_len >= 15 {
        let mut remaining = literal_len - 15;
        while remaining >= 255 {
            output.push(255);
            remaining -= 255;
        }
        output.push(remaining as u8);
    }

    // Literals
    output.extend_from_slice(&input[literal_start..literal_start + literal_len]);
}

/// Compress data using LZ4-HC with default settings.
pub fn compress_hc(input: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = HcEncoder::new();
    encoder.compress(input)
}

/// Compress data using LZ4-HC with specific compression level.
pub fn compress_hc_level(input: &[u8], level: HcLevel) -> Result<Vec<u8>> {
    let mut encoder = HcEncoder::with_level(level);
    if level.level() >= 12 {
        encoder.compress_optimal(input)
    } else {
        encoder.compress(input)
    }
}

/// Compress data using LZ4-HC with dictionary and a specific compression level.
///
/// The dictionary is used via the virtual-buffer strategy, allowing HC's
/// chain-based longest-match search to reference dictionary content.
///
/// If `dict` is empty, this behaves identically to [`compress_hc_level`].
///
/// # Arguments
///
/// * `input` - Data to compress.
/// * `dict`  - Pre-trained LZ4 dictionary.
/// * `level` - HC compression level (1–12).
///
/// # Returns
///
/// Compressed data in LZ4 block format (compatible with the standard decoder
/// when the same dictionary is supplied at decompression time).
pub fn compress_hc_with_dict(
    input: &[u8],
    dict: &crate::dict::Lz4Dict,
    level: HcLevel,
) -> Result<Vec<u8>> {
    let mut encoder = HcEncoder::with_level(level);
    encoder.compress_with_dict(input, dict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress_block;

    #[test]
    fn test_hc_level() {
        assert!(HcLevel::new(0).is_none());
        assert!(HcLevel::new(1).is_some());
        assert!(HcLevel::new(12).is_some());
        assert!(HcLevel::new(13).is_none());
    }

    #[test]
    fn test_hc_roundtrip_simple() {
        let data = b"Hello, World! Hello, World!";
        let compressed = compress_hc(data).expect("compress failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_hc_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let compressed = compress_hc(data).expect("compress failed");
        // HC should compress well
        assert!(
            compressed.len() < data.len(),
            "compressed: {}, original: {}",
            compressed.len(),
            data.len()
        );
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_hc_roundtrip_pattern() {
        let data = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        let compressed = compress_hc(data).expect("compress failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_hc_empty() {
        let data: &[u8] = b"";
        let compressed = compress_hc(data).expect("compress failed");
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_hc_levels() {
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(100);

        for level in [1, 6, 9, 12] {
            let hc_level = HcLevel::new(level).expect("valid level");
            let compressed = compress_hc_level(&data, hc_level)
                .unwrap_or_else(|_| panic!("level {} failed", level));
            let decompressed =
                decompress_block(&compressed, data.len()).expect("decompress failed");
            assert_eq!(decompressed, data);
        }
    }

    #[test]
    fn test_hc_vs_fast() {
        // HC should achieve better compression than fast mode
        let data = b"The quick brown fox jumps over the lazy dog repeatedly. ".repeat(50);

        let fast_compressed = crate::compress_block(&data).expect("fast compress failed");
        let hc_compressed = compress_hc(&data).expect("hc compress failed");

        // Both should decompress correctly
        let fast_decompressed =
            decompress_block(&fast_compressed, data.len()).expect("decompress failed");
        let hc_decompressed =
            decompress_block(&hc_compressed, data.len()).expect("decompress failed");

        assert_eq!(fast_decompressed, data);
        assert_eq!(hc_decompressed, data);

        // HC should be smaller or equal
        assert!(
            hc_compressed.len() <= fast_compressed.len(),
            "HC: {}, Fast: {}",
            hc_compressed.len(),
            fast_compressed.len()
        );
    }

    #[test]
    fn test_hc_large_data() {
        // Test with larger data
        let data: Vec<u8> = (0..10000).map(|i| ((i * 17 + 13) % 256) as u8).collect();

        let compressed = compress_hc(&data).expect("compress failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_hc_optimal_level_12() {
        let data = b"abcdefghijklmnopqrstuvwxyz".repeat(20);

        let level = HcLevel::new(12).expect("valid level");
        let compressed = compress_hc_level(&data, level).expect("compress failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    // -----------------------------------------------------------------------
    // HC dictionary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_hc_dict_roundtrip() {
        let dict_bytes = b"The quick brown fox jumps over the lazy dog.";
        let dict = crate::dict::Lz4Dict::new(dict_bytes);

        let input = b"The quick brown fox jumps over the lazy dog. And again!";
        let level = HcLevel::default();
        let compressed =
            compress_hc_with_dict(input, &dict, level).expect("hc dict compress failed");
        let decompressed = crate::dict::decompress_with_dict(&compressed, input.len() * 2, &dict)
            .expect("hc dict decompress failed");
        assert_eq!(decompressed.as_slice(), input.as_ref());
    }

    #[test]
    fn test_hc_dict_empty_dict_fallback() {
        // Empty dictionary must produce output decompressable by the plain decoder.
        let dict = crate::dict::Lz4Dict::empty();
        let input = b"Hello world from HC with empty dict!";
        let level = HcLevel::default();
        let compressed =
            compress_hc_with_dict(input, &dict, level).expect("hc dict (empty) compress failed");
        // Empty dict delegates to regular HC — decompress with plain block decoder.
        let decompressed =
            decompress_block(&compressed, input.len() * 2).expect("decompress failed");
        assert_eq!(decompressed.as_slice(), input.as_ref());
    }

    #[test]
    fn test_hc_dict_improves_ratio() {
        // Input is highly similar to the dictionary; compression should be
        // at least as good with the dictionary as without.
        let common = b"Content-Type: application/json\r\nAccept-Encoding: gzip\r\n";
        let dict = crate::dict::Lz4Dict::new(common);

        let input = b"Content-Type: application/json\r\nAccept-Encoding: gzip\r\n\
                      Content-Length: 42\r\nX-Custom-Header: value\r\n\r\n{}";

        let with_dict = compress_hc_with_dict(input, &dict, HcLevel::default())
            .expect("hc dict compress failed");
        let without_dict = compress_hc(input).expect("hc compress failed");

        // Verify roundtrip correctness first
        let decompressed = crate::dict::decompress_with_dict(&with_dict, input.len() * 2, &dict)
            .expect("hc dict decompress failed");
        assert_eq!(decompressed.as_slice(), input.as_ref());

        // With a dictionary that matches the beginning of the input exactly,
        // the dict version should be at most as large as the non-dict version.
        assert!(
            with_dict.len() <= without_dict.len(),
            "with_dict={} without_dict={}",
            with_dict.len(),
            without_dict.len()
        );
    }

    #[test]
    fn test_hc_dict_all_levels() {
        let dict_bytes = b"abcdef repeated pattern 1234567890";
        let dict = crate::dict::Lz4Dict::new(dict_bytes);
        let input = b"abcdef repeated pattern 1234567890 with extra data abcdef";

        for level_num in [1u8, 3, 6, 9, 12] {
            let level = HcLevel::new(level_num).expect("valid level");
            let compressed = compress_hc_with_dict(input, &dict, level)
                .unwrap_or_else(|_| panic!("level {} failed", level_num));
            let decompressed =
                crate::dict::decompress_with_dict(&compressed, input.len() * 2, &dict)
                    .unwrap_or_else(|_| panic!("level {} decompress failed", level_num));
            assert_eq!(
                decompressed.as_slice(),
                input.as_ref(),
                "level {} roundtrip failed",
                level_num
            );
        }
    }

    #[test]
    fn test_hc_encoder_compress_with_dict_method() {
        let dict_bytes = b"shared prefix data that appears in inputs";
        let dict = crate::dict::Lz4Dict::new(dict_bytes);
        let input = b"shared prefix data that appears in inputs plus more";

        let mut encoder = HcEncoder::new();
        let compressed = encoder
            .compress_with_dict(input, &dict)
            .expect("compress_with_dict failed");
        let decompressed = crate::dict::decompress_with_dict(&compressed, input.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed.as_slice(), input.as_ref());
    }
}
