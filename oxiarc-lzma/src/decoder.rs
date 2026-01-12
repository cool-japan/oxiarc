//! LZMA decompression.
//!
//! This module implements LZMA decompression as used in 7z, xz, and lzma files.

use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, LengthModel,
    LzmaModel, LzmaProperties, MATCH_LEN_MIN, State,
};
use crate::range_coder::RangeDecoder;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum dictionary size (4 GB).
pub const DICT_SIZE_MAX: u32 = 0xFFFF_FFFF;

/// Decode a bit tree.
fn decode_bit_tree<R: Read>(
    rc: &mut RangeDecoder<R>,
    probs: &mut [u16],
    num_bits: u32,
) -> Result<u32> {
    let mut m = 1usize;

    for _ in 0..num_bits {
        let bit = rc.decode_bit(&mut probs[m])?;
        m = (m << 1) | bit as usize;
    }

    Ok((m as u32) - (1 << num_bits))
}

/// Decode a length.
fn decode_length<R: Read>(
    rc: &mut RangeDecoder<R>,
    len_model: &mut LengthModel,
    pos_state: usize,
) -> Result<u32> {
    if rc.decode_bit(&mut len_model.choice)? == 0 {
        // Low length (0-7)
        let len = decode_bit_tree(rc, &mut len_model.low[pos_state], LEN_LOW_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32)
    } else if rc.decode_bit(&mut len_model.choice2)? == 0 {
        // Mid length (8-15)
        let len = decode_bit_tree(rc, &mut len_model.mid[pos_state], LEN_MID_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32 + (1 << LEN_LOW_BITS))
    } else {
        // High length (16-271)
        let len = decode_bit_tree(rc, &mut len_model.high, LEN_HIGH_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32 + (1 << LEN_LOW_BITS) + (1 << LEN_MID_BITS))
    }
}

/// LZMA decoder.
pub struct LzmaDecoder<R: Read> {
    /// Range decoder.
    rc: RangeDecoder<R>,
    /// LZMA model.
    model: LzmaModel,
    /// Dictionary/output buffer.
    dict: Vec<u8>,
    /// Current position in dictionary.
    dict_pos: usize,
    /// Dictionary size (for wrapping).
    dict_size: usize,
    /// Current state.
    state: State,
    /// Rep distances.
    rep: [u32; 4],
    /// Uncompressed size (if known).
    uncompressed_size: Option<u64>,
    /// Bytes decoded.
    bytes_decoded: u64,
}

impl<R: Read> LzmaDecoder<R> {
    /// Create a new LZMA decoder.
    pub fn new(reader: R, props: LzmaProperties, dict_size: u32) -> Result<Self> {
        let dict_size = dict_size.max(4096) as usize;

        Ok(Self {
            rc: RangeDecoder::new(reader)?,
            model: LzmaModel::new(props),
            dict: vec![0u8; dict_size],
            dict_pos: 0,
            dict_size,
            state: State::new(),
            rep: [0; 4],
            uncompressed_size: None,
            bytes_decoded: 0,
        })
    }

    /// Create decoder from LZMA header.
    pub fn from_header(mut reader: R) -> Result<Self> {
        // Read properties byte
        let mut props_buf = [0u8; 1];
        reader.read_exact(&mut props_buf)?;

        let props = LzmaProperties::from_byte(props_buf[0])
            .ok_or_else(|| OxiArcError::invalid_header("Invalid LZMA properties"))?;

        // Read dictionary size (4 bytes, little-endian)
        let mut dict_buf = [0u8; 4];
        reader.read_exact(&mut dict_buf)?;
        let dict_size = u32::from_le_bytes(dict_buf);

        // Read uncompressed size (8 bytes, little-endian)
        let mut size_buf = [0u8; 8];
        reader.read_exact(&mut size_buf)?;
        let uncompressed_size = u64::from_le_bytes(size_buf);

        let mut decoder = Self::new(reader, props, dict_size)?;

        if uncompressed_size != u64::MAX {
            decoder.uncompressed_size = Some(uncompressed_size);
        }

        Ok(decoder)
    }

    /// Decode a literal byte.
    fn decode_literal(&mut self, prev_byte: u8, match_byte: u8) -> Result<u8> {
        let lit_state = self.model.literal.get_state(
            self.bytes_decoded,
            prev_byte,
            self.model.props.lc,
            self.model.props.lp,
        );

        if self.state.is_literal() {
            self.decode_literal_normal(lit_state)
        } else {
            self.decode_literal_matched(lit_state, match_byte)
        }
    }

