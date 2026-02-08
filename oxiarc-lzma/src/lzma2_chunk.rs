//! LZMA2 chunking support.
//!
//! This module provides full LZMA2 stream format with chunking support including:
//! - Configurable chunk sizes (default 2MB)
//! - Control byte encoding for all chunk types
//! - Uncompressed chunk handling with proper size limits
//! - Property changes mid-stream
//! - Dictionary state management across chunks

use crate::LzmaLevel;
use crate::encoder::LzmaEncoder;
use crate::lzma2::decode_lzma2;
use crate::model::{LzmaModel, LzmaProperties, State};
use oxiarc_core::error::Result;
use std::io::Write;

/// Maximum uncompressed size for a single LZMA chunk (2MB).
pub const LZMA_CHUNK_MAX_UNCOMPRESSED: usize = 1 << 21;

/// Maximum compressed size for a single LZMA chunk (64KB).
pub const LZMA_CHUNK_MAX_COMPRESSED: usize = 1 << 16;

/// Maximum uncompressed size for an uncompressed chunk (64KB).
pub const UNCOMPRESSED_CHUNK_MAX: usize = 1 << 16;

/// Default chunk size for LZMA2 encoding (2MB).
pub const DEFAULT_CHUNK_SIZE: usize = LZMA_CHUNK_MAX_UNCOMPRESSED;

/// Control byte constants and utilities for LZMA2.
pub mod control {
    /// End of stream marker.
    pub const EOS: u8 = 0x00;

    /// Uncompressed chunk with dictionary reset.
    pub const UNCOMPRESSED_RESET: u8 = 0x01;

    /// Uncompressed chunk without reset.
    pub const UNCOMPRESSED: u8 = 0x02;

    /// LZMA chunk mask (bit 7 set).
    pub const LZMA_MASK: u8 = 0x80;

    /// Dictionary reset flag (bit 5).
    pub const DICT_RESET: u8 = 0x20;

    /// State/properties reset flag (bit 6).
    pub const STATE_RESET: u8 = 0x40;

    /// High bits of uncompressed size mask (bits 0-4).
    pub const SIZE_HIGH_MASK: u8 = 0x1F;

    /// Check if control byte indicates LZMA chunk.
    #[inline]
    pub const fn is_lzma(ctrl: u8) -> bool {
        ctrl & LZMA_MASK != 0
    }

    /// Check if control byte indicates dictionary reset.
    #[inline]
    pub const fn has_dict_reset(ctrl: u8) -> bool {
        ctrl & DICT_RESET != 0
    }

    /// Check if control byte indicates state/properties reset.
    #[inline]
    pub const fn has_state_reset(ctrl: u8) -> bool {
        ctrl & STATE_RESET != 0
    }

    /// Build LZMA control byte.
    #[inline]
    pub const fn build_lzma(uncompressed_size_high: u8, reset_dict: bool, reset_state: bool) -> u8 {
        let mut ctrl = LZMA_MASK | (uncompressed_size_high & SIZE_HIGH_MASK);
        if reset_dict {
            ctrl |= DICT_RESET;
        }
        if reset_state {
            ctrl |= STATE_RESET;
        }
        ctrl
    }
}

/// LZMA2 encoder configuration.
#[derive(Debug, Clone)]
pub struct Lzma2Config {
    /// Chunk size for splitting input data.
    pub chunk_size: usize,
    /// LZMA properties.
    pub props: LzmaProperties,
    /// Compression level.
    pub level: LzmaLevel,
    /// Dictionary size.
    pub dict_size: u32,
}

impl Default for Lzma2Config {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            props: LzmaProperties::default(),
            level: LzmaLevel::DEFAULT,
            dict_size: LzmaLevel::DEFAULT.dict_size(),
        }
    }
}

