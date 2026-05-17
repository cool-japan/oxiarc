//! Snappy framed format (streaming) encoder and decoder.
//!
//! The Snappy framed format wraps raw Snappy blocks with:
//! - A stream identifier chunk at the start
//! - Compressed or uncompressed data chunks with CRC32C checksums
//! - Maximum uncompressed chunk size of 65536 bytes
//!
//! This module provides `FrameEncoder` (compression) and `FrameDecoder`
//! (decompression) that implement `Write` and `Read` respectively.

use std::io::{self, Read, Write};
use std::sync::Arc;

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::compress;
use crate::crc32c::{crc32c, masked_crc32c};
use crate::decompress;
use crate::error::SnappyError;
use crate::pool::{PoolInner, SnappyPool};

/// Stream identifier magic bytes: "sNaPpY" (0xff 0x06 0x00 0x00 0x73 0x4e 0x61 0x50 0x70 0x59)
const STREAM_IDENTIFIER: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

/// OxiArc dictionary-frame skippable chunk type.
/// Skippable chunks are 0x80..=0xFE.  We use 0xFE to identify the dict info.
const CHUNK_TYPE_OXIARC_DICT: u8 = 0xFE;

/// Magic prefix for the OxiArc dict info chunk body.
const OXIARC_DICT_MAGIC: &[u8] = b"OXIAD";

/// The "sNaPpY" body of the stream identifier (without the chunk header).
const STREAM_BODY: [u8; 6] = [0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

/// Chunk type: compressed data
const CHUNK_TYPE_COMPRESSED: u8 = 0x00;

/// Chunk type: uncompressed data
const CHUNK_TYPE_UNCOMPRESSED: u8 = 0x01;

/// Chunk type: stream identifier
const CHUNK_TYPE_STREAM_ID: u8 = 0xFF;

/// Maximum uncompressed chunk size (64 KiB).
const MAX_UNCOMPRESSED_CHUNK_SIZE: usize = 65536;

/// Snappy framed format encoder.
///
/// Wraps a writer and compresses data written to it using the Snappy
/// framed format. Data is buffered internally and flushed as complete
/// chunks.
///
/// # Example
/// ```
/// use oxiarc_snappy::FrameEncoder;
/// use std::io::Write;
///
/// let mut compressed = Vec::new();
/// {
///     let mut encoder = FrameEncoder::new(&mut compressed);
///     encoder.write_all(b"Hello, World!").unwrap();
///     encoder.finish().unwrap();
/// }
/// ```
pub struct FrameEncoder<W: Write> {
    inner: Option<W>,
    buffer: Vec<u8>,
    header_written: bool,
    /// Optional progress sink; receives cumulative bytes processed per chunk.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token; checked before each chunk is written.
    cancel: Option<CancellationToken>,
    /// Cumulative uncompressed bytes that have been encoded into chunks.
    bytes_processed: u64,
    /// Optional shared memory pool for buffer reuse.
    pool: Option<SnappyPool>,
}

impl<W: Write> FrameEncoder<W> {
    /// Create a new framed encoder wrapping the given writer.
    ///
    /// The stream identifier chunk will be written on the first `write` call.
    pub fn new(inner: W) -> Self {
        Self {
            inner: Some(inner),
            buffer: Vec::with_capacity(MAX_UNCOMPRESSED_CHUNK_SIZE),
            header_written: false,
            progress: None,
            cancel: None,
            bytes_processed: 0,
            pool: None,
        }
    }

    /// Create a new framed encoder that reuses scratch buffers from `pool`.
    ///
    /// All other behaviour is identical to [`FrameEncoder::new`].
    pub fn with_pool(inner: W, pool: &SnappyPool) -> Self {
        let mut enc = Self::new(inner);
        enc.pool = Some(pool.clone());
        enc
    }

    /// Attach a progress sink that will receive `on_progress` callbacks once
    /// per encoded chunk.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.  The token is checked before each chunk
    /// is written; if it has been cancelled the operation returns an I/O
    /// error with the message `"operation cancelled"`.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Finish encoding and return the underlying writer.
    ///
    /// This flushes any remaining buffered data as a final chunk.
    ///
    /// # Errors
    /// Returns an I/O error if writing fails.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_buffer()?;
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("encoder already finished"))
    }

    /// Write the stream identifier if it hasn't been written yet.
    fn ensure_header(&mut self) -> io::Result<()> {
        if !self.header_written {
            if let Some(ref mut w) = self.inner {
                w.write_all(&STREAM_IDENTIFIER)?;
            }
            self.header_written = true;
        }
        Ok(())
    }

    /// Flush the internal buffer as a compressed chunk.
    fn flush_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        self.ensure_header()?;

        // Check cancellation before committing the chunk.
        if let Some(ref token) = self.cancel {
            token
                .check()
                .map_err(|_| io::Error::other("operation cancelled"))?;
        }

        let chunk_len = self.buffer.len() as u64;

        // Swap the full staging buffer out so we can pass a slice to write_chunk
        // without holding a conflicting borrow on `self`.  When a pool is active,
        // the replacement buffer is acquired from the pool (preserving capacity);
        // otherwise we use the standard `mem::take` path.
        let data = if let Some(ref snappy_pool) = self.pool {
            let pi = &snappy_pool.inner;
            let mut replacement = {
                let mut guard = pi.encoder_scratch.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(mut b) = guard.pop() {
                    pi.encoder_scratch_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    b.clear();
                    b
                } else {
                    pi.encoder_scratch_allocs
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Vec::with_capacity(crate::pool::ENCODER_SCRATCH_CAP)
                }
            };
            // Swap: `replacement` (empty, capacity preserved) becomes new staging;
            // `self.buffer` (full data) is returned as `data`.
            std::mem::swap(&mut self.buffer, &mut replacement);
            replacement
        } else {
            std::mem::take(&mut self.buffer)
        };

        self.write_chunk(&data)?;

        // Return `data` to the pool after writing.
        if let Some(ref snappy_pool) = self.pool {
            let pi = &snappy_pool.inner;
            let mut buf = data;
            buf.clear();
            let mut guard = pi.encoder_scratch.lock().unwrap_or_else(|e| e.into_inner());
            if guard.len() < pi.cap {
                guard.push(buf);
            }
        }

        // Update the running total and notify the progress sink.
        self.bytes_processed += chunk_len;
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }

        Ok(())
    }

    /// Write a single chunk of data (must be <= MAX_UNCOMPRESSED_CHUNK_SIZE).
    fn write_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        let writer = self
            .inner
            .as_mut()
            .ok_or_else(|| io::Error::other("encoder already finished"))?;

        let checksum = masked_crc32c(data);
        let compressed = compress::compress(data);

        // Use compressed format only if it actually saves space.
        // The compressed data in a chunk includes the 4-byte checksum.
        if compressed.len() < data.len() {
            // Compressed chunk
            let chunk_len = 4 + compressed.len(); // 4 bytes checksum + compressed data
            write_chunk_header(writer, CHUNK_TYPE_COMPRESSED, chunk_len)?;
            writer.write_all(&checksum.to_le_bytes())?;
            writer.write_all(&compressed)?;
        } else {
            // Uncompressed chunk (compression didn't help)
            let chunk_len = 4 + data.len(); // 4 bytes checksum + raw data
            write_chunk_header(writer, CHUNK_TYPE_UNCOMPRESSED, chunk_len)?;
            writer.write_all(&checksum.to_le_bytes())?;
            writer.write_all(data)?;
        }

        Ok(())
    }
}

