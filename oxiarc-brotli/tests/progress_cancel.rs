//! Integration tests for progress callbacks and cancellation in oxiarc-brotli.
//!
//! Tests the `with_progress` and `with_cancel` builders on `BrotliCompressor`
//! and `BrotliDecompressor`.

use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use oxiarc_brotli::{
    compress::{BrotliParams, compress_with_params},
    streaming::{BrotliCompressor, BrotliDecompressor},
};
use oxiarc_core::{
    CancellationToken, ProgressHandle,
    progress::{ProgressSink, noop_progress},
};

// ─── CountingSink ────────────────────────────────────────────────────────────

/// A `ProgressSink` that counts `on_progress` calls and records the values.
struct CountingSink {
    calls: AtomicU64,
    /// Last `processed` value reported (monotonicity check).
    last_processed: AtomicU64,
    /// Whether any non-monotonic value was observed.
    monotonicity_violated: std::sync::atomic::AtomicBool,
}

impl CountingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
            monotonicity_violated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    fn was_monotonic(&self) -> bool {
        !self.monotonicity_violated.load(Ordering::SeqCst)
    }
}

impl ProgressSink for CountingSink {
    fn on_progress(&self, processed: u64, _total: Option<u64>) {
        let prev = self.last_processed.swap(processed, Ordering::SeqCst);
        if processed < prev {
            self.monotonicity_violated.store(true, Ordering::SeqCst);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Helper: compress then decompress `data` with counting sinks on both sides.
/// Returns `(encode_calls, decode_calls, encode_monotonic, decode_monotonic)`.
fn roundtrip_with_counting(data: &[u8]) -> (u64, u64, bool, bool) {
    let params = BrotliParams {
        quality: 1,
        ..BrotliParams::default()
    };

    // --- Encode ---------------------------------------------------------
    let encode_sink = CountingSink::new();
    let encode_handle: ProgressHandle = encode_sink.clone();

    let mut compressed = Vec::new();
    {
        let mut compressor =
            BrotliCompressor::new(&mut compressed, params.clone()).with_progress(encode_handle);
        compressor.write_all(data).expect("compressor write_all");
        compressor.finish().expect("compressor finish");
    }

    // --- Decode ---------------------------------------------------------
    let decode_sink = CountingSink::new();
    let decode_handle: ProgressHandle = decode_sink.clone();

    let mut decompressor =
        BrotliDecompressor::new(compressed.as_slice()).with_progress(decode_handle);
    let mut decompressed = Vec::new();
    decompressor
        .read_to_end(&mut decompressed)
        .expect("decompressor read_to_end");

    assert_eq!(decompressed, data, "round-trip data mismatch");

    (
        encode_sink.call_count(),
        decode_sink.call_count(),
        encode_sink.was_monotonic(),
        decode_sink.was_monotonic(),
    )
}

#[test]
fn test_progress_counting_sink_on_compress_and_decompress() {
    // 64 KiB of patterned data (highly compressible).
    let data: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();

    let (encode_calls, decode_calls, encode_mono, decode_mono) = roundtrip_with_counting(&data);

    assert!(
        encode_calls >= 1,
        "on_progress should be called at least once during encode, got {encode_calls}"
    );
    assert!(
        decode_calls >= 1,
        "on_progress should be called at least once during decode, got {decode_calls}"
    );
    assert!(
        encode_mono,
        "encode processed values must be non-decreasing"
    );
    assert!(
        decode_mono,
        "decode processed values must be non-decreasing"
    );
}

#[test]
fn test_progress_counting_sink_multiple_write_calls() {
    // Write in smaller chunks to test accumulation.
    let params = BrotliParams {
        quality: 0,
        ..BrotliParams::default()
    };

    let encode_sink = CountingSink::new();
    let encode_handle: ProgressHandle = encode_sink.clone();

    let chunk = b"Hello, Brotli progress callback! ";
    let mut compressed = Vec::new();
    {
        let mut compressor =
            BrotliCompressor::new(&mut compressed, params).with_progress(encode_handle);
        // Write many small chunks.
        for _ in 0..2000 {
            compressor.write_all(chunk).expect("write chunk");
        }
        compressor.finish().expect("finish");
    }

    assert!(
        encode_sink.call_count() >= 1,
        "expected at least one on_progress call, got {}",
        encode_sink.call_count()
    );
}

#[test]
fn test_cancellation_before_decode_returns_cancelled_error() {
    use oxiarc_brotli::compress::compress;

    // Produce a valid compressed payload.
    let data: Vec<u8> = b"The quick brown fox jumps over the lazy dog.".repeat(100);
    let compressed = compress(&data, 1).expect("compress");

    // Cancel the token BEFORE decoding starts.
    let token = CancellationToken::new();
    token.cancel();

    let mut decompressor = BrotliDecompressor::new(compressed.as_slice()).with_cancel(token);

    let mut output = Vec::new();
    let result = decompressor.read_to_end(&mut output);

    // The Read impl surfaces io::ErrorKind::Other when cancelled.
    assert!(
        result.is_err(),
        "expected an error when token is cancelled before decode"
    );
    let err = result.expect_err("should be error");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Other,
        "expected Other kind, got {:?}",
        err.kind()
    );
}

#[test]
fn test_cancellation_before_encode_returns_cancelled_error() {
    let params = BrotliParams {
        quality: 0,
        ..BrotliParams::default()
    };

    let token = CancellationToken::new();
    token.cancel();

    let mut output = Vec::new();
    let mut compressor = BrotliCompressor::new(&mut output, params).with_cancel(token);
    compressor.write_all(b"some data to compress").ok();
    let result = compressor.finish();

    assert!(
        result.is_err(),
        "expected an error when token is cancelled before encode"
    );
    let err = result.expect_err("should be error");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Other,
        "expected Other kind, got {:?}",
        err.kind()
    );
}

#[test]
fn test_cancellation_token_not_fired_allows_normal_completion() {
    use oxiarc_brotli::compress::compress;

    // Use a data pattern known to round-trip correctly with the brotli implementation.
    let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let compressed = compress(&data, 1).expect("compress");

    let token = CancellationToken::new();
    // Token is NOT cancelled.

    let mut decompressor = BrotliDecompressor::new(compressed.as_slice()).with_cancel(token);

    let mut output = Vec::new();
    decompressor
        .read_to_end(&mut output)
        .expect("should complete without error when token not cancelled");

    assert_eq!(output, data.as_slice());
}

#[test]
fn test_noop_progress_no_panic() {
    // Verify that attaching a noop progress handle doesn't cause any issues.
    let data: Vec<u8> = vec![0xAB; 1024];
    let params = BrotliParams {
        quality: 0,
        ..BrotliParams::default()
    };

    let mut compressed = Vec::new();
    {
        let mut compressor =
            BrotliCompressor::new(&mut compressed, params).with_progress(noop_progress());
        compressor.write_all(&data).expect("write");
        compressor.finish().expect("finish");
    }

    let mut decompressor =
        BrotliDecompressor::new(compressed.as_slice()).with_progress(noop_progress());
    let mut output = Vec::new();
    decompressor.read_to_end(&mut output).expect("read");
    assert_eq!(output, data);
}

// ─── Per-meta-block granularity tests ────────────────────────────────────────

/// Quality 4 has a 256 KB block size; 512 KB input must produce ≥ 2 blocks,
/// so the progress sink must be called at least twice.
#[test]
fn test_progress_fires_multiple_times_for_large_input_encode() {
    // 512 KB of patterned data — forces ≥ 2 meta-blocks at quality 4.
    let data: Vec<u8> = (0..524288u32).map(|i| (i % 251) as u8).collect();

    let params = BrotliParams {
        quality: 4,
        ..BrotliParams::default()
    };

    let encode_sink = CountingSink::new();
    let encode_handle: ProgressHandle = encode_sink.clone();

    let mut compressed = Vec::new();
    {
        let mut compressor =
            BrotliCompressor::new(&mut compressed, params).with_progress(encode_handle);
        compressor.write_all(&data).expect("write_all");
        compressor.finish().expect("finish");
    }

    assert!(
        encode_sink.call_count() >= 2,
        "expected ≥ 2 on_progress calls for 512 KB at quality 4 (block_size=256 KB), \
         got {}",
        encode_sink.call_count()
    );
    assert!(
        encode_sink.was_monotonic(),
        "encode progress values must be non-decreasing"
    );
}

/// Progress should fire once per meta-block on the decode side as well.
/// Quality 0 produces uncompressed meta-blocks of 256 KB each; 512 KB of
/// input therefore produces exactly 2 meta-blocks that can be decoded without
/// hitting the known multi-block limitation in the compressed-block path.
#[test]
fn test_progress_fires_multiple_times_for_large_input_decode() {
    // 512 KB of patterned data with quality 0 → 2 uncompressed meta-blocks.
    let data: Vec<u8> = (0..524288u32).map(|i| (i % 251) as u8).collect();
    let params = BrotliParams {
        quality: 0,
        ..BrotliParams::default()
    };
    let compressed = compress_with_params(&data, &params).expect("compress");

    let decode_sink = CountingSink::new();
    let decode_handle: ProgressHandle = decode_sink.clone();

    let mut decompressor =
        BrotliDecompressor::new(compressed.as_slice()).with_progress(decode_handle);
    let mut decompressed = Vec::new();
    decompressor
        .read_to_end(&mut decompressed)
        .expect("decompress");

    assert_eq!(decompressed, data, "round-trip data mismatch");
    assert!(
        decode_sink.call_count() >= 2,
        "expected ≥ 2 on_progress calls when decoding ≥ 2 meta-blocks, got {}",
        decode_sink.call_count()
    );
    assert!(
        decode_sink.was_monotonic(),
        "decode progress values must be non-decreasing"
    );
}

// ─── Direct BrotliError::Cancelled assertion ─────────────────────────────────

/// Tests that cancellation propagates as `BrotliError::Cancelled` when calling
/// the raw `decompress_with_hooks` equivalent via the streaming path.
///
/// We verify this by using `BrotliDecompressor` (which routes through
/// `decompress_with_hooks`) and checking that the surfaced `io::Error`
/// message contains "operation cancelled", confirming the `Cancelled`
/// variant was triggered.
#[test]
fn test_cancelled_error_message_identifies_cancellation() {
    use oxiarc_brotli::compress::compress;

    let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let compressed = compress(&data, 1).expect("compress");

    let token = CancellationToken::new();
    token.cancel();

    let mut decompressor = BrotliDecompressor::new(compressed.as_slice()).with_cancel(token);
    let mut output = Vec::new();
    let err = decompressor
        .read_to_end(&mut output)
        .expect_err("should fail when cancelled");

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Other,
        "expected Other kind, got {:?}",
        err.kind()
    );
    assert!(
        err.to_string().contains("cancelled"),
        "error message should contain 'cancelled', got: {err}"
    );
}

/// Tests that cancellation propagates as `BrotliError::Cancelled` on encode
/// and that the message confirms it is a cancellation.
#[test]
fn test_cancelled_encode_error_message_identifies_cancellation() {
    let params = BrotliParams {
        quality: 4,
        ..BrotliParams::default()
    };

    let token = CancellationToken::new();
    token.cancel();

    let mut output = Vec::new();
    let mut compressor = BrotliCompressor::new(&mut output, params).with_cancel(token);
    compressor.write_all(b"data to compress").ok();
    let err = compressor.finish().expect_err("should fail when cancelled");

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Other,
        "expected Other kind, got {:?}",
        err.kind()
    );
    assert!(
        err.to_string().contains("cancelled"),
        "error message should contain 'cancelled', got: {err}"
    );
}
