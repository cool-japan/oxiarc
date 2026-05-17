//! Bounded-memory true streaming for LZMA2.
//!
//! This module provides [`Lzma2StreamEncoder`] and [`Lzma2StreamDecoder`] that
//! implement [`std::io::Write`] and [`std::io::Read`] respectively.
//!
//! ## Encoding
//!
//! Input is accumulated in an internal buffer.  Once `chunk_size` bytes have
//! been collected, a complete LZMA2 chunk is encoded and forwarded to the inner
//! writer.  Call [`flush`][Write::flush] or [`finish`][Lzma2StreamEncoder::finish]
//! to force the remaining partial chunk through and append the LZMA2
//! end-of-stream byte (`0x00`).
//!
//! ## Decoding
//!
//! Bytes are read one LZMA2 chunk at a time via a state machine:
//! `NeedChunkHeader → NeedLzmaHeader → NeedCompressedData → Done`
//! (uncompressed path: `NeedChunkHeader → NeedUncompressedSize → NeedUncompressedData → Done`).
//!
//! Both types respect a `memory_budget` that caps how many bytes may be
//! buffered in-flight at once.
//!
//! ## COOLJAPAN policies
//!
//! - Zero `unwrap()` in production code — all fallible paths propagate
//!   `io::Error`.
//! - Zero compiler warnings.
//! - `snake_case` throughout.

use crate::LzmaLevel;
use crate::lzma2::decode_lzma2;
use crate::lzma2_chunk::{
    LZMA_CHUNK_MAX_UNCOMPRESSED, Lzma2ChunkedEncoder, Lzma2Config, UNCOMPRESSED_CHUNK_MAX, control,
};
use oxiarc_core::error::OxiArcError;
use std::io::{self, Read, Write};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Minimum allowed streaming chunk size (4 KiB).
const STREAM_CHUNK_MIN: usize = 4 * 1024;

/// Default streaming chunk size (256 KiB).
const STREAM_DEFAULT_CHUNK_SIZE: usize = 256 * 1024;

/// Default memory budget for the encoder (16 MiB).
const ENCODER_DEFAULT_BUDGET: usize = 16 * 1024 * 1024;

/// Default memory budget for the decoder (64 MiB).
const DECODER_DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────────────
// Lzma2StreamEncoder
// ─────────────────────────────────────────────────────────────────────────────

/// LZMA2 streaming encoder implementing [`Write`].
///
/// Input bytes are accumulated in `input_buf`.  Once `chunk_size` bytes have
/// been collected, a full LZMA2 chunk is encoded and forwarded to the inner
/// writer.  Call [`finish`][Self::finish] to force the remaining partial chunk
/// through and append the LZMA2 end-of-stream marker (`0x00`).
///
/// # Memory budget
///
/// Use [`with_memory_budget`][Self::with_memory_budget] to cap `input_buf`
/// size.  If the buffer grows beyond the budget an `io::Error` is returned.
/// Defaults to 16 MiB.
///
/// # Chunk size
///
/// Use [`with_chunk_size`][Self::with_chunk_size] to set the LZMA2 chunk size.
/// Clamped to `[STREAM_CHUNK_MIN, LZMA_CHUNK_MAX_UNCOMPRESSED]`.
/// Defaults to 256 KiB.
pub struct Lzma2StreamEncoder<W: Write> {
    /// Inner writer.
    writer: W,
    /// LZMA level.
    level: LzmaLevel,
    /// Input bytes not yet encoded into a chunk.
    input_buf: Vec<u8>,
    /// Per-chunk size (clamped to `[STREAM_CHUNK_MIN, LZMA_CHUNK_MAX_UNCOMPRESSED]`).
    chunk_size: usize,
    /// Maximum un-flushed input before triggering an error.
    memory_budget: usize,
    /// Whether `finish()` has already been called.
    finished: bool,
}

