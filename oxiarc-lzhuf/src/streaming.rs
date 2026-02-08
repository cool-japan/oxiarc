//! Streaming LZH decompression.
//!
//! This module provides a true streaming decompressor that can handle partial
//! input/output and resume decompression across multiple calls.

use crate::methods::LzhMethod;
use crate::methods::constants::{NC, NT};
use oxiarc_core::RingBuffer;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::DecompressStatus;

/// Maximum code length for LZH Huffman codes.
const MAX_CODE_LENGTH: usize = 16;

// ============================================================================
// Streaming Bit Reader
// ============================================================================

/// A bit reader for streaming decompression that works with byte slices.
///
/// Unlike `BitReader<R: Read>`, this reader:
/// - Works with fixed byte slices, not streams
/// - Can report how much input was consumed
/// - Supports saving/restoring state for resumption
#[derive(Debug, Clone)]
pub struct StreamingBitReader {
    /// Bit buffer (LSB-first).
    buffer: u64,
    /// Number of valid bits in buffer.
    bits_in_buffer: u8,
    /// Current position in input slice.
    input_pos: usize,
    /// Total bits consumed (for progress tracking).
    total_bits_consumed: u64,
}

impl Default for StreamingBitReader {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingBitReader {
    /// Create a new streaming bit reader.
    pub fn new() -> Self {
        Self {
            buffer: 0,
            bits_in_buffer: 0,
            input_pos: 0,
            total_bits_consumed: 0,
        }
    }

    /// Reset the reader state for a new input slice.
    pub fn reset_for_new_input(&mut self) {
        self.input_pos = 0;
    }

    /// Get the number of bytes consumed from the current input.
    pub fn bytes_consumed(&self) -> usize {
        self.input_pos
    }

    /// Get total bits consumed.
    pub fn total_bits(&self) -> u64 {
        self.total_bits_consumed
    }

    /// Get bits currently available in buffer.
    pub fn bits_available(&self) -> u8 {
        self.bits_in_buffer
    }

    /// Read up to 32 bits from the stream.
    /// Returns None if not enough bits are available.
    pub fn read_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "Cannot read more than 32 bits at once");

        if count == 0 {
            return Some(0);
        }

        // Try to fill buffer if needed
        while self.bits_in_buffer < count && self.input_pos < input.len() {
            self.buffer |= (input[self.input_pos] as u64) << self.bits_in_buffer;
            self.bits_in_buffer += 8;
            self.input_pos += 1;
        }

        if self.bits_in_buffer < count {
            return None; // Not enough input
        }

        // Extract bits from buffer
        let mask = (1u64 << count).wrapping_sub(1);
        let result = (self.buffer & mask) as u32;

        // Remove read bits from buffer
        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_consumed += count as u64;

        Some(result)
    }

    /// Peek at up to 32 bits without consuming them.
    /// Returns None if not enough bits are available.
    pub fn peek_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "Cannot peek more than 32 bits at once");

        if count == 0 {
            return Some(0);
        }

        // Try to fill buffer if needed
        while self.bits_in_buffer < count && self.input_pos < input.len() {
            self.buffer |= (input[self.input_pos] as u64) << self.bits_in_buffer;
            self.bits_in_buffer += 8;
            self.input_pos += 1;
        }

        if self.bits_in_buffer < count {
            return None;
        }

        let mask = (1u64 << count).wrapping_sub(1);
        Some((self.buffer & mask) as u32)
    }

    /// Skip a number of bits.
    pub fn skip_bits(&mut self, count: u8) {
        if count == 0 || self.bits_in_buffer < count {
            return;
        }

        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_consumed += count as u64;
    }

    /// Read a single bit.
    pub fn read_bit(&mut self, input: &[u8]) -> Option<bool> {
        self.read_bits(input, 1).map(|b| b != 0)
    }

    /// Save the current state for potential rollback.
    pub fn save_state(&self) -> BitReaderState {
        BitReaderState {
            buffer: self.buffer,
            bits_in_buffer: self.bits_in_buffer,
            input_pos: self.input_pos,
            total_bits_consumed: self.total_bits_consumed,
        }
    }

    /// Restore a previously saved state.
    pub fn restore_state(&mut self, state: BitReaderState) {
        self.buffer = state.buffer;
        self.bits_in_buffer = state.bits_in_buffer;
        self.input_pos = state.input_pos;
        self.total_bits_consumed = state.total_bits_consumed;
    }
}

/// Saved state of a StreamingBitReader for rollback.
#[derive(Debug, Clone, Copy)]
pub struct BitReaderState {
    buffer: u64,
    bits_in_buffer: u8,
    input_pos: usize,
    total_bits_consumed: u64,
}

// ============================================================================
// Huffman Tree for Streaming
// ============================================================================

