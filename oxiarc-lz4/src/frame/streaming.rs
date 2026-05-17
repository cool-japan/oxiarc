//! LZ4 bounded-memory true streaming compressor and decompressor.
//!
//! Both [`Lz4Compressor`] and [`Lz4Decompressor`] implement block-level
//! streaming: compressed blocks are emitted / consumed one at a time, so
//! neither accumulates the full input/output in memory.

use super::types::{FrameDescriptor, LZ4_FRAME_MAGIC};
use crate::block::{compress_block, decompress_block};
use crate::xxhash::{XxHash32, xxhash32};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};

// ─────────────────────────────────────────────────────────────────────────────
// Default memory budget constants
// ─────────────────────────────────────────────────────────────────────────────

/// Default maximum input-buffer size for the compressor (16 MiB).
const COMPRESSOR_DEFAULT_BUDGET: usize = 16 * 1024 * 1024;

/// Default maximum input-buffer size for the decompressor (64 MiB).
const DECOMPRESSOR_DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────────────
// Lz4Compressor
// ─────────────────────────────────────────────────────────────────────────────

/// LZ4 compressor implementing the [`Compressor`] trait with true block-level
/// streaming.
///
/// Input is accumulated in an internal buffer.  Whenever the buffer reaches
/// `block_max_size` bytes a complete LZ4 block is compressed and appended to
/// an output staging buffer.  On [`FlushMode::Finish`] the remaining (possibly
/// partial) block is flushed, and the end-marker / content-checksum are
/// written.  Each [`Compressor::compress`] call drains whatever is available
/// in the staging buffer into the caller-supplied output slice.
///
/// # Memory budget
///
/// Call [`Lz4Compressor::with_memory_budget`] to cap the total un-flushed
/// input.  If the accumulator exceeds the budget between block boundaries an
/// error is returned.  The default cap is 16 MiB.
pub struct Lz4Compressor {
    desc: FrameDescriptor,
    progress: Option<ProgressHandle>,
    cancel: Option<CancellationToken>,
    /// Un-compressed input accumulated since the last full-block flush.
    input_buf: Vec<u8>,
    /// Compressed output ready to be handed to the caller.
    output_buf: Vec<u8>,
    /// Position in `output_buf` that has already been written to the caller.
    output_pos: usize,
    /// Whether the LZ4 frame header has already been pushed to `output_buf`.
    header_written: bool,
    /// Running content checksum (when `desc.content_checksum` is true).
    content_hasher: XxHash32,
    /// Number of full blocks already emitted.
    block_count: u64,
    /// Whether [`FlushMode::Finish`] has been processed.
    finished: bool,
    /// Maximum un-flushed input before returning a memory-budget error.
    memory_budget: usize,
}