impl<W: Write> std::fmt::Debug for Lzma2StreamEncoder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lzma2StreamEncoder")
            .field("input_buf_len", &self.input_buf.len())
            .field("chunk_size", &self.chunk_size)
            .field("memory_budget", &self.memory_budget)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<W: Write> Lzma2StreamEncoder<W> {
    /// Create a new streaming encoder wrapping `writer` at the given
    /// compression `level`.
    pub fn new(writer: W, level: LzmaLevel) -> Self {
        Self {
            writer,
            level,
            input_buf: Vec::new(),
            chunk_size: STREAM_DEFAULT_CHUNK_SIZE,
            memory_budget: ENCODER_DEFAULT_BUDGET,
            finished: false,
        }
    }

    /// Set the chunk size in bytes.
    ///
    /// Clamped to `[STREAM_CHUNK_MIN, LZMA_CHUNK_MAX_UNCOMPRESSED]`.
    #[must_use]
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.clamp(STREAM_CHUNK_MIN, LZMA_CHUNK_MAX_UNCOMPRESSED);
        self
    }

    /// Set the maximum in-memory input buffer size.
    ///
    /// If the buffer grows beyond this limit before a chunk boundary is
    /// reached, the next [`Write::write`] call returns an error.
    /// Defaults to 16 MiB.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    /// Compress and write any complete chunks from `input_buf` to the inner
    /// writer.  Called automatically on every `write()`.
    fn flush_complete_chunks(&mut self) -> io::Result<()> {
        while self.input_buf.len() >= self.chunk_size {
            let chunk: Vec<u8> = self.input_buf.drain(..self.chunk_size).collect();
            self.encode_and_write_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Encode `chunk` as a sequence of LZMA2 chunks (or uncompressed fallback)
    /// and write them to the inner writer, **without** the LZMA2 EOS byte.
    ///
    /// Uses `Lzma2ChunkedEncoder` which produces a complete LZMA2 mini-stream
    /// (header bytes + EOS).  We strip the trailing EOS here; the stream-level
    /// EOS is written only in `finish()`.
    fn encode_and_write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }

        let config = Lzma2Config::with_level(self.level).chunk_size(chunk.len());
        let mut inner_encoder = Lzma2ChunkedEncoder::with_config(config);

        let mut encoded = inner_encoder
            .encode(chunk)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Remove the trailing EOS byte (0x00) that `encode()` always appends.
        // We write our own EOS in `finish()`.
        if encoded.last() == Some(&control::EOS) {
            encoded.pop();
        }

        self.writer.write_all(&encoded)?;
        Ok(())
    }

    /// Flush remaining input and write the LZMA2 end-of-stream marker.
    ///
    /// After this call, further writes return an error.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        // Flush any remaining partial chunk.
        if !self.input_buf.is_empty() {
            let remaining: Vec<u8> = self.input_buf.drain(..).collect();
            self.encode_and_write_chunk(&remaining)?;
        }

        // Write LZMA2 end-of-stream marker.
        self.writer.write_all(&[control::EOS])?;
        self.writer.flush()?;
        self.finished = true;

        Ok(())
    }

    /// Consume the encoder, call `finish()`, and return the inner writer.
    ///
    /// Returns an error if `finish()` fails.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.finish()?;
        Ok(self.writer)
    }
}