/// Entry in the Huffman lookup table.
#[derive(Debug, Clone, Copy)]
struct TableEntry(i32);

impl TableEntry {
    const INVALID: TableEntry = TableEntry(-1);

    fn new(symbol: u16, length: u8) -> Self {
        TableEntry(((length as i32) << 16) | (symbol as i32))
    }

    fn is_valid(self) -> bool {
        self.0 >= 0
    }

    fn symbol(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn length(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }
}

/// Streaming Huffman tree for decoding.
#[derive(Debug, Clone)]
pub struct StreamingHuffmanTree {
    /// Lookup table for fast decoding.
    table: Vec<TableEntry>,
    /// Table bits for fast lookup.
    table_bits: u8,
    /// Maximum code length.
    max_length: u8,
}

impl StreamingHuffmanTree {
    /// Create a Huffman tree from code lengths.
    pub fn from_lengths(lengths: &[u8], table_bits: u8) -> Result<Self> {
        let table_size = 1 << table_bits;
        let mut table = vec![TableEntry::INVALID; table_size];

        if lengths.is_empty() {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Find max length
        let max_length = lengths.iter().copied().max().unwrap_or(0);
        if max_length == 0 {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Count codes of each length
        let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
        let mut code = 0u32;
        for bits in 1..=max_length as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Build lookup table
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 && len <= table_bits {
                let len_usize = len as usize;
                let code = next_code[len_usize];
                next_code[len_usize] += 1;

                // Fill table entries (reversed for LSB-first)
                let reversed = Self::reverse_bits(code as u16, len);
                let fill_count = 1 << (table_bits as usize - len_usize);

                for i in 0..fill_count {
                    let index = reversed as usize | (i << len_usize);
                    if index < table_size {
                        table[index] = TableEntry::new(symbol as u16, len);
                    }
                }
            }
        }

        Ok(Self {
            table,
            table_bits,
            max_length,
        })
    }

    /// Reverse bits.
    fn reverse_bits(mut value: u16, length: u8) -> u16 {
        let mut result = 0u16;
        for _ in 0..length {
            result = (result << 1) | (value & 1);
            value >>= 1;
        }
        result
    }

    /// Decode a symbol using the streaming bit reader.
    /// Returns None if not enough input is available.
    pub fn decode(&self, reader: &mut StreamingBitReader, input: &[u8]) -> Option<u16> {
        if self.max_length == 0 {
            return None;
        }

        let bits = reader.peek_bits(input, self.table_bits)?;
        let entry = self.table[bits as usize];

        if entry.is_valid() {
            reader.skip_bits(entry.length());
            Some(entry.symbol())
        } else {
            None
        }
    }
}

// ============================================================================
// Streaming Decoder State Machine
// ============================================================================

/// State of the streaming decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderPhase {
    /// Initial state, ready to start a new block.
    Ready,
    /// Reading block size (16 bits).
    ReadBlockSize,
    /// Reading the C-tree (character/length codes).
    ReadCTree,
    /// Reading the P-tree (position/distance codes).
    ReadPTree,
    /// Decoding block data.
    DecodeBlock,
    /// Decompression complete.
    Done,
    /// Error state.
    Error,
}

/// State for reading C-tree.
#[derive(Debug, Clone)]
struct CTreeState {
    n: usize,
    i: usize,
    lengths: Vec<u8>,
    pt_tree: Option<StreamingHuffmanTree>,
    phase: CTreePhase,
    // PT tree reading state (embedded to avoid borrow issues)
    pt_n: usize,
    pt_i: usize,
    pt_lengths: Vec<u8>,
    pt_phase: PTTreePhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CTreePhase {
    ReadN,
    ReadSingleCode,
    ReadPTTreeN,
    ReadPTTreeSingleCode,
    ReadPTTreeLengths,
    ReadPTTreeSkipCount,
    ReadPTTreeExtendedLength,
    ReadLengths,
}

/// Phase for reading PT tree (part of C-tree reading state machine).
/// Some variants may not be reached in all code paths but are kept for
/// completeness of the state machine design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum PTTreePhase {
    ReadN,
    ReadSingleCode,
    ReadLengths,
    ReadSkipCount,
    ReadExtendedLength,
}

/// State for reading P-tree.
#[derive(Debug, Clone)]
struct PTreeState {
    n: usize,
    i: usize,
    lengths: Vec<u8>,
    np: usize,
    phase: PTreePhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PTreePhase {
    ReadN,
    ReadSingleCode,
    ReadLengths,
    ReadExtendedLength,
}

/// Pending match to be output.
#[derive(Debug, Clone, Copy)]
struct PendingMatch {
    length: u16,
    distance: u16,
    output_so_far: u16,
}

/// Streaming LZH decoder with full state preservation.
#[derive(Debug)]
pub struct StreamingLzhDecoder {
    /// Compression method.
    method: LzhMethod,
    /// Ring buffer for history.
    ring: RingBuffer,
    /// Streaming bit reader.
    bit_reader: StreamingBitReader,
    /// Expected uncompressed size.
    uncompressed_size: u64,
    /// Bytes decoded so far.
    bytes_decoded: u64,
    /// Current decoder phase.
    phase: DecoderPhase,
    /// Number of position codes (depends on method).
    np: usize,
    /// Current block size.
    block_size: usize,
    /// Bytes decoded in current block.
    block_bytes_decoded: usize,
    /// C-tree (character/length codes).
    c_tree: Option<StreamingHuffmanTree>,
    /// P-tree (position/distance codes).
    p_tree: Option<StreamingHuffmanTree>,
    /// State for reading C-tree.
    c_tree_state: Option<CTreeState>,
    /// State for reading P-tree.
    p_tree_state: Option<PTreeState>,
    /// Pending match (partially output).
    pending_match: Option<PendingMatch>,
    /// Last error (if any).
    last_error: Option<String>,
}

impl StreamingLzhDecoder {
    /// Create a new streaming LZH decoder.
    pub fn new(method: LzhMethod, uncompressed_size: u64) -> Self {
        let window_size = method.window_size().max(256);
        let np = match method {
            LzhMethod::Lh4 | LzhMethod::Lh5 => 14,
            LzhMethod::Lh6 => 16,
            LzhMethod::Lh7 => 17,
            LzhMethod::Lh0 => 0,
        };

        Self {
            method,
            ring: RingBuffer::new(window_size),
            bit_reader: StreamingBitReader::new(),
            uncompressed_size,
            bytes_decoded: 0,
            phase: if method.is_stored() {
                DecoderPhase::DecodeBlock
            } else {
                DecoderPhase::ReadBlockSize
            },
            np,
            block_size: 0,
            block_bytes_decoded: 0,
            c_tree: None,
            p_tree: None,
            c_tree_state: None,
            p_tree_state: None,
            pending_match: None,
            last_error: None,
        }
    }

