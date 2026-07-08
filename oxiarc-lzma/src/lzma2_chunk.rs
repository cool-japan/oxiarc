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
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
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

// `LzmaProperties` (defined in the internal `model` module) does not derive
// `PartialEq`/`Eq`, so a plain `#[derive(PartialEq, Eq)]` on `Lzma2Config`
// would not compile. Compare `props` structurally via its encoded byte
// (`lc`/`lp`/`pb` round-trip losslessly through `to_byte`/`from_byte`) so
// config values can still be compared in tests without touching `model.rs`.
impl PartialEq for Lzma2Config {
    fn eq(&self, other: &Self) -> bool {
        self.chunk_size == other.chunk_size
            && self.props.to_byte() == other.props.to_byte()
            && self.level == other.level
            && self.dict_size == other.dict_size
    }
}

impl Eq for Lzma2Config {}

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
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`Lzma2ChunkedEncoder::with_progress`] / [`Lzma2ChunkedEncoder::with_cancel`] builders.
pub struct Lzma2ChunkedEncoder {
    /// Configuration.
    config: Lzma2Config,
    /// Internal state.
    encoder_state: ChunkedEncoderState,
    /// Optional progress sink.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token.
    cancel: Option<CancellationToken>,
    /// Cumulative uncompressed bytes encoded so far.
    bytes_processed: u64,
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
            progress: None,
            cancel: None,
            bytes_processed: 0,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(cumulative_bytes, None)` is called after each
    /// chunk is encoded. `on_finish()` is called after the end-of-stream marker.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each chunk is encoded.
    /// If cancelled, returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Encode data to LZMA2 format with proper chunking.
    pub fn encode(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        if data.is_empty() {
            output.push(control::EOS);
            if let Some(ref handle) = self.progress {
                handle.on_progress(0, None);
                handle.on_finish();
            }
            return Ok(output);
        }

        // Split data into chunks and encode
        let mut offset = 0;
        while offset < data.len() {
            // Cooperative cancellation check before each chunk.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            let remaining = data.len() - offset;
            let chunk_size = remaining.min(self.config.chunk_size);
            let chunk = &data[offset..offset + chunk_size];

            self.encode_chunk(&mut output, chunk)?;
            offset += chunk_size;
            self.bytes_processed += chunk_size as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(self.bytes_processed, None);
            }
        }

        // End marker
        output.push(control::EOS);

        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }

        Ok(output)
    }

    /// Encode a single chunk.
    fn encode_chunk(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // Every chunk is compressed with a fresh `LzmaEncoder` that carries no
        // history from prior chunks, so each chunk must reset BOTH the decoder's
        // state AND its dictionary. Resetting only the state (leaving the
        // dictionary intact for chunks after the first) makes the decoder seed
        // the literal-coder context (`prev_byte` / `match_byte`) from the tail of
        // the previous chunk, while the encoder used an empty history — the two
        // sides then update different literal-probability entries and the range
        // coder desynchronizes on the first colliding literal. Repeated-byte
        // payloads happen to survive because their single per-chunk literal is
        // always decoded from a still-pristine (0.5) probability table, so which
        // table index is touched is immaterial; varied data crossing a chunk
        // boundary corrupts as soon as two literals collide.
        let reset_dict = true;
        let reset_state = true;

        // Try to compress with LZMA (chunk payload: no end-of-stream marker,
        // since the chunk header carries the exact sizes)
        let encoder = LzmaEncoder::new(self.config.level, self.config.dict_size);
        let compressed = encoder.compress_chunk(data)?;

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
            return self.write_lzma_chunks_split(output, uncompressed);
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
    ///
    /// Each sub-chunk is compressed with its own fresh [`LzmaEncoder`] (empty
    /// history), so — exactly like [`Self::encode_chunk`] — every sub-chunk must
    /// reset the decoder's dictionary as well as its state. The one exception is
    /// when a single incompressible sub-chunk is itself split across several
    /// 64 KiB uncompressed chunks: only the first of those pieces resets the
    /// dictionary, because the later pieces continue the same verbatim run and
    /// must not wipe the bytes just written.
    fn write_lzma_chunks_split(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        // Use a conservative sub-chunk size that will compress under 64KB
        let sub_chunk_size = 16 * 1024;
        let mut offset = 0;

        while offset < data.len() {
            let remaining = data.len() - offset;
            let chunk_size = remaining.min(sub_chunk_size);
            let chunk = &data[offset..offset + chunk_size];

            // Compress this sub-chunk (chunk payload: no end-of-stream marker)
            let encoder = LzmaEncoder::new(self.config.level, self.config.dict_size);
            let compressed = encoder.compress_chunk(chunk)?;

            // Check if compression is worthwhile
            if compressed.len() >= chunk.len() || compressed.len() > LZMA_CHUNK_MAX_COMPRESSED {
                // Write as uncompressed (may need to split further). Reset the
                // dictionary on the first piece only; subsequent pieces continue
                // the same verbatim run.
                let mut unc_offset = 0;
                let mut reset_dict = true;
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
                // Write as LZMA chunk. The sub-chunk was compressed with a fresh
                // encoder, so reset both dictionary and state.
                self.write_single_lzma_chunk(output, chunk.len(), &compressed, true, true)?;
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

    /// Deterministic pseudo-varied bytes via a byte LCG (no `rand`).
    ///
    /// Produces non-repetitive, literal-heavy data so the LZMA2 chunk stream
    /// actually exercises the per-chunk literal-coder context. Repeated-byte
    /// payloads (`vec![b; n]`) cannot reproduce the cross-chunk desync this
    /// guards against — see the note on [`Lzma2ChunkedEncoder::encode_chunk`].
    fn varied_bytes(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..len {
            // Numerical Recipes LCG constants.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.push(0x20u8.wrapping_add(((state >> 16) as u8) % 0x5f));
        }
        out
    }

    #[test]
    fn test_varied_data_across_multiple_small_chunks() {
        // Regression: varied (non-repetitive) data crossing several small chunk
        // boundaries must round-trip byte-for-byte. Before the dictionary-reset
        // fix this failed with "Invalid LZMA data" once two literals collided.
        let data = varied_bytes(300 * 1024);
        let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(4 * 1024);
        let encoded = encode_lzma2_with_config(&data, config).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, data, "varied multi-chunk round-trip mismatch");
    }

    #[test]
    fn test_varied_data_multiple_chunk_sizes() {
        // Exercise a range of small chunk sizes so the boundary is crossed a
        // varying number of times, including many crossings.
        let data = varied_bytes(64 * 1024);
        for chunk in [512usize, 1024, 3000, 7000, 20_000] {
            let config = Lzma2Config::with_level(LzmaLevel::FAST).chunk_size(chunk);
            let encoded = encode_lzma2_with_config(&data, config).expect("encode failed");
            let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
            assert_eq!(decoded, data, "mismatch at chunk size {chunk}");
        }
    }

    #[test]
    fn test_varied_data_default_chunk_over_2mb() {
        // Regression: a >2 MiB varied input routed through the DEFAULT chunk
        // path (crate::lzma2::encode_lzma2 -> encode_chunked) must round-trip.
        let data = varied_bytes(3 * 1024 * 1024 + 777);
        let encoded = crate::lzma2::encode_lzma2(&data, LzmaLevel::DEFAULT).expect("encode failed");
        let decoded = crate::lzma2::decode_lzma2(&encoded, 1 << 24).expect("decode failed");
        assert_eq!(decoded, data, "varied default-chunk round-trip mismatch");
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

    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::progress::ProgressSink;
    use std::sync::{Arc, Mutex};

    type ProgressLog = Arc<Mutex<Vec<(u64, Option<u64>)>>>;

    struct MockSink(ProgressLog);

    impl ProgressSink for MockSink {
        fn on_progress(&self, processed: u64, total: Option<u64>) {
            self.0
                .lock()
                .expect("lock poisoned")
                .push((processed, total));
        }
    }

    fn make_compressible_data(size: usize) -> Vec<u8> {
        // Use highly compressible repeating data for fast LZMA tests.
        vec![b'B'; size]
    }

    #[test]
    fn test_lzma2_chunked_encoder_progress_reports() {
        // Use small data with FAST level and small chunk size for quick test.
        let data = make_compressible_data(8 * 1024); // 8 KB

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let config = Lzma2Config::with_level(LzmaLevel::FAST).chunk_size(4 * 1024);
        let mut encoder = Lzma2ChunkedEncoder::with_config(config)
            .with_progress(sink as oxiarc_core::progress::ProgressHandle);
        encoder.encode(&data).expect("encode failed");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
        let (last_processed, _) = *recorded.last().expect("non-empty");
        assert_eq!(
            last_processed,
            data.len() as u64,
            "final processed count must equal input size"
        );
    }

    #[test]
    fn test_lzma2_chunked_encoder_cancel_aborts() {
        let data = make_compressible_data(8 * 1024);
        let token = CancellationToken::new();

        let config = Lzma2Config::with_level(LzmaLevel::FAST).chunk_size(1024);
        let mut encoder = Lzma2ChunkedEncoder::with_config(config).with_cancel(token.clone());

        token.cancel();
        let result = encoder.encode(&data);
        assert!(result.is_err(), "expected cancellation error");
    }
}