impl<W: Write> Write for FrameEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        self.ensure_header()?;

        let mut written = 0;

        while written < buf.len() {
            let remaining_capacity = MAX_UNCOMPRESSED_CHUNK_SIZE - self.buffer.len();
            let to_copy = remaining_capacity.min(buf.len() - written);

            self.buffer
                .extend_from_slice(&buf[written..written + to_copy]);
            written += to_copy;

            if self.buffer.len() >= MAX_UNCOMPRESSED_CHUNK_SIZE {
                self.flush_buffer()?;
            }
        }

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        if let Some(ref mut w) = self.inner {
            w.flush()?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for FrameEncoder<W> {
    fn drop(&mut self) {
        // Best-effort flush on drop; errors are silently ignored
        // since we can't return them from Drop.
        if !self.buffer.is_empty() && self.inner.is_some() {
            let _ = self.flush_buffer();
        }
    }
}

/// Snappy framed format decoder.
///
/// Wraps a reader and decompresses framed Snappy data read from it.
///
/// # Example
/// ```no_run
/// use oxiarc_snappy::FrameDecoder;
/// use std::io::Read;
///
/// let compressed_data: Vec<u8> = vec![];
/// let mut decoder = FrameDecoder::new(&compressed_data[..]);
/// let mut output = Vec::new();
/// decoder.read_to_end(&mut output).unwrap();
/// ```
pub struct FrameDecoder<R: Read> {
    inner: R,
    /// Decoded but not yet consumed output data.
    output_buffer: Vec<u8>,
    /// Current read position within output_buffer.
    output_pos: usize,
    /// Whether the stream identifier has been validated.
    header_validated: bool,
    /// Whether we've reached the end of the stream.
    at_eof: bool,
    /// Optional progress sink; receives cumulative bytes processed per chunk.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token; checked before each chunk is decoded.
    cancel: Option<CancellationToken>,
    /// Cumulative decompressed bytes that have been produced.
    bytes_processed: u64,
    /// Optional shared memory pool for scratch buffer reuse.
    pool: Option<Arc<PoolInner>>,
}

impl<R: Read> FrameDecoder<R> {
    /// Create a new framed decoder wrapping the given reader.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            output_buffer: Vec::new(),
            output_pos: 0,
            header_validated: false,
            at_eof: false,
            progress: None,
            cancel: None,
            bytes_processed: 0,
            pool: None,
        }
    }

    /// Create a new framed decoder that reuses scratch buffers from `pool`.
    ///
    /// All other behaviour is identical to [`FrameDecoder::new`].
    pub fn with_pool(inner: R, pool: &SnappyPool) -> Self {
        let mut dec = Self::new(inner);
        dec.pool = Some(Arc::clone(&pool.inner));
        dec
    }

    /// Attach a progress sink that will receive `on_progress` callbacks once
    /// per decoded chunk.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.  The token is checked before each chunk
    /// is decoded; if it has been cancelled the operation returns an I/O
    /// error with the message `"operation cancelled"`.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Read and validate the stream identifier chunk.
    fn validate_header(&mut self) -> io::Result<()> {
        if self.header_validated {
            return Ok(());
        }

        let mut header = [0u8; 10];
        match self.inner.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.at_eof = true;
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        if header != STREAM_IDENTIFIER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                SnappyError::InvalidStreamIdentifier.to_string(),
            ));
        }

        self.header_validated = true;
        Ok(())
    }

    /// Read the next chunk from the stream and decode it into the output buffer.
    fn read_next_chunk(&mut self) -> io::Result<bool> {
        // Check cancellation before beginning each new chunk.
        if let Some(ref token) = self.cancel {
            token
                .check()
                .map_err(|_| io::Error::other("operation cancelled"))?;
        }

        // Read chunk header: 1 byte type + 3 bytes length (little-endian)
        let mut chunk_header = [0u8; 4];
        match self.inner.read_exact(&mut chunk_header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.at_eof = true;
                return Ok(false);
            }
            Err(e) => return Err(e),
        }

        let chunk_type = chunk_header[0];
        let chunk_len = (chunk_header[1] as usize)
            | ((chunk_header[2] as usize) << 8)
            | ((chunk_header[3] as usize) << 16);

        match chunk_type {
            CHUNK_TYPE_COMPRESSED => {
                self.read_compressed_chunk(chunk_len)?;
                self.emit_chunk_progress();
                Ok(true)
            }
            CHUNK_TYPE_UNCOMPRESSED => {
                self.read_uncompressed_chunk(chunk_len)?;
                self.emit_chunk_progress();
                Ok(true)
            }
            CHUNK_TYPE_STREAM_ID => {
                // Another stream identifier (valid, just skip/validate)
                self.read_stream_identifier_chunk(chunk_len)?;
                Ok(true)
            }
            0x02..=0x7F => {
                // Reserved unskippable chunk -- error
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    SnappyError::InvalidChunkType { chunk_type }.to_string(),
                ))
            }
            _ => {
                // 0x80..=0xFE: Skippable chunk -- skip the data
                let mut skip_buf = vec![0u8; chunk_len];
                self.inner.read_exact(&mut skip_buf)?;
                Ok(true)
            }
        }
    }

    /// Update `bytes_processed` with the latest chunk size and notify the
    /// progress sink.  Called after a data chunk has been placed in
    /// `output_buffer`.
    fn emit_chunk_progress(&mut self) {
        let chunk_size = self.output_buffer.len() as u64;
        self.bytes_processed += chunk_size;
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }
    }

    /// Acquire a scratch buffer for reading chunk data.
    ///
    /// Returns a `Vec<u8>` sized to `chunk_len` bytes (zeroed), either from
    /// the pool or freshly allocated.
    fn acquire_decoder_scratch(&mut self, chunk_len: usize) -> Vec<u8> {
        if let Some(ref pool_inner) = self.pool {
            let mut guard = pool_inner
                .decoder_scratch
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(mut b) = guard.pop() {
                pool_inner
                    .decoder_scratch_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                b.clear();
                b.resize(chunk_len, 0);
                return b;
            }
            pool_inner
                .decoder_scratch_allocs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        vec![0u8; chunk_len]
    }

    /// Return a scratch buffer to the pool (no-op if no pool is active).
    fn release_decoder_scratch(&self, mut buf: Vec<u8>) {
        if let Some(ref pool_inner) = self.pool {
            buf.clear();
            let mut guard = pool_inner
                .decoder_scratch
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if guard.len() < pool_inner.cap {
                guard.push(buf);
            }
        }
    }

    /// Read and decompress a compressed data chunk.
    fn read_compressed_chunk(&mut self, chunk_len: usize) -> io::Result<()> {
        if chunk_len < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed chunk too short for checksum",
            ));
        }

        let mut chunk_data = self.acquire_decoder_scratch(chunk_len);
        self.inner.read_exact(&mut chunk_data)?;

        // First 4 bytes are the masked CRC32C
        let expected_checksum =
            u32::from_le_bytes([chunk_data[0], chunk_data[1], chunk_data[2], chunk_data[3]]);

        let compressed_data = &chunk_data[4..];
        let decompressed = decompress::decompress(compressed_data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        // Return scratch buffer to pool before we replace output_buffer.
        self.release_decoder_scratch(chunk_data);

        // Verify checksum
        let computed_checksum = masked_crc32c(&decompressed);
        if expected_checksum != computed_checksum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                SnappyError::ChecksumMismatch {
                    expected: expected_checksum,
                    computed: computed_checksum,
                }
                .to_string(),
            ));
        }

        self.output_buffer = decompressed;
        self.output_pos = 0;
        Ok(())
    }

    /// Read an uncompressed data chunk.
    fn read_uncompressed_chunk(&mut self, chunk_len: usize) -> io::Result<()> {
        if chunk_len < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "uncompressed chunk too short for checksum",
            ));
        }

        let mut chunk_data = self.acquire_decoder_scratch(chunk_len);
        self.inner.read_exact(&mut chunk_data)?;

        let expected_checksum =
            u32::from_le_bytes([chunk_data[0], chunk_data[1], chunk_data[2], chunk_data[3]]);

        let data_slice = chunk_data[4..].to_vec();

        self.release_decoder_scratch(chunk_data);

        let computed_checksum = masked_crc32c(&data_slice);
        if expected_checksum != computed_checksum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                SnappyError::ChecksumMismatch {
                    expected: expected_checksum,
                    computed: computed_checksum,
                }
                .to_string(),
            ));
        }

        self.output_buffer = data_slice;
        self.output_pos = 0;
        Ok(())
    }

    /// Read and validate a stream identifier chunk body.
    fn read_stream_identifier_chunk(&mut self, chunk_len: usize) -> io::Result<()> {
        if chunk_len != 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid stream identifier length",
            ));
        }

        let mut body = [0u8; 6];
        self.inner.read_exact(&mut body)?;

        if body != STREAM_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                SnappyError::InvalidStreamIdentifier.to_string(),
            ));
        }

        Ok(())
    }
}