    /// Reset the decoder.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.bit_reader = StreamingBitReader::new();
        self.bytes_decoded = 0;
        self.phase = if self.method.is_stored() {
            DecoderPhase::DecodeBlock
        } else {
            DecoderPhase::ReadBlockSize
        };
        self.block_size = 0;
        self.block_bytes_decoded = 0;
        self.c_tree = None;
        self.p_tree = None;
        self.c_tree_state = None;
        self.p_tree_state = None;
        self.pending_match = None;
        self.last_error = None;
    }

    /// Check if decoding is finished.
    pub fn is_finished(&self) -> bool {
        self.phase == DecoderPhase::Done
    }

    /// Get bytes decoded so far.
    pub fn bytes_decoded(&self) -> u64 {
        self.bytes_decoded
    }

    /// Get expected uncompressed size.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Get the current phase.
    pub fn phase(&self) -> DecoderPhase {
        self.phase
    }

    /// Get the last error message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Decompress data from input to output.
    ///
    /// Returns (bytes_consumed, bytes_produced, status).
    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        // Handle stored (lh0) data separately
        if self.method.is_stored() {
            return self.decompress_stored(input, output);
        }

        // Reset bit reader position for new input
        self.bit_reader.reset_for_new_input();

        let mut output_pos = 0;

        // Process pending match first
        if let Some(pending) = self.pending_match.take() {
            let result = self.continue_match(&pending, output, &mut output_pos)?;
            if result.is_some() {
                self.pending_match = result;
                return Ok((
                    self.bit_reader.bytes_consumed(),
                    output_pos,
                    DecompressStatus::NeedsOutput,
                ));
            }
        }

