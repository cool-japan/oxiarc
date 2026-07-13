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
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::{Read, Write};

/// LZMA2 decoder.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`Lzma2Decoder::with_progress`] / [`Lzma2Decoder::with_cancel`] builders.
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
    /// Total uncompressed bytes decoded since the last dictionary reset.
    ///
    /// LZMA seeds `pos_state` and the literal position context from the
    /// *global* uncompressed position (`processedPos` in the reference
    /// LzmaDec), so this must persist across chunks and reset **only** on a
    /// dictionary reset. Resetting it per chunk desynchronizes the range
    /// decoder on every continuation chunk whose start offset is not a
    /// multiple of `2^pb` — which is how real `xz` output looks.
    uncompressed_pos: u64,
    /// The stream has not yet seen a dictionary reset; the first data chunk
    /// must perform one (LZMA2 spec, enforced by liblzma).
    need_dict_reset: bool,
    /// The previous chunk was uncompressed; the next LZMA chunk must reset
    /// the decoder state (LZMA2 spec, enforced by liblzma).
    need_state_reset: bool,
    /// Whether decoding is finished.
    finished: bool,
    /// Optional progress sink.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token.
    cancel: Option<CancellationToken>,
    /// Cumulative decompressed bytes.
    bytes_processed: u64,
}