impl std::fmt::Debug for Lz4Compressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4Compressor")
            .field("input_buf_len", &self.input_buf.len())
            .field(
                "output_buf_pending",
                &(self.output_buf.len() - self.output_pos),
            )
            .field("header_written", &self.header_written)
            .field("block_count", &self.block_count)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Lz4Compressor {
    /// Create a new LZ4 compressor with default options.
    pub fn new() -> Self {
        Self::with_options(FrameDescriptor::new())
    }

    /// Create a new LZ4 compressor with custom options.
    pub fn with_options(desc: FrameDescriptor) -> Self {
        Self {
            desc,
            progress: None,
            cancel: None,
            input_buf: Vec::new(),
            output_buf: Vec::new(),
            output_pos: 0,
            header_written: false,
            content_hasher: XxHash32::new(),
            block_count: 0,
            finished: false,
            memory_budget: COMPRESSOR_DEFAULT_BUDGET,
        }
    }

    /// Attach a progress sink.
    ///
    /// `on_progress(bytes, None)` is called once per full block with the total
    /// number of raw (uncompressed) bytes processed so far.  `on_finish()` is
    /// called when [`FlushMode::Finish`] completes.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`Compressor::compress`] call.
    /// If cancelled, the call returns [`OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Set the maximum un-flushed input buffer size.
    ///
    /// If the accumulator grows beyond this limit before a full block can be
    /// emitted, [`Compressor::compress`] returns a
    /// [`OxiArcError::BufferTooSmall`]-style error.  Defaults to 16 MiB.
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    // ── internal helpers ──────────────────────────────────────────────────

    /// Serialize the frame header into `self.output_buf`.
    fn write_frame_header(&mut self) {
        let out = &mut self.output_buf;

        // Magic number
        out.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

        // FLG and BD bytes
        let flg = self.desc.flg_byte();
        let bd = self.desc.block_max_size.to_bd();
        out.push(flg);
        out.push(bd);

        // Content size (8 bytes, if requested).
        // For streaming we don't know the total ahead of time so we write 0.
        if let Some(size) = self.desc.content_size {
            out.extend_from_slice(&size.to_le_bytes());
        }

        // Dictionary ID (4 bytes, if present)
        if let Some(dict_id) = self.desc.dict_id {
            out.extend_from_slice(&dict_id.to_le_bytes());
        }

        // Header checksum: XXH32(FLG..optional_fields) >> 8, keep 1 byte.
        let header_end = out.len();
        let hc = (xxhash32(&out[4..header_end]) >> 8) as u8;
        out.push(hc);
    }

    /// Compress `data` as one LZ4 block and append it (with its 4-byte length
    /// prefix) to `self.output_buf`.  Also updates `self.content_hasher`.
    fn emit_block(&mut self, data: &[u8]) -> Result<()> {
        // Update content checksum with the raw data.
        self.content_hasher.update(data);

        let compressed = compress_block(data)?;

        if compressed.len() < data.len() {
            // Emit as compressed block.
            let block_len = compressed.len() as u32;
            self.output_buf.extend_from_slice(&block_len.to_le_bytes());
            self.output_buf.extend_from_slice(&compressed);

            if self.desc.block_checksum {
                let ck = xxhash32(&compressed);
                self.output_buf.extend_from_slice(&ck.to_le_bytes());
            }
        } else {
            // Emit as uncompressed block (high bit set in length prefix).
            let block_len = (data.len() as u32) | 0x8000_0000;
            self.output_buf.extend_from_slice(&block_len.to_le_bytes());
            self.output_buf.extend_from_slice(data);

            if self.desc.block_checksum {
                let ck = xxhash32(data);
                self.output_buf.extend_from_slice(&ck.to_le_bytes());
            }
        }

        self.block_count += 1;
        Ok(())
    }

    /// Drain as many complete blocks as possible from `self.input_buf` into
    /// `self.output_buf`.
    fn flush_complete_blocks(&mut self) -> Result<()> {
        let block_size = self.desc.block_max_size.size_bytes();

        while self.input_buf.len() >= block_size {
            // Drain exactly one block worth of bytes.
            let block_data: Vec<u8> = self.input_buf.drain(..block_size).collect();
            self.emit_block(&block_data)?;

            if let Some(ref handle) = self.progress {
                let raw_bytes = self.block_count * block_size as u64;
                handle.on_progress(raw_bytes, None);
            }
        }

        Ok(())
    }

    /// Copy bytes from `self.output_buf[self.output_pos..]` into `output`.
    /// Returns the number of bytes written.
    fn drain_output_buf(&mut self, output: &mut [u8]) -> usize {
        let pending = &self.output_buf[self.output_pos..];
        let to_copy = pending.len().min(output.len());
        output[..to_copy].copy_from_slice(&pending[..to_copy]);
        self.output_pos += to_copy;

        // Reclaim memory once everything is consumed.
        if self.output_pos == self.output_buf.len() {
            self.output_buf.clear();
            self.output_pos = 0;
        }

        to_copy
    }
}

impl Default for Lz4Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor for Lz4Compressor {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        // If already finished but there is still staged output, drain it.
        if self.finished {
            let written = self.drain_output_buf(output);
            let status = if self.output_buf.len() > self.output_pos {
                CompressStatus::NeedsOutput
            } else {
                CompressStatus::Done
            };
            return Ok((0, written, status));
        }

        // Cooperative cancellation.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Emit the frame header on first call.
        if !self.header_written {
            self.write_frame_header();
            self.header_written = true;
        }

        // Accumulate input.
        self.input_buf.extend_from_slice(input);

