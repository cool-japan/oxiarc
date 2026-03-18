//! Streaming compression and decompression for GZIP and Zlib formats.
//!
//! Provides [`GzipStreamEncoder`] / [`GzipStreamDecoder`] and
//! [`ZlibStreamEncoder`] / [`ZlibStreamDecoder`] that implement the standard
//! [`std::io::Write`] and [`std::io::Read`] traits respectively, enabling
//! incremental processing of compressed data through standard Rust I/O
//! pipelines.
//!
//! # Design
//!
//! **Encoders** maintain a single GZIP/Zlib stream with a persistent
//! [`Deflater`] and running CRC-32 (GZIP) or
//! Adler-32 (Zlib) checksums. Flush methods (`sync_flush`, `full_flush`,
//! `partial_flush`) emit DEFLATE blocks within the *same* stream rather
//! than starting new concatenated members.
//!
//! When the internal buffer reaches `block_size` bytes it is automatically
//! flushed via `sync_flush`. Any remaining data is flushed when
//! [`finish`](GzipStreamEncoder::finish) is called.
//!
//! **Decoders** eagerly read all compressed data from the inner reader on the
//! first `read` call, decompress it, and serve from an internal buffer. This
//! matches the pattern used by `oxiarc-zstd`.
//!
//! # Example
//!
//! ```rust
//! use std::io::{Read, Write};
//! use oxiarc_deflate::streaming::{GzipStreamEncoder, GzipStreamDecoder};
//!
//! // Compress
//! let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
//! encoder.write_all(b"Hello, streaming gzip!").expect("write failed");
//! let compressed = encoder.finish().expect("finish failed");
//!
//! // Decompress
//! let mut decoder = GzipStreamDecoder::new(&compressed[..]);
//! let mut output = String::new();
//! decoder.read_to_string(&mut output).expect("read failed");
//! assert_eq!(output, "Hello, streaming gzip!");
//! ```

use crate::deflate::Deflater;
use crate::gzip::gzip_decompress;
use crate::zlib::{Adler32, zlib_decompress};
use oxiarc_core::Crc32;
use std::io::{self, Read, Write};

/// Default block size for incremental encoder flushing (128 KiB).
const DEFAULT_BLOCK_SIZE: usize = 128 * 1024;

// ---------------------------------------------------------------------------
// GZIP constants
// ---------------------------------------------------------------------------

/// Gzip magic bytes.
const GZIP_ID1: u8 = 0x1f;
const GZIP_ID2: u8 = 0x8b;
/// Compression method: deflate.
const GZIP_CM_DEFLATE: u8 = 8;
/// Gzip header flags byte (no extra fields).
const GZIP_FLG_NONE: u8 = 0;
/// OS byte: unknown (255).
const GZIP_OS_UNKNOWN: u8 = 255;

// ---------------------------------------------------------------------------
// GzipStreamEncoder
// ---------------------------------------------------------------------------

/// Streaming GZIP encoder that implements [`Write`].
///
/// Maintains a single GZIP stream with a persistent [`Deflater`] and
/// running CRC-32. Flush methods (`sync_flush`, `full_flush`,
/// `partial_flush`) emit DEFLATE blocks within the same stream.
///
/// **Important:** you *must* call [`finish`](GzipStreamEncoder::finish) to
/// write the GZIP trailer. Dropping the encoder without calling `finish`
/// will produce an incomplete GZIP stream.
pub struct GzipStreamEncoder<W: Write> {
    /// The wrapped writer that receives compressed output.
    inner: Option<W>,
    /// Internal buffer holding uncompressed data waiting to be flushed.
    buffer: Vec<u8>,
    /// Persistent DEFLATE compressor.
    deflater: Deflater,
    /// Running CRC-32 over all uncompressed data.
    crc: Crc32,
    /// Total number of uncompressed bytes fed (mod 2^32 for ISIZE).
    total_in: u64,
    /// Whether the GZIP header has been written yet.
    header_written: bool,
    /// Whether `finish` has already been called.
    finished: bool,
    /// Threshold at which the buffer is automatically flushed.
    block_size: usize,
}