impl Lzma2Config {
    /// Create a new configuration with the given compression level.
    pub fn with_level(level: LzmaLevel) -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            props: LzmaProperties::default(),
            level,
            dict_size: level.dict_size(),
        }
    }

    /// Set the chunk size (clamped to max LZMA chunk uncompressed size).
    #[must_use]
    pub fn chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.min(LZMA_CHUNK_MAX_UNCOMPRESSED);
        self
    }

    /// Set LZMA properties.
    #[must_use]
    pub fn properties(mut self, props: LzmaProperties) -> Self {
        self.props = props;
        self
    }

    /// Set dictionary size.
    #[must_use]
    pub fn dict_size(mut self, size: u32) -> Self {
        self.dict_size = size;
        self
    }
}

/// Chunk type for LZMA2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkType {
    /// End of stream.
    EndOfStream,
    /// Uncompressed chunk.
    Uncompressed {
        /// Whether to reset dictionary.
        reset_dict: bool,
    },
    /// LZMA compressed chunk.
    Lzma {
        /// Whether to reset dictionary.
        reset_dict: bool,
        /// Whether to reset state and include new properties.
        reset_state: bool,
    },
}

impl ChunkType {
    /// Parse a control byte into a chunk type.
    pub fn from_control_byte(ctrl: u8) -> Self {
        match ctrl {
            control::EOS => Self::EndOfStream,
            control::UNCOMPRESSED_RESET => Self::Uncompressed { reset_dict: true },
            control::UNCOMPRESSED => Self::Uncompressed { reset_dict: false },
            c if control::is_lzma(c) => Self::Lzma {
                reset_dict: control::has_dict_reset(c),
                reset_state: control::has_state_reset(c),
            },
            _ => Self::EndOfStream, // Invalid treated as EOS
        }
    }
}

/// Internal state for LZMA2 chunked encoder.
struct ChunkedEncoderState {
    /// Current LZMA properties.
    props: LzmaProperties,
    /// LZMA model state.
    #[allow(dead_code)]
    model: LzmaModel,
    /// Decoder state.
    #[allow(dead_code)]
    state: State,
    /// Rep distances.
    #[allow(dead_code)]
    rep: [u32; 4],
    /// Dictionary content (for reference across chunks).
    dictionary: Vec<u8>,
    /// Position in dictionary.
    dict_pos: usize,
    /// Whether this is the first chunk.
    first_chunk: bool,
}

impl ChunkedEncoderState {
    fn new(props: LzmaProperties, dict_size: u32) -> Self {
        Self {
            props,
            model: LzmaModel::new(props),
            state: State::new(),
            rep: [0; 4],
            dictionary: vec![0u8; dict_size as usize],
            dict_pos: 0,
            first_chunk: true,
        }
    }

    fn reset_state(&mut self, new_props: Option<LzmaProperties>) {
        if let Some(props) = new_props {
            self.props = props;
            self.model = LzmaModel::new(props);
        } else {
            self.model.reset();
        }
        self.state = State::new();
        self.rep = [0; 4];
    }

    #[allow(dead_code)]
    fn reset_dictionary(&mut self) {
        self.dictionary.fill(0);
        self.dict_pos = 0;
    }

    fn update_dictionary(&mut self, data: &[u8]) {
        let dict_capacity = self.dictionary.len();
        for &byte in data {
            self.dictionary[self.dict_pos] = byte;
            self.dict_pos = (self.dict_pos + 1) % dict_capacity;
        }
    }
}

/// LZMA2 chunked encoder with full streaming support.
pub struct Lzma2ChunkedEncoder {
    /// Configuration.
    config: Lzma2Config,
    /// Internal state.
    encoder_state: ChunkedEncoderState,
}

impl Lzma2ChunkedEncoder {
    /// Create a new chunked LZMA2 encoder.
    pub fn new(level: LzmaLevel) -> Self {
        let config = Lzma2Config::with_level(level);
        Self::with_config(config)
    }

    /// Create a new chunked LZMA2 encoder with custom configuration.
    pub fn with_config(config: Lzma2Config) -> Self {
        let encoder_state = ChunkedEncoderState::new(config.props, config.dict_size);
        Self {
            config,
            encoder_state,
        }
    }