        // Budget check: input_buf must not exceed the memory budget.
        if self.input_buf.len() > self.memory_budget {
            return Err(OxiArcError::buffer_too_small(
                self.memory_budget,
                self.input_buf.len(),
            ));
        }

        // Flush all complete blocks.
        self.flush_complete_blocks()?;

        if matches!(flush, FlushMode::Finish) {
            // Emit the final (possibly partial) block if any data remains.
            if !self.input_buf.is_empty() {
                let last_block: Vec<u8> = self.input_buf.drain(..).collect();
                self.emit_block(&last_block)?;
            }

            // End marker (4 zero bytes).
            self.output_buf.extend_from_slice(&0u32.to_le_bytes());

            // Content checksum.
            if self.desc.content_checksum {
                let ck = self.content_hasher.finish();
                self.output_buf.extend_from_slice(&ck.to_le_bytes());
            }

            self.finished = true;

            if let Some(ref handle) = self.progress {
                let total_raw = self.block_count * self.desc.block_max_size.size_bytes() as u64;
                handle.on_progress(total_raw, None);
                handle.on_finish();
            }
        }

        // Copy whatever is staged to the caller's output slice.
        let written = self.drain_output_buf(output);

        let status = if self.finished && self.output_buf.len() <= self.output_pos {
            // Fully finished and all output drained.
            CompressStatus::Done
        } else if self.output_buf.len() > self.output_pos {
            // More staged output waiting — caller must call again.
            CompressStatus::NeedsOutput
        } else if self.finished {
            CompressStatus::Done
        } else {
            CompressStatus::NeedsInput
        };

        Ok((input.len(), written, status))
    }

    fn reset(&mut self) {
        self.input_buf.clear();
        self.output_buf.clear();
        self.output_pos = 0;
        self.header_written = false;
        self.content_hasher = XxHash32::new();
        self.block_count = 0;
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Lz4Decompressor — state machine
// ─────────────────────────────────────────────────────────────────────────────

/// Parse state for the streaming LZ4 frame decompressor.
#[derive(Debug, Clone)]
enum DecompressState {
    /// Waiting to accumulate enough bytes to parse the frame header.
    NeedHeader,
    /// Waiting for the 4-byte block-length prefix.
    NeedBlock,
    /// Waiting for the block payload (and optionally a block checksum).
    NeedBlockData {
        /// Raw block payload length (high-bit stripped).
        block_len: u32,
        /// `true` when the high bit was set in the length prefix, meaning the
        /// block payload is stored uncompressed.
        is_uncompressed: bool,
    },
    /// Saw end-marker; waiting for the optional 4-byte content checksum.
    EndMarker,
    /// The complete frame has been successfully decompressed.
    Done,
}

// ─────────────────────────────────────────────────────────────────────────────
// Lz4Decompressor
// ─────────────────────────────────────────────────────────────────────────────

/// LZ4 decompressor implementing the [`Decompressor`] trait with true
/// block-level streaming.
///
/// Bytes are fed in arbitrary-sized chunks.  The internal state machine
/// processes as many complete frame components (header, blocks, end-marker)
/// as the buffered input allows, emitting decompressed output incrementally.
///
/// # Memory budget
///
/// Call [`Lz4Decompressor::with_memory_budget`] to limit how many compressed
/// bytes may be buffered before being processed.  Exceeding the budget returns
/// an error.  Defaults to 64 MiB.
pub struct Lz4Decompressor {
    progress: Option<ProgressHandle>,
    cancel: Option<CancellationToken>,
    /// Bytes received but not yet consumed by the state machine.
    input_buf: Vec<u8>,
    /// Decompressed output awaiting delivery to the caller.
    output_buf: Vec<u8>,
    /// Position in `output_buf` already copied to the caller.
    output_pos: usize,
    /// State-machine position.
    state: DecompressState,
    /// Parsed frame descriptor (available once `NeedHeader` succeeds).
    desc: Option<FrameDescriptor>,
    /// Running content checksum (when `content_checksum` is set in `desc`).
    content_hasher: XxHash32,
    /// Maximum allowed size of `input_buf`.
    memory_budget: usize,
    /// Number of decompressed bytes emitted so far (for progress reporting).
    bytes_produced: u64,
}

impl std::fmt::Debug for Lz4Decompressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4Decompressor")
            .field("input_buf_len", &self.input_buf.len())
            .field(
                "output_buf_pending",
                &(self.output_buf.len() - self.output_pos),
            )
            .field("state", &self.state)
            .field("bytes_produced", &self.bytes_produced)
            .finish()
    }
}