    /// Decode a normal literal (no match context).
    fn decode_literal_normal(&mut self, lit_state: usize) -> Result<u8> {
        let mut symbol = 1usize;

        loop {
            let bit = self
                .rc
                .decode_bit(&mut self.model.literal.probs[lit_state][symbol])?;

            symbol = (symbol << 1) | bit as usize;

            if symbol >= 0x100 {
                break;
            }
        }

        Ok((symbol - 0x100) as u8)
    }

    /// Decode a literal with match byte context.
    fn decode_literal_matched(&mut self, lit_state: usize, match_byte: u8) -> Result<u8> {
        let mut symbol = 1usize;
        let mut match_byte = match_byte as usize;

        loop {
            let match_bit = (match_byte >> 7) & 1;
            match_byte <<= 1;

            let prob_idx = 0x100 + (match_bit << 8) + symbol;
            let bit = self
                .rc
                .decode_bit(&mut self.model.literal.probs[lit_state][prob_idx])?;
            symbol = (symbol << 1) | bit as usize;

            if symbol >= 0x100 {
                break;
            }

            if bit as usize != match_bit {
                // Mismatch, continue without match context
                while symbol < 0x100 {
                    let bit = self
                        .rc
                        .decode_bit(&mut self.model.literal.probs[lit_state][symbol])?;
                    symbol = (symbol << 1) | bit as usize;
                }
                break;
            }
        }

        Ok((symbol - 0x100) as u8)
    }

    /// Decode a distance.
    fn decode_distance(&mut self, len: u32) -> Result<u32> {
        let len_state = ((len - MATCH_LEN_MIN as u32).min(3)) as usize;

        // Decode distance slot
        let slot = decode_bit_tree(&mut self.rc, &mut self.model.distance.slot[len_state], 6)?;

        if slot < 4 {
            return Ok(slot);
        }

        let num_direct_bits = ((slot >> 1) - 1) as u32;
        let mut dist = (2 | (slot & 1)) << num_direct_bits;

        if slot < END_POS_MODEL_INDEX as u32 {
            // Use special probabilities (reverse bit tree)
            // base_idx points to start of probability block for this slot
            let base_idx = (slot as usize) - (slot as usize >> 1) - 1;

            let mut result = 0u32;
            let mut m = 1usize;

            for i in 0..num_direct_bits {
                let bit = self
                    .rc
                    .decode_bit(&mut self.model.distance.special[base_idx + m - 1])?;
                m = (m << 1) | bit as usize;
                result |= bit << i;
            }

            dist += result;
        } else {
            // Direct bits + alignment bits
            let num_align_bits = DIST_ALIGN_BITS;
            let num_direct = num_direct_bits - num_align_bits;

            // Decode direct bits with fixed probability
            let direct = self.rc.decode_direct_bits(num_direct)?;
            dist += direct << num_align_bits;

            // Decode alignment bits with model
            let align = self
                .rc
                .decode_bit_tree_reverse(&mut self.model.distance.align, num_align_bits)?;
            dist += align;
        }

        Ok(dist)
    }

    /// Get byte from dictionary at distance.
    fn get_byte(&self, dist: usize) -> u8 {
        let pos = if self.dict_pos > dist {
            self.dict_pos - dist - 1
        } else {
            self.dict_size - (dist - self.dict_pos) - 1
        };
        self.dict[pos]
    }