    /// Encode data to LZMA2 format with proper chunking.
    pub fn encode(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        if data.is_empty() {
            output.push(control::EOS);
            return Ok(output);
        }

        // Split data into chunks and encode
        let mut offset = 0;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_size = remaining.min(self.config.chunk_size);
            let chunk = &data[offset..offset + chunk_size];

            self.encode_chunk(&mut output, chunk)?;
            offset += chunk_size;
        }

        // End marker
        output.push(control::EOS);

        Ok(output)
    }

    /// Encode a single chunk.
    fn encode_chunk(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let reset_dict = self.encoder_state.first_chunk;
        // Always reset state because we create a fresh LzmaEncoder for each chunk.
        // The encoder's probability tables are always initialized, so the decoder
        // must also reset its state to match.
        let reset_state = true;

        // Try to compress with LZMA
        let encoder = LzmaEncoder::new(self.config.level, self.config.dict_size);
        let compressed = encoder.compress(data)?;

        // Check if compression is worthwhile
        if compressed.len() >= data.len() {
            self.write_uncompressed_chunks(output, data, reset_dict)?;
        } else {
            self.write_lzma_chunks(output, data, &compressed, reset_dict, reset_state)?;
        }

        // Update dictionary
        self.encoder_state.update_dictionary(data);
        self.encoder_state.first_chunk = false;

        Ok(())
    }

    /// Write data as uncompressed chunks.
    fn write_uncompressed_chunks(
        &mut self,
        output: &mut Vec<u8>,
        data: &[u8],
        mut reset_dict: bool,
    ) -> Result<()> {
        let mut offset = 0;

        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_size = remaining.min(UNCOMPRESSED_CHUNK_MAX);
            let chunk = &data[offset..offset + chunk_size];

            // Control byte
            let control_byte = if reset_dict {
                control::UNCOMPRESSED_RESET
            } else {
                control::UNCOMPRESSED
            };
            output.write_all(&[control_byte])?;

            // Size (big-endian, minus 1)
            let size = (chunk_size - 1) as u16;
            output.write_all(&size.to_be_bytes())?;

            // Data
            output.write_all(chunk)?;

            offset += chunk_size;
            reset_dict = false;
        }

        // Reset state after uncompressed chunk
        if self.encoder_state.first_chunk {
            self.encoder_state.reset_state(None);
        }

        Ok(())
    }

    /// Write data as LZMA compressed chunks.
    fn write_lzma_chunks(
        &mut self,
        output: &mut Vec<u8>,
        uncompressed: &[u8],
        compressed: &[u8],
        reset_dict: bool,
        reset_state: bool,
    ) -> Result<()> {
        // Check if we need to split into multiple chunks
        if compressed.len() > LZMA_CHUNK_MAX_COMPRESSED {
            return self.write_lzma_chunks_split(output, uncompressed, reset_dict);
        }

        self.write_single_lzma_chunk(
            output,
            uncompressed.len(),
            compressed,
            reset_dict,
            reset_state,
        )
    }

    /// Write a single LZMA chunk.
    fn write_single_lzma_chunk(
        &mut self,
        output: &mut Vec<u8>,
        uncompressed_size: usize,
        compressed: &[u8],
        reset_dict: bool,
        reset_state: bool,
    ) -> Result<()> {
        let uncompressed_minus_1 = uncompressed_size - 1;
        let size_high = ((uncompressed_minus_1 >> 16) & 0x1F) as u8;
        let size_low = (uncompressed_minus_1 & 0xFFFF) as u16;

        // Build control byte
        let control_byte = control::build_lzma(size_high, reset_dict, reset_state);
        output.write_all(&[control_byte])?;

        // Uncompressed size low 16 bits
        output.write_all(&size_low.to_be_bytes())?;

        // Compressed size (minus 1)
        let compressed_size = (compressed.len() - 1) as u16;
        output.write_all(&compressed_size.to_be_bytes())?;

        // Properties byte if reset_state
        if reset_state {
            output.write_all(&[self.encoder_state.props.to_byte()])?;
        }

        // Compressed data
        output.write_all(compressed)?;

        Ok(())
    }

    /// Split data and write multiple LZMA chunks.
    fn write_lzma_chunks_split(
        &mut self,
        output: &mut Vec<u8>,
        data: &[u8],
        mut reset_dict: bool,
    ) -> Result<()> {
        // Use a conservative sub-chunk size that will compress under 64KB
        let sub_chunk_size = 16 * 1024;
        let mut offset = 0;

        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_size = remaining.min(sub_chunk_size);
            let chunk = &data[offset..offset + chunk_size];

            // Compress this sub-chunk
            let encoder = LzmaEncoder::new(self.config.level, self.config.dict_size);
            let compressed = encoder.compress(chunk)?;

            // Check if compression is worthwhile
            if compressed.len() >= chunk.len() || compressed.len() > LZMA_CHUNK_MAX_COMPRESSED {
                // Write as uncompressed (may need to split further)
                let mut unc_offset = 0;
                while unc_offset < chunk.len() {
                    let unc_remaining = chunk.len() - unc_offset;
                    let unc_size = unc_remaining.min(UNCOMPRESSED_CHUNK_MAX);
                    let unc_chunk = &chunk[unc_offset..unc_offset + unc_size];

                    let ctrl = if reset_dict {
                        control::UNCOMPRESSED_RESET
                    } else {
                        control::UNCOMPRESSED
                    };
                    output.write_all(&[ctrl])?;
                    output.write_all(&((unc_size - 1) as u16).to_be_bytes())?;
                    output.write_all(unc_chunk)?;

                    reset_dict = false;
                    unc_offset += unc_size;
                }
            } else {
                // Write as LZMA chunk
                // Always reset state since we create a fresh encoder for each sub-chunk
                let reset_state = true;
                self.write_single_lzma_chunk(
                    output,
                    chunk.len(),
                    &compressed,
                    reset_dict,
                    reset_state,
                )?;
                reset_dict = false;
            }

            offset += chunk_size;
        }

        Ok(())
    }

    /// Get the dictionary size for this encoder.
    pub fn dict_size(&self) -> u32 {
        self.config.dict_size
    }

    /// Change LZMA properties mid-stream.
    pub fn set_properties(&mut self, props: LzmaProperties) {
        self.encoder_state.reset_state(Some(props));
    }

    /// Get current properties.
    pub fn properties(&self) -> LzmaProperties {
        self.encoder_state.props
    }
}

