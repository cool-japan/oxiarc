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

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::compress;
use crate::crc32c::masked_crc32c;
use crate::decompress;
use crate::error::SnappyError;

/// Stream identifier magic bytes: "sNaPpY" (0xff 0x06 0x00 0x00 0x73 0x4e 0x61 0x50 0x70 0x59)
const STREAM_IDENTIFIER: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

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
        }
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
        let data = std::mem::take(&mut self.buffer);
        self.write_chunk(&data)?;

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
        }
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

    /// Read and decompress a compressed data chunk.
    fn read_compressed_chunk(&mut self, chunk_len: usize) -> io::Result<()> {
        if chunk_len < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed chunk too short for checksum",
            ));
        }

        let mut chunk_data = vec![0u8; chunk_len];
        self.inner.read_exact(&mut chunk_data)?;

        // First 4 bytes are the masked CRC32C
        let expected_checksum =
            u32::from_le_bytes([chunk_data[0], chunk_data[1], chunk_data[2], chunk_data[3]]);

        let compressed_data = &chunk_data[4..];
        let decompressed = decompress::decompress(compressed_data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

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

        let mut chunk_data = vec![0u8; chunk_len];
        self.inner.read_exact(&mut chunk_data)?;

        let expected_checksum =
            u32::from_le_bytes([chunk_data[0], chunk_data[1], chunk_data[2], chunk_data[3]]);

        let data = &chunk_data[4..];

        let computed_checksum = masked_crc32c(data);
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

        self.output_buffer = data.to_vec();
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
}
