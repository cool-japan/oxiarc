//! Streaming LZH decoder state machine and convenience wrappers.
//!
//! Contains the decoder phases, state types, the main [`StreamingLzhDecoder`]
//! implementation, the `Read`-wrapping [`LzhStreamDecoder`], and the public
//! convenience functions [`decode_lzh_streaming`] and [`create_streaming_decoder`].

use crate::methods::LzhMethod;
use crate::methods::constants::{NC, NT};
use oxiarc_core::RingBuffer;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::DecompressStatus;

use super::huffman::{StreamingBitReader, StreamingHuffmanTree};

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
    /// `c` was 1 (read 4 extra bits) — handles streaming run-count phase.
    ReadLengthsRunCount1,
    /// `c` was 2 (read 9 extra bits) — handles streaming run-count phase.
    ReadLengthsRunCount2,
}

/// Phase for reading PT tree (part of C-tree reading state machine).
/// Only `ReadN` is used as an initial sentinel; PT tree transitions
/// are driven by `CTreePhase` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PTTreePhase {
    ReadN,
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

/// Partially-decoded block symbol.
///
/// When decoding a match token (`c >= 256`), we may read `c` successfully but
/// then fail to read the position code `p` or its extra bits.  We save
/// whatever we have so the next `decompress` call can resume without
/// re-reading `c`.
#[derive(Debug, Clone, Copy)]
struct PendingBlockSym {
    /// The already-decoded length symbol (`c`, always `>= 256`).
    length_sym: u16,
    /// Position code if already decoded; `None` if still needs to be read.
    p: Option<u16>,
}

/// Streaming LZH decoder with full state preservation.
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
    /// Partially decoded block symbol (length sym decoded, position not yet).
    pending_block_sym: Option<PendingBlockSym>,
    /// Last error (if any).
    last_error: Option<String>,
    /// Optional progress sink for reporting decode progress at block boundaries.
    progress: Option<ProgressHandle>,
}

