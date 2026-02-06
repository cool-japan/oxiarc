//! LZMA2 codec for XZ files.
//!
//! LZMA2 is a container format around LZMA that provides:
//! - Support for uncompressible chunks (stored as-is)
//! - Dictionary/state reset capability
//! - Chunk-based format for better streaming
//!
//! ## Chunk Format
//!
//! Each chunk starts with a control byte:
//! - 0x00: End of LZMA2 stream
//! - 0x01: Uncompressed chunk, dictionary reset
//! - 0x02: Uncompressed chunk, no reset
//! - 0x80-0xFF: LZMA compressed chunk (with various reset flags)

use crate::encoder::LzmaEncoder;
use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, LengthModel,
    LzmaModel, LzmaProperties, MATCH_LEN_MIN, State,
};
use crate::{LzmaLevel, RangeDecoder};
use oxiarc_core::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// LZMA2 decoder.
pub struct Lzma2Decoder {
    /// Dictionary size.
    dict_size: u32,
    /// Current dictionary/history buffer (ring buffer).
    dictionary: Vec<u8>,
    /// Current write position in dictionary.
    dict_pos: usize,
    /// How many bytes are currently in the dictionary.
    dict_len: usize,
    /// LZMA properties (may change between chunks).
    props: Option<LzmaProperties>,
    /// LZMA model state (preserved across chunks unless reset).
    model: Option<LzmaModel>,
    /// Decoder state (preserved across chunks unless reset).
    state: State,
    /// Rep distances (preserved across chunks unless reset).
    rep: [u32; 4],
    /// Whether decoding is finished.
    finished: bool,
}

impl Lzma2Decoder {
    /// Create a new LZMA2 decoder with the given dictionary size.
    pub fn new(dict_size: u32) -> Self {
        let dict_size = dict_size.max(4096);
        Self {
            dict_size,
            dictionary: vec![0u8; dict_size as usize],
            dict_pos: 0,
            dict_len: 0,
            props: None,
            model: None,
            state: State::new(),
            rep: [0; 4],
            finished: false,
        }
    }

    /// Decode an LZMA2 stream.
    pub fn decode<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        loop {
            // Read control byte
            let mut control = [0u8; 1];
            if reader.read_exact(&mut control).is_err() {
                break;
            }
            let control = control[0];

            if control == 0x00 {
                // End of stream
                self.finished = true;
                break;
            }

            if control == 0x01 || control == 0x02 {
                // Uncompressed chunk
                let reset_dict = control == 0x01;
                self.decode_uncompressed_chunk(reader, &mut output, reset_dict)?;
            } else if control >= 0x80 {
                // LZMA compressed chunk
                self.decode_lzma_chunk(reader, &mut output, control)?;
            } else {
                return Err(OxiArcError::invalid_header(format!(
                    "Invalid LZMA2 control byte: 0x{:02X}",
                    control
                )));
            }
        }

