//! Streaming LZW compression and decompression.
//!
//! Provides [`LzwStreamEncoder`] and [`LzwStreamDecoder`] that implement the
//! standard [`std::io::Write`] and [`std::io::Read`] traits respectively,
//! enabling incremental processing of LZW-compressed data through standard
//! Rust I/O pipelines.
//!
//! # Design
//!
//! **Encoders** buffer all written data internally. When the internal buffer
//! reaches `block_size` bytes it is automatically flushed as a complete
//! LZW-compressed frame to the inner writer. Any remaining data is flushed
//! when [`finish`](LzwStreamEncoder::finish) is called. The output is a
//! sequence of independently decompressible LZW frames.
//!
//! **Decoders** eagerly read all compressed data from the inner reader on the
//! first `read` call, decompress it, and serve from an internal buffer.
//!
//! # Frame format
//!
//! Each frame is preceded by an 8-byte header:
//!
//! - 4 bytes: compressed frame length (`u32`, little-endian)
//! - 4 bytes: uncompressed frame length (`u32`, little-endian)
//!
//! Carrying the true uncompressed length lets the decoder size its output
//! exactly and validate each frame, instead of decompressing against a large
//! sentinel bound (a previous revision allocated a hardcoded 64 MiB buffer
//! per frame, which small malicious inputs could exploit as an allocation
//! DoS). This framing is private to this crate's streaming API and is not
//! part of the TIFF/GIF bitstream formats.
//!
//! # Modes
//!
//! [`LzwStreamMode`] selects between TIFF and GIF LZW variants:
//!
//! - **TIFF**: MSB-first bit order, early code change, clear codes
//!   (TIFF 6.0 / libtiff-compatible).
//! - **GIF**: LSB-first bit order, clear codes, standard code change.
//! - **Config**: an explicit [`crate::LzwConfig`], which is how a streamed
//!   frame gets a bit order ([`crate::LzwBitOrder`]) and a code width above
//!   12 (new in 0.4.2).
//!
//! # Example
//!
//! ```rust
//! use std::io::{Read, Write};
//! use oxiarc_lzw::streaming::{LzwStreamEncoder, LzwStreamDecoder, LzwStreamMode};
//!
//! // Compress using TIFF mode
//! let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
//! encoder.write_all(b"Hello, streaming LZW!").expect("write failed");
//! let compressed = encoder.finish().expect("finish failed");
//!
//! // Decompress
//! let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
//! let mut output = String::new();
//! decoder.read_to_string(&mut output).expect("read failed");
//! assert_eq!(output, "Hello, streaming LZW!");
//! ```

use crate::config::LzwConfig;
use crate::gif_lzw::{gif_compress, gif_decompress};
use crate::{compress, compress_tiff, decompress, decompress_tiff};
use std::io::{self, Read, Write};

/// Default block size for incremental encoder flushing (128 KiB).
const DEFAULT_BLOCK_SIZE: usize = 128 * 1024;

/// GIF default minimum code size (8 bits for 256-colour data).
const GIF_DEFAULT_MIN_CODE_SIZE: u8 = 8;

// ---------------------------------------------------------------------------
// LzwStreamMode
// ---------------------------------------------------------------------------

/// Selects the LZW variant used for streaming compression/decompression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LzwStreamMode {
    /// TIFF LZW: MSB-first, early code change, clear codes (TIFF 6.0).
    Tiff,
    /// GIF LZW: LSB-first, clear codes, standard code change.
    ///
    /// The inner `u8` is the GIF `minimum_code_size` (typically 8 for
    /// 256-colour images). Must satisfy `2 <= value <= 11`.
    Gif(u8),
    /// An explicit [`LzwConfig`], so a stream can use any supported code
    /// width (9-16) and either bit order (new in 0.4.2).
    ///
    /// [`LzwStreamMode::Tiff`] and [`LzwStreamMode::Gif`] each hard-code one
    /// packing; this variant is what carries
    /// [`crate::LzwBitOrder`] — and `max_bits` above 12 — through the
    /// streaming API. Frames are produced by [`crate::compress`] and read
    /// back by [`crate::decompress`], so
    /// `LzwStreamMode::Config(LzwConfig::TIFF)` is byte-identical to
    /// [`LzwStreamMode::Tiff`].
    ///
    /// An invalid configuration (see [`LzwConfig::validate`]) surfaces as an
    /// [`io::Error`] from the first `write`/`read`, not as a panic.
    ///
    /// Note that `LzwStreamMode::Config(LzwConfig::TIFF)` and
    /// [`LzwStreamMode::Tiff`] are **not** equal under `PartialEq` even
    /// though they produce identical bytes: compare the produced streams, or
    /// match on the variant, rather than testing modes for equality.
    Config(LzwConfig),
}

