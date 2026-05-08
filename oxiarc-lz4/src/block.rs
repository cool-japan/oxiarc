//! LZ4 block compression/decompression.
//!
//! LZ4 block format:
//! - Sequences of (token, [literal_length_ext], literals, [match_length_ext], offset)
//! - Token: 4-bit literal length + 4-bit match length
//! - If literal length = 15, additional bytes follow (add 255 until byte < 255)
//! - Literals: raw bytes
//! - Offset: 2 bytes little-endian (match offset, 1-65535)
//! - If match length = 15, additional bytes follow (add 255 until byte < 255)
//! - Match length is +4 (minimum match = 4)

use oxiarc_core::error::{OxiArcError, Result};

/// Minimum match length for LZ4.
const MIN_MATCH: usize = 4;

/// Maximum match offset (16-bit).
const MAX_OFFSET: usize = 65535;

/// Hash table size (must be power of 2).
const HASH_SIZE: usize = 1 << 14; // 16K entries

/// Compress data using LZ4 block format.
pub fn compress_block(input: &[u8]) -> Result<Vec<u8>> {
    compress_block_with_accel(input, 1)
}

/// Compress data using LZ4 block format with an acceleration parameter.
///
/// `acceleration` controls how aggressively the compressor skips positions
/// after a hash miss. A value of 1 is the default (no extra skipping). Higher
/// values make compression faster at the cost of a worse compression ratio.
/// Values less than 1 are clamped to 1.
///
/// This mirrors the `LZ4_compress_fast` acceleration parameter from the C
/// reference implementation.
pub fn compress_block_with_accel(input: &[u8], acceleration: i32) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let accel = acceleration.max(1) as usize;
    let mut output = Vec::with_capacity(input.len());
    let mut encoder = BlockEncoder::new(input);
    encoder.encode_with_accel(&mut output, accel)?;
    Ok(output)
}

/// Compress data using LZ4-HC (High Compression) block format.
///
/// `compression_level` ranges from 1 to 12. Higher values produce better
/// compression ratios but take longer. Values outside the valid range are
/// clamped to the nearest bound (1 or 12).
///
/// The output is a standard LZ4 block that can be decompressed with
/// [`decompress_block`].
pub fn compress_block_hc(input: &[u8], compression_level: i32) -> Result<Vec<u8>> {
    let clamped = compression_level.clamp(1, 12) as u8;
    // HcLevel::new will always succeed for 1..=12
    let level = match crate::hc::HcLevel::new(clamped) {
        Some(l) => l,
        None => crate::hc::HcLevel::DEFAULT,
    };
    crate::hc::compress_hc_level(input, level)
}

/// Decompress LZ4 block data.
pub fn decompress_block(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::with_capacity(max_output.min(input.len() * 4));
    let mut decoder = BlockDecoder::new(input);
    decoder.decode(&mut output, max_output)?;
    Ok(output)
}

/// LZ4 block encoder.
struct BlockEncoder<'a> {
    input: &'a [u8],
    hash_table: Vec<u32>,
}