        // Main decompression loop
        loop {
            match self.phase {
                DecoderPhase::Done => {
                    return Ok((
                        self.bit_reader.bytes_consumed(),
                        output_pos,
                        DecompressStatus::Done,
                    ));
                }

                DecoderPhase::Error => {
                    return Err(OxiArcError::corrupted(
                        0,
                        self.last_error.as_deref().unwrap_or("Unknown error"),
                    ));
                }

                DecoderPhase::Ready => {
                    if self.bytes_decoded >= self.uncompressed_size {
                        self.phase = DecoderPhase::Done;
                        continue;
                    }
                    self.phase = DecoderPhase::ReadBlockSize;
                }

                DecoderPhase::ReadBlockSize => {
                    match self.bit_reader.read_bits(input, 16) {
                        Some(size) => {
                            self.block_size = size as usize;
                            self.block_bytes_decoded = 0;

                            if self.block_size == 0 {
                                self.phase = DecoderPhase::Done;
                            } else {
                                // Initialize C-tree state
                                self.c_tree_state = Some(CTreeState {
                                    n: 0,
                                    i: 0,
                                    lengths: vec![0u8; NC],
                                    pt_tree: None,
                                    phase: CTreePhase::ReadN,
                                    pt_n: 0,
                                    pt_i: 0,
                                    pt_lengths: vec![0u8; NT],
                                    pt_phase: PTTreePhase::ReadN,
                                });
                                self.phase = DecoderPhase::ReadCTree;
                            }
                        }
                        None => {
                            return Ok((
                                self.bit_reader.bytes_consumed(),
                                output_pos,
                                DecompressStatus::NeedsInput,
                            ));
                        }
                    }
                }

                DecoderPhase::ReadCTree => {
                    if !self.read_c_tree(input)? {
                        return Ok((
                            self.bit_reader.bytes_consumed(),
                            output_pos,
                            DecompressStatus::NeedsInput,
                        ));
                    }
                    // Initialize P-tree state
                    self.p_tree_state = Some(PTreeState {
                        n: 0,
                        i: 0,
                        lengths: vec![0u8; self.np],
                        np: self.np,
                        phase: PTreePhase::ReadN,
                    });
                    self.phase = DecoderPhase::ReadPTree;
                }

                DecoderPhase::ReadPTree => {
                    if !self.read_p_tree(input)? {
                        return Ok((
                            self.bit_reader.bytes_consumed(),
                            output_pos,
                            DecompressStatus::NeedsInput,
                        ));
                    }
                    self.phase = DecoderPhase::DecodeBlock;
                }

                DecoderPhase::DecodeBlock => {
                    match self.decode_block_streaming(input, output, &mut output_pos)? {
                        BlockDecodeResult::NeedsInput => {
                            return Ok((
                                self.bit_reader.bytes_consumed(),
                                output_pos,
                                DecompressStatus::NeedsInput,
                            ));
                        }
                        BlockDecodeResult::NeedsOutput => {
                            return Ok((
                                self.bit_reader.bytes_consumed(),
                                output_pos,
                                DecompressStatus::NeedsOutput,
                            ));
                        }
                        BlockDecodeResult::BlockDone => {
                            self.phase = DecoderPhase::Ready;
                        }
                        BlockDecodeResult::AllDone => {
                            self.phase = DecoderPhase::Done;
                        }
                    }
                }
            }
        }
    }

    /// Decompress stored (lh0) data.
    fn decompress_stored(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        let remaining = (self.uncompressed_size - self.bytes_decoded) as usize;
        let to_copy = input.len().min(output.len()).min(remaining);

        output[..to_copy].copy_from_slice(&input[..to_copy]);
        self.bytes_decoded += to_copy as u64;

        let status = if self.bytes_decoded >= self.uncompressed_size {
            self.phase = DecoderPhase::Done;
            DecompressStatus::Done
        } else if to_copy == input.len() && to_copy < remaining {
            DecompressStatus::NeedsInput
        } else {
            DecompressStatus::NeedsOutput
        };

        Ok((to_copy, to_copy, status))
    }

    /// Continue outputting a pending match.
    fn continue_match(
        &mut self,
        pending: &PendingMatch,
        output: &mut [u8],
        output_pos: &mut usize,
    ) -> Result<Option<PendingMatch>> {
        let remaining = pending.length - pending.output_so_far;
        let available = output.len() - *output_pos;

        let to_output = (remaining as usize).min(available);
        let mut output_count = 0u16;

        for _ in 0..to_output {
            let byte = self.ring.read_at_distance(pending.distance as usize)?;
            output[*output_pos] = byte;
            self.ring.write_byte(byte);
            *output_pos += 1;
            output_count += 1;
            self.bytes_decoded += 1;
            self.block_bytes_decoded += 1;
        }

        let new_output_so_far = pending.output_so_far + output_count;
        if new_output_so_far < pending.length {
            Ok(Some(PendingMatch {
                length: pending.length,
                distance: pending.distance,
                output_so_far: new_output_so_far,
            }))
        } else {
            Ok(None)
        }
    }