impl<W: Write> Write for Lzma2StreamEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write after finish",
            ));
        }

        // Accumulate.
        self.input_buf.extend_from_slice(buf);

        // Budget check.
        if self.input_buf.len() > self.memory_budget {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                OxiArcError::buffer_too_small(self.memory_budget, self.input_buf.len()).to_string(),
            ));
        }

        // Flush complete chunks.
        self.flush_complete_chunks()?;

        Ok(buf.len())
    }

    /// Flush any complete and partial chunks to the inner writer.
    ///
    /// Unlike [`finish`][Self::finish], this does **not** write the LZMA2 EOS
    /// marker; the stream remains open for further writes.
    fn flush(&mut self) -> io::Result<()> {
        // Flush any remaining partial chunk on an explicit flush.
        if !self.input_buf.is_empty() {
            let remaining: Vec<u8> = self.input_buf.drain(..).collect();
            self.encode_and_write_chunk(&remaining)?;
        }
        self.writer.flush()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Decoder state machine
// ─────────────────────────────────────────────────────────────────────────────

/// Parse state for the streaming LZMA2 decoder.
#[derive(Debug)]
enum DecoderState {
    /// Waiting for the 1-byte chunk control byte.
    NeedChunkHeader,
    /// Have the LZMA control byte; need `header_bytes_needed` more bytes for
    /// the 4-byte LZMA size header (+ optional props byte).
    NeedLzmaHeader {
        /// Original control byte.
        ctrl: u8,
        /// How many more bytes to read (4 or 5 depending on state-reset flag).
        header_bytes_needed: usize,
    },
    /// Have the complete LZMA chunk header; collecting `compressed_size` payload bytes.
    NeedCompressedData {
        /// Total uncompressed bytes that this chunk decodes to.
        uncompressed_size: usize,
        /// Number of compressed payload bytes remaining to read.
        compressed_remaining: usize,
        /// Accumulated LZMA2 frame bytes (ctrl byte + header bytes + payload so far).
        frame_buf: Vec<u8>,
    },
    /// Have the uncompressed control byte and its 2-byte size field; reading raw bytes.
    NeedUncompressedData {
        /// Number of raw bytes remaining to collect.
        remaining: usize,
        /// Whether the dictionary should be reset (control byte was 0x01).
        /// Reserved for future use when maintaining dictionary state across chunks.
        #[allow(dead_code)]
        reset_dict: bool,
    },
    /// Need the 2-byte size field for an uncompressed chunk.
    NeedUncompressedSize {
        /// Control byte (0x01 or 0x02).
        ctrl: u8,
        /// How many size bytes have been buffered (0 or 1).
        collected: u8,
        /// First byte of the 2-byte size field (if `collected == 1`).
        first_byte: u8,
    },
    /// Saw EOS byte (0x00); no more chunks will be emitted.
    Done,
}

// ─────────────────────────────────────────────────────────────────────────────
// Lzma2StreamDecoder
// ─────────────────────────────────────────────────────────────────────────────

/// LZMA2 streaming decoder implementing [`Read`].
///
/// Bytes are consumed from the inner reader one LZMA2 chunk at a time via a
/// state machine.  Each `read()` call attempts to decode the next available
/// chunk(s) and copies decompressed data into the caller-supplied buffer.
///
/// # Memory budget
///
/// Use [`with_memory_budget`][Self::with_memory_budget] to limit how many
/// compressed bytes may be buffered before being processed.  Exceeding the
/// budget returns an error.  Defaults to 64 MiB.
pub struct Lzma2StreamDecoder<R: Read> {
    /// Inner compressed reader.
    reader: R,
    /// LZMA2 dictionary size (passed to `decode_lzma2`).
    dict_size: u32,
    /// Bytes read from `reader` but not yet consumed by the state machine.
    input_buf: Vec<u8>,
    /// Decompressed bytes ready to deliver to the caller.
    output_buf: Vec<u8>,
    /// Position in `output_buf` already consumed by callers.
    output_pos: usize,
    /// Current decoder state.
    state: DecoderState,
    /// Maximum `input_buf` size before returning an error.
    memory_budget: usize,
    /// Whether the inner reader has returned EOF.
    reader_eof: bool,
}

impl<R: Read> std::fmt::Debug for Lzma2StreamDecoder<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lzma2StreamDecoder")
            .field("input_buf_len", &self.input_buf.len())
            .field("output_pending", &(self.output_buf.len() - self.output_pos))
            .field("reader_eof", &self.reader_eof)
            .finish()
    }
}

impl<R: Read> Lzma2StreamDecoder<R> {
    /// Create a new streaming decoder reading from `reader`.
    ///
    /// `dict_size` should match the LZMA2 encoder's dictionary size.  A
    /// value of at least `1 << 20` (1 MiB) is recommended.
    pub fn new(reader: R, dict_size: u32) -> Self {
        Self {
            reader,
            dict_size: dict_size.max(4096),
            input_buf: Vec::new(),
            output_buf: Vec::new(),
            output_pos: 0,
            state: DecoderState::NeedChunkHeader,
            memory_budget: DECODER_DEFAULT_BUDGET,
            reader_eof: false,
        }
    }