        Ok(output)
    }

    /// Decode an uncompressed chunk.
    fn decode_uncompressed_chunk<R: Read>(
        &mut self,
        reader: &mut R,
        output: &mut Vec<u8>,
        reset_dict: bool,
    ) -> Result<()> {
        // Read size (big-endian, 16-bit) + 1
        let mut size_bytes = [0u8; 2];
        reader.read_exact(&mut size_bytes)?;
        let size = u16::from_be_bytes(size_bytes) as usize + 1;

        if reset_dict {
            self.dict_pos = 0;
            self.dict_len = 0;
        }

        // Read uncompressed data
        let start = output.len();
        output.resize(start + size, 0);
        reader.read_exact(&mut output[start..])?;

        // Update dictionary
        self.update_dictionary(&output[start..]);

        Ok(())
    }

    /// Decode an LZMA compressed chunk.
    fn decode_lzma_chunk<R: Read>(
        &mut self,
        reader: &mut R,
        output: &mut Vec<u8>,
        control: u8,
    ) -> Result<()> {
        // Parse control byte
        let reset_dict = (control & 0x20) != 0;
        let reset_state = (control & 0x40) != 0 || reset_dict;
        let new_props = (control & 0x40) != 0;

        // Read uncompressed size (high 5 bits from control + 16-bit)
        let uncompressed_hi = ((control & 0x1F) as usize) << 16;
        let mut size_bytes = [0u8; 2];
        reader.read_exact(&mut size_bytes)?;
        let uncompressed_size = (uncompressed_hi | (u16::from_be_bytes(size_bytes) as usize)) + 1;

        // Read compressed size (16-bit) + 1
        reader.read_exact(&mut size_bytes)?;
        let compressed_size = u16::from_be_bytes(size_bytes) as usize + 1;

        // Read properties byte if needed
        if new_props {
            let mut props_byte = [0u8; 1];
            reader.read_exact(&mut props_byte)?;
            self.props = Some(
                LzmaProperties::from_byte(props_byte[0])
                    .ok_or_else(|| OxiArcError::invalid_header("Invalid LZMA properties"))?,
            );
        }

        if reset_dict {
            self.dict_pos = 0;
            self.dict_len = 0;
        }

        if reset_state {
            self.state = State::new();
            self.rep = [0; 4];
            // Reset model with new properties
            if let Some(props) = self.props {
                self.model = Some(LzmaModel::new(props));
            }
        }

        // Read compressed data
        let mut compressed = vec![0u8; compressed_size];
        reader.read_exact(&mut compressed)?;

        // Decompress using LZMA
        let props = self
            .props
            .ok_or_else(|| OxiArcError::invalid_header("LZMA2 chunk requires properties"))?;

        let decompressed = self.decompress_lzma_chunk(&compressed, props, uncompressed_size)?;

        // Update dictionary and output
        self.update_dictionary(&decompressed);
        output.extend_from_slice(&decompressed);

        Ok(())
    }

    /// Decompress LZMA data for a chunk using internal state.
    fn decompress_lzma_chunk(
        &mut self,
        data: &[u8],
        props: LzmaProperties,
        uncompressed_size: usize,
    ) -> Result<Vec<u8>> {
        let mut cursor = std::io::Cursor::new(data);
        let mut rc = RangeDecoder::new(&mut cursor)?;

        // Ensure model exists
        if self.model.is_none() {
            self.model = Some(LzmaModel::new(props));
        }

        let mut output = Vec::with_capacity(uncompressed_size);
        let mut bytes_decoded = 0u64;

        while bytes_decoded < uncompressed_size as u64 {
            let pos_state = (bytes_decoded as usize) & (props.num_pos_states() - 1);
            let state_idx = self.state.value();

            // Get mutable reference to model
            let model = self
                .model
                .as_mut()
                .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;

            // Decode is_match
            let is_match = rc.decode_bit(&mut model.is_match[state_idx][pos_state])?;

            if is_match == 0 {
                // Literal
                let prev_byte = if bytes_decoded == 0 && self.dict_len == 0 {
                    0
                } else {
                    self.get_byte_from_dict(0, bytes_decoded)
                };

                let match_byte = if !self.state.is_literal()
                    && self.rep[0] < (self.dict_len as u64 + bytes_decoded) as u32
                {
                    self.get_byte_from_dict(self.rep[0] as usize, bytes_decoded)
                } else {
                    0
                };

                let byte = self.decode_literal(&mut rc, prev_byte, match_byte, bytes_decoded)?;

                output.push(byte);
                bytes_decoded += 1;
                self.state.update_literal();
            } else {
                // Match or rep
                let model = self
                    .model
                    .as_mut()
                    .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;
                let is_rep = rc.decode_bit(&mut model.is_rep[state_idx])?;

                if is_rep == 0 {
                    // Normal match
                    let model = self
                        .model
                        .as_mut()
                        .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;
                    let len = decode_length(&mut rc, &mut model.match_len, pos_state)?;
                    let dist = self.decode_distance(&mut rc, len)?;

                    // Shift rep distances
                    self.rep[3] = self.rep[2];
                    self.rep[2] = self.rep[1];
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;

                    // Check for end marker
                    if dist == 0xFFFF_FFFF {
                        break;
                    }

                    self.state.update_match();
                    self.copy_from_dict(&mut output, dist as usize, len as usize, bytes_decoded)?;
                    bytes_decoded += len as u64;
                } else {
                    // Rep match
                    let model = self
                        .model
                        .as_mut()
                        .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;
                    let is_rep0 = rc.decode_bit(&mut model.is_rep0[state_idx])?;

                    if is_rep0 == 0 {
                        // Rep0
                        let model = self.model.as_mut().ok_or_else(|| {
                            OxiArcError::corrupted(0, "LZMA model not initialized")
                        })?;
                        let is_rep0_long =
                            rc.decode_bit(&mut model.is_rep0_long[state_idx][pos_state])?;

                        if is_rep0_long == 0 {
                            // Short rep (length 1)
                            let dist = self.rep[0];

                            if dist as u64 >= self.dict_len as u64 + bytes_decoded {
                                return Err(OxiArcError::corrupted(
                                    bytes_decoded,
                                    "Invalid LZMA data",
                                ));
                            }

                            let byte = self.get_byte_from_dict(dist as usize, bytes_decoded);
                            output.push(byte);
                            bytes_decoded += 1;
                            self.state.update_short_rep();
                            continue;
                        }

                        self.state.update_long_rep();
                        let model = self.model.as_mut().ok_or_else(|| {
                            OxiArcError::corrupted(0, "LZMA model not initialized")
                        })?;
                        let len = decode_length(&mut rc, &mut model.rep_len, pos_state)?;
                        self.copy_from_dict(
                            &mut output,
                            self.rep[0] as usize,
                            len as usize,
                            bytes_decoded,
                        )?;
                        bytes_decoded += len as u64;
                    } else {
                        let model = self.model.as_mut().ok_or_else(|| {
                            OxiArcError::corrupted(0, "LZMA model not initialized")
                        })?;
                        let is_rep1 = rc.decode_bit(&mut model.is_rep1[state_idx])?;

                        let dist = if is_rep1 == 0 {
                            // Rep1
                            self.rep.swap(0, 1);
                            self.rep[0]
                        } else {
                            let model = self.model.as_mut().ok_or_else(|| {
                                OxiArcError::corrupted(0, "LZMA model not initialized")
                            })?;
                            let is_rep2 = rc.decode_bit(&mut model.is_rep2[state_idx])?;

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
                        let model = self.model.as_mut().ok_or_else(|| {
                            OxiArcError::corrupted(0, "LZMA model not initialized")
                        })?;
                        let len = decode_length(&mut rc, &mut model.rep_len, pos_state)?;
                        self.copy_from_dict(
                            &mut output,
                            dist as usize,
                            len as usize,
                            bytes_decoded,
                        )?;
                        bytes_decoded += len as u64;
                    }
                }
            }
        }

        Ok(output)
    }

    /// Get a byte from the combined dictionary + output buffer.
    fn get_byte_from_dict(&self, dist: usize, current_output_len: u64) -> u8 {
        // If dist is within current output, read from there
        if dist < current_output_len as usize {
            // This would need access to output, which we handle differently
            // For now, we rely on the dictionary being properly populated
        }

        // Calculate position in dictionary ring buffer
        let total_len = self.dict_len;
        if dist >= total_len {
            return 0;
        }

        let pos = if self.dict_pos > dist {
            self.dict_pos - dist - 1
        } else {
            self.dict_size as usize - (dist - self.dict_pos) - 1
        };
        self.dictionary[pos]
    }

    /// Decode a literal byte.
    fn decode_literal<R: Read>(
        &mut self,
        rc: &mut RangeDecoder<R>,
        prev_byte: u8,
        match_byte: u8,
        bytes_decoded: u64,
    ) -> Result<u8> {
        let props = self
            .props
            .ok_or_else(|| OxiArcError::corrupted(0, "LZMA properties not initialized"))?;
        let model = self
            .model
            .as_mut()
            .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;

        let lit_state = model
            .literal
            .get_state(bytes_decoded, prev_byte, props.lc, props.lp);

        if self.state.is_literal() {
            // Normal literal
            let mut symbol = 1usize;
            loop {
                let bit = rc.decode_bit(&mut model.literal.probs[lit_state][symbol])?;
                symbol = (symbol << 1) | bit as usize;
                if symbol >= 0x100 {
                    break;
                }
            }
            Ok((symbol - 0x100) as u8)
        } else {
            // Literal with match context
            let mut symbol = 1usize;
            let mut match_byte = match_byte as usize;

            loop {
                let match_bit = (match_byte >> 7) & 1;
                match_byte <<= 1;

                let prob_idx = 0x100 + (match_bit << 8) + symbol;
                let bit = rc.decode_bit(&mut model.literal.probs[lit_state][prob_idx])?;
                symbol = (symbol << 1) | bit as usize;

                if symbol >= 0x100 {
                    break;
                }

                if bit as usize != match_bit {
                    // Mismatch, continue without match context
                    while symbol < 0x100 {
                        let bit = rc.decode_bit(&mut model.literal.probs[lit_state][symbol])?;
                        symbol = (symbol << 1) | bit as usize;
                    }
                    break;
                }
            }
            Ok((symbol - 0x100) as u8)
        }
    }

    /// Decode a distance.
    fn decode_distance<R: Read>(&mut self, rc: &mut RangeDecoder<R>, len: u32) -> Result<u32> {
        let model = self
            .model
            .as_mut()
            .ok_or_else(|| OxiArcError::corrupted(0, "LZMA model not initialized"))?;
        let len_state = ((len - MATCH_LEN_MIN as u32).min(3)) as usize;

        // Decode distance slot
        let slot = decode_bit_tree(rc, &mut model.distance.slot[len_state], 6)?;

        if slot < 4 {
            return Ok(slot);
        }

        let num_direct_bits = ((slot >> 1) - 1) as u32;
        let mut dist = (2 | (slot & 1)) << num_direct_bits;

        if slot < END_POS_MODEL_INDEX as u32 {
            let base_idx = (slot as usize) - (slot as usize >> 1) - 1;

            let mut result = 0u32;
            let mut m = 1usize;

            for i in 0..num_direct_bits {
                let bit = rc.decode_bit(&mut model.distance.special[base_idx + m - 1])?;
                m = (m << 1) | bit as usize;
                result |= bit << i;
            }

            dist += result;
        } else {
            let num_align_bits = DIST_ALIGN_BITS;
            let num_direct = num_direct_bits - num_align_bits;

            let direct = rc.decode_direct_bits(num_direct)?;
            dist += direct << num_align_bits;

            let align = rc.decode_bit_tree_reverse(&mut model.distance.align, num_align_bits)?;
            dist += align;
        }

        Ok(dist)
    }

    /// Copy bytes from dictionary to output.
    fn copy_from_dict(
        &self,
        output: &mut Vec<u8>,
        dist: usize,
        len: usize,
        _current_len: u64,
    ) -> Result<()> {
        // Copy from output buffer - dist is 0-indexed from the end
        // dist=0 means copy from the last byte written
        for _ in 0..len {
            let out_len = output.len();
            let byte = if dist < out_len {
                // Copy from within current output
                output[out_len - dist - 1]
            } else {
                // From external dictionary (shouldn't happen often in LZMA2)
                self.get_byte_from_dict(dist - out_len, 0)
            };
            output.push(byte);
        }
        Ok(())
    }

    /// Update the dictionary with new data.
    fn update_dictionary(&mut self, data: &[u8]) {
        let dict_capacity = self.dict_size as usize;

        for &byte in data {
            self.dictionary[self.dict_pos] = byte;
            self.dict_pos = (self.dict_pos + 1) % dict_capacity;
            if self.dict_len < dict_capacity {
                self.dict_len += 1;
            }
        }
    }

    /// Check if decoding is finished.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

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
        let len = decode_bit_tree(rc, &mut len_model.low[pos_state], LEN_LOW_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32)
    } else if rc.decode_bit(&mut len_model.choice2)? == 0 {
        let len = decode_bit_tree(rc, &mut len_model.mid[pos_state], LEN_MID_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32 + (1 << LEN_LOW_BITS))
    } else {
        let len = decode_bit_tree(rc, &mut len_model.high, LEN_HIGH_BITS)?;
        Ok(len + MATCH_LEN_MIN as u32 + (1 << LEN_LOW_BITS) + (1 << LEN_MID_BITS))
    }
}