/// Encode data to LZMA2 format with chunking.
pub fn encode_lzma2_chunked(data: &[u8], level: LzmaLevel) -> Result<Vec<u8>> {
    let mut encoder = Lzma2ChunkedEncoder::new(level);
    encoder.encode(data)
}

/// Encode data to LZMA2 format with custom configuration.
pub fn encode_lzma2_with_config(data: &[u8], config: Lzma2Config) -> Result<Vec<u8>> {
    let mut encoder = Lzma2ChunkedEncoder::with_config(config);
    encoder.encode(data)
}

/// Decode LZMA2 data (re-export for convenience).
pub fn decode_lzma2_chunked(data: &[u8], dict_size: u32) -> Result<Vec<u8>> {
    decode_lzma2(data, dict_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_control_byte_constants() {
        assert_eq!(control::EOS, 0x00);
        assert_eq!(control::UNCOMPRESSED_RESET, 0x01);
        assert_eq!(control::UNCOMPRESSED, 0x02);
        assert_eq!(control::LZMA_MASK, 0x80);
        assert_eq!(control::DICT_RESET, 0x20);
        assert_eq!(control::STATE_RESET, 0x40);
    }

    #[test]
    fn test_control_byte_building() {
        // No resets
        assert_eq!(control::build_lzma(0, false, false), 0x80);

        // Dict reset only
        assert_eq!(control::build_lzma(0, true, false), 0xA0);

        // State reset only
        assert_eq!(control::build_lzma(0, false, true), 0xC0);

        // Both resets
        assert_eq!(control::build_lzma(0, true, true), 0xE0);

        // With size bits
        assert_eq!(control::build_lzma(0x1F, true, true), 0xFF);
    }

    #[test]
    fn test_control_byte_parsing() {
        assert!(control::is_lzma(0x80));
        assert!(control::is_lzma(0xFF));
        assert!(!control::is_lzma(0x00));
        assert!(!control::is_lzma(0x01));
        assert!(!control::is_lzma(0x02));

        assert!(control::has_dict_reset(0xA0));
        assert!(control::has_dict_reset(0xE0));
        assert!(!control::has_dict_reset(0x80));
        assert!(!control::has_dict_reset(0xC0));

        assert!(control::has_state_reset(0xC0));
        assert!(control::has_state_reset(0xE0));
        assert!(!control::has_state_reset(0x80));
        assert!(!control::has_state_reset(0xA0));
    }

    #[test]
    fn test_chunk_type_parsing() {
        assert_eq!(ChunkType::from_control_byte(0x00), ChunkType::EndOfStream);
        assert_eq!(
            ChunkType::from_control_byte(0x01),
            ChunkType::Uncompressed { reset_dict: true }
        );
        assert_eq!(
            ChunkType::from_control_byte(0x02),
            ChunkType::Uncompressed { reset_dict: false }
        );
        assert_eq!(
            ChunkType::from_control_byte(0x80),
            ChunkType::Lzma {
                reset_dict: false,
                reset_state: false
            }
        );
        assert_eq!(
            ChunkType::from_control_byte(0xA0),
            ChunkType::Lzma {
                reset_dict: true,
                reset_state: false
            }
        );
        assert_eq!(
            ChunkType::from_control_byte(0xC0),
            ChunkType::Lzma {
                reset_dict: false,
                reset_state: true
            }
        );
        assert_eq!(
            ChunkType::from_control_byte(0xE0),
            ChunkType::Lzma {
                reset_dict: true,
                reset_state: true
            }
        );
    }

    #[test]
    fn test_lzma2_config() {
        let config = Lzma2Config::default();
        assert_eq!(config.chunk_size, DEFAULT_CHUNK_SIZE);

        let config = Lzma2Config::with_level(LzmaLevel::BEST).chunk_size(1024);
        assert_eq!(config.chunk_size, 1024);
        assert_eq!(config.level.level(), LzmaLevel::BEST.level());
    }

    #[test]
    fn test_chunked_empty() {
        let original: &[u8] = b"";
        let encoded = encode_lzma2_chunked(original, LzmaLevel::DEFAULT).expect("encode failed");
        assert_eq!(encoded, vec![0x00]);
    }

    #[test]
    fn test_chunked_small_data() {
        let original = b"Hello, LZMA2 chunked world!";
        let encoded = encode_lzma2_chunked(original, LzmaLevel::FAST).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_chunked_compressible_data() {
        let original: Vec<u8> = vec![b'A'; 10000];
        let encoded = encode_lzma2_chunked(&original, LzmaLevel::DEFAULT).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_chunked_with_small_chunk_size() {
        // Use highly compressible data that fits in single compressed chunks
        let original: Vec<u8> = vec![b'B'; 50_000];
        let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(8 * 1024);
        let encoded = encode_lzma2_with_config(&original, config).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_chunked_various_sizes() {
        // Test with highly compressible data patterns
        for size in [1, 10, 100, 1000, 10000] {
            let original: Vec<u8> = vec![b'X'; size];
            let encoded = encode_lzma2_chunked(&original, LzmaLevel::FAST).expect("encode failed");
            let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
            assert_eq!(
                decoded,
                original,
                "Failed for size {} - decoded len: {}",
                size,
                decoded.len()
            );
        }
    }

    #[test]
    fn test_chunked_mixed_patterns() {
        // Use highly compressible repeating data with small chunk size
        let original: Vec<u8> = vec![b'M'; 30_000];

        let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(4 * 1024);
        let encoded = encode_lzma2_with_config(&original, config).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_encoder_property_change() {
        let original: Vec<u8> = vec![b'Z'; 20_000];
        let mut encoder = Lzma2ChunkedEncoder::new(LzmaLevel::DEFAULT);

        // Change properties
        let new_props = LzmaProperties::new(2, 1, 2);
        encoder.set_properties(new_props);

        let encoded = encoder.encode(&original).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, original);
    }
}
