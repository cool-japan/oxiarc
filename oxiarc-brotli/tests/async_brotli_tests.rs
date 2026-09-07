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

// ---------------------------------------------------------------------------
// Incremental decoder behaviour
// ---------------------------------------------------------------------------

/// A `tokio` source that yields at most `step` bytes per poll, so a test can
/// prove the async decoder does not need the whole body before it writes.
struct AsyncTrickle {
    data: Vec<u8>,
    pos: usize,
    step: usize,
}

impl tokio::io::AsyncRead for AsyncTrickle {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let me = self.get_mut();
        if me.pos == me.data.len() {
            return std::task::Poll::Ready(Ok(()));
        }
        let n = buf.remaining().min(me.step).min(me.data.len() - me.pos);
        buf.put_slice(&me.data[me.pos..me.pos + n]);
        me.pos += n;
        std::task::Poll::Ready(Ok(()))
    }
}

/// A source that trickles one byte at a time must still decode correctly, and
/// with a tiny staging buffer.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_decode_with_a_trickling_source() {
    let original = b"async incremental decode over a trickling source. ".repeat(2000);
    let compressed = compress(&original, 6).expect("compress");
    let mut dec = BrotliAsyncDecompressor::new();
    let mut source = AsyncTrickle {
        data: compressed,
        pos: 0,
        step: 1,
    };
    let mut out = Vec::new();
    let written = dec
        .decompress_async_with_buffer(&mut source, &mut out, 256)
        .await
        .expect("decompress_async");
    assert_eq!(written, original.len());
    assert_eq!(out, original);
}

/// A source that stops mid-stream is an error, never a short write.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_truncated_source_is_an_error() {
    let original = b"truncated async body ".repeat(3000);
    let compressed = compress(&original, 6).expect("compress");
    for fraction in [1usize, 3, 8] {
        let cut = compressed.len() * fraction / 10;
        let mut dec = BrotliAsyncDecompressor::new();
        let mut source = Cursor::new(compressed[..cut].to_vec());
        let mut out = Vec::new();
        let result = dec.decompress_async(&mut source, &mut out).await;
        assert!(
            result.is_err(),
            "a {cut}-byte prefix decoded successfully as a complete stream"
        );
    }
}

/// `with_max_output` is honoured by the async adapter, before the expansion is
/// produced.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_max_output_is_enforced() {
    let bomb = vec![0u8; 16 * 1024 * 1024];
    let compressed = compress(&bomb, 5).expect("compress");
    let mut dec = BrotliAsyncDecompressor::new().with_max_output(1 << 20);
    let mut source = Cursor::new(compressed);
    let mut out = Vec::new();
    let err = dec
        .decompress_async(&mut source, &mut out)
        .await
        .expect_err("over-budget stream must fail");
    assert!(
        err.to_string().contains("memory budget exceeded"),
        "unexpected error: {err}"
    );
    assert!(
        out.len() <= 1 << 20,
        "produced {} bytes past the 1 MiB budget",
        out.len()
    );
}

/// `with_max_window` refuses an over-large declared window before allocating.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_max_window_is_enforced() {
    let params = BrotliParams {
        quality: 4,
        lgwin: 22,
        ..BrotliParams::default()
    };
    let compressed = compress_with_params(b"async window ceiling", &params).expect("compress");
    let mut dec = BrotliAsyncDecompressor::new().with_max_window(1 << 16);
    let mut source = Cursor::new(compressed);
    let mut out = Vec::new();
    let err = dec
        .decompress_async(&mut source, &mut out)
        .await
        .expect_err("declared window over the ceiling must be refused");
    assert!(
        err.to_string().contains("exceeds"),
        "unexpected error: {err}"
    );
    assert!(out.is_empty(), "bytes escaped the window refusal");
}

/// The reference `brotli` 1.1.0 fixtures decode through the async adapter with
/// a minimal staging buffer.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_decodes_reference_streams() {
    // A stream this crate's encoder does not produce: reference q11 output
    // with static-dictionary references and word transforms.
    let hex = "1b2b00f89dc9e3de3b8d9adaa9a8d9de90d216885095c965080b5d301502f4a1dc0e";
    let compressed: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
        .collect();
    let expected = decompress(&compressed).expect("one-shot decode");
    let mut dec = BrotliAsyncDecompressor::new();
    let mut source = AsyncTrickle {
        data: compressed,
        pos: 0,
        step: 1,
    };
    let mut out = Vec::new();
    dec.decompress_async_with_buffer(&mut source, &mut out, 256)
        .await
        .expect("decompress_async");
    assert_eq!(out, expected);
}

/// A shared (custom LZ77) dictionary reaches the async adapter, and the same
/// body decoded without it does not silently produce the input.
///
/// The body is compressed against the dictionary at a ratio the dictionary-free
/// encoder cannot approach, so a `with_dictionary` that quietly did nothing
/// would fail the first assertion rather than pass the round trip.
#[tokio::test(flavor = "multi_thread")]
async fn test_async_decompressor_accepts_a_shared_dictionary() {
    let dictionary: Vec<u8> = (0..600u32)
        .flat_map(|i| format!("<li class=\"row-{i}\">item number {i}</li>\n").into_bytes())
        .collect();
    let data = dictionary[1000..12_000].to_vec();
    let params = BrotliParams {
        quality: 9,
        lgwin: 22,
        lgblock: 0,
    };
    let compressed =
        oxiarc_brotli::compress_with_dictionary(&data, &dictionary, &params).expect("compress -D");
    let plain = compress_with_params(&data, &params).expect("compress");
    assert!(
        compressed.len() * 4 < plain.len(),
        "the dictionary must be doing the work: {} with vs {} without",
        compressed.len(),
        plain.len()
    );

    // Trickled 7 bytes at a time through a 256-byte staging buffer: the
    // dictionary has to survive every resumption point, not just a whole-body
    // decode.
    let mut dec = BrotliAsyncDecompressor::new().with_dictionary(dictionary.clone());
    let mut source = AsyncTrickle {
        data: compressed.clone(),
        pos: 0,
        step: 7,
    };
    let mut out = Vec::new();
    dec.decompress_async_with_buffer(&mut source, &mut out, 256)
        .await
        .expect("decompress_async with a dictionary");
    assert_eq!(out, data);

    // Without the dictionary the distances mean something else entirely; the
    // stream stays structurally decodable, so the assertion is that it does not
    // reproduce the input, not that it errors.
    let mut bare = BrotliAsyncDecompressor::new();
    let mut source = Cursor::new(compressed.clone());
    let mut bare_out = Vec::new();
    let bare_result = bare.decompress_async(&mut source, &mut bare_out).await;
    assert!(
        bare_result.is_err() || bare_out != data,
        "a dictionary-compressed body must not decode without the dictionary"
    );

    // The output cap is enforced with a dictionary attached, too.
    let mut capped = BrotliAsyncDecompressor::new()
        .with_dictionary(dictionary)
        .with_max_output(64);
    let mut source = Cursor::new(compressed);
    let mut capped_out = Vec::new();
    let err = capped
        .decompress_async(&mut source, &mut capped_out)
        .await
        .expect_err("64-byte cap on an 11 KiB body");
    assert!(
        err.to_string().contains("exceed") || err.to_string().contains("limit"),
        "unexpected error: {err}"
    );
}