/// LZMA2 encoder.
pub struct Lzma2Encoder {
    /// Compression level.
    #[allow(dead_code)]
    level: LzmaLevel,
    /// Dictionary size.
    dict_size: u32,
}

impl Lzma2Encoder {
    /// Create a new LZMA2 encoder.
    pub fn new(level: LzmaLevel) -> Self {
        Self {
            level,
            dict_size: level.dict_size(),
        }
    }

    /// Encode data to LZMA2 format.
    pub fn encode(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        if data.is_empty() {
            // Empty stream - just end marker
            output.push(0x00);
            return Ok(output);
        }

        // For simplicity, encode all data in a single LZMA chunk
        // A full implementation would split into multiple chunks

        // Create encoder to get properties
        let encoder = LzmaEncoder::new(self.level, self.dict_size);
        let props = encoder.properties();

        // Compress with LZMA
        let compressed = encoder.compress(data)?;

        // Check if compression is worthwhile
        if compressed.len() >= data.len() {
            // Use uncompressed chunk
            self.write_uncompressed_chunk(&mut output, data, true)?;
        } else {
            // Use LZMA compressed chunk
            self.write_lzma_chunk(&mut output, data.len(), &compressed, props, true)?;
        }

        // End marker
        output.push(0x00);

        Ok(output)
    }