impl<R: Read> Read for FrameDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Validate stream header on first read
        if !self.header_validated && !self.at_eof {
            self.validate_header()?;
        }

        loop {
            if self.at_eof {
                return Ok(0);
            }

            // If there's data in the output buffer, return it
            let available = self.output_buffer.len() - self.output_pos;
            if available > 0 {
                let to_copy = available.min(buf.len());
                buf[..to_copy].copy_from_slice(
                    &self.output_buffer[self.output_pos..self.output_pos + to_copy],
                );
                self.output_pos += to_copy;
                return Ok(to_copy);
            }

            // Try to read the next chunk
            if !self.read_next_chunk()? {
                return Ok(0);
            }
        }
    }
}

/// Write a chunk header (type byte + 3-byte little-endian length).
fn write_chunk_header(writer: &mut impl Write, chunk_type: u8, data_len: usize) -> io::Result<()> {
    let header = [
        chunk_type,
        (data_len & 0xFF) as u8,
        ((data_len >> 8) & 0xFF) as u8,
        ((data_len >> 16) & 0xFF) as u8,
    ];
    writer.write_all(&header)
}

/// Compress `input` using the Snappy framing format, reusing scratch buffers
/// from `pool` to amortise per-chunk allocation costs.
///
/// Output is byte-for-byte compatible with the serial [`FrameEncoder`] and
/// can be decoded by [`FrameDecoder`].
///
/// # Errors
///
/// Returns an [`io::Error`] if the internal write operations fail (in practice
/// this only happens on allocation failures when writing to a `Vec`).
pub fn compress_frame_pooled(input: &[u8], pool: &SnappyPool) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut encoder = FrameEncoder::with_pool(&mut output, pool);
    encoder.write_all(input)?;
    encoder.finish()?;
    Ok(output)
}