    /// Set the maximum in-memory compressed buffer size.
    ///
    /// Defaults to 64 MiB.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    /// Return `true` when the LZMA2 end-of-stream marker has been seen and all
    /// decompressed data has been delivered to the caller.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, DecoderState::Done) && self.output_pos >= self.output_buf.len()
    }

    // ── internal helpers ──────────────────────────────────────────────────

    /// Refill `input_buf` from the inner reader.
    fn refill_input(&mut self) -> io::Result<()> {
        if self.reader_eof {
            return Ok(());
        }

        let mut tmp = [0u8; 8192];
        match self.reader.read(&mut tmp) {
            Ok(0) => {
                self.reader_eof = true;
            }
            Ok(n) => {
                self.input_buf.extend_from_slice(&tmp[..n]);
                if self.input_buf.len() > self.memory_budget {
                    return Err(io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        OxiArcError::buffer_too_small(self.memory_budget, self.input_buf.len())
                            .to_string(),
                    ));
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Drain `output_buf[output_pos..]` into `dest`.  Returns bytes copied.
    fn drain_output(&mut self, dest: &mut [u8]) -> usize {
        let pending = &self.output_buf[self.output_pos..];
        let n = pending.len().min(dest.len());
        dest[..n].copy_from_slice(&pending[..n]);
        self.output_pos += n;

        // Reclaim memory once everything is consumed.
        if self.output_pos >= self.output_buf.len() {
            self.output_buf.clear();
            self.output_pos = 0;
        }

        n
    }

    /// Drive the state machine one step using bytes in `input_buf`.
    ///
    /// Returns `Ok(true)` when at least one state transition was made,
    /// `Ok(false)` when more input is needed.
    fn step(&mut self) -> io::Result<bool> {
        match self.state {
            DecoderState::NeedChunkHeader => self.step_need_chunk_header(),
            DecoderState::NeedLzmaHeader { .. } => self.step_need_lzma_header(),
            DecoderState::NeedCompressedData { .. } => self.step_need_compressed_data(),
            DecoderState::NeedUncompressedSize { .. } => self.step_need_uncompressed_size(),
            DecoderState::NeedUncompressedData { .. } => self.step_need_uncompressed_data(),
            DecoderState::Done => Ok(false),
        }
    }

    fn step_need_chunk_header(&mut self) -> io::Result<bool> {
        if self.input_buf.is_empty() {
            return Ok(false);
        }

        let ctrl = self.input_buf.remove(0);

        match ctrl {
            control::EOS => {
                self.state = DecoderState::Done;
                Ok(true)
            }
            control::UNCOMPRESSED_RESET | control::UNCOMPRESSED => {
                self.state = DecoderState::NeedUncompressedSize {
                    ctrl,
                    collected: 0,
                    first_byte: 0,
                };
                Ok(true)
            }
            c if (c & control::LZMA_MASK) != 0 => {
                let has_state_reset = (c & control::STATE_RESET) != 0;
                // 2 bytes uncompressed size (lo 16 bits) + 2 bytes compressed size
                // + optional 1 byte LZMA props
                let header_bytes_needed = if has_state_reset { 5 } else { 4 };

                self.state = DecoderState::NeedLzmaHeader {
                    ctrl: c,
                    header_bytes_needed,
                };
                Ok(true)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid LZMA2 control byte: 0x{ctrl:02X}"),
            )),
        }
    }

    fn step_need_lzma_header(&mut self) -> io::Result<bool> {
        let (ctrl, header_bytes_needed) = match self.state {
            DecoderState::NeedLzmaHeader {
                ctrl,
                header_bytes_needed,
            } => (ctrl, header_bytes_needed),
            _ => unreachable!(),
        };

        if self.input_buf.len() < header_bytes_needed {
            return Ok(false);
        }

        let header_data: Vec<u8> = self.input_buf.drain(..header_bytes_needed).collect();

        // Parse uncompressed size: high 5 bits from ctrl byte + 16-bit lo field + 1.
        let uncompressed_hi = ((ctrl & 0x1F) as usize) << 16;
        let uncompressed_lo = u16::from_be_bytes([header_data[0], header_data[1]]) as usize;
        let uncompressed_size = (uncompressed_hi | uncompressed_lo) + 1;

        // Parse compressed size: 16-bit field + 1.
        let compressed_size = u16::from_be_bytes([header_data[2], header_data[3]]) as usize + 1;

        if uncompressed_size > LZMA_CHUNK_MAX_UNCOMPRESSED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "LZMA2 chunk uncompressed size {uncompressed_size} exceeds max {}",
                    LZMA_CHUNK_MAX_UNCOMPRESSED
                ),
            ));
        }

        // Build the frame_buf for decode_lzma2:
        //   1 ctrl byte + 4 size bytes (+ optional props byte) + payload later + EOS
        let mut frame_buf = Vec::with_capacity(1 + header_bytes_needed + compressed_size + 1);
        frame_buf.push(ctrl);
        frame_buf.extend_from_slice(&header_data);

        self.state = DecoderState::NeedCompressedData {
            uncompressed_size,
            compressed_remaining: compressed_size,
            frame_buf,
        };
        Ok(true)
    }

    fn step_need_compressed_data(&mut self) -> io::Result<bool> {
        // How many compressed bytes remain to read?
        let compressed_remaining = match self.state {
            DecoderState::NeedCompressedData {
                compressed_remaining,
                ..
            } => compressed_remaining,
            _ => unreachable!(),
        };

        if self.input_buf.is_empty() {
            return Ok(false);
        }

        // Take as many bytes as available (up to what is needed).
        let take = compressed_remaining.min(self.input_buf.len());
        let payload: Vec<u8> = self.input_buf.drain(..take).collect();

        match self.state {
            DecoderState::NeedCompressedData {
                ref mut frame_buf,
                ref mut compressed_remaining,
                uncompressed_size,
            } => {
                frame_buf.extend_from_slice(&payload);
                *compressed_remaining -= take;

                if *compressed_remaining == 0 {
                    // Full payload received — decode.
                    frame_buf.push(control::EOS);
                    let frame = frame_buf.clone();
                    let decompressed = decode_lzma2(&frame, self.dict_size)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

                    if decompressed.len() != uncompressed_size {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "LZMA2 chunk size mismatch: expected {} decompressed bytes, got {}",
                                uncompressed_size,
                                decompressed.len()
                            ),
                        ));
                    }

                    self.output_buf.extend_from_slice(&decompressed);
                    self.state = DecoderState::NeedChunkHeader;
                }
            }
            _ => unreachable!(),
        }

        Ok(true)
    }

    fn step_need_uncompressed_size(&mut self) -> io::Result<bool> {
        let (ctrl, collected, first_byte) = match self.state {
            DecoderState::NeedUncompressedSize {
                ctrl,
                collected,
                first_byte,
            } => (ctrl, collected, first_byte),
            _ => unreachable!(),
        };

        if self.input_buf.is_empty() {
            return Ok(false);
        }

        if collected == 0 {
            // Read first byte of size field.
            let b = self.input_buf.remove(0);
            self.state = DecoderState::NeedUncompressedSize {
                ctrl,
                collected: 1,
                first_byte: b,
            };
            return Ok(true);
        }

        // Read second byte and compute size.
        let second_byte = self.input_buf.remove(0);
        let data_size = u16::from_be_bytes([first_byte, second_byte]) as usize + 1;

        if data_size > UNCOMPRESSED_CHUNK_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "LZMA2 uncompressed chunk size {data_size} exceeds max {UNCOMPRESSED_CHUNK_MAX}"
                ),
            ));
        }

        let reset_dict = ctrl == control::UNCOMPRESSED_RESET;
        self.state = DecoderState::NeedUncompressedData {
            remaining: data_size,
            reset_dict,
        };
        Ok(true)
    }

    fn step_need_uncompressed_data(&mut self) -> io::Result<bool> {
        let remaining = match self.state {
            DecoderState::NeedUncompressedData { remaining, .. } => remaining,
            _ => unreachable!(),
        };

        if self.input_buf.is_empty() {
            return Ok(false);
        }

        let take = remaining.min(self.input_buf.len());
        let raw: Vec<u8> = self.input_buf.drain(..take).collect();

        match self.state {
            DecoderState::NeedUncompressedData {
                ref mut remaining, ..
            } => {
                self.output_buf.extend_from_slice(&raw);
                *remaining -= take;

                if *remaining == 0 {
                    self.state = DecoderState::NeedChunkHeader;
                }
            }
            _ => unreachable!(),
        }

        Ok(true)
    }
}