impl<'a> BlockEncoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            hash_table: vec![0; HASH_SIZE],
        }
    }

    /// Compute hash for 4 bytes.
    fn hash(data: u32) -> usize {
        // FNV-like hash multiplied by a prime
        ((data.wrapping_mul(2654435761)) >> 18) as usize & (HASH_SIZE - 1)
    }

    /// Read 4 bytes as u32 (little-endian).
    fn read_u32(data: &[u8], pos: usize) -> u32 {
        if pos + 4 <= data.len() {
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
        } else {
            0
        }
    }

    /// Encode the input data with the given acceleration factor.
    ///
    /// `accel` controls the step size after a hash miss. The position
    /// advances by `1 + (misses >> accel_shift)` where `accel_shift` is
    /// derived from `accel`. This matches the spirit of the C reference
    /// `LZ4_compress_fast` implementation.
    fn encode_with_accel(&mut self, output: &mut Vec<u8>, accel: usize) -> Result<()> {
        let input = self.input;
        let len = input.len();

        if len < MIN_MATCH {
            // Too small to compress, emit as literals
            self.emit_literals(output, 0, len, 0, 0)?;
            return Ok(());
        }

        // The skip-acceleration logic: after each consecutive miss we
        // increase the step. `accel` scales how quickly the step grows.
        // accel = 1 → standard behaviour (step grows slowly)
        // accel > 1 → step grows faster → fewer probes → faster, worse ratio
        let accel_shift = match accel {
            0..=1 => 6, // default: step grows by 1 every 64 misses
            2 => 5,     // every 32
            3..=4 => 4, // every 16
            5..=8 => 3, // every 8
            9..=16 => 2,
            17..=64 => 1,
            _ => 0, // extreme: step = 1 + misses (very fast, poor ratio)
        };

        let mut pos = 0;
        let mut anchor = 0; // Start of current literal run
        let end = len.saturating_sub(5); // Leave room for last literals

        let mut misses: usize = 0;

        while pos < end {
            let cur_u32 = Self::read_u32(input, pos);
            let h = Self::hash(cur_u32);
            let match_pos = self.hash_table[h] as usize;
            self.hash_table[h] = pos as u32;

            // Check for match
            if match_pos > 0
                && pos.saturating_sub(match_pos) <= MAX_OFFSET
                && Self::read_u32(input, match_pos) == cur_u32
            {
                // Found a match!
                let offset = pos - match_pos;

                // Extend match forwards
                let mut match_len = MIN_MATCH;
                while pos + match_len < len
                    && input[match_pos + match_len] == input[pos + match_len]
                {
                    match_len += 1;
                }

                // Emit literals before match
                let literal_len = pos - anchor;
                self.emit_sequence(output, anchor, literal_len, offset, match_len)?;

                pos += match_len;
                anchor = pos;
                misses = 0;
            } else {
                // Miss — advance by acceleration-scaled step
                let step = 1 + (misses >> accel_shift);
                misses += 1;
                pos += step;
            }
        }

        // Emit remaining literals
        let remaining = len - anchor;
        if remaining > 0 {
            self.emit_last_literals(output, anchor, remaining)?;
        }

        Ok(())
    }

    /// Emit a sequence (literals + match).
    fn emit_sequence(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
        offset: usize,
        match_len: usize,
    ) -> Result<()> {
        self.emit_literals(output, literal_start, literal_len, offset, match_len)
    }

    /// Emit literals followed by a match reference.
    fn emit_literals(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
        offset: usize,
        match_len: usize,
    ) -> Result<()> {
        // Token: upper 4 bits = literal length, lower 4 bits = match length - 4
        let lit_token = if literal_len >= 15 { 15 } else { literal_len };
        let match_token = if match_len >= MIN_MATCH {
            let ml = match_len - MIN_MATCH;
            if ml >= 15 { 15 } else { ml }
        } else {
            0
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
        output.extend_from_slice(&self.input[literal_start..literal_start + literal_len]);

        // Match offset and length (if there's a match)
        if match_len >= MIN_MATCH {
            // Offset: 2 bytes little-endian
            output.push(offset as u8);
            output.push((offset >> 8) as u8);

            // Extended match length
            if match_len - MIN_MATCH >= 15 {
                let mut remaining = match_len - MIN_MATCH - 15;
                while remaining >= 255 {
                    output.push(255);
                    remaining -= 255;
                }
                output.push(remaining as u8);
            }
        }

        Ok(())
    }

    /// Emit the last literals (no match at the end).
    fn emit_last_literals(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
    ) -> Result<()> {
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
        output.extend_from_slice(&self.input[literal_start..literal_start + literal_len]);

        Ok(())
    }
}