impl Lz4Decompressor {
    /// Create a new LZ4 decompressor.
    pub fn new() -> Self {
        Self {
            progress: None,
            cancel: None,
            input_buf: Vec::new(),
            output_buf: Vec::new(),
            output_pos: 0,
            state: DecompressState::NeedHeader,
            desc: None,
            content_hasher: XxHash32::new(),
            memory_budget: DECOMPRESSOR_DEFAULT_BUDGET,
            bytes_produced: 0,
        }
    }

    /// Attach a progress sink.
    ///
    /// `on_progress(bytes, None)` is called after each block with the total
    /// decompressed byte count.  `on_finish()` is called when the frame is
    /// fully decompressed.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`Decompressor::decompress`]
    /// call.  If cancelled, the call returns [`OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Set the maximum input-buffer size.
    ///
    /// If the accumulator grows beyond this limit an error is returned.
    /// Defaults to 64 MiB.
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    // ── internal helpers ──────────────────────────────────────────────────

    /// Try to advance the state machine using bytes already in `self.input_buf`.
    ///
    /// Returns `Ok(true)` when at least one state transition was made (so the
    /// caller should keep looping), `Ok(false)` when we need more input.
    fn step(&mut self) -> Result<bool> {
        match self.state.clone() {
            DecompressState::NeedHeader => self.step_need_header(),
            DecompressState::NeedBlock => self.step_need_block(),
            DecompressState::NeedBlockData {
                block_len,
                is_uncompressed,
            } => self.step_need_block_data(block_len, is_uncompressed),
            DecompressState::EndMarker => self.step_end_marker(),
            DecompressState::Done => Ok(false),
        }
    }

    fn step_need_header(&mut self) -> Result<bool> {
        // Minimum header: 4 magic + 1 FLG + 1 BD + 1 HC = 7 bytes.
        // Optional extras: content_size (8) + dict_id (4).
        // We peek ahead to figure out the full header length.

        let buf = &self.input_buf;
        if buf.len() < 7 {
            return Ok(false);
        }

        // Validate magic.
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != LZ4_FRAME_MAGIC {
            return Err(OxiArcError::invalid_magic(
                LZ4_FRAME_MAGIC.to_le_bytes().to_vec(),
                buf[..4].to_vec(),
            ));
        }

        let flg = buf[4];
        let bd = buf[5];

        let has_content_size = (flg & 0x08) != 0;
        let has_dict_id = (flg & 0x01) != 0;

        // Total header length: 4(magic) + 1(FLG) + 1(BD)
        //   + optional 8 (content_size) + optional 4 (dict_id) + 1 (HC)
        let mut header_len = 7usize;
        if has_content_size {
            header_len += 8;
        }
        if has_dict_id {
            header_len += 4;
        }

        if buf.len() < header_len {
            return Ok(false); // need more bytes
        }

        // Parse descriptor.
        let mut desc = FrameDescriptor::parse(flg, bd)?;

        let mut pos = 6usize;

        // Content size.
        if has_content_size {
            let size = u64::from_le_bytes([
                buf[pos],
                buf[pos + 1],
                buf[pos + 2],
                buf[pos + 3],
                buf[pos + 4],
                buf[pos + 5],
                buf[pos + 6],
                buf[pos + 7],
            ]);
            desc.content_size = Some(size);
            pos += 8;
        }

        // Dictionary ID.
        if has_dict_id {
            let dict_id = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
            desc.dict_id = Some(dict_id);
            pos += 4;
        }

        // Header checksum (last byte before block data).
        let stored_hc = buf[pos];
        let header_data = &buf[4..pos]; // FLG..optional fields
        let computed_hc = (xxhash32(header_data) >> 8) as u8;
        if stored_hc != computed_hc {
            return Err(OxiArcError::crc_mismatch(
                computed_hc as u32,
                stored_hc as u32,
            ));
        }

        self.desc = Some(desc);
        self.input_buf.drain(..header_len);
        self.state = DecompressState::NeedBlock;
        Ok(true)
    }