    /// Read C-tree (character/length codes).
    /// Returns true if complete, false if more input needed.
    fn read_c_tree(&mut self, input: &[u8]) -> Result<bool> {
        loop {
            // Get mutable access to state, but drop it before recursing
            let phase = {
                let state = self
                    .c_tree_state
                    .as_ref()
                    .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                state.phase
            };

            match phase {
                CTreePhase::ReadN => {
                    match self.bit_reader.read_bits(input, 9) {
                        Some(n) => {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.n = n as usize;
                            if state.n == 0 {
                                state.phase = CTreePhase::ReadSingleCode;
                            } else {
                                // Initialize PT tree reading state
                                state.pt_n = 0;
                                state.pt_i = 0;
                                state.pt_lengths = vec![0u8; NT];
                                state.pt_phase = PTTreePhase::ReadN;
                                state.phase = CTreePhase::ReadPTTreeN;
                            }
                        }
                        None => return Ok(false),
                    }
                }

                CTreePhase::ReadSingleCode => match self.bit_reader.read_bits(input, 9) {
                    Some(c) => {
                        let mut lengths = vec![0u8; NC];
                        if (c as usize) < NC {
                            lengths[c as usize] = 1;
                        }
                        self.c_tree = Some(StreamingHuffmanTree::from_lengths(&lengths, 12)?);
                        self.c_tree_state = None;
                        return Ok(true);
                    }
                    None => return Ok(false),
                },

                CTreePhase::ReadPTTreeN => match self.bit_reader.read_bits(input, 5) {
                    Some(n) => {
                        let state = self
                            .c_tree_state
                            .as_mut()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        state.pt_n = n as usize;
                        if state.pt_n == 0 {
                            state.phase = CTreePhase::ReadPTTreeSingleCode;
                        } else {
                            state.phase = CTreePhase::ReadPTTreeLengths;
                        }
                    }
                    None => return Ok(false),
                },

                CTreePhase::ReadPTTreeSingleCode => {
                    match self.bit_reader.read_bits(input, 5) {
                        Some(c) => {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            if (c as usize) < NT {
                                state.pt_lengths[c as usize] = 1;
                            }
                            // Build PT tree and move to reading lengths
                            let pt_tree = StreamingHuffmanTree::from_lengths(&state.pt_lengths, 5)?;
                            state.pt_tree = Some(pt_tree);
                            state.phase = CTreePhase::ReadLengths;
                        }
                        None => return Ok(false),
                    }
                }

                CTreePhase::ReadPTTreeLengths => {
                    let (pt_n, pt_i) = {
                        let state = self
                            .c_tree_state
                            .as_ref()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        (state.pt_n, state.pt_i)
                    };

                    while pt_i < pt_n.min(NT) {
                        let current_i = {
                            let state = self
                                .c_tree_state
                                .as_ref()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.pt_i
                        };

                        // Special case at i=3: read skip count
                        if current_i == 3 {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.phase = CTreePhase::ReadPTTreeSkipCount;
                            break;
                        }

                        match self.bit_reader.read_bits(input, 3) {
                            Some(len) => {
                                let state = self.c_tree_state.as_mut().ok_or_else(|| {
                                    OxiArcError::corrupted(0, "C-tree state missing")
                                })?;
                                state.pt_lengths[state.pt_i] = len as u8;
                                if len == 7 {
                                    state.phase = CTreePhase::ReadPTTreeExtendedLength;
                                    break;
                                }
                                state.pt_i += 1;
                            }
                            None => return Ok(false),
                        }
                    }

                    let (pt_n_now, pt_i_now, current_phase) = {
                        let state = self
                            .c_tree_state
                            .as_ref()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        (state.pt_n, state.pt_i, state.phase)
                    };

                    if current_phase == CTreePhase::ReadPTTreeLengths
                        && pt_i_now >= pt_n_now.min(NT)
                    {
                        // PT tree is complete
                        let state = self
                            .c_tree_state
                            .as_mut()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        let pt_tree = StreamingHuffmanTree::from_lengths(&state.pt_lengths, 5)?;
                        state.pt_tree = Some(pt_tree);
                        state.phase = CTreePhase::ReadLengths;
                    }
                }

                CTreePhase::ReadPTTreeSkipCount => match self.bit_reader.read_bits(input, 2) {
                    Some(skip) => {
                        let state = self
                            .c_tree_state
                            .as_mut()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        for j in 0..(skip as usize) {
                            if state.pt_i + j < state.pt_lengths.len() {
                                state.pt_lengths[state.pt_i + j] = 0;
                            }
                        }
                        state.pt_i += skip as usize;
                        state.phase = CTreePhase::ReadPTTreeLengths;
                    }
                    None => return Ok(false),
                },

                CTreePhase::ReadPTTreeExtendedLength => {
                    match self.bit_reader.read_bit(input) {
                        Some(true) => {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.pt_lengths[state.pt_i] += 1;
                            // Continue reading extended length
                        }
                        Some(false) => {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.pt_i += 1;
                            state.phase = CTreePhase::ReadPTTreeLengths;
                        }
                        None => return Ok(false),
                    }
                }

                CTreePhase::ReadLengths => {
                    loop {
                        let (n, i) = {
                            let state = self
                                .c_tree_state
                                .as_ref()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            (state.n, state.i)
                        };

                        if i >= n.min(NC) {
                            break;
                        }

                        // Get the PT tree
                        let pt_tree = {
                            let state = self
                                .c_tree_state
                                .as_ref()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state
                                .pt_tree
                                .clone()
                                .ok_or_else(|| OxiArcError::corrupted(0, "PT tree missing"))?
                        };

                        let c = match pt_tree.decode(&mut self.bit_reader, input) {
                            Some(c) => c,
                            None => return Ok(false),
                        };

                        if c <= 2 {
                            // Run of zeros
                            let count = match c {
                                0 => 1,
                                1 => match self.bit_reader.read_bits(input, 4) {
                                    Some(n) => n as usize + 3,
                                    None => return Ok(false),
                                },
                                2 => match self.bit_reader.read_bits(input, 9) {
                                    Some(n) => n as usize + 20,
                                    None => return Ok(false),
                                },
                                _ => 1,
                            };

                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            for _ in 0..count {
                                if state.i < state.lengths.len() {
                                    state.lengths[state.i] = 0;
                                    state.i += 1;
                                }
                            }
                        } else if c == 3 {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.lengths[state.i] = 0;
                            state.i += 1;
                        } else {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.lengths[state.i] = (c - 3) as u8;
                            state.i += 1;
                        }
                    }

                    // C-tree complete
                    let lengths = {
                        let state = self
                            .c_tree_state
                            .as_ref()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        state.lengths.clone()
                    };
                    self.c_tree = Some(StreamingHuffmanTree::from_lengths(&lengths, 12)?);
                    self.c_tree_state = None;
                    return Ok(true);
                }
            }
        }
    }