impl LzwStreamMode {
    /// Convenience constructor for GIF mode with the default minimum code
    /// size of 8.
    pub fn gif_default() -> Self {
        LzwStreamMode::Gif(GIF_DEFAULT_MIN_CODE_SIZE)
    }
}

// ---------------------------------------------------------------------------
// LzwStreamEncoder
// ---------------------------------------------------------------------------

/// Streaming LZW encoder that implements [`Write`].
///
/// Data written to this encoder is buffered internally. When the internal
/// buffer reaches `block_size` bytes it is automatically flushed as a
/// complete LZW-compressed frame to the inner writer. Any remaining data is
/// flushed when [`finish`](LzwStreamEncoder::finish) is called.
///
/// **Important:** you *must* call [`finish`](LzwStreamEncoder::finish) to
/// flush the final compressed data. Dropping the encoder without calling
/// `finish` will silently discard any buffered data.
pub struct LzwStreamEncoder<W: Write> {
    /// The wrapped writer that receives compressed output.
    inner: Option<W>,
    /// Internal buffer holding uncompressed data waiting to be flushed.
    buffer: Vec<u8>,
    /// LZW mode (TIFF or GIF).
    mode: LzwStreamMode,
    /// Whether `finish` has already been called.
    finished: bool,
    /// Threshold at which the buffer is automatically flushed.
    block_size: usize,
}

impl<W: Write> LzwStreamEncoder<W> {
    /// Create a new streaming LZW encoder wrapping `writer`.
    ///
    /// The `mode` parameter selects between TIFF and GIF LZW variants.
    pub fn new(writer: W, mode: LzwStreamMode) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            mode,
            finished: false,
            block_size: DEFAULT_BLOCK_SIZE,
        }
    }

    /// Set the block size used for incremental flushing.
    ///
    /// When the internal buffer reaches this many bytes it is automatically
    /// compressed and written to the inner writer.
    #[must_use]
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size.max(1);
        self
    }

    /// Finish compression and return the inner writer.
    ///
    /// This **must** be called to flush the final compressed data. Failing to
    /// call `finish` means all buffered data is lost.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if compression or writing to the inner writer
    /// fails.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.finished {
            self.flush_buffer_unconditional()?;
            self.finished = true;
        }
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("inner writer already taken"))
    }

    /// Compress `data` using the configured LZW mode and write it to `inner`.
    fn compress_and_write(&mut self, data: &[u8]) -> io::Result<()> {
        // Both header fields are u32; frames are bounded by `block_size`
        // via `maybe_flush_block`, but guard explicitly for callers that
        // configure absurd block sizes.
        let uncompressed_len = u32::try_from(data.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "LZW frame exceeds u32::MAX uncompressed bytes",
            )
        })?;
        let compressed = self.compress_block(data)?;
        let compressed_len = u32::try_from(compressed.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "LZW frame exceeds u32::MAX compressed bytes",
            )
        })?;
        if let Some(ref mut w) = self.inner {
            // 8-byte frame header: compressed length then uncompressed
            // length, both little-endian u32 (see module docs).
            w.write_all(&compressed_len.to_le_bytes())?;
            w.write_all(&uncompressed_len.to_le_bytes())?;
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Compress a single block according to the current mode.
    fn compress_block(&self, data: &[u8]) -> io::Result<Vec<u8>> {
        match self.mode {
            LzwStreamMode::Tiff => compress_tiff(data).map_err(|e| io::Error::other(e.to_string())),
            LzwStreamMode::Gif(min_code_size) => {
                gif_compress(data, min_code_size).map_err(|e| io::Error::other(e.to_string()))
            }
            LzwStreamMode::Config(config) => {
                compress(data, config).map_err(|e| io::Error::other(e.to_string()))
            }
        }
    }

    /// While the buffer holds at least `block_size` bytes, flush frames of
    /// exactly `block_size` bytes. This bounds every frame (and therefore
    /// the header's u32 length fields and the decoder's per-frame output
    /// allocation) regardless of how large a single `write` call was.
    fn maybe_flush_block(&mut self) -> io::Result<()> {
        while self.buffer.len() >= self.block_size {
            let rest = self.buffer.split_off(self.block_size);
            let data = std::mem::replace(&mut self.buffer, rest);
            self.compress_and_write(&data)?;
        }
        Ok(())
    }

    /// Always flush the current buffer contents.
    fn flush_buffer_unconditional(&mut self) -> io::Result<()> {
        let data = std::mem::take(&mut self.buffer);
        self.compress_and_write(&data)
    }

    /// Returns the number of uncompressed bytes currently buffered.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if `finish` has already been called.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns the current LZW mode.
    pub fn mode(&self) -> LzwStreamMode {
        self.mode
    }
}