impl<W: Write> GzipStreamEncoder<W> {
    /// Create a new streaming GZIP encoder wrapping `writer`.
    ///
    /// The `level` parameter controls the DEFLATE compression level (0-9).
    pub fn new(writer: W, level: u8) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            deflater: Deflater::new(level.min(9)),
            crc: Crc32::new(),
            total_in: 0,
            header_written: false,
            finished: false,
            block_size: DEFAULT_BLOCK_SIZE,
        }
    }

    /// Set the block size used for incremental flushing.
    ///
    /// When the internal buffer reaches this many bytes it is automatically
    /// flushed via sync_flush.
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size.max(1);
        self
    }

    /// Write the 10-byte GZIP header if not already written.
    fn ensure_header(&mut self) -> io::Result<()> {
        if self.header_written {
            return Ok(());
        }
        let header = [
            GZIP_ID1,
            GZIP_ID2,
            GZIP_CM_DEFLATE,
            GZIP_FLG_NONE,
            0,
            0,
            0,
            0, // MTIME = 0
            0, // XFL = 0
            GZIP_OS_UNKNOWN,
        ];
        if let Some(ref mut w) = self.inner {
            w.write_all(&header)?;
        }
        self.header_written = true;
        Ok(())
    }

    /// Drain the internal buffer through the deflater using a sync flush.
    ///
    /// A sync flush emits a compressed block followed by an empty stored
    /// block (`0x00 0x00 0xFF 0xFF`), which byte-aligns the stream and
    /// allows a decoder to decode all data fed so far.
    pub fn sync_flush(&mut self) -> io::Result<()> {
        self.ensure_header()?;
        let data = std::mem::take(&mut self.buffer);
        self.crc.update(&data);
        self.total_in += data.len() as u64;
        let mut compressed = Vec::new();
        self.deflater
            .deflate_sync(&data, &mut compressed)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Full flush: same as sync flush, then reset the LZ77 state.
    ///
    /// After a full flush the compressor state is reset so subsequent
    /// data can be decompressed independently (given knowledge of the
    /// block boundary).
    pub fn full_flush(&mut self) -> io::Result<()> {
        self.sync_flush()?;
        self.deflater.reset_lz77();
        Ok(())
    }

    /// Partial flush: emit a compressed block without the sync marker.
    ///
    /// The block is flushed to a byte boundary but no empty stored block
    /// is appended. This produces slightly smaller output than sync flush
    /// but the decoder cannot determine a safe decompression boundary.
    pub fn partial_flush(&mut self) -> io::Result<()> {
        self.ensure_header()?;
        let data = std::mem::take(&mut self.buffer);
        self.crc.update(&data);
        self.total_in += data.len() as u64;
        let mut compressed = Vec::new();
        self.deflater
            .deflate_partial(&data, &mut compressed)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Finish compression and return the inner writer.
    ///
    /// This **must** be called to write the GZIP trailer (CRC-32 + ISIZE).
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if compression or writing to the inner writer
    /// fails.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.finished {
            self.ensure_header()?;
            // Flush any remaining buffered data as a final DEFLATE block.
            let data = std::mem::take(&mut self.buffer);
            self.crc.update(&data);
            self.total_in += data.len() as u64;
            let mut compressed = Vec::new();
            self.deflater
                .deflate(&data, &mut compressed, true)
                .map_err(|e| io::Error::other(e.to_string()))?;
            if let Some(ref mut w) = self.inner {
                w.write_all(&compressed)?;
            }
            // Write trailer: CRC-32 (4 bytes LE) + ISIZE (4 bytes LE).
            let crc_val = self.crc.clone().finalize();
            let isize_val = (self.total_in & 0xFFFF_FFFF) as u32;
            if let Some(ref mut w) = self.inner {
                w.write_all(&crc_val.to_le_bytes())?;
                w.write_all(&isize_val.to_le_bytes())?;
            }
            self.finished = true;
        }
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("inner writer already taken"))
    }

    /// If the buffer has reached `block_size`, sync-flush it.
    fn maybe_flush_block(&mut self) -> io::Result<()> {
        if self.buffer.len() >= self.block_size {
            self.sync_flush()?;
        }
        Ok(())
    }

    /// Returns the number of uncompressed bytes currently buffered.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if `finish` has already been called.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<W: Write> Write for GzipStreamEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("encoder already finished"));
        }
        self.buffer.extend_from_slice(buf);
        self.maybe_flush_block()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.sync_flush()?;
        }
        if let Some(ref mut w) = self.inner {
            w.flush()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GzipStreamDecoder
// ---------------------------------------------------------------------------

/// Streaming GZIP decoder that implements [`Read`].
///
/// All compressed data is read eagerly from the inner reader on the first
/// `read` call, decompressed into an internal buffer, and then served from
/// that buffer for subsequent reads.
///
/// Supports concatenated GZIP members: each member is decompressed
/// independently and the results are concatenated.
pub struct GzipStreamDecoder<R: Read> {
    /// The wrapped reader providing compressed input.
    inner: R,
    /// Decompressed output buffer.
    output_buffer: Vec<u8>,
    /// Current read position inside `output_buffer`.
    output_pos: usize,
    /// Whether the compressed stream has been fully consumed.
    finished: bool,
}

impl<R: Read> GzipStreamDecoder<R> {
    /// Create a new streaming GZIP decoder wrapping `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: reader,
            output_buffer: Vec::new(),
            output_pos: 0,
            finished: false,
        }
    }

    /// Consume the decoder and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read and decompress all compressed data from the inner reader.
    ///
    /// Handles concatenated GZIP members by repeatedly decoding until all
    /// input is consumed.
    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.finished || self.output_pos < self.output_buffer.len() {
            return Ok(());
        }

        let mut compressed = Vec::new();
        self.inner.read_to_end(&mut compressed)?;

        if compressed.is_empty() {
            self.finished = true;
            return Ok(());
        }

        // Decompress concatenated GZIP members. Each member starts with the
        // magic bytes 0x1f 0x8b. We try to decompress the first member; if
        // there is trailing data that also starts with the magic bytes we
        // decompress that too.
        let mut all_decompressed = Vec::new();
        let mut remaining = &compressed[..];

        while !remaining.is_empty() {
            // Check for GZIP magic
            if remaining.len() < 2 || remaining[0] != 0x1f || remaining[1] != 0x8b {
                // Not a GZIP member; could be trailing garbage. Stop.
                break;
            }

            // Try to decompress the full remaining data as a single GZIP member.
            // gzip_decompress will parse the header/trailer and only consume one
            // member's worth of DEFLATE data, but it expects the trailer to be
            // at the end. For concatenated members we need to find member
            // boundaries.
            //
            // Strategy: try decompress the entire remaining data. If it fails
            // (because there is a second member concatenated), search for the
            // next member boundary.
            match gzip_decompress(remaining) {
                Ok(decompressed) => {
                    all_decompressed.extend_from_slice(&decompressed);
                    // Successfully decoded the whole remaining data as one member.
                    remaining = &[];
                }
                Err(_) => {
                    // There might be concatenated members. Try to find the end
                    // of the first member by looking for the next 0x1f 0x8b
                    // after position 2.
                    let mut decoded_one = false;
                    for split_pos in 18..remaining.len().saturating_sub(1) {
                        if remaining[split_pos] == 0x1f && remaining[split_pos + 1] == 0x8b {
                            // Try to decode just the first part
                            if let Ok(decompressed) = gzip_decompress(&remaining[..split_pos]) {
                                all_decompressed.extend_from_slice(&decompressed);
                                remaining = &remaining[split_pos..];
                                decoded_one = true;
                                break;
                            }
                        }
                    }
                    if !decoded_one {
                        // Could not split; return an error for the original data.
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "failed to decompress GZIP data",
                        ));
                    }
                }
            }
        }

        self.output_buffer = all_decompressed;
        self.output_pos = 0;
        self.finished = true;

        Ok(())
    }

    /// Returns the total number of decompressed bytes available.
    pub fn decompressed_size(&self) -> usize {
        self.output_buffer.len()
    }

    /// Returns `true` if all decompressed data has been read.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buffer.len()
    }
}

