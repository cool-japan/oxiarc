//! Integration tests for async Brotli I/O support.

#![cfg(feature = "async-io")]

use oxiarc_brotli::compress::BrotliParams;
use oxiarc_brotli::{
    BrotliAsyncCompressor, BrotliAsyncDecompressor, compress, compress_with_params, decompress,
};
use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
use std::io::Cursor;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn async_roundtrip(data: &[u8], quality: u32) -> (Vec<u8>, Vec<u8>) {
    let mut enc = BrotliAsyncCompressor::new(quality);
    let mut input = Cursor::new(data.to_vec());
    let mut compressed = Vec::new();
    enc.compress_async(&mut input, &mut compressed)
        .await
        .expect("compress_async failed");

    let mut dec = BrotliAsyncDecompressor::new();
    let mut comp_cursor = Cursor::new(compressed.clone());
    let mut decompressed = Vec::new();
    dec.decompress_async(&mut comp_cursor, &mut decompressed)
        .await
        .expect("decompress_async failed");

    (compressed, decompressed)
}

// ---------------------------------------------------------------------------
// Roundtrip tests at different quality levels
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_async_roundtrip_quality_1() {
    // 200 KiB — fits in one brotli meta-block at quality 1 (block_size = 256 KiB).
    // Using a pattern with enough variety to exercise the LZ77 fast path.
    let original: Vec<u8> = (0..200 * 1024).map(|i| (i % 64) as u8).collect();
    let (_compressed, decompressed) = async_roundtrip(&original, 1).await;
    assert_eq!(decompressed, original, "quality-1 round-trip mismatch");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_async_roundtrip_quality_5() {
    let original: Vec<u8> = (0..512 * 1024).map(|i| (i % 128) as u8).collect(); // 512 KiB
    let (_compressed, decompressed) = async_roundtrip(&original, 5).await;
    assert_eq!(decompressed, original, "quality-5 round-trip mismatch");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_async_roundtrip_quality_11() {
    let original = b"The quick brown fox jumps over the lazy dog. ".repeat(200);
    let (_compressed, decompressed) = async_roundtrip(&original, 11).await;
    assert_eq!(
        decompressed,
        original.as_slice(),
        "quality-11 round-trip mismatch"
    );
}

// ---------------------------------------------------------------------------
// Cross-path interop tests
// ---------------------------------------------------------------------------

/// Synchronous encode → async decode
#[tokio::test(flavor = "multi_thread")]
async fn test_async_decode_serial_output() {
    let original = b"Hello, serial Brotli encoding, async decoding!".repeat(50);

    // Synchronous compress
    let compressed = compress(&original, 4).expect("sync compress failed");

    // Asynchronous decompress
    let mut dec = BrotliAsyncDecompressor::new();
    let mut comp_cursor = Cursor::new(compressed);
    let mut decompressed = Vec::new();
    let n = dec
        .decompress_async(&mut comp_cursor, &mut decompressed)
        .await
        .expect("async decompress failed");

    assert_eq!(n, decompressed.len());
    assert_eq!(decompressed, original.as_slice());
}

/// Async encode → synchronous decode
#[tokio::test(flavor = "multi_thread")]
async fn test_async_encode_serial_decode() {
    let original: Vec<u8> = (0..200_000).map(|i| ((i * 7) % 256) as u8).collect();

    // Asynchronous compress
    let mut enc = BrotliAsyncCompressor::new(3);
    let mut input = Cursor::new(original.clone());
    let mut compressed = Vec::new();
    let n = enc
        .compress_async(&mut input, &mut compressed)
        .await
        .expect("async compress failed");

    assert_eq!(n, compressed.len());

    // Synchronous decompress
    let decompressed = decompress(&compressed).expect("sync decompress failed");
    assert_eq!(decompressed, original);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_async_empty() {
    let original: &[u8] = b"";

    let mut enc = BrotliAsyncCompressor::new(6);
    let mut input = Cursor::new(original.to_vec());
    let mut compressed = Vec::new();
    enc.compress_async(&mut input, &mut compressed)
        .await
        .expect("async compress empty failed");

    // Compressed form of an empty Brotli stream is non-empty (it has headers).
    assert!(
        !compressed.is_empty(),
        "empty brotli stream must have bytes"
    );

    let mut dec = BrotliAsyncDecompressor::new();
    let mut comp_cursor = Cursor::new(compressed);
    let mut decompressed = Vec::new();
    dec.decompress_async(&mut comp_cursor, &mut decompressed)
        .await
        .expect("async decompress empty failed");

    assert!(decompressed.is_empty(), "decompressed empty must be empty");
}

/// Verify that custom buffer sizes don't affect correctness.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_roundtrip_small_buffer() {
    let original: Vec<u8> = (0..16_384).map(|i| (i % 251) as u8).collect();

    let mut enc = BrotliAsyncCompressor::new(4);
    let mut input = Cursor::new(original.clone());
    let mut compressed = Vec::new();
    enc.compress_async_with_buffer(&mut input, &mut compressed, 512)
        .await
        .expect("compress small buffer failed");

    let mut dec = BrotliAsyncDecompressor::new();
    let mut comp_cursor = Cursor::new(compressed);
    let mut decompressed = Vec::new();
    dec.decompress_async_with_buffer(&mut comp_cursor, &mut decompressed, 512)
        .await
        .expect("decompress small buffer failed");

    assert_eq!(decompressed, original);
}

/// Test using `with_params` constructor on the compressor.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_with_params_constructor() {
    let original = b"Testing with_params constructor.".repeat(100);
    let params = BrotliParams {
        quality: 2,
        lgwin: 18,
        lgblock: 0,
    };

    let mut enc = BrotliAsyncCompressor::with_params(params);
    let mut input = Cursor::new(original.clone());
    let mut compressed = Vec::new();
    enc.compress_async(&mut input, &mut compressed)
        .await
        .expect("compress with_params failed");

    let decompressed = decompress(&compressed).expect("sync decompress failed");
    assert_eq!(decompressed, original.as_slice());
}

/// Async encode followed by async decode — bytes-written return values are correct.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_return_byte_counts() {
    let original: Vec<u8> = (0..4096).map(|i| (i % 17) as u8).collect();

    let mut enc = BrotliAsyncCompressor::new(6);
    let mut input = Cursor::new(original.clone());
    let mut compressed = Vec::new();
    let compressed_n = enc
        .compress_async(&mut input, &mut compressed)
        .await
        .expect("compress failed");
    assert_eq!(
        compressed_n,
        compressed.len(),
        "return n must equal vec len"
    );

    let mut dec = BrotliAsyncDecompressor::new();
    let mut comp_cursor = Cursor::new(compressed);
    let mut decompressed = Vec::new();
    let decompressed_n = dec
        .decompress_async(&mut comp_cursor, &mut decompressed)
        .await
        .expect("decompress failed");
    assert_eq!(
        decompressed_n,
        decompressed.len(),
        "return n must equal vec len"
    );
    assert_eq!(decompressed, original);
}