    fn step_need_block(&mut self) -> Result<bool> {
        if self.input_buf.len() < 4 {
            return Ok(false);
        }

        let raw = u32::from_le_bytes([
            self.input_buf[0],
            self.input_buf[1],
            self.input_buf[2],
            self.input_buf[3],
        ]);
        self.input_buf.drain(..4);

        if raw == 0 {
            // End marker.
            self.state = DecompressState::EndMarker;
        } else {
            let is_uncompressed = (raw & 0x8000_0000) != 0;
            let block_len = raw & 0x7FFF_FFFF;

            // Sanity check against block_max_size.
            if let Some(ref d) = self.desc {
                let block_max = d.block_max_size.size_bytes() as u32;
                if block_len > block_max {
                    return Err(OxiArcError::corrupted(
                        0,
                        "block length exceeds block_max_size",
                    ));
                }
            }

            self.state = DecompressState::NeedBlockData {
                block_len,
                is_uncompressed,
            };
        }

        Ok(true)
    }

    fn step_need_block_data(&mut self, block_len: u32, is_uncompressed: bool) -> Result<bool> {
        let blen = block_len as usize;
        let need_block_checksum = self.desc.as_ref().is_some_and(|d| d.block_checksum);
        let total_needed = blen + if need_block_checksum { 4 } else { 0 };

        if self.input_buf.len() < total_needed {
            return Ok(false);
        }

        let block_data = self.input_buf[..blen].to_vec();

        // Verify optional per-block checksum.
        if need_block_checksum {
            let stored = u32::from_le_bytes([
                self.input_buf[blen],
                self.input_buf[blen + 1],
                self.input_buf[blen + 2],
                self.input_buf[blen + 3],
            ]);
            let computed = xxhash32(&block_data);
            if stored != computed {
                return Err(OxiArcError::crc_mismatch(computed, stored));
            }
        }

        self.input_buf.drain(..total_needed);

        // Decompress (or pass through if uncompressed).
        let block_max = self
            .desc
            .as_ref()
            .map_or(4 * 1024 * 1024, |d| d.block_max_size.size_bytes());

        let decompressed = if is_uncompressed {
            block_data
        } else {
            decompress_block(&block_data, block_max)?
        };

        // Update content checksum.
        self.content_hasher.update(&decompressed);
        self.bytes_produced += decompressed.len() as u64;

        // Stage output.
        self.output_buf.extend_from_slice(&decompressed);

        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_produced, None);
        }

        self.state = DecompressState::NeedBlock;
        Ok(true)
    }

    fn step_end_marker(&mut self) -> Result<bool> {
        let needs_checksum = self.desc.as_ref().is_some_and(|d| d.content_checksum);

        if needs_checksum {
            if self.input_buf.len() < 4 {
                return Ok(false);
            }

            let stored = u32::from_le_bytes([
                self.input_buf[0],
                self.input_buf[1],
                self.input_buf[2],
                self.input_buf[3],
            ]);
            self.input_buf.drain(..4);

            let computed = self.content_hasher.finish();
            if stored != computed {
                return Err(OxiArcError::crc_mismatch(computed, stored));
            }
        }

        self.state = DecompressState::Done;

        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }

        Ok(true)
    }

    /// Copy bytes from `self.output_buf[self.output_pos..]` into `output`.
    /// Returns the number of bytes written.
    fn drain_output_buf(&mut self, output: &mut [u8]) -> usize {
        let pending = &self.output_buf[self.output_pos..];
        let to_copy = pending.len().min(output.len());
        output[..to_copy].copy_from_slice(&pending[..to_copy]);
        self.output_pos += to_copy;

        if self.output_pos == self.output_buf.len() {
            self.output_buf.clear();
            self.output_pos = 0;
        }

        to_copy
    }
}

impl Default for Lz4Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Lz4Decompressor {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        // Short-circuit when already done.
        if matches!(self.state, DecompressState::Done) {
            // Drain any remaining staged output.
            let written = self.drain_output_buf(output);
            return Ok((0, written, DecompressStatus::Done));
        }