/// Compress `input` using the Snappy framing format with a prefix dictionary.
///
/// Each data chunk is compressed via
/// [`crate::compress::compress_block_with_dict`] so matches into `dict` can
/// reduce the compressed size when the input shares substrings with the dict.
///
/// The output begins with a custom **OxiArc dictionary frame** chunk (chunk
/// type `0xFE`) that embeds a CRC32C of `dict` and the dict length.
/// [`decompress_frame_with_dict`] uses this header to detect mismatched
/// dictionaries before attempting decompression.
///
/// # Format
/// ```text
/// [Snappy stream identifier, 10 bytes]
/// [OxiArc dict-info chunk, chunk-type=0xFE]
///   body: b"OXIAD" (5) | crc32c(dict) as LE u32 (4) | dict_len as LE u32 (4)
/// [compressed data chunks, each chunk-compressed with dict]
/// ```
///
/// **This format is NOT compatible with standard Snappy frame readers.**
/// Use [`decompress_frame_with_dict`] to decode.
///
/// The maximum dictionary size is 64 KiB; if `dict` is longer only the last
/// 64 KiB is used (mirroring the block-level encoder).
pub fn compress_frame_with_dict(input: &[u8], dict: &[u8]) -> Vec<u8> {
    // Clamp dict to the last 64 KiB.
    let dict = if dict.len() > 65536 {
        &dict[dict.len() - 65536..]
    } else {
        dict
    };

    let mut output = Vec::new();

    // 1. Snappy stream identifier.
    output.extend_from_slice(&STREAM_IDENTIFIER);

    // 2. OxiArc dict-info skippable chunk.
    //    body: b"OXIAD" (5) | crc32c(dict) LE u32 (4) | dict_len LE u32 (4) = 13 bytes.
    let dict_crc = crc32c(dict);
    let dict_len_u32 = dict.len() as u32;
    let mut dict_body = Vec::with_capacity(13);
    dict_body.extend_from_slice(OXIARC_DICT_MAGIC);
    dict_body.extend_from_slice(&dict_crc.to_le_bytes());
    dict_body.extend_from_slice(&dict_len_u32.to_le_bytes());

    // Chunk header: [chunk_type (1)] [body_len (3 bytes LE)]
    let body_len = dict_body.len();
    output.push(CHUNK_TYPE_OXIARC_DICT);
    output.push((body_len & 0xFF) as u8);
    output.push(((body_len >> 8) & 0xFF) as u8);
    output.push(((body_len >> 16) & 0xFF) as u8);
    output.extend_from_slice(&dict_body);

    // 3. Data chunks (standard framing but block-compressed with dict).
    let mut src_pos = 0usize;
    while src_pos < input.len() {
        let chunk_end = (src_pos + MAX_UNCOMPRESSED_CHUNK_SIZE).min(input.len());
        let chunk_data = &input[src_pos..chunk_end];

        let checksum = masked_crc32c(chunk_data);
        let compressed = compress::compress_block_with_dict(chunk_data, dict);

        if compressed.len() < chunk_data.len() {
            // Compressed chunk.
            let chunk_len = 4 + compressed.len();
            write_chunk_header_vec(&mut output, CHUNK_TYPE_COMPRESSED, chunk_len);
            output.extend_from_slice(&checksum.to_le_bytes());
            output.extend_from_slice(&compressed);
        } else {
            // Uncompressed chunk (compression didn't help).
            let chunk_len = 4 + chunk_data.len();
            write_chunk_header_vec(&mut output, CHUNK_TYPE_UNCOMPRESSED, chunk_len);
            output.extend_from_slice(&checksum.to_le_bytes());
            output.extend_from_slice(chunk_data);
        }

        src_pos = chunk_end;
    }

    output
}