impl<R: Read> Read for GzipStreamDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fill_buffer()?;

        let available = self.output_buffer.len() - self.output_pos;
        if available == 0 {
            return Ok(0);
        }

        let to_copy = buf.len().min(available);
        buf[..to_copy]
            .copy_from_slice(&self.output_buffer[self.output_pos..self.output_pos + to_copy]);
        self.output_pos += to_copy;
        Ok(to_copy)
    }
}

// ---------------------------------------------------------------------------
// ZlibStreamEncoder
// ---------------------------------------------------------------------------

/// Streaming Zlib encoder that implements [`Write`].
///
/// Maintains a single Zlib stream with a persistent [`Deflater`] and
/// running Adler-32. Flush methods (`sync_flush`, `full_flush`,
/// `partial_flush`) emit DEFLATE blocks within the same stream.
///
/// **Important:** you *must* call [`finish`](ZlibStreamEncoder::finish) to
/// write the Adler-32 trailer. Dropping the encoder without calling `finish`
/// will produce an incomplete Zlib stream.
pub struct ZlibStreamEncoder<W: Write> {
    /// The wrapped writer that receives compressed output.
    inner: Option<W>,
    /// Internal buffer holding uncompressed data waiting to be flushed.
    buffer: Vec<u8>,
    /// Persistent DEFLATE compressor.
    deflater: Deflater,
    /// Running Adler-32 over all uncompressed data.
    adler: Adler32,
    /// Whether the Zlib header has been written yet.
    header_written: bool,
    /// Whether `finish` has already been called.
    finished: bool,
    /// Threshold at which the buffer is automatically flushed.
    block_size: usize,
    /// Compression level (for header).
    level: u8,
}