    /// Read P-tree (position/distance codes).
    /// Returns true if complete, false if more input needed.
    fn read_p_tree(&mut self, input: &[u8]) -> Result<bool> {
        loop {
            let phase = {
                let state = self
                    .p_tree_state
                    .as_ref()
                    .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                state.phase
            };

            match phase {
                PTreePhase::ReadN => match self.bit_reader.read_bits(input, 4) {
                    Some(n) => {
                        let state = self
                            .p_tree_state
                            .as_mut()
                            .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                        state.n = n as usize;
                        if state.n == 0 {
                            state.phase = PTreePhase::ReadSingleCode;
                        } else {
                            state.phase = PTreePhase::ReadLengths;
                        }
                    }
                    None => return Ok(false),
                },

                PTreePhase::ReadSingleCode => match self.bit_reader.read_bits(input, 4) {
                    Some(c) => {
                        let lengths = {
                            let state = self
                                .p_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                            if (c as usize) < state.np {
                                state.lengths[c as usize] = 1;
                            }
                            state.lengths.clone()
                        };
                        self.p_tree = Some(StreamingHuffmanTree::from_lengths(&lengths, 8)?);
                        self.p_tree_state = None;
                        return Ok(true);
                    }
                    None => return Ok(false),
                },

                PTreePhase::ReadLengths => {
                    loop {
                        let (n, i, np) = {
                            let state = self
                                .p_tree_state
                                .as_ref()
                                .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                            (state.n, state.i, state.np)
                        };

                        if i >= n.min(np) {
                            break;
                        }

                        match self.bit_reader.read_bits(input, 3) {
                            Some(len) => {
                                let state = self.p_tree_state.as_mut().ok_or_else(|| {
                                    OxiArcError::corrupted(0, "P-tree state missing")
                                })?;
                                state.lengths[state.i] = len as u8;
                                if len == 7 {
                                    state.phase = PTreePhase::ReadExtendedLength;
                                    return Ok(false); // Continue in next call
                                }
                                state.i += 1;
                            }
                            None => return Ok(false),
                        }
                    }

                    // P-tree complete
                    let lengths = {
                        let state = self
                            .p_tree_state
                            .as_ref()
                            .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                        state.lengths.clone()
                    };
                    self.p_tree = Some(StreamingHuffmanTree::from_lengths(&lengths, 8)?);
                    self.p_tree_state = None;
                    return Ok(true);
                }

                PTreePhase::ReadExtendedLength => {
                    match self.bit_reader.read_bit(input) {
                        Some(true) => {
                            let state = self
                                .p_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                            state.lengths[state.i] += 1;
                            // Continue reading extended length
                        }
                        Some(false) => {
                            let state = self
                                .p_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "P-tree state missing"))?;
                            state.i += 1;
                            state.phase = PTreePhase::ReadLengths;
                        }
                        None => return Ok(false),
                    }
                }
            }
        }
    }

    /// Decode block data in streaming mode.
    fn decode_block_streaming(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        output_pos: &mut usize,
    ) -> Result<BlockDecodeResult> {
        let target = (self.bytes_decoded + self.block_size as u64).min(self.uncompressed_size);

        let c_tree = self
            .c_tree
            .clone()
            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree missing during decode"))?;
        let p_tree = self
            .p_tree
            .clone()
            .ok_or_else(|| OxiArcError::corrupted(0, "P-tree missing during decode"))?;

        while self.bytes_decoded < target {
            // Check if output buffer is full
            if *output_pos >= output.len() {
                return Ok(BlockDecodeResult::NeedsOutput);
            }

            // Decode character/length code
            let c = match c_tree.decode(&mut self.bit_reader, input) {
                Some(c) => c,
                None => return Ok(BlockDecodeResult::NeedsInput),
            };

            if c < 256 {
                // Literal byte
                let byte = c as u8;
                self.ring.write_byte(byte);
                output[*output_pos] = byte;
                *output_pos += 1;
                self.bytes_decoded += 1;
                self.block_bytes_decoded += 1;
            } else {
                // Length + distance match
                let length = c - 256 + 3; // Minimum match = 3

                // Decode position code
                let p = match p_tree.decode(&mut self.bit_reader, input) {
                    Some(p) => p,
                    None => return Ok(BlockDecodeResult::NeedsInput),
                };

                // Calculate distance
                let distance = if p == 0 {
                    1
                } else {
                    let extra_bits = p as u8;
                    let extra = match self.bit_reader.read_bits(input, extra_bits) {
                        Some(e) => e,
                        None => return Ok(BlockDecodeResult::NeedsInput),
                    };
                    (1 << p) + extra as u16
                };

                // Output match bytes
                let available = output.len() - *output_pos;
                let to_output = (length as usize).min(available);

                for _ in 0..to_output {
                    let byte = self.ring.read_at_distance(distance as usize)?;
                    output[*output_pos] = byte;
                    self.ring.write_byte(byte);
                    *output_pos += 1;
                    self.bytes_decoded += 1;
                    self.block_bytes_decoded += 1;
                }

                // If we couldn't output the full match, save it
                if to_output < length as usize {
                    self.pending_match = Some(PendingMatch {
                        length,
                        distance,
                        output_so_far: to_output as u16,
                    });
                    return Ok(BlockDecodeResult::NeedsOutput);
                }
            }
        }

        if self.bytes_decoded >= self.uncompressed_size {
            Ok(BlockDecodeResult::AllDone)
        } else {
            Ok(BlockDecodeResult::BlockDone)
        }
    }
}