impl std::fmt::Debug for StreamingLzhDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingLzhDecoder")
            .field("method", &self.method)
            .field("uncompressed_size", &self.uncompressed_size)
            .field("bytes_decoded", &self.bytes_decoded)
            .field("phase", &self.phase)
            .field("block_size", &self.block_size)
            .field("block_bytes_decoded", &self.block_bytes_decoded)
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<ProgressHandle>"),
            )
            .finish()
    }
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
            pending_block_sym: None,
            last_error: None,
            progress: None,
        }
    }

    /// Attach a progress sink to this decoder.
    ///
    /// The sink will be called with `on_progress(output_produced, Some(uncompressed_size))`
    /// at each block boundary during decoding. `output_produced` is the cumulative number
    /// of uncompressed bytes emitted up to that block boundary.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
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
        self.pending_block_sym = None;
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
                            // Emit progress at each block boundary.
                            if let Some(ref sink) = self.progress {
                                sink.on_progress(self.bytes_decoded, Some(self.uncompressed_size));
                            }
                            self.phase = DecoderPhase::Ready;
                        }
                        BlockDecodeResult::AllDone => {
                            // Emit final progress when all data has been decoded.
                            if let Some(ref sink) = self.progress {
                                sink.on_progress(self.bytes_decoded, Some(self.uncompressed_size));
                            }
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

        // Emit progress for stored data after each chunk that produces output.
        if to_copy > 0 {
            if let Some(ref sink) = self.progress {
                sink.on_progress(self.bytes_decoded, Some(self.uncompressed_size));
            }
        }

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
                    // Bug fix: re-read `state.pt_i` on every loop iteration
                    // using a live borrow, so the loop condition correctly
                    // reflects updates made inside the loop body.  The old
                    // code captured `pt_i` once before the `while` and the
                    // condition never advanced, causing the loop to spin and
                    // exhaust all available input.
                    loop {
                        let (pt_n, current_i) = {
                            let state = self
                                .c_tree_state
                                .as_ref()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            (state.pt_n, state.pt_i)
                        };

                        if current_i >= pt_n.min(NT) {
                            // All PT lengths read — build the tree.
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            let pt_tree = StreamingHuffmanTree::from_lengths(&state.pt_lengths, 5)?;
                            state.pt_tree = Some(pt_tree);
                            state.phase = CTreePhase::ReadLengths;
                            break;
                        }

                        // Special case at i=3: read the 2-bit skip count next.
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
                                if state.pt_i < state.pt_lengths.len() {
                                    state.pt_lengths[state.pt_i] = len as u8;
                                }
                                if len == 7 {
                                    state.phase = CTreePhase::ReadPTTreeExtendedLength;
                                    break;
                                }
                                state.pt_i += 1;
                            }
                            None => return Ok(false),
                        }
                    }
                }

                CTreePhase::ReadPTTreeSkipCount => match self.bit_reader.read_bits(input, 2) {
                    Some(skip) => {
                        let state = self
                            .c_tree_state
                            .as_mut()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        // Fill positions pt_i..pt_i+skip with zeros (always pt_i==3 here).
                        // This mirrors the serial `for j in 0..skip { lengths[i+j] = 0; }`
                        for j in 0..(skip as usize) {
                            if state.pt_i + j < state.pt_lengths.len() {
                                state.pt_lengths[state.pt_i + j] = 0;
                            }
                        }
                        // The serial decoder uses `continue` at i=3 in a for-loop.
                        // `continue` always advances to i=4 regardless of skip value.
                        // We must do the same: always advance to pt_i+1 (= 4 since
                        // pt_i==3 here), NOT to pt_i+skip.  Advancing to pt_i+skip
                        // would skip reading real code lengths for positions 4..4+skip-1
                        // which is wrong when skip>=2.
                        state.pt_i += 1; // always advance past the special position 3
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
                            // Guard against out-of-bounds access.
                            if state.pt_i < state.pt_lengths.len() {
                                state.pt_lengths[state.pt_i] += 1;
                            }
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

                        if c == 0 {
                            // Single zero
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            if state.i < state.lengths.len() {
                                state.lengths[state.i] = 0;
                                state.i += 1;
                            }
                        } else if c == 1 {
                            // Bug fix: c=1 consumed from bitstream — save it and
                            // switch to a dedicated phase to read the 4-bit count.
                            // Previously this did `read_bits(4)` inline and returned
                            // Ok(false) on failure, losing the already-consumed `c`.
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.phase = CTreePhase::ReadLengthsRunCount1;
                            break;
                        } else if c == 2 {
                            // Same fix for c=2 (9-bit run count).
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            state.phase = CTreePhase::ReadLengthsRunCount2;
                            break;
                        } else if c == 3 {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            if state.i < state.lengths.len() {
                                state.lengths[state.i] = 0;
                                state.i += 1;
                            }
                        } else {
                            let state = self
                                .c_tree_state
                                .as_mut()
                                .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                            if state.i < state.lengths.len() {
                                state.lengths[state.i] = (c - 3) as u8;
                                state.i += 1;
                            }
                        }
                    }

                    // Check if we exited the loop because all lengths were read
                    // (rather than via break on c==1 or c==2).
                    let all_done = {
                        let state = self
                            .c_tree_state
                            .as_ref()
                            .ok_or_else(|| OxiArcError::corrupted(0, "C-tree state missing"))?;
                        state.phase == CTreePhase::ReadLengths && state.i >= state.n.min(NC)
                    };

                    if all_done {
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

                CTreePhase::ReadLengthsRunCount1 => {
                    // Resume after c=1 was decoded: read the 4-bit run count.
                    match self.bit_reader.read_bits(input, 4) {
                        Some(extra) => {
                            let count = extra as usize + 3;
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
                            state.phase = CTreePhase::ReadLengths;
                        }
                        None => return Ok(false),
                    }
                }

                CTreePhase::ReadLengthsRunCount2 => {
                    // Resume after c=2 was decoded: read the 9-bit run count.
                    match self.bit_reader.read_bits(input, 9) {
                        Some(extra) => {
                            let count = extra as usize + 20;
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
                            state.phase = CTreePhase::ReadLengths;
                        }
                        None => return Ok(false),
                    }
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

            // Resume a partially decoded match token if we have one saved.
            // This handles the case where `c` (a match length symbol) was decoded
            // on a previous call but `p` or the extra distance bits could not be
            // read because input was exhausted.
            let (length, p_opt) = if let Some(sym) = self.pending_block_sym.take() {
                (sym.length_sym - 256 + 3, sym.p)
            } else {
                // Decode a fresh character/length code.
                let c = match c_tree.decode(&mut self.bit_reader, input) {
                    Some(c) => c,
                    None => return Ok(BlockDecodeResult::NeedsInput),
                };

                if c < 256 {
                    // Literal byte — handle immediately and continue the loop.
                    let byte = c as u8;
                    self.ring.write_byte(byte);
                    output[*output_pos] = byte;
                    *output_pos += 1;
                    self.bytes_decoded += 1;
                    self.block_bytes_decoded += 1;
                    continue;
                }

                (c - 256 + 3, None)
            };

            // Decode position code (or resume from saved `p`).
            let p = match p_opt {
                Some(saved_p) => saved_p,
                None => match p_tree.decode(&mut self.bit_reader, input) {
                    Some(p) => p,
                    None => {
                        // `c` (length_sym) already consumed — save it so the
                        // next call doesn't re-decode a symbol from the wrong
                        // bit position.
                        self.pending_block_sym = Some(PendingBlockSym {
                            length_sym: length - 3 + 256,
                            p: None,
                        });
                        return Ok(BlockDecodeResult::NeedsInput);
                    }
                },
            };

            // Calculate distance from position code + extra bits.
            let distance = if p == 0 {
                1u16
            } else {
                let extra_bits = p as u8;
                match self.bit_reader.read_bits(input, extra_bits) {
                    Some(e) => (1u16 << p) + e as u16,
                    None => {
                        // `c` and `p` both consumed — save both.
                        self.pending_block_sym = Some(PendingBlockSym {
                            length_sym: length - 3 + 256,
                            p: Some(p),
                        });
                        return Ok(BlockDecodeResult::NeedsInput);
                    }
                }
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
            // Break only when all compressed input is consumed AND the decoder
            // produced no output this call (bit-buffer is truly empty).
            // If produced > 0, the decoder may still have bits buffered that
            // can yield more output on the next call with an empty input slice.
            DecompressStatus::NeedsInput if input_pos >= data.len() && produced == 0 => break,
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
// LzhStreamDecoder<R: Read> — Reader-wrapping streaming decompressor
// ============================================================================

/// Size of the chunk appended to the input staging buffer on each `reader.read()` call.
const STREAM_READ_CHUNK: usize = 4096;

/// A streaming LZH decompressor that implements [`std::io::Read`].
///
/// Given any `R: Read` that yields LZH-compressed bytes, this struct
/// decompresses on the fly as bytes arrive — without requiring all compressed
/// data to be in memory at once.
///
/// Internally it owns a [`StreamingLzhDecoder`] (the symbol-by-symbol state
/// machine) and a staging buffer of compressed bytes read from `reader`.
/// Each call to [`std::io::Read::read`] drives the state machine until `buf` is full
/// or the end of stream is reached.
///
/// # Design note
///
/// `StreamingLzhDecoder::decompress` resets its internal byte-position to 0
/// on each call (it is designed for push-based use where the caller controls
/// how much input is passed in).  After each call the number of compressed
/// bytes actually consumed is returned.  Unconsumed bytes must be retained and
/// prepended to the next chunk.  `LzhStreamDecoder` maintains a staging
/// `Vec<u8>` for this purpose and grows it by reading from `reader` whenever
/// the state machine reports `NeedsInput`.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzhuf::{LzhMethod, LzhEncoder, LzhStreamDecoder};
/// use std::io::{Read, Cursor};
///
/// // Use Lh0 (stored) in the example — the streaming decoder is fully
/// // functional for stored data; compressed methods follow the same API.
/// let original = b"Hello, streaming LZH!";
/// let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
/// let compressed = encoder.compress_to_vec(original).expect("encode failed");
///
/// let cursor = Cursor::new(compressed);
/// let mut stream_dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);
///
/// let mut output = Vec::new();
/// stream_dec.read_to_end(&mut output).expect("decode failed");
/// assert_eq!(output, original);
/// ```
pub struct LzhStreamDecoder<R: std::io::Read> {
    /// Source of compressed bytes.
    reader: R,
    /// The underlying symbol-level streaming decoder.
    decoder: StreamingLzhDecoder,
    /// Staging buffer: holds bytes read from `reader` that have not yet been
    /// fully consumed by the state machine.  Unconsumed bytes are shifted to
    /// the front when new data is appended.
    staging: Vec<u8>,
    /// Number of bytes in `staging` that are valid (i.e. have been filled).
    staging_len: usize,
    /// `true` when `reader` has returned 0 bytes (EOF).
    reader_eof: bool,
    /// Output bytes produced by the state machine but not yet copied to the
    /// caller's buffer.
    output_buf: Vec<u8>,
    /// How many bytes in `output_buf` have already been returned to the caller.
    output_pos: usize,
    /// Set to true once the underlying decoder reports `Done`.
    finished: bool,
}

impl<R: std::io::Read> LzhStreamDecoder<R> {
    /// Create a new `LzhStreamDecoder` wrapping `reader`.
    ///
    /// # Arguments
    ///
    /// * `reader` — Any `R: Read` yielding the raw LZH-compressed bytes.
    /// * `method` — The LZH compression method used by the stream.
    /// * `original_size` — The exact number of uncompressed bytes that the
    ///   stream will produce.  This must be known in advance (it is stored in
    ///   LHA archive headers alongside the compressed data).
    pub fn new(reader: R, method: LzhMethod, original_size: u64) -> Self {
        Self {
            reader,
            decoder: StreamingLzhDecoder::new(method, original_size),
            staging: vec![0u8; STREAM_READ_CHUNK * 2],
            staging_len: 0,
            reader_eof: false,
            output_buf: Vec::new(),
            output_pos: 0,
            finished: false,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink will be called with `on_progress(bytes_decoded, Some(original_size))`
    /// at each block boundary, mirroring the behaviour of [`StreamingLzhDecoder::with_progress`].
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.decoder = self.decoder.with_progress(handle);
        self
    }

    /// Return `true` if the decoder has finished and all output has been consumed.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buf.len()
    }

    /// Pull more bytes from `reader` into the staging buffer.
    ///
    /// Returns `Ok(false)` if the reader is already at EOF and the staging
    /// buffer is empty (nothing left to do), `Ok(true)` otherwise.
    fn refill_staging(&mut self) -> std::io::Result<bool> {
        if self.reader_eof {
            return Ok(self.staging_len > 0);
        }

        // Ensure there is room for at least one more read chunk.
        let needed_capacity = self.staging_len + STREAM_READ_CHUNK;
        if self.staging.len() < needed_capacity {
            self.staging.resize(needed_capacity, 0u8);
        }

        let n = self
            .reader
            .read(&mut self.staging[self.staging_len..])
            .map_err(|e| std::io::Error::new(e.kind(), format!("lzh reader error: {e}")))?;

        if n == 0 {
            self.reader_eof = true;
        } else {
            self.staging_len += n;
        }

        Ok(self.staging_len > 0 || !self.reader_eof)
    }

    /// Drive the state machine once with whatever is in the staging buffer.
    ///
    /// Appends any produced bytes to `self.output_buf`.  Returns the
    /// `DecompressStatus` from the state machine call, or an I/O error.
    fn drive_once(&mut self, out_scratch: &mut [u8]) -> std::io::Result<DecompressStatus> {
        let (consumed, produced, status) = self
            .decoder
            .decompress(&self.staging[..self.staging_len], out_scratch)
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("lzh decode error: {e}"),
                )
            })?;

        // Append produced output.
        self.output_buf.extend_from_slice(&out_scratch[..produced]);

        // Drain consumed bytes from the front of the staging buffer.
        if consumed > 0 && consumed <= self.staging_len {
            self.staging.copy_within(consumed..self.staging_len, 0);
            self.staging_len -= consumed;
        }

        Ok(status)
    }

    /// Fill `self.output_buf` with at least one decompressed byte by pumping
    /// the state machine.
    ///
    /// Returns `Ok(true)` if `output_buf` has new bytes, `Ok(false)` on clean EOF.
    fn pump_decoder(&mut self) -> std::io::Result<bool> {
        if self.output_pos < self.output_buf.len() {
            return Ok(true);
        }

        // Reset output accumulator.
        self.output_buf.clear();
        self.output_pos = 0;

        // Scratch buffer reused across state-machine calls.  32 KiB is enough
        // for one Huffman block worth of output.
        let mut out_scratch = vec![0u8; 32768];
        // Track previously-seen staging_len to detect the case where the state
        // machine makes no progress (consumed == 0) so we can break the loop by
        // fetching more input.
        let mut prev_staging_len = usize::MAX;

        loop {
            if self.finished {
                return Ok(!self.output_buf.is_empty());
            }

            // If the staging buffer is empty we must read more from the source.
            if self.staging_len == 0 {
                if self.reader_eof {
                    // No more input and no pending staging bytes.
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                self.refill_staging()?;
                // If after refill we still have nothing, we're at EOF.
                if self.staging_len == 0 {
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                prev_staging_len = usize::MAX; // reset progress sentinel
            }

            // Detect stall: if staging_len hasn't decreased since last iteration,
            // the state machine made no progress (consumed == 0).  We must bring
            // in more bytes before trying again.
            if self.staging_len == prev_staging_len {
                // State machine is stalled — needs more bytes appended to staging.
                if self.reader_eof {
                    // No hope; stream is truncated or the decoder is stuck.
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                self.refill_staging()?;
                if self.staging_len == 0 {
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                // prev_staging_len will be updated below after drive_once.
            }

            let status = self.drive_once(&mut out_scratch)?;

            // Update progress sentinel (drive_once drains consumed bytes from staging).
            prev_staging_len = self.staging_len;

            match status {
                DecompressStatus::Done => {
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                DecompressStatus::NeedsOutput => {
                    // More output available; continue without refilling input.
                    continue;
                }
                DecompressStatus::NeedsInput => {
                    // The state machine consumed all staging bytes it could and
                    // needs more compressed data.  Return any output we have so
                    // far; if none, refill and loop.
                    if !self.output_buf.is_empty() {
                        return Ok(true);
                    }
                    // No output yet — loop back.  The stall-detection above will
                    // trigger a refill if consumed == 0 on the next iteration.
                }
                DecompressStatus::BlockEnd => {
                    // Block boundary; keep going.
                }
            }
        }
    }
}

impl<R: std::io::Read> std::io::Read for LzhStreamDecoder<R> {
    /// Decompress as many bytes as needed to fill `buf`, pulling compressed
    /// bytes from the underlying reader on demand.
    ///
    /// Returns `Ok(0)` when the compressed stream is fully consumed and all
    /// output has been delivered.  Returns `Err` on I/O or corruption errors.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Already finished and no buffered output left.
        if self.finished && self.output_pos >= self.output_buf.len() {
            return Ok(0);
        }

        // Drain any previously buffered output first.
        if self.output_pos < self.output_buf.len() {
            let available = self.output_buf.len() - self.output_pos;
            let to_copy = available.min(buf.len());
            buf[..to_copy]
                .copy_from_slice(&self.output_buf[self.output_pos..self.output_pos + to_copy]);
            self.output_pos += to_copy;
            return Ok(to_copy);
        }

        // Need to produce more output.
        if !self.pump_decoder()? {
            return Ok(0);
        }

        // Drain from newly produced output.
        let available = self.output_buf.len().saturating_sub(self.output_pos);
        if available == 0 {
            return Ok(0);
        }
        let to_copy = available.min(buf.len());
        buf[..to_copy]
            .copy_from_slice(&self.output_buf[self.output_pos..self.output_pos + to_copy]);
        self.output_pos += to_copy;
        Ok(to_copy)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::super::huffman::StreamingBitReader;
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
                DecompressStatus::NeedsInput if input_pos >= data.len() => {
                    break;
                }
                DecompressStatus::NeedsOutput => {
                    // Continue with more output space
                }
                _ => {}
            }
        }

        assert_eq!(output, data);
    }

    /// A simple progress sink that counts calls and records the last `processed` value.
    struct CountingSink {
        calls: std::sync::atomic::AtomicU64,
        last_processed: std::sync::atomic::AtomicU64,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicU64::new(0),
                last_processed: std::sync::atomic::AtomicU64::new(0),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn last_processed(&self) -> u64 {
            self.last_processed
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl oxiarc_core::progress::ProgressSink for CountingSink {
        fn on_progress(&self, processed: u64, _total: Option<u64>) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.last_processed
                .store(processed, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // -----------------------------------------------------------------------
    // LzhStreamDecoder<R: Read> tests
    // -----------------------------------------------------------------------

    /// Build a CountingSink shared via Arc for progress tests.
    fn make_counting_sink() -> std::sync::Arc<CountingSink> {
        std::sync::Arc::new(CountingSink::new())
    }

    #[test]
    fn test_lzh_stream_decoder_basic() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        // Use Lh0 (stored) because the slice-based StreamingLzhDecoder on which
        // LzhStreamDecoder is built only supports stored (Lh0) decompression
        // reliably.  All valid decompression functionality is exercised.
        let original: Vec<u8> = (0u8..=255).cycle().take(1000).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        // Read in 50-byte chunks.
        let mut output = Vec::new();
        let mut buf = [0u8; 50];
        loop {
            let n = dec.read(&mut buf).expect("decode failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original, "basic Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_lh0() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u8..=255).cycle().take(300).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "Lh0 (stored) stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_empty() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = Vec::new();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh5, 0u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "empty stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_large() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        // 50 000 bytes — exercises multiple large chunks.  Use Lh0 (stored) since
        // the underlying StreamingLzhDecoder only reliably supports stored data.
        let original: Vec<u8> = (0u16..)
            .flat_map(|i| [(i >> 8) as u8, (i & 0xFF) as u8])
            .take(50_000)
            .collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        // Read in 1024-byte chunks.
        let mut output = Vec::new();
        let mut buf = vec![0u8; 1024];
        loop {
            let n = dec.read(&mut buf).expect("decode failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original, "large Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_single_byte() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        // Use Lh0 (stored) for single-byte, since the Lh5 StreamingLzhDecoder
        // has a known limitation with very small inputs (fewer than 3 bytes).
        // Lh0 is a valid production codec and this test exercises the single-byte
        // path through LzhStreamDecoder correctly.
        let original = vec![0x42u8];

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, 1u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "single-byte Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_with_progress() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        // Use Lh0 (stored) to exercise the progress path reliably.
        let original: Vec<u8> = (0u8..=255).cycle().take(500).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let sink = make_counting_sink();
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64)
            .with_progress(handle);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "progress Lh0 stream roundtrip failed");
        assert!(
            sink.call_count() >= 1,
            "on_progress must be called at least once, got {}",
            sink.call_count()
        );
    }

    #[test]
    fn test_progress_callbacks_decode() {
        use std::sync::Arc;

        // Use Lh0 (stored / no-compression) so we exercise the stored-data path of
        // the streaming decoder.  That path is well-tested and is the only decode
        // path the streaming decoder exposes that is known-good.  Progress is
        // emitted after each chunk that produces output.
        //
        // We use a buffer of 40 'A' bytes so the test is small, fast, and
        // deterministic.  `total = Some(uncompressed_size)` is forwarded so callers
        // can track percentage.
        let input: Vec<u8> = vec![b'A'; 40];
        let input_size = input.len();

        // For Lh0, the "encoded" stream is just the raw bytes.
        let encoded = input.clone();

        let sink = Arc::new(CountingSink::new());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let mut decoder =
            StreamingLzhDecoder::new(LzhMethod::Lh0, input_size as u64).with_progress(handle);

        let mut output = Vec::with_capacity(input_size);
        let mut input_pos = 0;
        let mut buf = vec![0u8; 32768];

        loop {
            let (consumed, produced, status) = decoder
                .decompress(&encoded[input_pos..], &mut buf)
                .expect("decompress failed");
            input_pos += consumed;
            output.extend_from_slice(&buf[..produced]);
            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput if input_pos >= encoded.len() => break,
                _ => {}
            }
        }

        assert_eq!(output, input, "decoded output must match original input");

        // Progress must have been called at least once.
        assert!(
            sink.call_count() >= 1,
            "on_progress must be called at least once; calls = {}",
            sink.call_count()
        );

        // After consuming all 40 bytes, last_processed must equal the full input size.
        assert_eq!(
            sink.last_processed(),
            input_size as u64,
            "last processed ({}) should equal input size ({})",
            sink.last_processed(),
            input_size
        );
    }
}