impl<W: Write> Write for LzwStreamEncoder<W> {
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
            let data = std::mem::take(&mut self.buffer);
            self.compress_and_write(&data)?;
        }
        if let Some(ref mut w) = self.inner {
            w.flush()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LzwStreamDecoder
// ---------------------------------------------------------------------------

/// Streaming LZW decoder that implements [`Read`].
///
/// All compressed data is read eagerly from the inner reader on the first
/// `read` call, decompressed into an internal buffer, and then served from
/// that buffer for subsequent reads.
///
/// The decoder expects the framing produced by [`LzwStreamEncoder`]: each
/// compressed frame is preceded by an 8-byte header carrying the compressed
/// and uncompressed frame lengths (see the module docs).
pub struct LzwStreamDecoder<R: Read> {
    /// The wrapped reader providing compressed input.
    inner: R,
    /// Decompressed output buffer.
    output_buffer: Vec<u8>,
    /// Current read position inside `output_buffer`.
    output_pos: usize,
    /// Whether the compressed stream has been fully consumed.
    finished: bool,
    /// LZW mode (TIFF or GIF).
    mode: LzwStreamMode,
}

impl<R: Read> LzwStreamDecoder<R> {
    /// Create a new streaming LZW decoder wrapping `reader`.
    ///
    /// The `mode` must match the mode used during encoding.
    pub fn new(reader: R, mode: LzwStreamMode) -> Self {
        Self {
            inner: reader,
            output_buffer: Vec::new(),
            output_pos: 0,
            finished: false,
            mode,
        }
    }

    /// Consume the decoder and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read and decompress all compressed data from the inner reader.
    ///
    /// Handles multiple concatenated frames by reading the length-prefix for
    /// each frame and decompressing independently.
    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.finished || self.output_pos < self.output_buffer.len() {
            return Ok(());
        }

        let mut raw = Vec::new();
        self.inner.read_to_end(&mut raw)?;

        if raw.is_empty() {
            self.finished = true;
            return Ok(());
        }

        let mut all_decompressed = Vec::new();
        let mut offset = 0;

        while offset < raw.len() {
            // Read the 8-byte frame header (compressed + uncompressed len).
            if offset + 8 > raw.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated LZW frame header",
                ));
            }
            let frame_len = u32::from_le_bytes([
                raw[offset],
                raw[offset + 1],
                raw[offset + 2],
                raw[offset + 3],
            ]) as usize;
            let uncompressed_len = u32::from_le_bytes([
                raw[offset + 4],
                raw[offset + 5],
                raw[offset + 6],
                raw[offset + 7],
            ]) as usize;
            offset += 8;

            if offset + frame_len > raw.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated LZW frame data",
                ));
            }

            let frame_data = &raw[offset..offset + frame_len];
            offset += frame_len;

            let decompressed = self.decompress_frame(frame_data, uncompressed_len)?;
            // A frame that decodes to a different length than its header
            // declares is corrupt; never silently return short/padded data.
            if decompressed.len() != uncompressed_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "LZW frame decoded to {} bytes, header declared {}",
                        decompressed.len(),
                        uncompressed_len
                    ),
                ));
            }
            all_decompressed.extend_from_slice(&decompressed);
        }

        self.output_buffer = all_decompressed;
        self.output_pos = 0;
        self.finished = true;

        Ok(())
    }

    /// Decompress a single frame according to the current mode.
    ///
    /// `uncompressed_len` is the frame header's declared output size. It is
    /// untrusted: [`crate::LzwDecoder`] clamps its up-front allocation and
    /// grows incrementally, so a lying header cannot force a large
    /// allocation — the caller cross-checks the decoded length afterwards.
    fn decompress_frame(&self, data: &[u8], uncompressed_len: usize) -> io::Result<Vec<u8>> {
        match self.mode {
            LzwStreamMode::Tiff => {
                decompress_tiff(data, uncompressed_len).map_err(|e| io::Error::other(e.to_string()))
            }
            LzwStreamMode::Gif(min_code_size) => {
                gif_decompress(data, min_code_size).map_err(|e| io::Error::other(e.to_string()))
            }
            LzwStreamMode::Config(config) => decompress(data, uncompressed_len, config)
                .map_err(|e| io::Error::other(e.to_string())),
        }
    }

    /// Returns the total number of decompressed bytes available.
    pub fn decompressed_size(&self) -> usize {
        self.output_buffer.len()
    }

    /// Returns `true` if all decompressed data has been read.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buffer.len()
    }

    /// Returns the current LZW mode.
    pub fn mode(&self) -> LzwStreamMode {
        self.mode
    }
}