impl Lzma2Decoder {
    /// Create a new LZMA2 decoder with the given dictionary size.
    ///
    /// `dict_size` is clamped to [`crate::decoder::DICT_SIZE_ALLOC_CAP`] to
    /// guard against a maliciously (or accidentally) huge header-declared
    /// dictionary forcing an outsized allocation — the LZMA2 properties byte
    /// can legitimately encode dictionary sizes up to ~4 GiB. The backing
    /// buffer itself is grown lazily as data is decoded (via the internal
    /// `update_dictionary` path) rather than eagerly zero-filled at
    /// `dict_size`.
    pub fn new(dict_size: u32) -> Self {
        let dict_size = dict_size.clamp(4096, crate::decoder::DICT_SIZE_ALLOC_CAP);
        Self {
            dict_size,
            dictionary: Vec::new(),
            dict_pos: 0,
            dict_len: 0,
            props: None,
            model: None,
            state: State::new(),
            rep: [0; 4],
            uncompressed_pos: 0,
            need_dict_reset: true,
            need_state_reset: false,
            finished: false,
            progress: None,
            cancel: None,
            bytes_processed: 0,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(cumulative_decompressed_bytes, None)` is called
    /// after each chunk is decoded. `on_finish()` is called after the end-of-stream marker.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each chunk is decoded.
    /// If cancelled, returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Decode an LZMA2 stream.
    ///
    /// Decodes chunks until the end-of-stream marker (`0x00`). A stream that
    /// ends without the marker is truncated and returns an error rather than
    /// silently yielding partial output.
    pub fn decode<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        loop {
            // Cooperative cancellation check before each chunk.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            let before = output.len();

            if !self.decode_chunk(reader, &mut output)? {
                // End-of-stream marker consumed.
                if let Some(ref handle) = self.progress {
                    handle.on_finish();
                }
                break;
            }

            self.bytes_processed += (output.len() - before) as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(self.bytes_processed, None);
            }
        }

        Ok(output)
    }

    /// Decode a single LZMA2 chunk from `reader`, appending its uncompressed
    /// bytes to `output`.
    ///
    /// Dictionary, probability model, LZMA state, rep distances and the
    /// global uncompressed position all persist on `self` between calls, so
    /// chunks may be fed one at a time (as [`crate::Lzma2StreamDecoder`]
    /// does) with semantics identical to decoding the whole stream at once.
    ///
    /// Returns `Ok(true)` after a data chunk, `Ok(false)` after the
    /// end-of-stream marker (`0x00`). A read failure on the control byte
    /// (truncated stream) is an error.
    pub fn decode_chunk<R: Read>(&mut self, reader: &mut R, output: &mut Vec<u8>) -> Result<bool> {
        // Read control byte
        let mut control = [0u8; 1];
        if reader.read_exact(&mut control).is_err() {
            return Err(OxiArcError::corrupted(
                self.bytes_processed,
                "Truncated LZMA2 stream: missing end-of-stream marker",
            ));
        }
        let control = control[0];

        if control == 0x00 {
            // End of stream
            self.finished = true;
            return Ok(false);
        }

        if control == 0x01 || control == 0x02 {
            // Uncompressed chunk
            let reset_dict = control == 0x01;
            self.decode_uncompressed_chunk(reader, output, reset_dict)?;
        } else if control >= 0x80 {
            // LZMA compressed chunk
            self.decode_lzma_chunk(reader, output, control)?;
        } else {
            return Err(OxiArcError::invalid_header(format!(
                "Invalid LZMA2 control byte: 0x{:02X}",
                control
            )));
        }

        Ok(true)
    }

    /// Decode an uncompressed chunk.
    fn decode_uncompressed_chunk<R: Read>(
        &mut self,
        reader: &mut R,
        output: &mut Vec<u8>,
        reset_dict: bool,
    ) -> Result<()> {
        if self.need_dict_reset && !reset_dict {
            return Err(OxiArcError::invalid_header(
                "LZMA2: first chunk must reset the dictionary",
            ));
        }

        // Read size (big-endian, 16-bit) + 1
        let mut size_bytes = [0u8; 2];
        reader.read_exact(&mut size_bytes)?;
        let size = u16::from_be_bytes(size_bytes) as usize + 1;

        if reset_dict {
            self.dict_pos = 0;
            self.dict_len = 0;
            self.uncompressed_pos = 0;
        }
        self.need_dict_reset = false;
        // An LZMA chunk following an uncompressed chunk must reset the state
        // (LZMA2 spec).
        self.need_state_reset = true;

        // Read uncompressed data
        let start = output.len();
        output.resize(start + size, 0);
        reader.read_exact(&mut output[start..])?;

        // Update dictionary and advance the global uncompressed position:
        // verbatim bytes count toward `pos_state` in later chunks exactly like
        // LZMA-coded bytes do.
        self.update_dictionary(&output[start..]);
        self.uncompressed_pos += size as u64;

        Ok(())
    }

    /// Decode an LZMA compressed chunk.
    fn decode_lzma_chunk<R: Read>(
        &mut self,
        reader: &mut R,
        output: &mut Vec<u8>,
        control: u8,
    ) -> Result<()> {
        // Parse control byte. Bits 5-6 form the reset field (LZMA2 spec):
        //   0 = no reset, 1 = state reset,
        //   2 = state reset + new properties,
        //   3 = state reset + new properties + dictionary reset.
        let reset = (control >> 5) & 0x3;
        let reset_state = reset >= 1;
        let new_props = reset >= 2;
        let reset_dict = reset == 3;

        if self.need_dict_reset && !reset_dict {
            return Err(OxiArcError::invalid_header(
                "LZMA2: first chunk must reset the dictionary",
            ));
        }
        if self.need_state_reset && !reset_state {
            return Err(OxiArcError::invalid_header(
                "LZMA2: LZMA chunk after an uncompressed chunk must reset the state",
            ));
        }

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
            self.uncompressed_pos = 0;
        }
        self.need_dict_reset = false;
        self.need_state_reset = false;

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

        // The chunk header declares the exact uncompressed size; anything
        // else (an embedded end marker cutting the chunk short, or a match
        // overshooting the declared size) is corruption and must never be
        // passed through silently.
        if decompressed.len() != uncompressed_size {
            return Err(OxiArcError::corrupted(
                self.uncompressed_pos + decompressed.len() as u64,
                format!(
                    "LZMA2 chunk decoded to {} bytes but header declared {}",
                    decompressed.len(),
                    uncompressed_size
                ),
            ));
        }

        // Update dictionary, global position and output
        self.update_dictionary(&decompressed);
        self.uncompressed_pos += decompressed.len() as u64;
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
        // Position of this chunk's first byte within the LZMA2 stream since
        // the last dictionary reset. `pos_state` and the literal position
        // context are seeded from the *global* position (reference LzmaDec's
        // `processedPos`), not the chunk-local offset: real `xz` emits
        // continuation chunks starting at arbitrary global offsets.
        let chunk_start = self.uncompressed_pos;
        let mut bytes_decoded = 0u64;

        while bytes_decoded < uncompressed_size as u64 {
            let global_pos = chunk_start + bytes_decoded;
            let pos_state = (global_pos as usize) & (props.num_pos_states() - 1);
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
                // prev_byte: the byte immediately before the current decode position.
                // We first look in the `output` buffer that we are building for this
                // chunk; only if that is empty do we fall back to the external
                // dictionary ring-buffer.
                let prev_byte = if let Some(&b) = output.last() {
                    b
                } else {
                    // output is empty — try the external dictionary.
                    if self.dict_len > 0 {
                        self.get_byte_from_dict(0)
                    } else {
                        0
                    }
                };

                // match_byte: the byte at distance rep[0] from the current position,
                // used as match context for the literal coder when we are not in a
                // literal state.  The combined window is  output ++ dictionary.
                let match_byte = if !self.state.is_literal() {
                    let dist = self.rep[0] as usize;
                    let out_len = output.len();
                    let total_avail = out_len + self.dict_len;
                    if dist < total_avail {
                        if dist < out_len {
                            // Within the current chunk's output buffer.
                            output[out_len - dist - 1]
                        } else {
                            // In the external dictionary ring-buffer.
                            self.get_byte_from_dict(dist - out_len)
                        }
                    } else {
                        0
                    }
                } else {
                    0
                };

                let byte = self.decode_literal(&mut rc, prev_byte, match_byte, global_pos)?;

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

                    // An end marker must never appear inside an LZMA2 chunk:
                    // the chunk header already declares the exact sizes
                    // (liblzma rejects this as corrupt data too).
                    if dist == 0xFFFF_FFFF {
                        return Err(OxiArcError::corrupted(
                            global_pos,
                            "Unexpected LZMA end marker inside an LZMA2 chunk",
                        ));
                    }

                    // Shift rep distances
                    self.rep[3] = self.rep[2];
                    self.rep[2] = self.rep[1];
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;

                    self.state.update_match();
                    self.copy_from_dict(&mut output, dist as usize, len as usize, global_pos)?;
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
                            let dist = self.rep[0] as usize;
                            let out_len = output.len();
                            let total_avail = out_len + self.dict_len;

                            if dist >= total_avail {
                                return Err(OxiArcError::corrupted(
                                    global_pos,
                                    "Invalid LZMA data",
                                ));
                            }

                            // Read from output first, then fall back to the external dict.
                            let byte = if dist < out_len {
                                output[out_len - dist - 1]
                            } else {
                                self.get_byte_from_dict(dist - out_len)
                            };
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
                            global_pos,
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
                        self.copy_from_dict(&mut output, dist as usize, len as usize, global_pos)?;
                        bytes_decoded += len as u64;
                    }
                }
            }
        }

        Ok(output)
    }

    /// Get a byte from the dictionary ring buffer, `dist` bytes back from the
    /// most recently written byte (`dist == 0` is the last byte written).
    ///
    /// Callers must validate `dist < self.dict_len` (all call sites bound the
    /// distance against `output.len() + self.dict_len` first); out-of-range
    /// distances fall back to `0` only as a defensive backstop.
    fn get_byte_from_dict(&self, dist: usize) -> u8 {
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
    ///
    /// `global_pos` is the uncompressed position since the last dictionary
    /// reset (not the chunk-local offset); the literal position context
    /// (`lp` bits) is derived from it.
    fn decode_literal<R: Read>(
        &mut self,
        rc: &mut RangeDecoder<R>,
        prev_byte: u8,
        match_byte: u8,
        global_pos: u64,
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
            .get_state(global_pos, prev_byte, props.lc, props.lp);

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
            // Specification layout `PosDecoders + dist - posSlot` (LzmaSpec.cpp):
            // the reverse bit tree for this slot starts at `dist_base - slot`
            // and is addressed by the bit-tree node index `m` (starting at 1).
            let base_idx = (dist as usize) - (slot as usize);

            let mut result = 0u32;
            let mut m = 1usize;

            for i in 0..num_direct_bits {
                let bit = rc.decode_bit(&mut model.distance.special[base_idx + m])?;
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
    ///
    /// `global_pos` is used only for error reporting.
    fn copy_from_dict(
        &self,
        output: &mut Vec<u8>,
        dist: usize,
        len: usize,
        global_pos: u64,
    ) -> Result<()> {
        // The distance must lie within the data decoded so far (this chunk's
        // output plus the persistent dictionary). Anything further back is
        // corruption; zero-filling it would be silent data corruption.
        if dist >= output.len() + self.dict_len {
            return Err(OxiArcError::corrupted(
                global_pos,
                "Invalid LZMA data: match distance exceeds dictionary contents",
            ));
        }

        // Copy from output buffer - dist is 0-indexed from the end
        // dist=0 means copy from the last byte written
        for _ in 0..len {
            let out_len = output.len();
            let byte = if dist < out_len {
                // Copy from within current output
                output[out_len - dist - 1]
            } else {
                // From the persistent dictionary (back-reference into a
                // previous chunk).
                self.get_byte_from_dict(dist - out_len)
            };
            output.push(byte);
        }
        Ok(())
    }

    /// Update the dictionary with new data.
    ///
    /// The backing `dictionary` buffer grows lazily: a byte is appended
    /// (extending physical storage) only when `dict_pos` has reached the
    /// current end of the buffer; otherwise it overwrites an already-grown
    /// slot in place. This means the buffer only ever grows up to
    /// `dict_size` bytes as data is actually decoded — even across
    /// mid-stream dictionary resets (which rewind the logical `dict_pos`/
    /// `dict_len` counters to 0 without shrinking the physical buffer) — and
    /// never eagerly allocates the full header-declared size up front.
    fn update_dictionary(&mut self, data: &[u8]) {
        let dict_capacity = self.dict_size as usize;

        for &byte in data {
            if self.dict_pos < self.dictionary.len() {
                self.dictionary[self.dict_pos] = byte;
            } else {
                self.dictionary.push(byte);
            }
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
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`Lzma2Encoder::with_progress`] / [`Lzma2Encoder::with_cancel`] builders.
pub struct Lzma2Encoder {
    /// Compression level.
    #[allow(dead_code)]
    level: LzmaLevel,
    /// Dictionary size.
    dict_size: u32,
    /// Optional progress sink.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token.
    cancel: Option<CancellationToken>,
}

impl Lzma2Encoder {
    /// Create a new LZMA2 encoder.
    pub fn new(level: LzmaLevel) -> Self {
        Self {
            level,
            dict_size: level.dict_size(),
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes, None)` is called once after the full
    /// encode completes. `on_finish()` is called at the same point.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of `encode`.
    /// If cancelled, returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Encode data to LZMA2 format.
    pub fn encode(&self, data: &[u8]) -> Result<Vec<u8>> {
        // Cooperative cancellation check at the start.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let mut output = Vec::new();

        if data.is_empty() {
            // Empty stream - just end marker
            output.push(0x00);
            if let Some(ref handle) = self.progress {
                handle.on_progress(0, None);
                handle.on_finish();
            }
            return Ok(output);
        }

        // A single LZMA2 chunk header carries a 21-bit uncompressed size and a
        // 16-bit compressed size. Inputs that cannot fit either field must be
        // split into multiple chunks; delegate those to the chunked encoder
        // instead of silently truncating the size fields.
        if data.len() > crate::lzma2_chunk::LZMA_CHUNK_MAX_UNCOMPRESSED {
            return self.encode_chunked(data);
        }

        // Create encoder to get properties
        let encoder = LzmaEncoder::new(self.level, self.dict_size);
        let props = encoder.properties();

        // Compress with LZMA (chunk payload: no end-of-stream marker, since
        // the chunk header carries the exact sizes)
        let compressed = encoder.compress_chunk(data)?;

        // Check if compression is worthwhile
        if compressed.len() >= data.len() {
            if data.len() > crate::lzma2_chunk::UNCOMPRESSED_CHUNK_MAX {
                // A single uncompressed chunk holds at most 64 KiB; split via
                // the chunked encoder.
                return self.encode_chunked(data);
            }
            // Use uncompressed chunk
            self.write_uncompressed_chunk(&mut output, data, true)?;
        } else if compressed.len() > crate::lzma2_chunk::LZMA_CHUNK_MAX_COMPRESSED {
            // The compressed payload overflows the 16-bit chunk size field;
            // split via the chunked encoder.
            return self.encode_chunked(data);
        } else {
            // Use LZMA compressed chunk
            self.write_lzma_chunk(&mut output, data.len(), &compressed, props, true)?;
        }

        // End marker
        output.push(0x00);

        if let Some(ref handle) = self.progress {
            handle.on_progress(data.len() as u64, None);
            handle.on_finish();
        }

        Ok(output)
    }

    /// Encode via the multi-chunk LZMA2 encoder.
    ///
    /// Used for inputs that cannot be represented as a single LZMA2 chunk
    /// (uncompressed size over 2 MiB, compressed payload over 64 KiB, or an
    /// incompressible input over 64 KiB).
    fn encode_chunked(&self, data: &[u8]) -> Result<Vec<u8>> {
        let config =
            crate::lzma2_chunk::Lzma2Config::with_level(self.level).dict_size(self.dict_size);
        let mut encoder = crate::lzma2_chunk::Lzma2ChunkedEncoder::with_config(config);
        if let Some(ref handle) = self.progress {
            encoder = encoder.with_progress(handle.clone());
        }
        if let Some(ref token) = self.cancel {
            encoder = encoder.with_cancel(token.clone());
        }
        encoder.encode(data)
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
        let encoded =
            encode_lzma2(original, LzmaLevel::DEFAULT).expect("compression/encoding failed");
        assert_eq!(encoded, vec![0x00]); // Just end marker
    }

    #[test]
    fn test_lzma2_uncompressed_roundtrip() {
        // Test with small data that won't compress well
        let original = b"ABCD";
        let encoded = encode_lzma2(original, LzmaLevel::FAST).expect("compression/encoding failed");
        let decoded = decode_lzma2(&encoded, 4096).expect("decompression failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_lzma2_compressed_roundtrip() {
        // Test with repeating data that compresses well
        let original: Vec<u8> = vec![b'A'; 1000];
        let encoded =
            encode_lzma2(&original, LzmaLevel::DEFAULT).expect("compression/encoding failed");
        let decoded = decode_lzma2(&encoded, 1 << 20).expect("decompression failed");
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
        vec![b'A'; size]
    }

    #[test]
    fn test_lzma2_encoder_progress_reports() {
        // Use small data and FAST level to keep the test quick.
        let data = make_compressible_data(8 * 1024); // 8 KB

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let encoder = Lzma2Encoder::new(LzmaLevel::FAST)
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
    fn test_lzma2_encoder_cancel_aborts() {
        let data = make_compressible_data(8 * 1024);
        let token = CancellationToken::new();
        let encoder = Lzma2Encoder::new(LzmaLevel::FAST).with_cancel(token.clone());

        token.cancel();
        let result = encoder.encode(&data);
        assert!(result.is_err(), "expected cancellation error");
    }

    #[test]
    fn test_lzma2_decoder_progress_reports() {
        let data = make_compressible_data(8 * 1024); // 8 KB
        let encoded = encode_lzma2(&data, LzmaLevel::FAST).expect("encode failed");

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let mut decoder =
            Lzma2Decoder::new(1 << 20).with_progress(sink as oxiarc_core::progress::ProgressHandle);
        let mut cursor = std::io::Cursor::new(&encoded);
        decoder.decode(&mut cursor).expect("decode failed");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
        let (last_processed, _) = *recorded.last().expect("non-empty");
        assert_eq!(
            last_processed,
            data.len() as u64,
            "final processed count must equal decompressed size"
        );
    }

    #[test]
    fn test_lzma2_decoder_cancel_aborts() {
        let data = make_compressible_data(8 * 1024);
        let encoded = encode_lzma2(&data, LzmaLevel::FAST).expect("encode failed");

        let token = CancellationToken::new();
        let mut decoder = Lzma2Decoder::new(1 << 20).with_cancel(token.clone());
        let mut cursor = std::io::Cursor::new(&encoded);

        token.cancel();
        let result = decoder.decode(&mut cursor);
        assert!(result.is_err(), "expected cancellation error");
    }
}