    /// Decompress all data.
    pub fn decompress(mut self) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        loop {
            // Check if we've reached the end
            if let Some(size) = self.uncompressed_size {
                if self.bytes_decoded >= size {
                    break;
                }
            }

            let pos_state = (self.bytes_decoded as usize) & (self.model.props.num_pos_states() - 1);
            let state_idx = self.state.value();

            // Decode is_match
            let is_match = self
                .rc
                .decode_bit(&mut self.model.is_match[state_idx][pos_state])?;

            if is_match == 0 {
                // Literal
                let prev_byte = if self.bytes_decoded == 0 {
                    0
                } else {
                    self.get_byte(0)
                };

                let match_byte =
                    if !self.state.is_literal() && self.rep[0] < self.bytes_decoded as u32 {
                        self.get_byte(self.rep[0] as usize)
                    } else {
                        0
                    };

                let byte = self.decode_literal(prev_byte, match_byte)?;

                self.dict[self.dict_pos] = byte;
                self.dict_pos = (self.dict_pos + 1) % self.dict_size;
                output.push(byte);
                self.bytes_decoded += 1;
                self.state.update_literal();
            } else {
                // Match or rep
                let is_rep = self.rc.decode_bit(&mut self.model.is_rep[state_idx])?;

                let (len, dist) = if is_rep == 0 {
                    // Normal match
                    let len = decode_length(&mut self.rc, &mut self.model.match_len, pos_state)?;
                    let dist = self.decode_distance(len)?;

                    // Shift rep distances
                    self.rep[3] = self.rep[2];
                    self.rep[2] = self.rep[1];
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;

                    // Check for end marker
                    if dist == 0xFFFF_FFFF {
                        if self.uncompressed_size.is_none() {
                            break;
                        } else {
                            return Err(OxiArcError::corrupted(
                                self.bytes_decoded,
                                "Invalid LZMA data",
                            ));
                        }
                    }

                    self.state.update_match();
                    (len, dist)
                } else {
                    // Rep match
                    let is_rep0 = self.rc.decode_bit(&mut self.model.is_rep0[state_idx])?;

                    if is_rep0 == 0 {
                        // Rep0
                        let is_rep0_long = self
                            .rc
                            .decode_bit(&mut self.model.is_rep0_long[state_idx][pos_state])?;

                        if is_rep0_long == 0 {
                            // Short rep (length 1)
                            let dist = self.rep[0];

                            if dist >= self.bytes_decoded as u32 {
                                return Err(OxiArcError::corrupted(
                                    self.bytes_decoded,
                                    "Invalid LZMA data",
                                ));
                            }

                            let byte = self.get_byte(dist as usize);
                            self.dict[self.dict_pos] = byte;
                            self.dict_pos = (self.dict_pos + 1) % self.dict_size;
                            output.push(byte);
                            self.bytes_decoded += 1;
                            self.state.update_short_rep();
                            continue;
                        }

                        self.state.update_long_rep();
                        let len = decode_length(&mut self.rc, &mut self.model.rep_len, pos_state)?;
                        (len, self.rep[0])
                    } else {
                        let is_rep1 = self.rc.decode_bit(&mut self.model.is_rep1[state_idx])?;

                        let dist = if is_rep1 == 0 {
                            // Rep1
                            self.rep.swap(0, 1);
                            self.rep[0]
                        } else {
                            let is_rep2 = self.rc.decode_bit(&mut self.model.is_rep2[state_idx])?;

                            if is_rep2 == 0 {
                                // Rep2
                                let d = self.rep[2];
                                self.rep[2] = self.rep[1];
                                self.rep[1] = self.rep[0];
                                self.rep[0] = d;
                                d
                            } else {
                                // Rep3
                                let d = self.rep[3];
                                self.rep[3] = self.rep[2];
                                self.rep[2] = self.rep[1];
                                self.rep[1] = self.rep[0];
                                self.rep[0] = d;
                                d
                            }
                        };

                        self.state.update_long_rep();
                        let len = decode_length(&mut self.rc, &mut self.model.rep_len, pos_state)?;
                        (len, dist)
                    }
                };

                // Validate distance
                if dist as u64 >= self.bytes_decoded {
                    return Err(OxiArcError::corrupted(
                        self.bytes_decoded,
                        "Invalid LZMA data",
                    ));
                }

                // Copy from dictionary
                let len = len as usize;
                for _ in 0..len {
                    let byte = self.get_byte(dist as usize);
                    self.dict[self.dict_pos] = byte;
                    self.dict_pos = (self.dict_pos + 1) % self.dict_size;
                    output.push(byte);
                    self.bytes_decoded += 1;
                }
            }
        }

        Ok(output)
    }
}

/// Decompress LZMA data with header.
pub fn decompress<R: Read>(reader: R) -> Result<Vec<u8>> {
    let decoder = LzmaDecoder::from_header(reader)?;
    decoder.decompress()
}

/// Decompress raw LZMA data (no header).
pub fn decompress_raw<R: Read>(
    reader: R,
    props: LzmaProperties,
    dict_size: u32,
    uncompressed_size: Option<u64>,
) -> Result<Vec<u8>> {
    let mut decoder = LzmaDecoder::new(reader, props, dict_size)?;
    decoder.uncompressed_size = uncompressed_size;
    decoder.decompress()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_decoder_creation() {
        let props = LzmaProperties::default();
        // Minimal valid LZMA stream (just header bytes for range decoder)
        let data = vec![0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(data);

        let result = LzmaDecoder::new(cursor, props, 4096);
        assert!(result.is_ok());
    }

    #[test]
    fn test_properties_round_trip() {
        let props = LzmaProperties::new(3, 0, 2);
        let byte = props.to_byte();
        let decoded = LzmaProperties::from_byte(byte).unwrap();

        assert_eq!(decoded.lc, 3);
        assert_eq!(decoded.lp, 0);
        assert_eq!(decoded.pb, 2);
    }
}