impl<R: Read> Read for LzwStreamDecoder<R> {
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TIFF mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tiff_roundtrip_basic() {
        let original = b"TOBEORNOTTOBEORTOBEORNOT";

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_tiff_roundtrip_multiple_writes() {
        let parts: &[&[u8]] = &[b"Hello, ", b"streaming ", b"LZW!"];

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        for part in parts {
            encoder.write_all(part).expect("write failed");
        }
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"Hello, streaming LZW!");
    }

    #[test]
    fn test_tiff_roundtrip_large_data() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        encoder.write_all(&original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original);
    }

    // -----------------------------------------------------------------------
    // GIF mode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_gif_roundtrip_basic() {
        let original = b"TOBEORNOTTOBEORTOBEORNOT";

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::gif_default());
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::gif_default());
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_gif_roundtrip_large_data() {
        let original = b"The quick brown fox jumps over the lazy dog. ".repeat(100);

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::gif_default());
        encoder.write_all(&original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::gif_default());
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original);
    }

    // -----------------------------------------------------------------------
    // Empty data
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_tiff() {
        let encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert!(output.is_empty());
    }

    #[test]
    fn test_empty_gif() {
        let encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::gif_default());
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::gif_default());
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert!(output.is_empty());
    }

    #[test]
    fn test_decoder_empty_input() {
        let mut decoder = LzwStreamDecoder::new(&[][..], LzwStreamMode::Tiff);
        let mut buf = [0u8; 16];
        let n = decoder.read(&mut buf).expect("read failed");
        assert_eq!(n, 0);
    }

    // -----------------------------------------------------------------------
    // Small reads
    // -----------------------------------------------------------------------

    #[test]
    fn test_small_reads() {
        let original = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";

        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
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

    // -----------------------------------------------------------------------
    // Block size
    // -----------------------------------------------------------------------

    #[test]
    fn test_with_block_size() {
        // Use a very small block size to force multiple flushes.
        let mut encoder =
            LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff).with_block_size(10);
        encoder
            .write_all(b"This is more than ten bytes of data")
            .expect("write failed");
        // After writing 35 bytes with block_size=10, there should have been
        // at least one automatic flush.
        assert!(encoder.buffered_bytes() < 35);
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"This is more than ten bytes of data");
    }

    #[test]
    fn test_gif_with_block_size() {
        let mut encoder =
            LzwStreamEncoder::new(Vec::new(), LzwStreamMode::gif_default()).with_block_size(20);
        let data = b"AAAAAABBBBBBCCCCCCDDDDDDEEEEEE";
        encoder.write_all(data).expect("write failed");
        assert!(encoder.buffered_bytes() < data.len());
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::gif_default());
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, data.as_slice());
    }

    // -----------------------------------------------------------------------
    // Flush
    // -----------------------------------------------------------------------

    #[test]
    fn test_flush() {
        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        encoder
            .write_all(b"data before flush")
            .expect("write failed");
        encoder.flush().expect("flush failed");
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b" more data").expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        // The output contains two concatenated LZW frames.
        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"data before flush more data");
    }

    // -----------------------------------------------------------------------
    // Utility methods
    // -----------------------------------------------------------------------

    #[test]
    fn test_buffered_bytes() {
        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b"12345").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 5);
        encoder.write_all(b"67890").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 10);
    }

    #[test]
    fn test_is_finished() {
        let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        assert!(!encoder.is_finished());
        encoder.write_all(b"data").expect("write failed");
        assert!(!encoder.is_finished());
    }

    #[test]
    fn test_decoder_is_finished() {
        let original = b"short";

        let mut enc = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        enc.write_all(original).expect("write failed");
        let compressed = enc.finish().expect("finish failed");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        assert!(!decoder.is_finished());

        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read failed");
        assert!(decoder.is_finished());
    }

    #[test]
    fn test_decoder_into_inner() {
        let data = vec![1u8, 2, 3, 4, 5];
        let decoder = LzwStreamDecoder::new(data.as_slice(), LzwStreamMode::Tiff);
        let inner = decoder.into_inner();
        assert_eq!(inner, data.as_slice());
    }

    #[test]
    fn test_mode_accessor() {
        let encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
        assert_eq!(encoder.mode(), LzwStreamMode::Tiff);

        let decoder = LzwStreamDecoder::new(&[][..], LzwStreamMode::gif_default());
        assert_eq!(decoder.mode(), LzwStreamMode::Gif(8));
    }

    // -----------------------------------------------------------------------
    // Frame-header hardening (LZW-02 regression tests)
    // -----------------------------------------------------------------------

    /// Build one framed TIFF frame (header + compressed payload) by hand.
    fn frame_tiff(data: &[u8]) -> Vec<u8> {
        let compressed = crate::compress_tiff(data).expect("compress frame");
        let mut out = Vec::with_capacity(8 + compressed.len());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&compressed);
        out
    }

    /// Regression for the 64 MiB-per-frame sentinel allocation DoS: a stream
    /// of many tiny frames previously forced one hardcoded 64 MiB
    /// `Vec::with_capacity` per frame (50,000 frames = ~3 TiB of cumulative
    /// allocator traffic for ~250 KiB of input). With the uncompressed
    /// length carried in the frame header and the decoder's clamped
    /// incremental growth, this must decode promptly and correctly.
    #[test]
    fn test_many_tiny_frames_no_allocation_amplification() {
        let one = frame_tiff(b"A");
        let count = 50_000usize;
        let mut stream = Vec::with_capacity(one.len() * count);
        for _ in 0..count {
            stream.extend_from_slice(&one);
        }

        let mut decoder = LzwStreamDecoder::new(&stream[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output.len(), count);
        assert!(output.iter().all(|&b| b == b'A'));
    }

    /// A frame whose header declares a different uncompressed length than
    /// the payload actually decodes to must be rejected, not padded or
    /// silently truncated.
    #[test]
    fn test_frame_length_mismatch_rejected() {
        let mut stream = frame_tiff(b"hello world");
        // Corrupt the declared uncompressed length (bytes 4..8).
        stream[4..8].copy_from_slice(&12u32.to_le_bytes());

        let mut decoder = LzwStreamDecoder::new(&stream[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        let err = decoder.read_to_end(&mut output).expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A tiny frame lying about a huge uncompressed length must fail fast
    /// without attempting a matching up-front allocation.
    #[test]
    fn test_huge_declared_length_rejected_without_prealloc() {
        let mut stream = frame_tiff(b"A");
        stream[4..8].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut decoder = LzwStreamDecoder::new(&stream[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        assert!(decoder.read_to_end(&mut output).is_err());
    }

    /// Truncated frame headers and payloads must error cleanly.
    #[test]
    fn test_truncated_frames_rejected() {
        let stream = frame_tiff(b"some frame payload");
        for cut in [1, 4, 7, stream.len() - 1] {
            let mut decoder = LzwStreamDecoder::new(&stream[..cut], LzwStreamMode::Tiff);
            let mut output = Vec::new();
            assert!(
                decoder.read_to_end(&mut output).is_err(),
                "cut at {cut} must be rejected"
            );
        }
    }

    /// Large writes are split into bounded frames of `block_size` bytes.
    #[test]
    fn test_large_write_split_into_bounded_frames() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut encoder =
            LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff).with_block_size(4096);
        encoder.write_all(&data).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        // Walk the frame headers: every declared uncompressed length must be
        // bounded by the block size.
        let mut offset = 0usize;
        let mut frames = 0usize;
        while offset < compressed.len() {
            let comp_len =
                u32::from_le_bytes(compressed[offset..offset + 4].try_into().expect("comp len"))
                    as usize;
            let uncomp_len = u32::from_le_bytes(
                compressed[offset + 4..offset + 8]
                    .try_into()
                    .expect("uncomp len"),
            ) as usize;
            assert!(uncomp_len <= 4096, "frame declares {uncomp_len} bytes");
            offset += 8 + comp_len;
            frames += 1;
        }
        assert!(frames >= 100_000 / 4096, "expected many bounded frames");

        let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");
        assert_eq!(output, data);
    }
}