/// LZ4 block decoder.
struct BlockDecoder<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> BlockDecoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Decode the block into output.
    fn decode(&mut self, output: &mut Vec<u8>, max_output: usize) -> Result<()> {
        while self.pos < self.input.len() && output.len() < max_output {
            // Read token
            let token = self.read_byte()?;
            let literal_len = (token >> 4) as usize;
            let match_len_base = (token & 0x0F) as usize;

            // Extended literal length
            let literal_len = self.read_length(literal_len)?;

            // Check bounds
            if self.pos + literal_len > self.input.len() {
                return Err(OxiArcError::corrupted(
                    self.pos as u64,
                    "truncated literals",
                ));
            }

            // Copy literals
            output.extend_from_slice(&self.input[self.pos..self.pos + literal_len]);
            self.pos += literal_len;

            // Check if this is the last sequence (no match)
            if self.pos >= self.input.len() {
                break;
            }

            // Read match offset
            let offset = self.read_u16_le()? as usize;
            if offset == 0 {
                return Err(OxiArcError::corrupted(self.pos as u64, "zero offset"));
            }

            // Extended match length
            let match_len = self.read_length(match_len_base)? + MIN_MATCH;

            // Check offset is valid
            if offset > output.len() {
                return Err(OxiArcError::corrupted(
                    self.pos as u64,
                    "offset exceeds output",
                ));
            }

            // Copy match (handle overlapping)
            let start = output.len() - offset;
            for i in 0..match_len {
                let byte = output[start + (i % offset)];
                output.push(byte);
            }
        }

        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8> {
        if self.pos >= self.input.len() {
            return Err(OxiArcError::unexpected_eof(1));
        }
        let b = self.input[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        if self.pos + 2 > self.input.len() {
            return Err(OxiArcError::unexpected_eof(2));
        }
        let value = u16::from_le_bytes([self.input[self.pos], self.input[self.pos + 1]]);
        self.pos += 2;
        Ok(value)
    }

    fn read_length(&mut self, base: usize) -> Result<usize> {
        let mut len = base;
        if base == 15 {
            loop {
                let b = self.read_byte()? as usize;
                len += b;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_empty() {
        let data: &[u8] = b"";
        let compressed = compress_block(data).expect("block compress empty");
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_decompress_empty() {
        let data: &[u8] = b"";
        let decompressed = decompress_block(data, 0).expect("block decompress empty");
        assert!(decompressed.is_empty());
    }

    #[test]
    fn test_roundtrip_small() {
        let data = b"ab";
        let compressed = compress_block(data).expect("block compress small");
        let decompressed =
            decompress_block(&compressed, data.len()).expect("block decompress small");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_simple() {
        let data = b"Hello, World!";
        let compressed = compress_block(data).expect("block compress simple");
        let decompressed =
            decompress_block(&compressed, data.len()).expect("block decompress simple");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let compressed = compress_block(data).expect("block compress repeated");
        // Repeated data should compress well
        assert!(
            compressed.len() < data.len(),
            "compressed: {}, original: {}",
            compressed.len(),
            data.len()
        );
        let decompressed =
            decompress_block(&compressed, data.len()).expect("block decompress repeated");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_pattern() {
        let data = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        let compressed = compress_block(data).expect("block compress pattern");
        let decompressed =
            decompress_block(&compressed, data.len()).expect("block decompress pattern");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_with_accel_default() {
        let data = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        let compressed = compress_block_with_accel(data, 1).expect("accel=1 failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_with_accel_high() {
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(100);
        let compressed = compress_block_with_accel(&data, 10).expect("accel=10 failed");
        let decompressed = decompress_block(&compressed, data.len()).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_accel_higher_means_faster_larger() {
        // Higher acceleration should produce larger (or equal) output
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(200);
        let c1 = compress_block_with_accel(&data, 1).expect("accel=1");
        let c10 = compress_block_with_accel(&data, 10).expect("accel=10");
        let c100 = compress_block_with_accel(&data, 100).expect("accel=100");

        // All must roundtrip
        assert_eq!(decompress_block(&c1, data.len()).expect("d1"), data);
        assert_eq!(decompress_block(&c10, data.len()).expect("d10"), data);
        assert_eq!(decompress_block(&c100, data.len()).expect("d100"), data);

        // Higher accel => larger (or equal) compressed output
        assert!(
            c10.len() >= c1.len(),
            "accel10={} should be >= accel1={}",
            c10.len(),
            c1.len()
        );
        assert!(
            c100.len() >= c1.len(),
            "accel100={} should be >= accel1={}",
            c100.len(),
            c1.len()
        );
    }

    #[test]
    fn test_accel_clamp_negative() {
        // Negative acceleration should be clamped to 1 (default behaviour)
        let data = b"Hello world Hello world Hello world";
        let c_neg = compress_block_with_accel(data, -5).expect("accel=-5");
        let c_def = compress_block(data).expect("default");
        assert_eq!(c_neg, c_def);
    }

    #[test]
    fn test_accel_zero_treated_as_default() {
        let data = b"ABCABCABCABCABCABCABCABCABCABC";
        let c0 = compress_block_with_accel(data, 0).expect("accel=0");
        let c1 = compress_block_with_accel(data, 1).expect("accel=1");
        assert_eq!(c0, c1);
    }

    #[test]
    fn test_compress_block_hc_roundtrip() {
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(50);
        for level in [1, 3, 6, 9, 12] {
            let compressed =
                compress_block_hc(&data, level).unwrap_or_else(|_| panic!("hc level={}", level));
            let decompressed = decompress_block(&compressed, data.len())
                .unwrap_or_else(|_| panic!("decompress hc level={}", level));
            assert_eq!(
                decompressed, data,
                "roundtrip failed for hc level={}",
                level
            );
        }
    }

    #[test]
    fn test_compress_block_hc_clamp() {
        // Out-of-range levels should be clamped, not error
        let data = b"some data to compress with clamped levels here!";
        let c_low = compress_block_hc(data, -10).expect("hc level=-10 (clamped to 1)");
        let c_high = compress_block_hc(data, 999).expect("hc level=999 (clamped to 12)");
        assert_eq!(decompress_block(&c_low, data.len()).expect("d"), data);
        assert_eq!(decompress_block(&c_high, data.len()).expect("d"), data);
    }

    #[test]
    fn test_hc_vs_fast_compression_ratio() {
        let data = b"The quick brown fox jumps over the lazy dog repeatedly. ".repeat(100);
        let fast = compress_block(&data).expect("fast");
        let hc = compress_block_hc(&data, 9).expect("hc9");

        // Both roundtrip correctly
        assert_eq!(decompress_block(&fast, data.len()).expect("d"), data);
        assert_eq!(decompress_block(&hc, data.len()).expect("d"), data);

        // HC should achieve at least as good (usually better) ratio
        assert!(
            hc.len() <= fast.len(),
            "HC={} should be <= Fast={}",
            hc.len(),
            fast.len()
        );
    }

    #[test]
    fn test_compress_block_hc_empty() {
        let data: &[u8] = b"";
        let compressed = compress_block_hc(data, 9).expect("hc empty");
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_compress_block_with_accel_empty() {
        let data: &[u8] = b"";
        let compressed = compress_block_with_accel(data, 5).expect("accel empty");
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_compress_block_with_accel_small() {
        let data = b"ab";
        let compressed = compress_block_with_accel(data, 1).expect("accel small");
        let decompressed = decompress_block(&compressed, data.len()).expect("d");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_hash_distribution() {
        // Test that hash function produces varied results
        let h1 = BlockEncoder::hash(0x12345678);
        let h2 = BlockEncoder::hash(0x87654321);
        let h3 = BlockEncoder::hash(0x00000000);
        let h4 = BlockEncoder::hash(0xFFFFFFFF);
        // All should be within hash table bounds
        assert!(h1 < HASH_SIZE);
        assert!(h2 < HASH_SIZE);
        assert!(h3 < HASH_SIZE);
        assert!(h4 < HASH_SIZE);
        // Should be somewhat distributed (not all the same)
        assert!(h1 != h2 || h2 != h3 || h3 != h4);
    }
}
