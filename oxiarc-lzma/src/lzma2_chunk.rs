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
///
/// Bits 5-6 of an LZMA chunk's control byte form a 2-bit *reset field*
/// (LZMA2 spec):
///
/// | field | meaning                                            |
/// |-------|----------------------------------------------------|
/// | 0     | no reset (continuation chunk)                      |
/// | 1     | state reset                                        |
/// | 2     | state reset + new properties byte follows          |
/// | 3     | state reset + new properties + dictionary reset    |
pub mod control {
    /// End of stream marker.
    pub const EOS: u8 = 0x00;

    /// Uncompressed chunk with dictionary reset.
    pub const UNCOMPRESSED_RESET: u8 = 0x01;

    /// Uncompressed chunk without reset.
    pub const UNCOMPRESSED: u8 = 0x02;

    /// LZMA chunk mask (bit 7 set).
    pub const LZMA_MASK: u8 = 0x80;

    /// Reset-field bit meaning "dictionary reset" *when combined with*
    /// [`STATE_RESET`] (field value 3). On its own (field value 1) it means
    /// "state reset without new properties" — see the module docs.
    pub const DICT_RESET: u8 = 0x20;

    /// Reset-field bit meaning "state reset + new properties byte follows"
    /// (field values 2 and 3).
    pub const STATE_RESET: u8 = 0x40;

    /// High bits of uncompressed size mask (bits 0-4).
    pub const SIZE_HIGH_MASK: u8 = 0x1F;

    /// Extract the 2-bit reset field (0-3) from an LZMA chunk control byte.
    #[inline]
    pub const fn reset_field(ctrl: u8) -> u8 {
        (ctrl >> 5) & 0x3
    }

    /// Check if control byte indicates LZMA chunk.
    #[inline]
    pub const fn is_lzma(ctrl: u8) -> bool {
        ctrl & LZMA_MASK != 0
    }

    /// Check if control byte indicates a dictionary reset (reset field 3).
    #[inline]
    pub const fn has_dict_reset(ctrl: u8) -> bool {
        reset_field(ctrl) == 3
    }

    /// Check if control byte indicates a state reset (reset field >= 1).
    #[inline]
    pub const fn has_state_reset(ctrl: u8) -> bool {
        reset_field(ctrl) >= 1
    }

    /// Check if a properties byte follows the chunk header (reset field >= 2).
    #[inline]
    pub const fn has_new_props(ctrl: u8) -> bool {
        reset_field(ctrl) >= 2
    }