    /// Write an uncompressed chunk.
    fn write_uncompressed_chunk<W: Write>(
        &self,
        writer: &mut W,
        data: &[u8],
        reset_dict: bool,
    ) -> Result<()> {
        // Control byte
        let control = if reset_dict { 0x01 } else { 0x02 };
        writer.write_all(&[control])?;

        // Size (big-endian, minus 1)
        let size = (data.len() - 1) as u16;
        writer.write_all(&size.to_be_bytes())?;

        // Data
        writer.write_all(data)?;

        Ok(())
    }

    /// Write an LZMA compressed chunk.
    fn write_lzma_chunk<W: Write>(
        &self,
        writer: &mut W,
        uncompressed_size: usize,
        compressed: &[u8],
        props: LzmaProperties,
        new_props: bool,
    ) -> Result<()> {
        // Control byte: 0x80 + flags + high bits of uncompressed size
        let reset_dict = true; // First chunk always resets
        let reset_state = true;

        let mut control = 0x80u8;
        if reset_dict {
            control |= 0x20;
        }
        if reset_state || new_props {
            control |= 0x40;
        }

        // Add high 5 bits of (uncompressed_size - 1)
        let uncompressed_minus_1 = uncompressed_size - 1;
        control |= ((uncompressed_minus_1 >> 16) & 0x1F) as u8;

        writer.write_all(&[control])?;

        // Uncompressed size low 16 bits
        let uncompressed_lo = (uncompressed_minus_1 & 0xFFFF) as u16;
        writer.write_all(&uncompressed_lo.to_be_bytes())?;

        // Compressed size (minus 1)
        let compressed_size = (compressed.len() - 1) as u16;
        writer.write_all(&compressed_size.to_be_bytes())?;

        // Properties byte if new
        if new_props {
            writer.write_all(&[props.to_byte()])?;
        }

        // Compressed data
        writer.write_all(compressed)?;

        Ok(())
    }