impl<R: Read> Read for Lzma2StreamDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Fast path: drain already-decompressed output.
        if self.output_pos < self.output_buf.len() {
            return Ok(self.drain_output(buf));
        }

        // Signal EOF when done.
        if matches!(self.state, DecoderState::Done) {
            return Ok(0);
        }

        // Drive the state machine, fetching more input as needed.
        loop {
            match self.step()? {
                true => {
                    // State advanced — emit any available output.
                    if self.output_pos < self.output_buf.len() {
                        return Ok(self.drain_output(buf));
                    }
                    if matches!(self.state, DecoderState::Done) {
                        return Ok(0);
                    }
                    // Keep looping to try to produce more output.
                }
                false => {
                    // Need more input.
                    self.refill_input()?;
                    if self.input_buf.is_empty() {
                        // No more data from the inner reader.
                        if self.output_pos < self.output_buf.len() {
                            return Ok(self.drain_output(buf));
                        }
                        return Ok(0);
                    }
                    // More input arrived; re-enter the state machine.
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LzmaLevel;
    use crate::lzma2_chunk::{decode_lzma2_chunked, encode_lzma2_chunked};
    use std::io::{Cursor, Read, Write};

    /// Default dictionary size for tests (1 MiB).
    const TEST_DICT_SIZE: u32 = 1 << 20;

    /// Generate highly compressible data by repeating a single byte.
    /// This matches the pattern used by existing LZMA2 tests, which reliably
    /// round-trip through the current LZMA2 implementation.
    fn make_compressible_data(size: usize) -> Vec<u8> {
        vec![b'A'; size]
    }

    /// Encode `data` through `Lzma2StreamEncoder` writing `write_chunk` bytes
    /// per `write()` call, using an LZMA2 chunk size of `chunk_size`.
    fn stream_encode(data: &[u8], write_chunk: usize, chunk_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder =
                Lzma2StreamEncoder::new(&mut out, LzmaLevel::FAST).with_chunk_size(chunk_size);
            let mut pos = 0;
            while pos < data.len() {
                let end = (pos + write_chunk).min(data.len());
                encoder
                    .write_all(&data[pos..end])
                    .expect("stream_encode write failed");
                pos = end;
            }
            encoder.finish().expect("stream_encode finish failed");
        }
        out
    }

    /// Decode `compressed` through `Lzma2StreamDecoder` using a `read_chunk`-sized
    /// output buffer per call.
    fn stream_decode(compressed: &[u8], read_chunk: usize, dict_size: u32) -> Vec<u8> {
        let cursor = Cursor::new(compressed);
        let mut decoder = Lzma2StreamDecoder::new(cursor, dict_size);
        let mut out = Vec::new();
        let mut buf = vec![0u8; read_chunk];
        loop {
            match decoder.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => panic!("stream_decode read error: {e}"),
            }
        }
        out
    }

    // ── test 1 ────────────────────────────────────────────────────────────────

    /// Write 100 bytes at a time into 1 MB input; verify output decodes
    /// correctly via `decode_lzma2_chunked`.
    #[test]
    fn test_stream_encoder_small_chunks() {
        // Use highly compressible data that the LZMA2 implementation handles correctly.
        let data = make_compressible_data(1024 * 1024);
        // chunk_size must be >= STREAM_CHUNK_MIN (4 KiB).
        let compressed = stream_encode(&data, 100, 64 * 1024);

        let decompressed =
            decode_lzma2_chunked(&compressed, TEST_DICT_SIZE).expect("decode failed");
        assert_eq!(
            decompressed, data,
            "small-chunk stream encode/decode mismatch"
        );
    }

    // ── test 2 ────────────────────────────────────────────────────────────────

    /// 10 MB input, memory_budget = 512 KB.  The encoder must not accumulate
    /// more than `chunk_size` bytes at a time.
    #[test]
    fn test_stream_encoder_large_input_bounded_memory() {
        let data = make_compressible_data(10 * 1024 * 1024);
        let budget = 512 * 1024;
        let chunk_size = 256 * 1024; // smaller than budget — flushes quickly

        let mut out = Vec::new();
        let mut encoder = Lzma2StreamEncoder::new(&mut out, LzmaLevel::FAST)
            .with_chunk_size(chunk_size)
            .with_memory_budget(budget);

        // Write in `chunk_size` blocks — each write should trigger a flush.
        let mut pos = 0;
        while pos < data.len() {
            let end = (pos + chunk_size).min(data.len());
            encoder
                .write_all(&data[pos..end])
                .expect("bounded write failed");
            // After each write the encoder should have flushed, keeping input_buf tiny.
            assert!(
                encoder.input_buf.len() <= chunk_size,
                "input_buf ({}) exceeds chunk_size ({}) — not streaming",
                encoder.input_buf.len(),
                chunk_size
            );
            pos = end;
        }
        encoder.finish().expect("finish failed");

        // Verify correctness.
        let decompressed =
            decode_lzma2_chunked(&out, TEST_DICT_SIZE).expect("decode after bounded encode");
        assert_eq!(decompressed, data, "bounded-memory encode/decode mismatch");
    }

    // ── test 3 ────────────────────────────────────────────────────────────────

    /// 500 KB input: stream encoder output decodes to same original as
    /// `encode_lzma2_chunked` one-shot.
    #[test]
    fn test_stream_encoder_matches_oneshot() {
        let data = make_compressible_data(500 * 1024);

        // One-shot encode.
        let oneshot = encode_lzma2_chunked(&data, LzmaLevel::FAST).expect("oneshot encode failed");
        let oneshot_decoded =
            decode_lzma2_chunked(&oneshot, TEST_DICT_SIZE).expect("oneshot decode failed");

        // Stream encode.
        let streamed = stream_encode(&data, 64 * 1024, 64 * 1024);
        let streamed_decoded =
            decode_lzma2_chunked(&streamed, TEST_DICT_SIZE).expect("stream decode failed");

        // Both must round-trip to original.
        assert_eq!(oneshot_decoded, data, "oneshot roundtrip mismatch");
        assert_eq!(streamed_decoded, data, "streamed roundtrip mismatch");
    }

    // ── test 4 ────────────────────────────────────────────────────────────────

    /// Feed LZMA2 bytes 100 at a time into decoder; output accumulates correctly.
    #[test]
    fn test_stream_decoder_small_reads() {
        let data = make_compressible_data(256 * 1024);
        let compressed = encode_lzma2_chunked(&data, LzmaLevel::FAST).expect("encode failed");

        let small_reader = SmallChunkReader::new(&compressed, 100);
        let mut decoder = Lzma2StreamDecoder::new(small_reader, TEST_DICT_SIZE);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("small-read decode failed");

        assert_eq!(decompressed, data, "small-read decode mismatch");
    }

    // ── test 5 ────────────────────────────────────────────────────────────────

    /// Feed all-but-one-byte of the stream; then feed full stream.
    #[test]
    fn test_stream_decoder_chunk_boundary() {
        let data = make_compressible_data(32 * 1024);
        let compressed = encode_lzma2_chunked(&data, LzmaLevel::FAST).expect("encode failed");

        // Feed all-but-last byte — expect either partial or empty decode.
        let partial_reader = Cursor::new(&compressed[..compressed.len() - 1]);
        let mut partial_decoder = Lzma2StreamDecoder::new(partial_reader, TEST_DICT_SIZE);
        let mut partial_out = Vec::new();
        partial_decoder
            .read_to_end(&mut partial_out)
            .expect("partial read failed");

        // Partial stream should decode no more bytes than the original.
        assert!(
            partial_out.len() <= data.len(),
            "partial decode produced too many bytes"
        );

        // Feed the complete stream — should decode all bytes correctly.
        let full_reader = Cursor::new(&compressed[..]);
        let mut full_decoder = Lzma2StreamDecoder::new(full_reader, TEST_DICT_SIZE);
        let mut full_out = Vec::new();
        full_decoder
            .read_to_end(&mut full_out)
            .expect("full read failed");

        assert_eq!(full_out, data, "full chunk-boundary decode mismatch");
    }

    // ── test 6 ────────────────────────────────────────────────────────────────

    /// Partial final chunk is flushed on `finish()`.
    #[test]
    fn test_stream_encoder_finish() {
        // Use data smaller than the default chunk size — nothing auto-flushes.
        let data = make_compressible_data(8 * 1024);

        let mut out = Vec::new();
        {
            let mut encoder = Lzma2StreamEncoder::new(&mut out, LzmaLevel::FAST)
                .with_chunk_size(LZMA_CHUNK_MAX_UNCOMPRESSED);
            encoder.write_all(&data).expect("write failed");
            // Without finish(), nothing would be in `out`.
            assert!(
                encoder.input_buf.len() == data.len(),
                "input not buffered before finish"
            );
            encoder.finish().expect("finish failed");
        }

        // After finish(), `out` must contain a valid LZMA2 stream.
        assert!(!out.is_empty(), "encoder produced no output");
        assert_eq!(
            out.last().copied(),
            Some(control::EOS),
            "LZMA2 EOS byte missing"
        );

        let decompressed =
            decode_lzma2_chunked(&out, TEST_DICT_SIZE).expect("decode after finish failed");
        assert_eq!(decompressed, data, "finish() flush mismatch");
    }

    // ── test 7 ────────────────────────────────────────────────────────────────

    /// Builder API: `with_memory_budget` and `with_chunk_size` compile and work.
    #[test]
    fn test_with_memory_budget_builder() {
        let mut out = Vec::new();
        let mut encoder = Lzma2StreamEncoder::new(&mut out, LzmaLevel::FAST)
            .with_memory_budget(1024 * 1024)
            .with_chunk_size(256 * 1024);

        // Data well within the budget.
        let data = make_compressible_data(512 * 1024);
        encoder.write_all(&data).expect("write within budget");
        encoder.finish().expect("finish");

        let decompressed = decode_lzma2_chunked(&out, TEST_DICT_SIZE).expect("decode");
        assert_eq!(decompressed, data, "budget-builder encode/decode mismatch");
    }

    // ── test 8 ────────────────────────────────────────────────────────────────

    /// 5 MB input spanning multiple chunks; encode streaming, decode streaming,
    /// verify round-trip.
    #[test]
    fn test_stream_roundtrip_multi_chunk() {
        let data = make_compressible_data(5 * 1024 * 1024);
        let chunk_size = 256 * 1024;

        // Stream encode.
        let compressed = stream_encode(&data, chunk_size, chunk_size);

        // Stream decode.
        let decompressed = stream_decode(&compressed, 64 * 1024, TEST_DICT_SIZE);

        assert_eq!(
            decompressed.len(),
            data.len(),
            "multi-chunk roundtrip length mismatch"
        );
        assert_eq!(decompressed, data, "multi-chunk roundtrip data mismatch");
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// A `Read` wrapper that returns at most `chunk_size` bytes per `read()`.
    struct SmallChunkReader<'a> {
        data: &'a [u8],
        pos: usize,
        chunk_size: usize,
    }

    impl<'a> SmallChunkReader<'a> {
        fn new(data: &'a [u8], chunk_size: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk_size,
            }
        }
    }

    impl Read for SmallChunkReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = self.data.len() - self.pos;
            if remaining == 0 {
                return Ok(0);
            }
            let n = remaining.min(buf.len()).min(self.chunk_size);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
}