        // Cooperative cancellation.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Accumulate input.
        self.input_buf.extend_from_slice(input);

        // Budget check.
        if self.input_buf.len() > self.memory_budget {
            return Err(OxiArcError::buffer_too_small(
                self.memory_budget,
                self.input_buf.len(),
            ));
        }

        // Drive the state machine as far as possible.
        loop {
            match self.step() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => return Err(e),
            }
        }

        // Copy staged output to the caller.
        let written = self.drain_output_buf(output);

        let status = if matches!(self.state, DecompressState::Done) {
            DecompressStatus::Done
        } else if self.output_buf.len() > self.output_pos {
            // More decompressed data waiting — caller needs to call again.
            DecompressStatus::NeedsOutput
        } else {
            DecompressStatus::NeedsInput
        };

        Ok((input.len(), written, status))
    }

    fn reset(&mut self) {
        self.input_buf.clear();
        self.output_buf.clear();
        self.output_pos = 0;
        self.state = DecompressState::NeedHeader;
        self.desc = None;
        self.content_hasher = XxHash32::new();
        self.bytes_produced = 0;
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, DecompressState::Done)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::frame::compress::compress;
    use crate::frame::decompress::decompress as frame_decompress;
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
        let pattern = b"LZ4 streaming test data with repeating pattern ABCDEFGH ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk = &pattern[..remaining.min(pattern.len())];
            data.extend_from_slice(chunk);
        }
        data
    }

    /// Compress `data` in streaming mode, feeding `chunk_size` bytes per call.
    /// Returns the raw compressed bytes (frame format).
    fn stream_compress(data: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut compressor = Lz4Compressor::new();
        let mut compressed = Vec::new();
        let mut out_buf = vec![0u8; chunk_size * 4 + 1024];

        let mut pos = 0;
        loop {
            let end = (pos + chunk_size).min(data.len());
            let flush = if end == data.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };

            // First call: feed the actual chunk.
            let (_, written, mut status) = compressor
                .compress(&data[pos..end], &mut out_buf, flush)
                .expect("compress failed");
            compressed.extend_from_slice(&out_buf[..written]);

            // Drain any additional staged output with empty-slice calls.
            while status == CompressStatus::NeedsOutput {
                let (_, w, s) = compressor
                    .compress(&[], &mut out_buf, flush)
                    .expect("compress drain failed");
                compressed.extend_from_slice(&out_buf[..w]);
                status = s;
            }

            pos = end;
            if pos >= data.len() {
                break;
            }
        }

        compressed
    }

    /// Decompress `compressed` in streaming mode, feeding `chunk_size` bytes per call.
    fn stream_decompress(compressed: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut decompressor = Lz4Decompressor::new();
        let mut decompressed = Vec::new();
        // Output buffer: large enough to hold a full 4 MB block.
        let mut out_buf = vec![0u8; 4 * 1024 * 1024 + 4096];

        let mut pos = 0;
        loop {
            let end = (pos + chunk_size).min(compressed.len());
            let chunk = &compressed[pos..end];
            pos = end;

            // First call: feed the new chunk only.
            let (_, written, mut status) = decompressor
                .decompress(chunk, &mut out_buf)
                .expect("decompress failed");
            decompressed.extend_from_slice(&out_buf[..written]);

            // Drain any additional staged output with empty-slice calls.
            while status == DecompressStatus::NeedsOutput {
                let (_, w, s) = decompressor
                    .decompress(&[], &mut out_buf)
                    .expect("decompress drain failed");
                decompressed.extend_from_slice(&out_buf[..w]);
                status = s;
            }

            if pos >= compressed.len() {
                // Final drain: keep emptying until truly done.
                loop {
                    let (_, w, s) = decompressor
                        .decompress(&[], &mut out_buf)
                        .expect("final drain failed");
                    decompressed.extend_from_slice(&out_buf[..w]);
                    if s == DecompressStatus::Done && w == 0 {
                        break;
                    }
                    if s == DecompressStatus::NeedsInput {
                        break;
                    }
                }
                break;
            }
        }

        decompressed
    }

    // ── existing tests (preserved) ─────────────────────────────────────────

    #[test]
    fn test_lz4_compressor_progress_reports() {
        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let data = make_compressible_data(1024 * 1024); // 1 MB
        let mut compressor = Lz4Compressor::new().with_progress(sink as ProgressHandle);

        let mut output = vec![0u8; data.len() * 2];
        let _ = compressor
            .compress(&data, &mut output, FlushMode::None)
            .expect("compress none");
        let _ = compressor
            .compress(&[], &mut output, FlushMode::Finish)
            .expect("compress finish");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
    }

    #[test]
    fn test_lz4_compressor_cancel_aborts() {
        let token = CancellationToken::new();
        let mut compressor = Lz4Compressor::new().with_cancel(token.clone());

        let data = make_compressible_data(1024);
        let mut output = vec![0u8; data.len() * 2];

        token.cancel();

        let result = compressor.compress(&data, &mut output, FlushMode::Finish);
        assert!(result.is_err(), "expected cancellation error");
    }

    #[test]
    fn test_lz4_decompressor_progress_reports() {
        let data = make_compressible_data(1024 * 1024); // 1 MB
        let compressed = compress(&data).expect("compress");

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let mut decompressor = Lz4Decompressor::new().with_progress(sink as ProgressHandle);
        let mut output = vec![0u8; data.len() * 2];
        decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress");
        // Drain any remaining staged output.
        while !decompressor.is_finished() {
            decompressor
                .decompress(&[], &mut output)
                .expect("drain decompress");
        }

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
    fn test_lz4_decompressor_cancel_aborts() {
        let data = make_compressible_data(1024);
        let compressed = compress(&data).expect("compress");

        let token = CancellationToken::new();
        let mut decompressor = Lz4Decompressor::new().with_cancel(token.clone());
        let mut output = vec![0u8; data.len() * 2];

        token.cancel();
        let result = decompressor.decompress(&compressed, &mut output);
        assert!(result.is_err(), "expected cancellation error");
    }

    // ── new streaming tests ────────────────────────────────────────────────

    /// Feed 200 bytes at a time into a 1 MB input; verify output is valid LZ4.
    #[test]
    fn test_compressor_streaming_small_chunks() {
        let data = make_compressible_data(1024 * 1024);
        let compressed = stream_compress(&data, 200);

        // Must decompress back to original.
        let decompressed = frame_decompress(&compressed, data.len() * 2).expect("frame decompress");
        assert_eq!(decompressed, data);
    }

    /// 10 MB input fed 64 KB at a time; compressor must NOT buffer all 10 MB.
    #[test]
    fn test_compressor_streaming_large_file_bounded_memory() {
        let total = 10 * 1024 * 1024;
        let data = make_compressible_data(total);
        let chunk = 64 * 1024;

        // With a 4 MB block_max_size (default) and a 16 MB budget the
        // compressor should emit blocks and keep input_buf well under budget.
        let mut compressor = Lz4Compressor::new();
        let mut compressed = Vec::new();
        let mut out_buf = vec![0u8; chunk * 8];

        let mut pos = 0;
        while pos < data.len() {
            let end = (pos + chunk).min(data.len());
            let flush = if end == data.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };

            loop {
                let (_, written, status) = compressor
                    .compress(&data[pos..end], &mut out_buf, flush)
                    .expect("compress failed");
                compressed.extend_from_slice(&out_buf[..written]);
                if status != CompressStatus::NeedsOutput {
                    break;
                }
            }

            pos = end;
        }

        // The compressor's internal input_buf should be empty now (all flushed).
        assert!(
            compressor.input_buf.is_empty(),
            "input_buf not drained after Finish"
        );

        // Verify correctness.
        let decompressed = frame_decompress(&compressed, data.len() * 2).expect("frame decompress");
        assert_eq!(decompressed, data);
    }

    /// Compress 500 KB in one shot vs streaming; both should decompress to original.
    #[test]
    fn test_compressor_output_matches_oneshot() {
        let data = make_compressible_data(500 * 1024);

        // One-shot (via frame::compress — sets content_size in header).
        let oneshot = compress(&data).expect("oneshot compress");
        let decompressed_oneshot =
            frame_decompress(&oneshot, data.len() * 2).expect("oneshot decompress");

        // Streaming (no content_size in header because we use default desc).
        let streaming = stream_compress(&data, 64 * 1024);
        let decompressed_streaming =
            frame_decompress(&streaming, data.len() * 2).expect("streaming decompress");

        // Both must round-trip to the same original data.
        assert_eq!(decompressed_oneshot, data, "oneshot decompress mismatch");
        assert_eq!(
            decompressed_streaming, data,
            "streaming decompress mismatch"
        );
    }

    /// Feed LZ4 frame bytes 100 at a time; output accumulates correctly.
    #[test]
    fn test_decompressor_streaming_small_chunks() {
        let data = make_compressible_data(256 * 1024);
        let compressed = compress(&data).expect("compress");

        let decompressed = stream_decompress(&compressed, 100);
        assert_eq!(decompressed, data);
    }

    /// memory_budget = 1 MB, feed 2 MB: should get a memory budget error.
    #[test]
    fn test_decompressor_bounded_memory_rejects_oversized() {
        let data = make_compressible_data(256 * 1024);
        let compressed = compress(&data).expect("compress");

        // Set a very tight budget: smaller than the whole compressed stream.
        let tiny_budget = 512; // 512 bytes — definitely smaller than the frame
        let mut decompressor = Lz4Decompressor::new().with_memory_budget(tiny_budget);
        let mut out_buf = vec![0u8; 4096];

        // Feed the entire compressed stream in one call; this should exceed budget.
        let result = decompressor.decompress(&compressed, &mut out_buf);
        assert!(
            result.is_err(),
            "expected memory budget error, got {:?}",
            result
        );
    }

    /// Feed all but last byte of a block; no output yet.  Feed final byte; output appears.
    #[test]
    fn test_decompressor_partial_block_waits() {
        let data = make_compressible_data(1024);
        let compressed = compress(&data).expect("compress");

        let mut decompressor = Lz4Decompressor::new();
        let mut out_buf = vec![0u8; data.len() * 4];

        // Feed all but the very last byte.
        let (_, written, status) = decompressor
            .decompress(&compressed[..compressed.len() - 1], &mut out_buf)
            .expect("partial feed");

        // The decompressor may have emitted blocks if the end-marker hadn't
        // yet been seen — but it must NOT be Done.
        assert!(
            !matches!(status, DecompressStatus::Done),
            "should not be Done after partial feed, got written={written}"
        );

        // Feed the final byte.
        let (_, written2, status2) = decompressor
            .decompress(&compressed[compressed.len() - 1..], &mut out_buf)
            .expect("final byte");

        // After draining any remaining output the decompressor should finish.
        let mut total_written = written + written2;
        if status2 == DecompressStatus::NeedsOutput {
            loop {
                let (_, w, s) = decompressor.decompress(&[], &mut out_buf).expect("drain");
                total_written += w;
                if s != DecompressStatus::NeedsOutput {
                    break;
                }
            }
        }

        assert!(
            decompressor.is_finished(),
            "decompressor should be Done after last byte"
        );
        let result = &out_buf[..total_written];
        assert_eq!(result, data.as_slice());
    }

    /// Builder compiles and works with 1 MiB budget.
    #[test]
    fn test_with_memory_budget_builder() {
        let budget = 1024 * 1024;
        let mut compressor = Lz4Compressor::new().with_memory_budget(budget);
        assert_eq!(compressor.memory_budget, budget);

        let data = make_compressible_data(512 * 1024); // 512 KB — fits in budget
        let mut out_buf = vec![0u8; data.len() * 2];
        let result = compressor.compress(&data, &mut out_buf, FlushMode::Finish);
        assert!(result.is_ok(), "compress within budget should succeed");
    }

    /// 8 MB input spanning multiple blocks; compress streaming, decompress streaming.
    #[test]
    fn test_streaming_roundtrip_multi_block() {
        let data = make_compressible_data(8 * 1024 * 1024);
        let chunk = 64 * 1024;

        let compressed = stream_compress(&data, chunk);
        let decompressed = stream_decompress(&compressed, chunk);

        assert_eq!(
            decompressed.len(),
            data.len(),
            "length mismatch after streaming roundtrip"
        );
        assert_eq!(
            decompressed, data,
            "data mismatch after streaming roundtrip"
        );
    }
}
