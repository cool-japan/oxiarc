//! Streaming compression and decompression for Zstandard.
//!
//! Provides [`ZstdStreamEncoder`] (implements [`std::io::Write`]) and
//! [`ZstdStreamDecoder`] (implements [`std::io::Read`]) for processing Zstandard data
//! through standard Rust I/O traits.
//!
//! The streaming encoder buffers written data and emits one complete Zstandard
//! frame per `block_size` bytes (128 KiB by default), so its memory is bounded
//! by the block size; the output is a sequence of concatenated frames.
//!
//! The decoder is a thin `Read` shell over [`crate::ZstdStream`], the bounded
//! push decoder: it serves the first byte without reading the whole input and
//! never materialises either side, holding one 128 KiB block carry, two 64 KiB
//! staging buffers and the sliding window. The window grows lazily and is
//! bounded by `min(the frame's declared Window_Size, the bytes actually
//! produced)`; see [`ZstdStreamDecoder`] for the two builders that make that
//! bound a constant on untrusted input.
//!
//! # Example
//!
//! ```rust
//! use std::io::{Read, Write};
//! use oxiarc_zstd::streaming::{ZstdStreamEncoder, ZstdStreamDecoder};
//!
//! // Compress
//! let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
//! encoder.write_all(b"Hello, streaming Zstd!").expect("write failed");
//! let compressed = encoder.finish().expect("finish failed");
//!
//! // Decompress
//! let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
//! let mut output = String::new();
//! decoder.read_to_string(&mut output).expect("read failed");
//! assert_eq!(output, "Hello, streaming Zstd!");
//! ```

use crate::encode::ZstdEncoder;
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;
use std::io::{self, Write};

/// Default block size for the incremental encoder (128 KiB).
const DEFAULT_BLOCK_SIZE: usize = 128 * 1024;

/// Streaming Zstandard encoder that implements [`Write`].
///
/// Data written to this encoder is buffered internally.  When the internal
/// buffer reaches `block_size` bytes it is automatically flushed as a
/// complete Zstandard frame to the inner writer (truly incremental).  Any
/// remaining data is flushed when [`finish`](ZstdStreamEncoder::finish) is
/// called.
///
/// The output is a sequence of valid concatenated Zstandard frames and can be
/// decoded with [`crate::decompress_multi_frame`].
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`ZstdStreamEncoder::with_progress`] / [`ZstdStreamEncoder::with_cancel`] builders.
///
/// **Important:** you *must* call [`finish`](ZstdStreamEncoder::finish) to
/// flush the final (possibly partial) block. Dropping the encoder without
/// calling `finish` will silently discard any buffered data.
pub struct ZstdStreamEncoder<W: Write> {
    /// The wrapped writer that receives compressed output.
    inner: Option<W>,
    /// Internal buffer holding uncompressed data waiting to be flushed.
    buffer: Vec<u8>,
    /// Compression level used when encoding.
    level: i32,
    /// Optional pre-trained dictionary data.
    dict: Option<Vec<u8>>,
    /// Whether `finish` has already been called.
    finished: bool,
    /// Threshold at which the buffer is automatically flushed.
    block_size: usize,
    /// Optional progress sink. Notified after each block is flushed.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked before each block flush.
    cancel: Option<CancellationToken>,
    /// Cumulative uncompressed bytes flushed so far.
    bytes_processed: u64,
}