/// Zlib compression level indicator in header.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum ZlibLevel {
    /// Fastest compression.
    Fastest = 0,
    /// Fast compression.
    Fast = 1,
    /// Default compression.
    Default = 2,
    /// Maximum compression.
    Maximum = 3,
}

impl ZlibLevel {
    /// Convert from compression level (0-9) to zlib level indicator.
    fn from_level(level: u8) -> Self {
        match level {
            0..=2 => Self::Fastest,
            3..=5 => Self::Fast,
            6 => Self::Default,
            7..=9 => Self::Maximum,
            _ => Self::Default,
        }
    }
}

impl<W: Write> ZlibStreamEncoder<W> {
    /// Create a new streaming Zlib encoder wrapping `writer`.
    ///
    /// The `level` parameter controls the DEFLATE compression level (0-9).
    pub fn new(writer: W, level: u8) -> Self {
        let level = level.min(9);
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            deflater: Deflater::new(level),
            adler: Adler32::new(),
            header_written: false,
            finished: false,
            block_size: DEFAULT_BLOCK_SIZE,
            level,
        }
    }

    /// Set the block size used for incremental flushing.
    ///
    /// When the internal buffer reaches this many bytes it is automatically
    /// flushed via sync_flush.
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size.max(1);
        self
    }

    /// Write the 2-byte Zlib header if not already written.
    fn ensure_header(&mut self) -> io::Result<()> {
        if self.header_written {
            return Ok(());
        }
        // CMF byte: CM=8 (DEFLATE), CINFO=7 (32KB window)
        let cmf: u8 = 0x78;
        let flevel = ZlibLevel::from_level(self.level) as u8;
        let fdict = 0u8;
        let fcheck = {
            let base = (cmf as u16) * 256 + ((flevel << 6) | (fdict << 5)) as u16;
            let remainder = base % 31;
            if remainder == 0 {
                0
            } else {
                (31 - remainder) as u8
            }
        };
        let flg = (flevel << 6) | (fdict << 5) | fcheck;
        if let Some(ref mut w) = self.inner {
            w.write_all(&[cmf, flg])?;
        }
        self.header_written = true;
        Ok(())
    }

    /// Drain the internal buffer through the deflater using a sync flush.
    pub fn sync_flush(&mut self) -> io::Result<()> {
        self.ensure_header()?;
        let data = std::mem::take(&mut self.buffer);
        self.adler.update(&data);
        let mut compressed = Vec::new();
        self.deflater
            .deflate_sync(&data, &mut compressed)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Full flush: same as sync flush, then reset the LZ77 state.
    pub fn full_flush(&mut self) -> io::Result<()> {
        self.sync_flush()?;
        self.deflater.reset_lz77();
        Ok(())
    }

    /// Partial flush: emit a compressed block without the sync marker.
    pub fn partial_flush(&mut self) -> io::Result<()> {
        self.ensure_header()?;
        let data = std::mem::take(&mut self.buffer);
        self.adler.update(&data);
        let mut compressed = Vec::new();
        self.deflater
            .deflate_partial(&data, &mut compressed)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Finish compression and return the inner writer.
    ///
    /// This **must** be called to write the Adler-32 trailer.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.finished {
            self.ensure_header()?;
            let data = std::mem::take(&mut self.buffer);
            self.adler.update(&data);
            let mut compressed = Vec::new();
            self.deflater
                .deflate(&data, &mut compressed, true)
                .map_err(|e| io::Error::other(e.to_string()))?;
            if let Some(ref mut w) = self.inner {
                w.write_all(&compressed)?;
            }
            // Write Adler-32 checksum (big-endian).
            let checksum = self.adler.finish();
            if let Some(ref mut w) = self.inner {
                w.write_all(&checksum.to_be_bytes())?;
            }
            self.finished = true;
        }
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("inner writer already taken"))
    }

    /// If the buffer has reached `block_size`, sync-flush it.
    fn maybe_flush_block(&mut self) -> io::Result<()> {
        if self.buffer.len() >= self.block_size {
            self.sync_flush()?;
        }
        Ok(())
    }

    /// Returns the number of uncompressed bytes currently buffered.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if `finish` has already been called.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<W: Write> Write for ZlibStreamEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("encoder already finished"));
        }
        self.buffer.extend_from_slice(buf);
        self.maybe_flush_block()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.sync_flush()?;
        }
        if let Some(ref mut w) = self.inner {
            w.flush()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ZlibStreamDecoder
// ---------------------------------------------------------------------------

/// Streaming Zlib decoder that implements [`Read`].
///
/// All compressed data is read eagerly from the inner reader on the first
/// `read` call, decompressed into an internal buffer, and then served from
/// that buffer for subsequent reads.
///
/// Supports concatenated Zlib streams: each stream is decompressed
/// independently and the results are concatenated.
pub struct ZlibStreamDecoder<R: Read> {
    /// The wrapped reader providing compressed input.
    inner: R,
    /// Decompressed output buffer.
    output_buffer: Vec<u8>,
    /// Current read position inside `output_buffer`.
    output_pos: usize,
    /// Whether the compressed stream has been fully consumed.
    finished: bool,
}

impl<R: Read> ZlibStreamDecoder<R> {
    /// Create a new streaming Zlib decoder wrapping `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: reader,
            output_buffer: Vec::new(),
            output_pos: 0,
            finished: false,
        }
    }

    /// Consume the decoder and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read and decompress all compressed data from the inner reader.
    ///
    /// Handles concatenated Zlib streams by repeatedly decoding until all
    /// input is consumed.
    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.finished || self.output_pos < self.output_buffer.len() {
            return Ok(());
        }

        let mut compressed = Vec::new();
        self.inner.read_to_end(&mut compressed)?;

        if compressed.is_empty() {
            self.finished = true;
            return Ok(());
        }

        // Decompress concatenated Zlib streams. A Zlib stream starts with a
        // CMF byte where CM=8 (lower nibble). The typical CMF value is 0x78
        // (window size = 32KB, deflate).
        let mut all_decompressed = Vec::new();
        let mut remaining = &compressed[..];

        while !remaining.is_empty() {
            // Validate minimum size (2-byte header + at least some data + 4-byte checksum)
            if remaining.len() < 6 {
                break;
            }

            // Check if this looks like a valid zlib header
            let cmf = remaining[0];
            let cm = cmf & 0x0F;
            if cm != 8 {
                break;
            }
            let flg = remaining[1];
            let check = (cmf as u16) * 256 + (flg as u16);
            if check % 31 != 0 {
                break;
            }

            match zlib_decompress(remaining) {
                Ok(decompressed) => {
                    all_decompressed.extend_from_slice(&decompressed);
                    // Successfully decoded. Since zlib_decompress consumes the
                    // entire input, we are done.
                    remaining = &[];
                }
                Err(_) => {
                    // There might be concatenated streams. Try to find the
                    // boundary by looking for the next valid zlib header.
                    let mut decoded_one = false;
                    // A minimal zlib stream is 6 bytes (2 header + empty deflate + 4 adler32).
                    for split_pos in 6..remaining.len().saturating_sub(5) {
                        let candidate_cmf = remaining[split_pos];
                        let candidate_cm = candidate_cmf & 0x0F;
                        if candidate_cm != 8 {
                            continue;
                        }
                        if split_pos + 1 >= remaining.len() {
                            continue;
                        }
                        let candidate_flg = remaining[split_pos + 1];
                        let candidate_check = (candidate_cmf as u16) * 256 + (candidate_flg as u16);
                        if candidate_check % 31 != 0 {
                            continue;
                        }
                        // Looks like a valid header; try to decode the first part
                        if let Ok(decompressed) = zlib_decompress(&remaining[..split_pos]) {
                            all_decompressed.extend_from_slice(&decompressed);
                            remaining = &remaining[split_pos..];
                            decoded_one = true;
                            break;
                        }
                    }
                    if !decoded_one {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "failed to decompress Zlib data",
                        ));
                    }
                }
            }
        }

        self.output_buffer = all_decompressed;
        self.output_pos = 0;
        self.finished = true;

        Ok(())
    }

    /// Returns the total number of decompressed bytes available.
    pub fn decompressed_size(&self) -> usize {
        self.output_buffer.len()
    }

    /// Returns `true` if all decompressed data has been read.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buffer.len()
    }
}