/// Result of block decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockDecodeResult {
    /// Need more input data.
    NeedsInput,
    /// Need more output space.
    NeedsOutput,
    /// Block is complete, more blocks may follow.
    BlockDone,
    /// All decompression is complete.
    AllDone,
}

// ============================================================================
// Decompressor Trait Implementation
// ============================================================================

impl oxiarc_core::traits::Decompressor for StreamingLzhDecoder {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        StreamingLzhDecoder::decompress(self, input, output)
    }

    fn reset(&mut self) {
        StreamingLzhDecoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.is_finished()
    }
}

// ============================================================================
// Convenience Functions
// ============================================================================

/// Decompress LZH data using the streaming decoder.
///
/// This is a convenience function that creates a streaming decoder and
/// decompresses all input data at once. For true streaming decompression
/// with partial input/output, use [`StreamingLzhDecoder`] directly.
///
/// # Arguments
///
/// * `data` - The compressed data
/// * `method` - The LZH compression method
/// * `uncompressed_size` - Expected uncompressed size
///
/// # Returns
///
/// The decompressed data, or an error if decompression fails.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzhuf::{LzhMethod, decode_lzh_streaming};
///
/// // Decompress stored (lh0) data
/// let data = b"Hello, World!";
/// let result = decode_lzh_streaming(data, LzhMethod::Lh0, data.len() as u64)
///     .expect("decompression failed");
/// assert_eq!(result, data);
/// ```
pub fn decode_lzh_streaming(
    data: &[u8],
    method: LzhMethod,
    uncompressed_size: u64,
) -> Result<Vec<u8>> {
    let mut decoder = StreamingLzhDecoder::new(method, uncompressed_size);
    let mut output = Vec::with_capacity(uncompressed_size as usize);
    let mut input_pos = 0;
    let mut buffer = vec![0u8; 32768];

    loop {
        let (consumed, produced, status) = decoder.decompress(&data[input_pos..], &mut buffer)?;

        input_pos += consumed;
        output.extend_from_slice(&buffer[..produced]);

        match status {
            DecompressStatus::Done => break,
            DecompressStatus::NeedsInput if input_pos >= data.len() => break,
            DecompressStatus::NeedsOutput | DecompressStatus::NeedsInput => continue,
            DecompressStatus::BlockEnd => continue,
        }
    }

    Ok(output)
}