    /// Get the dictionary size for this encoder.
    pub fn dict_size(&self) -> u32 {
        self.dict_size
    }
}

/// Decode LZMA2 data.
pub fn decode_lzma2(data: &[u8], dict_size: u32) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(data);
    let mut decoder = Lzma2Decoder::new(dict_size);
    decoder.decode(&mut cursor)
}

/// Encode data to LZMA2 format.
pub fn encode_lzma2(data: &[u8], level: LzmaLevel) -> Result<Vec<u8>> {
    let encoder = Lzma2Encoder::new(level);
    encoder.encode(data)
}

/// Get dictionary size from LZMA2 properties byte.
///
/// Formula: `(2 | (props & 1)) << (props / 2 + 11)`
pub fn dict_size_from_props(props: u8) -> u32 {
    if props > 40 {
        return 0xFFFF_FFFF; // Invalid
    }

    if props == 40 {
        return 0xFFFF_FFFF; // Max
    }

    // Size = (2 | (props & 1)) << (props / 2 + 11)
    let base = 2 | (props & 1);
    let shift = (props / 2) + 11;
    (base as u32) << shift
}

/// Encode dictionary size to LZMA2 properties byte.
pub fn props_from_dict_size(dict_size: u32) -> u8 {
    // Find the smallest properties byte that gives at least dict_size
    for props in 0..=40 {
        if dict_size_from_props(props) >= dict_size {
            return props;
        }
    }
    40 // Max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dict_size_props() {
        // Test some known values based on formula: (2 | (props & 1)) << (props / 2 + 11)
        assert_eq!(dict_size_from_props(0), 2 << 11); // 4 KB
        assert_eq!(dict_size_from_props(1), 3 << 11); // 6 KB
        assert_eq!(dict_size_from_props(2), 2 << 12); // 8 KB
        assert_eq!(dict_size_from_props(3), 3 << 12); // 12 KB
        assert_eq!(dict_size_from_props(14), 2 << 18); // 512 KB
        assert_eq!(dict_size_from_props(15), 3 << 18); // 768 KB
    }

    #[test]
    fn test_props_roundtrip() {
        for size in [4096, 8192, 65536, 1 << 20, 1 << 24] {
            let props = props_from_dict_size(size);
            let decoded = dict_size_from_props(props);
            assert!(
                decoded >= size,
                "props {} gave {} < {}",
                props,
                decoded,
                size
            );
        }
    }

    #[test]
    fn test_lzma2_empty() {
        let original: &[u8] = b"";
        let encoded = encode_lzma2(original, LzmaLevel::DEFAULT).unwrap();
        assert_eq!(encoded, vec![0x00]); // Just end marker
    }

    #[test]
    fn test_lzma2_uncompressed_roundtrip() {
        // Test with small data that won't compress well
        let original = b"ABCD";
        let encoded = encode_lzma2(original, LzmaLevel::FAST).unwrap();
        let decoded = decode_lzma2(&encoded, 4096).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_lzma2_compressed_roundtrip() {
        // Test with repeating data that compresses well
        let original: Vec<u8> = vec![b'A'; 1000];
        let encoded = encode_lzma2(&original, LzmaLevel::DEFAULT).unwrap();
        let decoded = decode_lzma2(&encoded, 1 << 20).unwrap();
        assert_eq!(decoded, original);
    }
}