/// Decompress data produced by [`compress_frame_with_dict`].
///
/// The `dict` must be identical to the one used during compression.  The
/// OxiArc dict-info chunk embedded in the frame is validated: if the CRC32C
/// of the supplied dict does not match the stored CRC, an `InvalidData` error
/// is returned before any decompression is attempted.
///
/// **This function only processes frames produced by [`compress_frame_with_dict`].**
/// Standard Snappy frames (without the `0xFE` dict chunk) will be rejected.
///
/// # Errors
/// Returns an error if the data is malformed, truncated, or the wrong dict is supplied.
pub fn decompress_frame_with_dict(input: &[u8], dict: &[u8]) -> Result<Vec<u8>, SnappyError> {
    // Clamp dict to the last 64 KiB.
    let dict = if dict.len() > 65536 {
        &dict[dict.len() - 65536..]
    } else {
        dict
    };

    let mut pos = 0usize;

    // 1. Read and validate the stream identifier.
    if pos + 10 > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "stream identifier",
        });
    }
    if input[pos..pos + 10] != STREAM_IDENTIFIER[..] {
        return Err(SnappyError::InvalidStreamIdentifier);
    }
    pos += 10;

    // 2. Read and validate the OxiArc dict-info chunk.
    if pos + 4 > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "dict-info chunk header",
        });
    }
    let dict_chunk_type = input[pos];
    let dict_chunk_body_len = (input[pos + 1] as usize)
        | ((input[pos + 2] as usize) << 8)
        | ((input[pos + 3] as usize) << 16);
    pos += 4;

    if dict_chunk_type != CHUNK_TYPE_OXIARC_DICT {
        return Err(SnappyError::CorruptedData {
            message: format!(
                "expected OxiArc dict-info chunk (0xFE), found {dict_chunk_type:#04x}"
            ),
        });
    }

    if dict_chunk_body_len < 13 {
        return Err(SnappyError::CorruptedData {
            message: format!("OxiArc dict-info chunk body too short: {dict_chunk_body_len} bytes"),
        });
    }

    if pos + dict_chunk_body_len > input.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "dict-info chunk body",
        });
    }

    let dict_body = &input[pos..pos + dict_chunk_body_len];
    pos += dict_chunk_body_len;

    // Validate magic.
    if &dict_body[..5] != OXIARC_DICT_MAGIC {
        return Err(SnappyError::CorruptedData {
            message: "OxiArc dict-info magic mismatch".to_string(),
        });
    }

    let stored_crc = u32::from_le_bytes([dict_body[5], dict_body[6], dict_body[7], dict_body[8]]);
    let stored_len =
        u32::from_le_bytes([dict_body[9], dict_body[10], dict_body[11], dict_body[12]]) as usize;

    // Validate dict CRC and length.
    let computed_crc = crc32c(dict);
    if computed_crc != stored_crc {
        return Err(SnappyError::ChecksumMismatch {
            expected: stored_crc,
            computed: computed_crc,
        });
    }
    if dict.len() != stored_len {
        return Err(SnappyError::CorruptedData {
            message: format!(
                "dict length mismatch: frame has {stored_len} bytes, supplied dict is {} bytes",
                dict.len()
            ),
        });
    }

    // 3. Decode data chunks.
    let mut output = Vec::new();

    while pos < input.len() {
        if pos + 4 > input.len() {
            return Err(SnappyError::UnexpectedEof {
                context: "chunk header",
            });
        }
        let chunk_type = input[pos];
        let chunk_body_len = (input[pos + 1] as usize)
            | ((input[pos + 2] as usize) << 8)
            | ((input[pos + 3] as usize) << 16);
        pos += 4;

        if pos + chunk_body_len > input.len() {
            return Err(SnappyError::UnexpectedEof {
                context: "chunk body",
            });
        }

        let chunk_body = &input[pos..pos + chunk_body_len];
        pos += chunk_body_len;

        match chunk_type {
            CHUNK_TYPE_COMPRESSED => {
                if chunk_body.len() < 4 {
                    return Err(SnappyError::CorruptedData {
                        message: "compressed chunk too short for checksum".to_string(),
                    });
                }
                let expected_checksum = u32::from_le_bytes([
                    chunk_body[0],
                    chunk_body[1],
                    chunk_body[2],
                    chunk_body[3],
                ]);
                let compressed_payload = &chunk_body[4..];
                let decompressed =
                    decompress::decompress_block_with_dict(compressed_payload, dict)?;

                let computed_checksum = masked_crc32c(&decompressed);
                if expected_checksum != computed_checksum {
                    return Err(SnappyError::ChecksumMismatch {
                        expected: expected_checksum,
                        computed: computed_checksum,
                    });
                }
                output.extend_from_slice(&decompressed);
            }
            CHUNK_TYPE_UNCOMPRESSED => {
                if chunk_body.len() < 4 {
                    return Err(SnappyError::CorruptedData {
                        message: "uncompressed chunk too short for checksum".to_string(),
                    });
                }
                let expected_checksum = u32::from_le_bytes([
                    chunk_body[0],
                    chunk_body[1],
                    chunk_body[2],
                    chunk_body[3],
                ]);
                let raw_data = &chunk_body[4..];

                let computed_checksum = masked_crc32c(raw_data);
                if expected_checksum != computed_checksum {
                    return Err(SnappyError::ChecksumMismatch {
                        expected: expected_checksum,
                        computed: computed_checksum,
                    });
                }
                output.extend_from_slice(raw_data);
            }
            CHUNK_TYPE_STREAM_ID => {
                // Ignore any additional stream identifiers.
            }
            0x02..=0x7F => {
                return Err(SnappyError::InvalidChunkType { chunk_type });
            }
            _ => {
                // Other skippable chunks (including 0xFE if seen again) — skip.
            }
        }
    }

    Ok(output)
}