/// Create a streaming decoder with default settings for the given method.
///
/// This is a convenience function that creates a [`StreamingLzhDecoder`]
/// with the specified method and uncompressed size.
///
/// # Arguments
///
/// * `method` - The LZH compression method
/// * `uncompressed_size` - Expected uncompressed size
///
/// # Returns
///
/// A new streaming decoder ready for decompression.
pub fn create_streaming_decoder(method: LzhMethod, uncompressed_size: u64) -> StreamingLzhDecoder {
    StreamingLzhDecoder::new(method, uncompressed_size)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_bit_reader_basic() {
        let data = [0xAB, 0xCD, 0xEF];
        let mut reader = StreamingBitReader::new();

        // Read some bits
        assert_eq!(reader.read_bits(&data, 4), Some(0xB)); // Low nibble of 0xAB
        assert_eq!(reader.read_bits(&data, 4), Some(0xA)); // High nibble of 0xAB
        assert_eq!(reader.read_bits(&data, 8), Some(0xCD));
        // Only 2 bytes consumed to read 16 bits (0xAB and 0xCD)
        assert_eq!(reader.bytes_consumed(), 2);
    }

    #[test]
    fn test_streaming_bit_reader_not_enough_input() {
        let data = [0xAB];
        let mut reader = StreamingBitReader::new();

        // Try to read more bits than available
        assert_eq!(reader.read_bits(&data, 12), None);
        // Reader state should not have advanced
        assert_eq!(reader.bits_available(), 8); // Only 8 bits from the one byte
    }

    #[test]
    fn test_streaming_bit_reader_state_save_restore() {
        let data = [0xAB, 0xCD];
        let mut reader = StreamingBitReader::new();

        reader.read_bits(&data, 4);
        let state = reader.save_state();

        reader.read_bits(&data, 8);
        assert_eq!(reader.bytes_consumed(), 2);

        reader.restore_state(state);
        assert_eq!(reader.read_bits(&data, 4), Some(0xA)); // Same as before
    }

    #[test]
    fn test_streaming_huffman_tree_basic() {
        // Create a simple tree with just a few symbols
        let mut lengths = vec![0u8; 256];
        lengths[b'A' as usize] = 2;
        lengths[b'B' as usize] = 2;
        lengths[b'C' as usize] = 2;
        lengths[b'D' as usize] = 2;

        let tree = StreamingHuffmanTree::from_lengths(&lengths, 8).expect("Failed to create tree");
        assert_eq!(tree.max_length, 2);
    }

    #[test]
    fn test_streaming_decoder_stored() {
        let data = b"Hello, World!";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = vec![0u8; data.len()];

        let (consumed, produced, status) = decoder
            .decompress(data, &mut output)
            .expect("Decompress failed");

        assert_eq!(consumed, data.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output, data);
    }

    #[test]
    fn test_streaming_decoder_stored_chunked() {
        let data = b"Hello, World!";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = Vec::new();
        let mut input_pos = 0;

        // Process in small chunks
        while input_pos < data.len() {
            let chunk_size = 3.min(data.len() - input_pos);
            let mut chunk_output = vec![0u8; chunk_size];

            let (consumed, produced, status) = decoder
                .decompress(&data[input_pos..input_pos + chunk_size], &mut chunk_output)
                .expect("Decompress failed");

            input_pos += consumed;
            output.extend_from_slice(&chunk_output[..produced]);

            if status == DecompressStatus::Done {
                break;
            }
        }

        assert_eq!(output, data);
    }

    #[test]
    fn test_streaming_decoder_phases() {
        // Test that decoder starts in correct phase
        let decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);
        assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);

        let stored_decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, 100);
        assert_eq!(stored_decoder.phase(), DecoderPhase::DecodeBlock);
    }

    #[test]
    fn test_streaming_decoder_reset() {
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);

        // Simulate some progress
        decoder.bytes_decoded = 50;
        decoder.phase = DecoderPhase::DecodeBlock;

        decoder.reset();

        assert_eq!(decoder.bytes_decoded(), 0);
        assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);
        assert!(!decoder.is_finished());
    }

    #[test]
    fn test_streaming_decoder_stored_small_output_buffer() {
        let data = b"Hello, World! This is a longer test string.";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = Vec::new();
        let mut input_pos = 0;

        // Process with small output buffer (5 bytes)
        loop {
            let mut chunk_output = vec![0u8; 5];
            let input_slice = &data[input_pos..];

            let (consumed, produced, status) = decoder
                .decompress(input_slice, &mut chunk_output)
                .expect("Decompress failed");

            input_pos += consumed;
            output.extend_from_slice(&chunk_output[..produced]);

            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput => {
                    if input_pos >= data.len() {
                        break;
                    }
                }
                DecompressStatus::NeedsOutput => {
                    // Continue with more output space
                }
                _ => {}
            }
        }

        assert_eq!(output, data);
    }
}