    /// Build LZMA control byte.
    ///
    /// A dictionary reset always implies a state reset and new properties
    /// (reset field 3) — LZMA2 cannot express a dictionary reset alone, so
    /// `reset_dict = true` produces field 3 regardless of `reset_state`.
    /// `reset_state = true` alone produces field 2 (state reset + new
    /// properties); callers must then emit the properties byte.
    #[inline]
    pub const fn build_lzma(uncompressed_size_high: u8, reset_dict: bool, reset_state: bool) -> u8 {
        let mut ctrl = LZMA_MASK | (uncompressed_size_high & SIZE_HIGH_MASK);
        if reset_dict {
            ctrl |= DICT_RESET | STATE_RESET;
        } else if reset_state {
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

/// Entropy-coder state carried between LZMA2 chunks (probability model,
/// LZMA state-machine state, rep distances).
struct CarriedState {
    model: LzmaModel,
    state: State,
    rep: [u32; 4],
}

/// Outcome of attempting to emit one input piece as a stateful LZMA chunk.
enum ChunkAttempt {
    /// The chunk was written and the persistent state advanced.
    Emitted,
    /// Compression did not shrink the piece; store it verbatim instead.
    Incompressible,
    /// The compressed payload overflowed the 16-bit chunk size field; the
    /// piece must be re-encoded as several smaller chunks.
    Overflow,
}

/// Internal state for the stateful LZMA2 chunked encoder.
///
/// Mirrors the persistent state of [`crate::Lzma2Decoder`]: the sliding
/// window (dictionary), the entropy-coder state, and the global uncompressed
/// position all survive chunk boundaries, so continuation chunks (reset
/// field 0) can be emitted exactly like liblzma does.
struct ChunkedEncoderState {
    /// Current LZMA properties.
    props: LzmaProperties,
    /// Entropy state carried into the next LZMA chunk. `None` when the next
    /// LZMA chunk must reset the state (stream start, after an uncompressed
    /// chunk, or after a mid-stream properties change).
    carry: Option<CarriedState>,
    /// Sliding window of the most recently encoded bytes (up to the
    /// dictionary size) — the decoder-visible history for back-references
    /// and literal contexts.
    window: Vec<u8>,
    /// Global uncompressed position since the last dictionary reset.
    global_pos: u64,
    /// True until the first chunk is emitted; that chunk must reset the
    /// dictionary (LZMA2 spec).
    need_dict_reset: bool,
}

impl ChunkedEncoderState {
    fn new(props: LzmaProperties) -> Self {
        Self {
            props,
            carry: None,
            window: Vec::new(),
            global_pos: 0,
            need_dict_reset: true,
        }
    }

    /// Drop any carried entropy state so the next LZMA chunk starts from a
    /// fresh state (reset field >= 2), optionally switching properties.
    fn reset_state(&mut self, new_props: Option<LzmaProperties>) {
        if let Some(props) = new_props {
            self.props = props;
        }
        self.carry = None;
    }

    /// Record `data` as emitted: advance the global position and slide the
    /// window forward, keeping at most `window_cap` bytes of history.
    fn push_history(&mut self, data: &[u8], window_cap: usize) {
        self.global_pos += data.len() as u64;
        if data.len() >= window_cap {
            self.window.clear();
            self.window
                .extend_from_slice(&data[data.len() - window_cap..]);
        } else {
            self.window.extend_from_slice(data);
            if self.window.len() > window_cap {
                let excess = self.window.len() - window_cap;
                self.window.drain(..excess);
            }
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
        let encoder_state = ChunkedEncoderState::new(config.props);
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
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each chunk is encoded.
    /// If cancelled, returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    #[must_use]
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

    /// Effective sliding-window capacity (the encoder clamps its dictionary
    /// to at least 4 KiB, so the history we keep must match).
    fn window_cap(&self) -> usize {
        self.config.dict_size.max(4096) as usize
    }

    /// Build a per-chunk [`LzmaEncoder`] seeded with the persistent stream
    /// state, and the reset field its chunk header must carry.
    ///
    /// The window is passed as a preset dictionary (virtual prefix) so the
    /// match finder can emit back-references across chunk boundaries, and the
    /// entropy state — when carried — makes the chunk a pure continuation
    /// (reset field 0), matching liblzma's chunked encoder.
    fn make_chunk_encoder(&mut self) -> (u8, LzmaEncoder) {
        let mut encoder = LzmaEncoder::with_props(
            self.config.level,
            self.config.dict_size,
            self.encoder_state.props,
        );
        let st = &self.encoder_state;

        if st.need_dict_reset {
            // First chunk of the stream: everything resets (field 3).
            return (3, encoder);
        }

        if !st.window.is_empty() {
            encoder.set_dictionary(&st.window);
        }

        match &st.carry {
            Some(carried) => {
                // Continuation chunk: model/state/reps carry over (field 0).
                encoder.preload_entropy_state(
                    carried.model.clone(),
                    carried.state,
                    carried.rep,
                    st.global_pos,
                );
                (0, encoder)
            }
            None => {
                // State reset with new properties (field 2): required after
                // an uncompressed chunk or a mid-stream properties change.
                // Field 2 (rather than 1) so the props byte is always present
                // even if no earlier LZMA chunk delivered one.
                encoder.set_stream_pos(st.global_pos);
                (2, encoder)
            }
        }
    }

    /// Encode a single chunk of input, continuing the persistent stream state.
    fn encode_chunk(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        match self.try_emit_lzma_chunk(output, data)? {
            ChunkAttempt::Emitted => Ok(()),
            ChunkAttempt::Incompressible => self.write_uncompressed_chunks(output, data),
            ChunkAttempt::Overflow => self.encode_chunk_split(output, data),
        }
    }

    /// Try to emit `data` as one stateful LZMA chunk.
    ///
    /// On [`ChunkAttempt::Emitted`] the persistent state has been advanced;
    /// otherwise the attempt is discarded and the state is untouched (the
    /// throwaway encoder worked on a clone of the carried model).
    fn try_emit_lzma_chunk(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<ChunkAttempt> {
        let (reset_field, encoder) = self.make_chunk_encoder();
        let (compressed, model, state, rep) = encoder.compress_chunk_stateful(data)?;

        if compressed.len() >= data.len() {
            return Ok(ChunkAttempt::Incompressible);
        }
        if compressed.len() > LZMA_CHUNK_MAX_COMPRESSED {
            return Ok(ChunkAttempt::Overflow);
        }

        self.write_single_lzma_chunk(output, data.len(), &compressed, reset_field)?;
        self.encoder_state.carry = Some(CarriedState { model, state, rep });
        self.encoder_state.need_dict_reset = false;
        let cap = self.window_cap();
        self.encoder_state.push_history(data, cap);
        Ok(ChunkAttempt::Emitted)
    }

    /// Split `data` into small pieces and emit each as a stateful LZMA chunk
    /// (or an uncompressed chunk when even the small piece will not shrink).
    ///
    /// Used when the whole chunk's compressed payload overflowed the 16-bit
    /// chunk size field: conservative 16 KiB pieces cannot overflow it.
    fn encode_chunk_split(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        const SUB_CHUNK_SIZE: usize = 16 * 1024;

        for piece in data.chunks(SUB_CHUNK_SIZE) {
            match self.try_emit_lzma_chunk(output, piece)? {
                ChunkAttempt::Emitted => {}
                // A 16 KiB piece can never overflow the 64 KiB compressed
                // field, so any non-fit means "store verbatim".
                ChunkAttempt::Incompressible | ChunkAttempt::Overflow => {
                    self.write_uncompressed_chunks(output, piece)?;
                }
            }
        }

        Ok(())
    }

    /// Write data as uncompressed chunks (64 KiB pieces), updating the
    /// persistent stream state.
    ///
    /// Only the stream's very first chunk resets the dictionary; verbatim
    /// bytes otherwise extend the decoder's dictionary exactly like LZMA-coded
    /// bytes. Per the LZMA2 spec the next LZMA chunk must then reset the
    /// entropy state, so the carried state is dropped.
    fn write_uncompressed_chunks(&mut self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        let mut reset_dict = self.encoder_state.need_dict_reset;

        for piece in data.chunks(UNCOMPRESSED_CHUNK_MAX) {
            let control_byte = if reset_dict {
                control::UNCOMPRESSED_RESET
            } else {
                control::UNCOMPRESSED
            };
            output.write_all(&[control_byte])?;

            // Size (big-endian, minus 1)
            let size = (piece.len() - 1) as u16;
            output.write_all(&size.to_be_bytes())?;

            // Data
            output.write_all(piece)?;

            reset_dict = false;
        }

        self.encoder_state.need_dict_reset = false;
        self.encoder_state.carry = None;
        let cap = self.window_cap();
        self.encoder_state.push_history(data, cap);

        Ok(())
    }

    /// Write a single LZMA chunk with the given reset field (0-3); the
    /// properties byte is emitted for fields 2 and 3 as the spec requires.
    fn write_single_lzma_chunk(
        &mut self,
        output: &mut Vec<u8>,
        uncompressed_size: usize,
        compressed: &[u8],
        reset_field: u8,
    ) -> Result<()> {
        let uncompressed_minus_1 = uncompressed_size - 1;
        let size_high = ((uncompressed_minus_1 >> 16) & 0x1F) as u8;
        let size_low = (uncompressed_minus_1 & 0xFFFF) as u16;

        // Build control byte: 0x80 | reset field (bits 5-6) | size high bits.
        let control_byte = control::LZMA_MASK | ((reset_field & 0x3) << 5) | size_high;
        output.write_all(&[control_byte])?;

        // Uncompressed size low 16 bits
        output.write_all(&size_low.to_be_bytes())?;

        // Compressed size (minus 1)
        let compressed_size = (compressed.len() - 1) as u16;
        output.write_all(&compressed_size.to_be_bytes())?;

        // Properties byte for reset fields 2 and 3
        if reset_field >= 2 {
            output.write_all(&[self.encoder_state.props.to_byte()])?;
        }

        // Compressed data
        output.write_all(compressed)?;

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
        // No resets (continuation chunk, reset field 0)
        assert_eq!(control::build_lzma(0, false, false), 0x80);

        // A dictionary reset always implies a state reset + new properties
        // (reset field 3): LZMA2 cannot express a dictionary reset alone.
        assert_eq!(control::build_lzma(0, true, false), 0xE0);

        // State reset + new properties (reset field 2)
        assert_eq!(control::build_lzma(0, false, true), 0xC0);

        // Both resets (reset field 3)
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

        // Reset field values (bits 5-6)
        assert_eq!(control::reset_field(0x80), 0);
        assert_eq!(control::reset_field(0xA0), 1);
        assert_eq!(control::reset_field(0xC0), 2);
        assert_eq!(control::reset_field(0xE0), 3);

        // Only reset field 3 resets the dictionary; 0xA0 is a state reset.
        assert!(control::has_dict_reset(0xE0));
        assert!(!control::has_dict_reset(0xA0));
        assert!(!control::has_dict_reset(0x80));
        assert!(!control::has_dict_reset(0xC0));

        // Reset fields 1-3 all reset the LZMA state.
        assert!(control::has_state_reset(0xA0));
        assert!(control::has_state_reset(0xC0));
        assert!(control::has_state_reset(0xE0));
        assert!(!control::has_state_reset(0x80));

        // A properties byte follows for reset fields 2 and 3 only.
        assert!(control::has_new_props(0xC0));
        assert!(control::has_new_props(0xE0));
        assert!(!control::has_new_props(0x80));
        assert!(!control::has_new_props(0xA0));
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
        // Reset field 1 (0xA0) is a state reset WITHOUT a dictionary reset;
        // the buggy pre-fix parser misread bit 5 as a dictionary reset.
        assert_eq!(
            ChunkType::from_control_byte(0xA0),
            ChunkType::Lzma {
                reset_dict: false,
                reset_state: true
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

    /// Regression: custom properties must actually be used to CODE the
    /// payload, not merely declared in the chunk header. Before the
    /// `LzmaEncoder::with_props` fix the payload was always coded with the
    /// default (3,0,2) while the header declared the custom values; the
    /// repeated-byte test above happened to survive that mismatch, varied
    /// data does not.
    #[test]
    fn test_encoder_property_change_varied_data() {
        let data = varied_bytes(50_000);
        let mut encoder = Lzma2ChunkedEncoder::new(LzmaLevel::DEFAULT);
        encoder.set_properties(LzmaProperties::new(2, 1, 2));

        let encoded = encoder.encode(&data).expect("encode failed");
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode failed");
        assert_eq!(decoded, data, "custom-props varied-data round-trip");
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