/// Write a chunk header to a plain `Vec<u8>` (infallible version used by
/// the non-streaming dict-frame helpers).
fn write_chunk_header_vec(output: &mut Vec<u8>, chunk_type: u8, data_len: usize) {
    output.push(chunk_type);
    output.push((data_len & 0xFF) as u8);
    output.push(((data_len >> 8) & 0xFF) as u8);
    output.push(((data_len >> 16) & 0xFF) as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_roundtrip_small() {
        let data = b"Hello, World! This is a test of Snappy framing.";

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // Verify stream identifier is present
        assert_eq!(&compressed[..10], &STREAM_IDENTIFIER);

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_roundtrip_empty() {
        let data = b"";

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_roundtrip_large() {
        // Data larger than one chunk (> 64 KiB)
        let mut data = Vec::with_capacity(100_000);
        for i in 0..100_000u32 {
            data.push((i % 256) as u8);
        }

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_roundtrip_repeated() {
        let data = vec![0xAB; 200_000];

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // Highly repeated data should compress well
        assert!(compressed.len() < data.len());

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_incremental_write() {
        let data = b"Hello, this is a test of incremental writing to the encoder.";

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            // Write in small increments
            for chunk in data.chunks(5) {
                encoder.write_all(chunk).expect("write should succeed");
            }
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_incremental_read() {
        let data = b"Test data for incremental reading from the decoder.";

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        let mut buf = [0u8; 7]; // Read in small chunks
        loop {
            let n = decoder.read(&mut buf).expect("read should succeed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, data);
    }

    #[test]
    fn test_frame_decoder_invalid_header() {
        let bad_data = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let mut decoder = FrameDecoder::new(&bad_data[..]);
        let mut output = Vec::new();
        let result = decoder.read_to_end(&mut output);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_decoder_empty_input() {
        let empty: &[u8] = &[];
        let mut decoder = FrameDecoder::new(empty);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("empty input should succeed");
        assert!(output.is_empty());
    }

    #[test]
    fn test_write_chunk_header() {
        let mut buf = Vec::new();
        write_chunk_header(&mut buf, 0x00, 0x123456).expect("should succeed");
        assert_eq!(buf, vec![0x00, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn test_stream_identifier_constant() {
        // Verify the stream identifier matches the spec
        assert_eq!(STREAM_IDENTIFIER[0], 0xFF); // chunk type
        assert_eq!(STREAM_IDENTIFIER[1], 0x06); // length low
        assert_eq!(STREAM_IDENTIFIER[2], 0x00); // length mid
        assert_eq!(STREAM_IDENTIFIER[3], 0x00); // length high
        assert_eq!(&STREAM_IDENTIFIER[4..], b"sNaPpY");
    }

    // -----------------------------------------------------------------------
    // Progress-callback tests
    // -----------------------------------------------------------------------

    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::progress::{ProgressHandle, ProgressSink};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// A `ProgressSink` that records call count and each `processed` value.
    struct CountingSink {
        calls: AtomicUsize,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ProgressSink for CountingSink {
        fn on_progress(&self, _processed: u64, _total: Option<u64>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A second counting sink that records all processed values for
    /// strict monotonicity verification.
    struct MonotonicSink {
        values: std::sync::Mutex<Vec<u64>>,
    }

    impl MonotonicSink {
        fn new() -> Self {
            Self {
                values: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn values(&self) -> Vec<u64> {
            self.values
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    impl ProgressSink for MonotonicSink {
        fn on_progress(&self, processed: u64, _total: Option<u64>) {
            let mut guard = self.values.lock().unwrap_or_else(|p| p.into_inner());
            guard.push(processed);
        }
    }

    /// Encode + decode a 128 KiB buffer and verify progress callbacks.
    ///
    /// A 128 KiB input produces exactly 2 chunks (each 64 KiB), so
    /// `on_progress` must be called ≥ 2 times for both encode and decode.
    /// Decode progress values must be strictly non-decreasing.
    #[test]
    fn test_progress_counting_sink() {
        // 128 KiB of pseudo-random-ish data (prevent too-aggressive compression
        // flattening chunk boundaries).
        let data: Vec<u8> = (0..131_072u64)
            .map(|i| (i.wrapping_mul(6_364_136_223_846_793_005_u64) >> 56) as u8)
            .collect();

        // --- Encode with progress ---
        let enc_sink = Arc::new(CountingSink::new());
        let enc_handle: ProgressHandle = enc_sink.clone();

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed).with_progress(enc_handle);
            encoder
                .write_all(&data)
                .expect("encode write should succeed");
            encoder.finish().expect("encode finish should succeed");
        }

        assert!(
            enc_sink.call_count() >= 2,
            "encoder on_progress called {} times, expected >= 2",
            enc_sink.call_count()
        );

        // --- Decode with progress (monotonicity check) ---
        let dec_sink = Arc::new(MonotonicSink::new());
        let dec_handle: ProgressHandle = dec_sink.clone();

        let mut decoder = FrameDecoder::new(&compressed[..]).with_progress(dec_handle);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("decode should succeed");

        assert_eq!(output, data, "decoded data must match original");

        let values = dec_sink.values();
        assert!(
            values.len() >= 2,
            "decoder on_progress called {} times, expected >= 2",
            values.len()
        );
        // Verify monotonically non-decreasing
        for window in values.windows(2) {
            assert!(
                window[1] >= window[0],
                "progress values not monotonic: {} followed by {}",
                window[0],
                window[1]
            );
        }
    }

    // -----------------------------------------------------------------------
    // Cancellation tests
    // -----------------------------------------------------------------------

    /// A pre-cancelled token must prevent encoding a multi-chunk buffer.
    #[test]
    fn test_cancellation_encoder_pre_cancelled() {
        // 128 KiB buffer → 2 chunks, so cancellation must trigger.
        let data: Vec<u8> = vec![0xBEu8; 131_072];

        let token = CancellationToken::new();
        token.cancel();

        let mut compressed = Vec::new();
        let mut encoder = FrameEncoder::new(&mut compressed).with_cancel(token);
        // write_all may succeed (it only buffers), but finish/flush must fail.
        let write_result = encoder.write_all(&data);
        let finish_result = encoder.finish();

        // At least one of write or finish must be Err.
        let triggered = write_result.is_err() || finish_result.is_err();
        assert!(
            triggered,
            "expected a cancellation error but neither write nor finish returned Err"
        );
    }

    /// A pre-cancelled token must prevent decoding.
    #[test]
    fn test_cancellation_decoder_pre_cancelled() {
        // First encode without cancellation.
        let data: Vec<u8> = vec![0xCAu8; 131_072];
        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder
                .write_all(&data)
                .expect("encode write should succeed");
            encoder.finish().expect("encode finish should succeed");
        }

        let token = CancellationToken::new();
        token.cancel();

        let mut decoder = FrameDecoder::new(&compressed[..]).with_cancel(token);
        let mut output = Vec::new();
        let result = decoder.read_to_end(&mut output);
        assert!(result.is_err(), "expected cancellation error from decoder");
    }

    // -----------------------------------------------------------------------
    // Edge-case tests: max-size blocks and boundary conditions
    // -----------------------------------------------------------------------

    /// Compress exactly MAX_UNCOMPRESSED_CHUNK_SIZE (65536) bytes using
    /// FrameEncoder, verify it decompresses correctly via FrameDecoder.
    #[test]
    fn test_frame_max_size_chunk() {
        let data: Vec<u8> = (0..MAX_UNCOMPRESSED_CHUNK_SIZE)
            .map(|i| (i % 251) as u8)
            .collect();
        assert_eq!(data.len(), MAX_UNCOMPRESSED_CHUNK_SIZE);

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");

        assert_eq!(output, data, "max-size chunk roundtrip failed");
    }

    /// Compress MAX_UNCOMPRESSED_CHUNK_SIZE + 1 (65537) bytes; verify that
    /// exactly two compressed data chunks are present in the output, and that
    /// the full roundtrip is correct.
    ///
    /// The framing spec splits input at exactly 65536-byte boundaries, so
    /// 65537 bytes → chunk of 65536 + chunk of 1.
    #[test]
    fn test_frame_just_over_max_chunk() {
        let size = MAX_UNCOMPRESSED_CHUNK_SIZE + 1;
        let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // Count data chunks (CHUNK_TYPE_COMPRESSED = 0x00 or CHUNK_TYPE_UNCOMPRESSED = 0x01).
        // Skip the 10-byte stream identifier first.
        let payload = &compressed[10..];
        let mut data_chunk_count = 0usize;
        let mut pos = 0usize;
        while pos + 4 <= payload.len() {
            let chunk_type = payload[pos];
            let chunk_len = (payload[pos + 1] as usize)
                | ((payload[pos + 2] as usize) << 8)
                | ((payload[pos + 3] as usize) << 16);
            if chunk_type == CHUNK_TYPE_COMPRESSED || chunk_type == CHUNK_TYPE_UNCOMPRESSED {
                data_chunk_count += 1;
            }
            pos += 4 + chunk_len;
        }

        assert_eq!(
            data_chunk_count, 2,
            "expected exactly 2 data chunks for 65537-byte input, got {data_chunk_count}"
        );

        // Full roundtrip
        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");
        assert_eq!(output, data, "just-over-max chunk roundtrip failed");
    }

    /// `max_compress_len(65536)` must be at least 65536 bytes (worst-case
    /// incompressible data must still fit in the output).
    /// `max_compress_len(0)` must be at least 1 (varint overhead for length).
    #[test]
    fn test_block_max_compress_len() {
        use crate::compress::max_compress_len;

        let max_len_65536 = max_compress_len(MAX_UNCOMPRESSED_CHUNK_SIZE);
        assert!(
            max_len_65536 >= MAX_UNCOMPRESSED_CHUNK_SIZE,
            "max_compress_len(65536) = {max_len_65536}, expected >= 65536"
        );

        let max_len_0 = max_compress_len(0);
        assert!(
            max_len_0 >= 1,
            "max_compress_len(0) = {max_len_0}, expected >= 1"
        );
    }

    /// Feed truncated compressed data to FrameDecoder::read_to_end; verify
    /// it returns an error and does not panic.
    #[test]
    fn test_decompress_truncated_frame() {
        // Encode valid data first.
        let data = vec![b'X'; 1000];
        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // Truncate to half the compressed length (but past the identifier).
        let truncated_len = compressed.len() / 2;
        let truncated = &compressed[..truncated_len];

        let mut decoder = FrameDecoder::new(truncated);
        let mut output = Vec::new();
        let result = decoder.read_to_end(&mut output);
        // Must return an error (UnexpectedEof or InvalidData), never panic.
        assert!(
            result.is_err(),
            "expected error on truncated input, but read_to_end succeeded"
        );
    }

    /// Flip one byte in the CRC field of the first compressed data chunk;
    /// verify that FrameDecoder returns a checksum error.
    #[test]
    fn test_decompress_corrupt_crc() {
        let data = vec![b'A'; 500];
        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // The stream layout: [10 bytes identifier][4 bytes chunk header][4 bytes CRC][…]
        // Flip the first byte of the CRC (byte index 14).
        let crc_offset = 14;
        assert!(
            crc_offset < compressed.len(),
            "compressed output is too short to contain a CRC field"
        );
        let mut corrupt = compressed.clone();
        corrupt[crc_offset] ^= 0xFF;

        let mut decoder = FrameDecoder::new(&corrupt[..]);
        let mut output = Vec::new();
        let result = decoder.read_to_end(&mut output);
        assert!(
            result.is_err(),
            "expected checksum error on corrupt CRC, but read_to_end succeeded"
        );
    }

    /// 65536 bytes of all zeros should compress to a much smaller output;
    /// roundtrip must be correct.
    #[test]
    fn test_compress_all_zeros() {
        let data = vec![0u8; MAX_UNCOMPRESSED_CHUNK_SIZE];

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // All-zero data should compress very well.
        assert!(
            compressed.len() < data.len() / 4,
            "expected compressed output much smaller than {}, got {}",
            data.len(),
            compressed.len()
        );

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");
        assert_eq!(output, data, "all-zeros roundtrip failed");
    }

    /// 65536 bytes of 0xFF; compression must be correct (roundtrip works),
    /// even if output is not smaller.
    #[test]
    fn test_compress_all_ones() {
        let data = vec![0xFFu8; MAX_UNCOMPRESSED_CHUNK_SIZE];

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read should succeed");
        assert_eq!(output, data, "all-0xFF roundtrip failed");
    }

    /// Feed a max-size compressed chunk byte-by-byte through a
    /// `std::io::BufReader`-backed FrameDecoder; the output must appear
    /// correctly (tests the incremental chunk-reading code path).
    #[test]
    fn test_frame_decoder_incremental_max_size() {
        use std::io::BufReader;

        let data: Vec<u8> = (0..MAX_UNCOMPRESSED_CHUNK_SIZE)
            .map(|i| (i % 199) as u8)
            .collect();

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder.write_all(&data).expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        // Wrap the compressed slice in a BufReader with a tiny buffer (1 byte)
        // so that the decoder must make many small reads to reassemble each chunk.
        let buf_reader = BufReader::with_capacity(1, &compressed[..]);
        let mut decoder = FrameDecoder::new(buf_reader);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("incremental read should succeed");

        assert_eq!(
            output, data,
            "incremental max-size chunk decoder roundtrip failed"
        );
    }
}