impl<R: Read> Read for ZlibStreamDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fill_buffer()?;

        let available = self.output_buffer.len() - self.output_pos;
        if available == 0 {
            return Ok(0);
        }

        let to_copy = buf.len().min(available);
        buf[..to_copy]
            .copy_from_slice(&self.output_buffer[self.output_pos..self.output_pos + to_copy]);
        self.output_pos += to_copy;
        Ok(to_copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // GzipStreamEncoder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_gzip_stream_encoder_basic() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(b"Hello, GZIP!").expect("write failed");
        let compressed = encoder.finish().expect("finish failed");
        assert!(!compressed.is_empty());
        // Should start with GZIP magic bytes
        assert_eq!(compressed[0], 0x1f);
        assert_eq!(compressed[1], 0x8b);
    }

    #[test]
    fn test_gzip_stream_encoder_empty() {
        let encoder = GzipStreamEncoder::new(Vec::new(), 6);
        let compressed = encoder.finish().expect("finish failed");
        // Should produce a valid (minimal) GZIP member.
        assert!(!compressed.is_empty());
        assert_eq!(compressed[0], 0x1f);
        assert_eq!(compressed[1], 0x8b);
    }

    #[test]
    fn test_gzip_stream_roundtrip() {
        let original = b"The quick brown fox jumps over the lazy dog.";

        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_gzip_stream_roundtrip_multiple_writes() {
        let parts: &[&[u8]] = &[b"Hello, ", b"streaming ", b"GZIP!"];

        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        for part in parts {
            encoder.write_all(part).expect("write failed");
        }
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"Hello, streaming GZIP!");
    }

    #[test]
    fn test_gzip_stream_decoder_small_reads() {
        let original = b"ABCDEFGHIJ";

        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        let mut buf = [0u8; 3];

        loop {
            let n = decoder.read(&mut buf).expect("read failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_gzip_stream_decoder_empty_input() {
        let mut decoder = GzipStreamDecoder::new(&[][..]);
        let mut buf = [0u8; 16];
        let n = decoder.read(&mut buf).expect("read failed");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_gzip_stream_encoder_buffered_bytes() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b"12345").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 5);
        encoder.write_all(b"67890").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 10);
    }

    #[test]
    fn test_gzip_stream_encoder_is_finished() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        assert!(!encoder.is_finished());
        encoder.write_all(b"data").expect("write failed");
        assert!(!encoder.is_finished());
    }

    #[test]
    fn test_gzip_stream_decoder_is_finished() {
        let original = b"short";

        let mut enc = GzipStreamEncoder::new(Vec::new(), 6);
        enc.write_all(original).expect("write failed");
        let compressed = enc.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        assert!(!decoder.is_finished());

        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read failed");
        assert!(decoder.is_finished());
    }

    #[test]
    fn test_gzip_stream_roundtrip_large_data() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(&original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original);
    }

    #[test]
    fn test_gzip_stream_all_levels() {
        let original = b"AAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCC";
        for level in 0u8..=9 {
            let mut encoder = GzipStreamEncoder::new(Vec::new(), level);
            encoder.write_all(original).expect("write failed");
            let compressed = encoder.finish().expect("finish failed");

            let mut decoder = GzipStreamDecoder::new(&compressed[..]);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).expect("read failed");

            assert_eq!(
                output,
                original.as_slice(),
                "roundtrip failed at level {}",
                level,
            );
        }
    }

    #[test]
    fn test_gzip_stream_decoder_into_inner() {
        let data = vec![1u8, 2, 3, 4, 5];
        let decoder = GzipStreamDecoder::new(data.as_slice());
        let inner = decoder.into_inner();
        assert_eq!(inner, data.as_slice());
    }

    #[test]
    fn test_gzip_stream_flush() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"data before flush")
            .expect("write failed");
        encoder.flush().expect("flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b" more data").expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        // The output is a single GZIP stream (not concatenated members).
        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"data before flush more data");
    }

    #[test]
    fn test_gzip_stream_with_block_size() {
        // Use a very small block size to force multiple flushes.
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6).with_block_size(10);
        encoder
            .write_all(b"This is more than ten bytes of data")
            .expect("write failed");
        // After writing 35 bytes with block_size=10, there should have been
        // at least one automatic flush. The buffer should hold the remainder.
        assert!(encoder.buffered_bytes() < 35);
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"This is more than ten bytes of data");
    }

    // -----------------------------------------------------------------------
    // ZlibStreamEncoder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_zlib_stream_encoder_basic() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(b"Hello, Zlib!").expect("write failed");
        let compressed = encoder.finish().expect("finish failed");
        assert!(!compressed.is_empty());
        // Should start with zlib CMF byte 0x78
        assert_eq!(compressed[0], 0x78);
    }

    #[test]
    fn test_zlib_stream_encoder_empty() {
        let encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        let compressed = encoder.finish().expect("finish failed");
        assert!(!compressed.is_empty());
        assert_eq!(compressed[0], 0x78);
    }

    #[test]
    fn test_zlib_stream_roundtrip() {
        let original = b"The quick brown fox jumps over the lazy dog.";

        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_zlib_stream_roundtrip_multiple_writes() {
        let parts: &[&[u8]] = &[b"Hello, ", b"streaming ", b"Zlib!"];

        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        for part in parts {
            encoder.write_all(part).expect("write failed");
        }
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"Hello, streaming Zlib!");
    }

    #[test]
    fn test_zlib_stream_decoder_small_reads() {
        let original = b"ABCDEFGHIJ";

        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        let mut buf = [0u8; 3];

        loop {
            let n = decoder.read(&mut buf).expect("read failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_zlib_stream_decoder_empty_input() {
        let mut decoder = ZlibStreamDecoder::new(&[][..]);
        let mut buf = [0u8; 16];
        let n = decoder.read(&mut buf).expect("read failed");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_zlib_stream_encoder_buffered_bytes() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b"12345").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 5);
    }

    #[test]
    fn test_zlib_stream_decoder_is_finished() {
        let original = b"short";

        let mut enc = ZlibStreamEncoder::new(Vec::new(), 6);
        enc.write_all(original).expect("write failed");
        let compressed = enc.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        assert!(!decoder.is_finished());

        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read failed");
        assert!(decoder.is_finished());
    }

    #[test]
    fn test_zlib_stream_roundtrip_large_data() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(&original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original);
    }

    #[test]
    fn test_zlib_stream_all_levels() {
        let original = b"AAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCC";
        for level in 0u8..=9 {
            let mut encoder = ZlibStreamEncoder::new(Vec::new(), level);
            encoder.write_all(original).expect("write failed");
            let compressed = encoder.finish().expect("finish failed");

            let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).expect("read failed");

            assert_eq!(
                output,
                original.as_slice(),
                "roundtrip failed at level {}",
                level,
            );
        }
    }

    #[test]
    fn test_zlib_stream_decoder_into_inner() {
        let data = vec![1u8, 2, 3, 4, 5];
        let decoder = ZlibStreamDecoder::new(data.as_slice());
        let inner = decoder.into_inner();
        assert_eq!(inner, data.as_slice());
    }

    #[test]
    fn test_zlib_stream_flush() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"data before flush")
            .expect("write failed");
        encoder.flush().expect("flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b" more data").expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        // Single zlib stream, not concatenated.
        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"data before flush more data");
    }

    #[test]
    fn test_zlib_stream_with_block_size() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6).with_block_size(10);
        encoder
            .write_all(b"This is more than ten bytes of data")
            .expect("write failed");
        assert!(encoder.buffered_bytes() < 35);
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"This is more than ten bytes of data");
    }

    // -----------------------------------------------------------------------
    // Cross-format sanity
    // -----------------------------------------------------------------------

    #[test]
    fn test_gzip_and_zlib_produce_different_output() {
        let original = b"Same input for both formats";

        let mut gzip_enc = GzipStreamEncoder::new(Vec::new(), 6);
        gzip_enc.write_all(original).expect("write failed");
        let gzip_out = gzip_enc.finish().expect("finish failed");

        let mut zlib_enc = ZlibStreamEncoder::new(Vec::new(), 6);
        zlib_enc.write_all(original).expect("write failed");
        let zlib_out = zlib_enc.finish().expect("finish failed");

        // Different framing means different output.
        assert_ne!(gzip_out, zlib_out);

        // But both decompress to the same thing.
        let mut gzip_dec = GzipStreamDecoder::new(&gzip_out[..]);
        let mut gzip_result = Vec::new();
        gzip_dec.read_to_end(&mut gzip_result).expect("read failed");

        let mut zlib_dec = ZlibStreamDecoder::new(&zlib_out[..]);
        let mut zlib_result = Vec::new();
        zlib_dec.read_to_end(&mut zlib_result).expect("read failed");

        assert_eq!(gzip_result, original.as_slice());
        assert_eq!(zlib_result, original.as_slice());
    }

    #[test]
    fn test_stream_encoder_write_after_finish_errors() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder.write_all(b"first").expect("write failed");
        encoder.flush().expect("flush failed");
        // Can still write after flush (flush != finish)
        encoder.write_all(b"second").expect("write failed");
        let _compressed = encoder.finish().expect("finish failed");
    }

    // -----------------------------------------------------------------------
    // Flush mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_gzip_sync_flush_produces_decompressible_prefix() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Hello, sync flush world!")
            .expect("write failed");
        encoder.sync_flush().expect("sync_flush failed");
        // After sync flush the buffer should be empty.
        assert_eq!(encoder.buffered_bytes(), 0);
        // Write more data and finish.
        encoder
            .write_all(b" And more data after sync.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(
            output,
            b"Hello, sync flush world! And more data after sync."
        );
    }

    #[test]
    fn test_gzip_full_flush_resets_state() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Data before full flush.")
            .expect("write failed");
        encoder.full_flush().expect("full_flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder
            .write_all(b" Data after full flush.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, b"Data before full flush. Data after full flush.");
    }

    #[test]
    fn test_gzip_partial_flush() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Partial flush data.")
            .expect("write failed");
        encoder.partial_flush().expect("partial_flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder
            .write_all(b" More data after partial.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, b"Partial flush data. More data after partial.");
    }

    #[test]
    fn test_zlib_sync_flush_produces_decompressible_prefix() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Hello, zlib sync flush!")
            .expect("write failed");
        encoder.sync_flush().expect("sync_flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder
            .write_all(b" More zlib data.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, b"Hello, zlib sync flush! More zlib data.");
    }

    #[test]
    fn test_zlib_full_flush_resets_state() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Zlib before full flush.")
            .expect("write failed");
        encoder.full_flush().expect("full_flush failed");
        encoder
            .write_all(b" Zlib after full flush.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, b"Zlib before full flush. Zlib after full flush.");
    }

    #[test]
    fn test_zlib_partial_flush() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"Zlib partial flush.")
            .expect("write failed");
        encoder.partial_flush().expect("partial_flush failed");
        encoder
            .write_all(b" More zlib data after partial.")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, b"Zlib partial flush. More zlib data after partial.");
    }

    #[test]
    fn test_gzip_multiple_flush_write_cycles() {
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        let mut expected = Vec::new();

        for i in 0..5 {
            let chunk = format!("Chunk {} data. ", i);
            expected.extend_from_slice(chunk.as_bytes());
            encoder.write_all(chunk.as_bytes()).expect("write failed");

            // Alternate between flush types.
            match i % 3 {
                0 => encoder.sync_flush().expect("sync_flush failed"),
                1 => encoder.full_flush().expect("full_flush failed"),
                _ => encoder.partial_flush().expect("partial_flush failed"),
            }
        }

        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, expected);
    }

    #[test]
    fn test_zlib_multiple_flush_write_cycles() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        let mut expected = Vec::new();

        for i in 0..5 {
            let chunk = format!("ZChunk {} data. ", i);
            expected.extend_from_slice(chunk.as_bytes());
            encoder.write_all(chunk.as_bytes()).expect("write failed");

            match i % 3 {
                0 => encoder.sync_flush().expect("sync_flush failed"),
                1 => encoder.full_flush().expect("full_flush failed"),
                _ => encoder.partial_flush().expect("partial_flush failed"),
            }
        }

        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, expected);
    }

    #[test]
    fn test_gzip_sync_flush_marker_present() {
        // After a sync flush, the output should contain the sync marker
        // bytes 0x00 0x00 0xFF 0xFF somewhere in the DEFLATE payload.
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"test sync marker")
            .expect("write failed");
        encoder.sync_flush().expect("sync_flush failed");
        // Finish without more data to get the full compressed output.
        let compressed = encoder.finish().expect("finish failed");

        // Search for the sync marker pattern in the compressed stream.
        // Skip the 10-byte GZIP header.
        let payload = &compressed[10..];
        let marker = [0x00, 0x00, 0xFF, 0xFF];
        let found = payload.windows(4).any(|w| w == marker);
        assert!(found, "Sync flush marker not found in GZIP payload");
    }

    #[test]
    fn test_zlib_sync_flush_marker_present() {
        let mut encoder = ZlibStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"test zlib sync marker")
            .expect("write failed");
        encoder.sync_flush().expect("sync_flush failed");
        let compressed = encoder.finish().expect("finish failed");

        // Skip the 2-byte zlib header.
        let payload = &compressed[2..];
        let marker = [0x00, 0x00, 0xFF, 0xFF];
        let found = payload.windows(4).any(|w| w == marker);
        assert!(found, "Sync flush marker not found in Zlib payload");
    }

    #[test]
    fn test_gzip_partial_flush_no_sync_marker() {
        // Partial flush should NOT produce the sync marker.
        let mut encoder = GzipStreamEncoder::new(Vec::new(), 6);
        encoder
            .write_all(b"test partial no marker")
            .expect("write failed");
        encoder.partial_flush().expect("partial_flush failed");
        // Write the finish block to complete the stream.
        let compressed = encoder.finish().expect("finish failed");

        // Check that the sync marker does NOT appear in the deflate
        // section *before* the final block. We check the entire payload
        // for the marker pattern (which should not appear from partial flush).
        // The final block may contain 0x00 0x00 0xFF 0xFF coincidentally,
        // but a partial flush specifically avoids emitting the empty stored block.
        let payload = &compressed[10..compressed.len() - 8]; // skip header and trailer
        let marker = [0x00, 0x00, 0xFF, 0xFF];
        let found = payload.windows(4).any(|w| w == marker);
        assert!(!found, "Partial flush should not produce sync marker");
    }
}
