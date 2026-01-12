//! LZSS algorithm for LZH compression.
//!
//! LZSS (Lempel-Ziv-Storer-Szymanski) is a derivative of LZ77 that uses
//! a flag bit to distinguish between literals and matches.

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

/// LZSS encoder.
#[derive(Debug)]
pub struct LzssEncoder {
    /// Sliding window buffer.
    window: Vec<u8>,
    /// Current position in window.
    position: usize,
    /// Window size.
    window_size: usize,
    /// Minimum match length.
    min_match: usize,
    /// Maximum match length.
    max_match: usize,
}

impl LzssEncoder {
    /// Create a new LZSS encoder.
    pub fn new(window_size: usize, min_match: usize, max_match: usize) -> Self {
        Self {
            window: vec![0; window_size * 2],
            position: 0,
            window_size,
            min_match,
            max_match,
        }
    }

    /// Create an encoder for lh5.
    pub fn lh5() -> Self {
        Self::new(8192, 3, 256)
    }

    /// Reset the encoder.
    pub fn reset(&mut self) {
        self.position = 0;
        self.window.fill(0);
    }

    /// Find the longest match at the current position.
    fn find_match(&self, pos: usize, lookahead: &[u8]) -> Option<(u16, u16)> {
        if lookahead.len() < self.min_match || pos < self.min_match {
            return None;
        }

        let search_start = pos.saturating_sub(self.window_size);
        let max_len = lookahead.len().min(self.max_match);

        let mut best_len = self.min_match - 1;
        let mut best_dist = 0usize;

        // Simple brute-force search (can be optimized with hash chains)
        for match_pos in search_start..pos {
            let dist = pos - match_pos;
            if dist == 0 || dist > self.window_size {
                continue;
            }

            // Check match length
            let mut len = 0;
            while len < max_len
                && match_pos + len < pos
                && match_pos + len < self.window.len()
                && self.window[match_pos + len] == lookahead[len]
            {
                len += 1;
            }

            // Also check overlap case (distance < length)
            if len == pos - match_pos && len < max_len {
                // Can extend into lookahead
                while len < max_len && lookahead[len % dist] == lookahead[len] {
                    len += 1;
                }
            }

            if len > best_len {
                best_len = len;
                best_dist = dist;
                if len >= max_len {
                    break;
                }
            }
        }

        if best_len >= self.min_match {
            Some((best_len as u16, best_dist as u16))
        } else {
            None
        }
    }

    /// Encode data to LZSS tokens.
    pub fn encode(&mut self, data: &[u8]) -> Vec<LzssToken> {
        let mut tokens = Vec::new();

        // Copy data to window
        let start = self.position;
        for (i, &byte) in data.iter().enumerate() {
            if start + i < self.window.len() {
                self.window[start + i] = byte;
            }
        }

        let mut pos = 0;
        while pos < data.len() {
            let lookahead = &data[pos..];

            if let Some((length, distance)) = self.find_match(start + pos, lookahead) {
                tokens.push(LzssToken::Match { length, distance });
                pos += length as usize;
            } else {
                tokens.push(LzssToken::Literal(data[pos]));
                pos += 1;
            }
        }

        self.position = start + data.len();

        // Slide window if needed
        if self.position >= self.window_size + self.window_size / 2 {
            let slide = self.window_size;
            self.window.copy_within(slide..self.position, 0);
            self.position -= slide;
        }

        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_lzss_encoder_literals() {
        let mut encoder = LzssEncoder::new(1024, 3, 256);

        let tokens = encoder.encode(b"abc");

        // No matches possible, all literals
        assert!(tokens.iter().all(|t| matches!(t, LzssToken::Literal(_))));
    }

    #[test]
    fn test_lzss_encoder_match() {
        let mut encoder = LzssEncoder::new(1024, 3, 256);

        let tokens = encoder.encode(b"abcabc");

        // Should find a match for the second "abc"
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
}