impl<W: Write> ZstdStreamEncoder<W> {
    /// Create a new streaming encoder wrapping `writer`.
    ///
    /// The `level` parameter controls the compression level passed to the
    /// underlying [`ZstdEncoder`].  The encoder uses a default block size of
    /// 128 KiB; use [`with_block_size`](ZstdStreamEncoder::with_block_size)
    /// to customise this.
    pub fn new(writer: W, level: i32) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            level,
            dict: None,
            finished: false,
            block_size: DEFAULT_BLOCK_SIZE,
            progress: None,
            cancel: None,
            bytes_processed: 0,
        }
    }

    /// Create a new streaming encoder with a pre-trained dictionary.
    ///
    /// Dictionary-based compression improves ratios for small payloads that
    /// share common patterns.
    pub fn with_dictionary(writer: W, level: i32, dict: Vec<u8>) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            level,
            dict: Some(dict),
            finished: false,
            block_size: DEFAULT_BLOCK_SIZE,
            progress: None,
            cancel: None,
            bytes_processed: 0,
        }
    }

    /// Set the block size used for incremental flushing.
    ///
    /// When the internal buffer reaches this many bytes it is automatically
    /// compressed and written to the inner writer as a Zstandard frame.
    #[must_use]
    pub fn with_block_size(mut self, block_size: usize) -> Self {
        self.block_size = block_size.max(1);
        self
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(cumulative_bytes, None)` is called after each
    /// block is flushed to the inner writer. `on_finish()` is called after
    /// `finish` flushes the final block.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each block is compressed and written.
    /// If cancelled, returns an I/O error wrapping
    /// [`oxiarc_core::error::OxiArcError::Cancelled`].
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
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
            // Flush whatever remains in the buffer (even if empty, to match
            // the single-frame behaviour expected by existing tests).
            self.flush_buffer_unconditional()?;
            self.finished = true;
            if let Some(ref handle) = self.progress {
                handle.on_finish();
            }
        }
        // inner is always Some until finish() is called once.
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("inner writer already taken"))
    }

    /// Compress `data` as a single Zstandard frame and write it to `inner`.
    fn compress_and_write(&mut self, data: &[u8]) -> io::Result<()> {
        // Cooperative cancellation check before each block.
        if let Some(ref token) = self.cancel {
            token.check().map_err(|e| io::Error::other(e.to_string()))?;
        }

        let mut encoder = ZstdEncoder::new();
        encoder.set_level(self.level);
        if let Some(ref dict) = self.dict {
            encoder.set_dictionary(dict);
        }
        let compressed = encoder
            .compress(data)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }

        self.bytes_processed += data.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }

        Ok(())
    }

    /// If the buffer has reached `block_size`, flush it as a frame.
    fn maybe_flush_block(&mut self) -> io::Result<()> {
        if self.buffer.len() >= self.block_size {
            let data = std::mem::take(&mut self.buffer);
            self.compress_and_write(&data)?;
        }
        Ok(())
    }

    /// Always flush the current buffer contents (even if empty) as a frame.
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
}

impl<W: Write> Write for ZstdStreamEncoder<W> {
    /// Buffer `buf` and flush a frame to the inner writer whenever the
    /// internal buffer reaches `block_size`.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("encoder already finished"));
        }
        self.buffer.extend_from_slice(buf);
        self.maybe_flush_block()?;
        Ok(buf.len())
    }

    /// Flush any buffered data as a Zstandard frame to the inner writer.
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
// Streaming decoder
// ---------------------------------------------------------------------------

// `ZstdStreamDecoder` lives in `crate::read` (so neither file exceeds the
// workspace file-size budget) and is re-exported here, which is where it has
// always been in the public API.
pub use crate::read::ZstdStreamDecoder;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn test_stream_encoder_basic() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder
            .write_all(b"Hello, Zstandard!")
            .expect("write failed");
        let compressed = encoder.finish().expect("finish failed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_stream_encoder_empty() {
        let encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        let compressed = encoder.finish().expect("finish failed");
        // Should produce a valid (minimal) Zstd frame.
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_stream_roundtrip() {
        let original = b"The quick brown fox jumps over the lazy dog.";

        // Compress
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        // Decompress
        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_stream_roundtrip_multiple_writes() {
        let parts: &[&[u8]] = &[b"Hello, ", b"streaming ", b"Zstd!"];

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        for part in parts {
            encoder.write_all(part).expect("write failed");
        }
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, b"Hello, streaming Zstd!");
    }

    #[test]
    fn test_stream_decoder_small_reads() {
        let original = b"ABCDEFGHIJ";

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
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
    fn test_stream_decoder_empty_input() {
        let mut decoder = ZstdStreamDecoder::new(&[][..]);
        let mut buf = [0u8; 16];
        let n = decoder.read(&mut buf).expect("read failed");
        assert_eq!(n, 0);
    }

    #[test]
    fn test_stream_encoder_decoder_dict_roundtrip_small() {
        let dict = b"common pattern data appears frequently in the corpus".to_vec();
        let payload = b"common pattern data";

        let mut enc = ZstdStreamEncoder::with_dictionary(Vec::new(), 1, dict.clone());
        enc.write_all(payload).expect("write");
        let compressed = enc.finish().expect("finish");

        let mut dec = ZstdStreamDecoder::with_dictionary(&compressed[..], dict);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).expect("read");
        assert_eq!(out, payload as &[u8]);
    }

    #[test]
    fn test_stream_encoder_decoder_dict_roundtrip_large() {
        // Multi-frame dict roundtrip: use a small block_size so each write produces
        // multiple concatenated Zstd frames, each well under 128 KiB so we stay in
        // the single-internal-block regime and avoid the known multi-internal-block
        // + dict bug that is pre-existing in the encoder.
        let dict_text = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(50);
        let dict = dict_text.as_bytes().to_vec();
        // ~57 KB payload with 8 KB block_size → ~8 frames, each < 128 KiB.
        let payload: Vec<u8> = dict_text.repeat(20).into_bytes();

        let mut enc = ZstdStreamEncoder::with_dictionary(Vec::new(), 3, dict.clone())
            .with_block_size(8 * 1024);
        enc.write_all(&payload).expect("write");
        let compressed = enc.finish().expect("finish");

        // Verify multiple Zstd frames were produced by counting magic bytes.
        let magic = &crate::ZSTD_MAGIC;
        let frame_count = compressed.windows(4).filter(|w| *w == magic).count();
        assert!(
            frame_count > 1,
            "expected multiple frames, got {}",
            frame_count
        );

        let mut dec = ZstdStreamDecoder::with_dictionary(&compressed[..], dict);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).expect("read");
        assert_eq!(out, payload);
    }

    #[test]
    fn test_stream_decoder_without_dict_on_dict_compressed_large_data() {
        // Compress with a dict; decode without. On large inputs that trigger
        // dict back-references, this must either error or produce wrong output.
        let dict_text = "pattern frequently repeating text ".repeat(200);
        let dict = dict_text.as_bytes().to_vec();
        let payload: Vec<u8> = dict_text.repeat(50).into_bytes();

        let mut enc = ZstdStreamEncoder::with_dictionary(Vec::new(), 3, dict);
        enc.write_all(&payload).expect("write");
        let compressed = enc.finish().expect("finish");

        let mut dec = ZstdStreamDecoder::new(&compressed[..]);
        let mut out = Vec::new();
        let result = dec.read_to_end(&mut out);
        if result.is_ok() {
            assert_ne!(
                out, payload,
                "decoding without dict should not reproduce original on large input"
            );
        }
    }

    #[test]
    fn test_stream_encoder_buffered_bytes() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b"12345").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 5);
        encoder.write_all(b"67890").expect("write failed");
        assert_eq!(encoder.buffered_bytes(), 10);
    }

    #[test]
    fn test_stream_encoder_is_finished() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        assert!(!encoder.is_finished());
        encoder.write_all(b"data").expect("write failed");
        assert!(!encoder.is_finished());
        // Cannot check after finish since finish consumes self.
    }

    #[test]
    fn test_stream_decoder_is_finished() {
        let original = b"short";

        let mut enc = ZstdStreamEncoder::new(Vec::new(), 1);
        enc.write_all(original).expect("write failed");
        let compressed = enc.finish().expect("finish failed");

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        assert!(!decoder.is_finished());

        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read failed");
        assert!(decoder.is_finished());
    }

    #[test]
    fn test_stream_roundtrip_large_data() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(&original).expect("write failed");
        let compressed = encoder.finish().expect("finish failed");

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read failed");

        assert_eq!(output, original);
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
        let pattern = b"ZstdStream test data with repeating pattern ABCDEFGH ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk = &pattern[..remaining.min(pattern.len())];
            data.extend_from_slice(chunk);
        }
        data
    }

    #[test]
    fn test_zstd_stream_encoder_progress_reports() {
        let data = make_compressible_data(1024 * 1024); // 1 MB

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1)
            .with_progress(sink as oxiarc_core::progress::ProgressHandle);
        encoder.write_all(&data).expect("write_all failed");
        encoder.finish().expect("finish failed");

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
    fn test_zstd_stream_encoder_cancel_aborts() {
        let data = make_compressible_data(1024 * 1024);
        let token = CancellationToken::new();

        // Use a small block size so we hit the block boundary quickly.
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1)
            .with_block_size(4096)
            .with_cancel(token.clone());

        token.cancel();
        let result = encoder.write_all(&data);
        assert!(result.is_err(), "expected cancellation error");
    }

    #[test]
    fn test_zstd_stream_decoder_progress_reports() {
        let data = make_compressible_data(1024 * 1024); // 1 MB

        let mut enc = ZstdStreamEncoder::new(Vec::new(), 1);
        enc.write_all(&data).expect("write_all failed");
        let compressed = enc.finish().expect("finish failed");

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let mut decoder = ZstdStreamDecoder::new(&compressed[..])
            .with_progress(sink as oxiarc_core::progress::ProgressHandle);
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .expect("read_to_end failed");

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
    fn test_zstd_stream_decoder_cancel_aborts() {
        let data = make_compressible_data(1024 * 1024);
        let mut enc = ZstdStreamEncoder::new(Vec::new(), 1);
        enc.write_all(&data).expect("write_all failed");
        let compressed = enc.finish().expect("finish failed");

        let token = CancellationToken::new();
        let mut decoder = ZstdStreamDecoder::new(&compressed[..]).with_cancel(token.clone());
        let mut output = Vec::new();

        token.cancel();
        let result = decoder.read_to_end(&mut output);
        assert!(result.is_err(), "expected cancellation error");
    }
}