/// Multiple sequential compressions with the same encoder instance.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_sequential_compressions() {
    let data_a: Vec<u8> = b"AAAA".repeat(1000);
    let data_b: Vec<u8> = b"BBBB".repeat(1000);

    let mut enc = BrotliAsyncCompressor::new(1);

    let mut comp_a = Vec::new();
    enc.compress_async(&mut Cursor::new(data_a.clone()), &mut comp_a)
        .await
        .expect("first compress failed");

    let mut comp_b = Vec::new();
    enc.compress_async(&mut Cursor::new(data_b.clone()), &mut comp_b)
        .await
        .expect("second compress failed");

    // Both should decompress correctly.
    let dec_a = decompress(&comp_a).expect("decompress a");
    let dec_b = decompress(&comp_b).expect("decompress b");
    assert_eq!(dec_a, data_a);
    assert_eq!(dec_b, data_b);
}

/// Verify sync and async compress_with_params produce identical output.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_matches_sync_output() {
    let original: Vec<u8> = (0..8192).map(|i| (i % 97) as u8).collect();
    let params = BrotliParams {
        quality: 6,
        lgwin: 22,
        lgblock: 0,
    };

    let sync_compressed = compress_with_params(&original, &params).expect("sync compress");

    let mut enc = BrotliAsyncCompressor::with_params(params);
    let mut input = Cursor::new(original.clone());
    let mut async_compressed = Vec::new();
    enc.compress_async(&mut input, &mut async_compressed)
        .await
        .expect("async compress");

    assert_eq!(
        sync_compressed, async_compressed,
        "sync and async output differ"
    );
}
