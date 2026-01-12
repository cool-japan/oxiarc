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
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::with_capacity(input.len());
    let mut encoder = BlockEncoder::new(input);
    encoder.encode(&mut output)?;
    Ok(output)
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

    /// Encode the input data.
    fn encode(&mut self, output: &mut Vec<u8>) -> Result<()> {
        let input = self.input;
        let len = input.len();

        if len < MIN_MATCH {
            // Too small to compress, emit as literals
            self.emit_literals(output, 0, len, 0, 0)?;
            return Ok(());
        }

        let mut pos = 0;
        let mut anchor = 0; // Start of current literal run
        let end = len.saturating_sub(5); // Leave room for last literals

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

                // Extend match backwards (optional, skip for simplicity)
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
            } else {
                pos += 1;
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
        let compressed = compress_block(data).unwrap();
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_decompress_empty() {
        let data: &[u8] = b"";
        let decompressed = decompress_block(data, 0).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn test_roundtrip_small() {
        let data = b"ab";
        let compressed = compress_block(data).unwrap();
        let decompressed = decompress_block(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_simple() {
        let data = b"Hello, World!";
        let compressed = compress_block(data).unwrap();
        let decompressed = decompress_block(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let compressed = compress_block(data).unwrap();
        // Repeated data should compress well
        assert!(
            compressed.len() < data.len(),
            "compressed: {}, original: {}",
            compressed.len(),
            data.len()
        );
        let decompressed = decompress_block(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_pattern() {
        let data = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        let compressed = compress_block(data).unwrap();
        let decompressed = decompress_block(&compressed, data.len()).unwrap();
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
