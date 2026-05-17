//! Tests for OxiArc-specific dictionary compression extension.
//!
//! The Snappy specification has no dictionary semantics; these tests cover
//! the OxiArc block-level and frame-level dictionary compression APIs.

use oxiarc_snappy::{
    compress, compress_block_with_dict, compress_frame_with_dict, decompress_block_with_dict,
    decompress_frame_with_dict,
};

// ---------------------------------------------------------------------------
// Block-level tests
// ---------------------------------------------------------------------------

/// 4 KiB dict + 64 KiB input round-trip via the block dict API.
#[test]
fn test_block_dict_roundtrip() {
    // Build a 4 KiB dictionary from repeating ASCII text.
    let dict: Vec<u8> = b"The quick brown fox jumps over the lazy dog. "
        .iter()
        .cloned()
        .cycle()
        .take(4096)
        .collect();

    // 64 KiB input: mix of dict content and novel bytes.
    let input: Vec<u8> = b"The quick brown fox jumps over the lazy dog. Hello world!"
        .iter()
        .cloned()
        .cycle()
        .take(65536)
        .collect();

    let compressed = compress_block_with_dict(&input, &dict);
    let decompressed = decompress_block_with_dict(&compressed, &dict)
        .expect("decompress_block_with_dict should succeed");

    assert_eq!(
        decompressed, input,
        "round-trip mismatch for 4 KiB dict + 64 KiB input"
    );
}

/// Compressed size with dict should be <= compressed size without dict
/// when the input repeats substrings from the dict.
#[test]
fn test_block_dict_better_compression() {
    // Dictionary: a paragraph of text.
    let dict_text = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
          Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
          Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.";
    let dict: Vec<u8> = dict_text.iter().cloned().cycle().take(4096).collect();

    // Input: repeatedly copies phrases from the dict.
    let input: Vec<u8> = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
          Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
          Ut enim ad minim veniam quis nostrud exercitation."
        .iter()
        .cloned()
        .cycle()
        .take(8192)
        .collect();

    let with_dict = compress_block_with_dict(&input, &dict);
    let without_dict = compress(&input);

    assert!(
        with_dict.len() <= without_dict.len(),
        "dict compression should not be worse: with_dict={} without_dict={}",
        with_dict.len(),
        without_dict.len()
    );

    // Verify correctness too.
    let recovered =
        decompress_block_with_dict(&with_dict, &dict).expect("decompress should succeed");
    assert_eq!(recovered, input);
}

/// Compressing with dict_a and decompressing with dict_b (different dict) must
/// produce output different from the original input.  (It should not error at
/// the block level; the format is structurally valid but the bytes are wrong.)
#[test]
fn test_block_dict_wrong_dict_garbles() {
    let dict_a: Vec<u8> = b"dictionary alpha content abcdefghijklmnopqrstuvwxyz"
        .iter()
        .cloned()
        .cycle()
        .take(2048)
        .collect();

    let dict_b: Vec<u8> = b"dictionary BETA CONTENT ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        .iter()
        .cloned()
        .cycle()
        .take(2048)
        .collect();

    let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz dictionary alpha content"
        .iter()
        .cloned()
        .cycle()
        .take(4096)
        .collect();

    let compressed_a = compress_block_with_dict(&input, &dict_a);

    // Decompressing with the correct dict must succeed and match.
    let recovered_correct = decompress_block_with_dict(&compressed_a, &dict_a)
        .expect("correct dict decompress should succeed");
    assert_eq!(
        recovered_correct, input,
        "correct dict must recover original"
    );

    // Decompressing with wrong dict: may succeed (structurally valid) or error,
    // but must NOT produce the original input.
    match decompress_block_with_dict(&compressed_a, &dict_b) {
        Ok(garbled) => {
            assert_ne!(
                garbled, input,
                "wrong dict must not produce the original input"
            );
        }
        Err(_) => {
            // An error is also acceptable — the important thing is it doesn't
            // silently return the original data.
        }
    }
}

/// Empty dict must produce byte-identical output to the standard `compress`.
#[test]
fn test_block_dict_empty_dict_parity() {
    let test_cases: &[&[u8]] = &[b"", b"Hello", b"abcdefghijklmnopqrstuvwxyz", &{
        let mut v = Vec::with_capacity(4096);
        for i in 0u16..4096 {
            v.push((i % 251) as u8);
        }
        v
    }];

    for &input in test_cases {
        let with_empty_dict = compress_block_with_dict(input, b"");
        let without_dict = compress(input);
        assert_eq!(
            with_empty_dict,
            without_dict,
            "empty dict must produce identical output for input of length {}",
            input.len()
        );
    }
}

/// A dict longer than 64 KiB must produce the same result as using only the
/// last 64 KiB of that dict.
#[test]
fn test_block_dict_overlong_dict_truncated() {
    // Build a dict > 64 KiB.
    let long_dict: Vec<u8> = (0u8..=255u8).cycle().take(65536 + 512).collect::<Vec<u8>>();

    // The "effective" dict is the last 64 KiB.
    let effective_dict = &long_dict[long_dict.len() - 65536..];

    let input: Vec<u8> = b"test input that repeats abcdefghijklmnopqrstuvwxyz bytes"
        .iter()
        .cloned()
        .cycle()
        .take(8192)
        .collect();

    let compressed_long = compress_block_with_dict(&input, &long_dict);
    let compressed_clamped = compress_block_with_dict(&input, effective_dict);

    assert_eq!(
        compressed_long, compressed_clamped,
        "long dict must behave identically to its last-64KiB truncation"
    );

    // Both should round-trip with the effective (or long) dict.
    let recovered = decompress_block_with_dict(&compressed_long, effective_dict)
        .expect("decompress with truncated dict should succeed");
    assert_eq!(recovered, input);
}

/// Boundary cases: dict exactly 64 KiB; input shorter/longer than dict.
#[test]
fn test_block_dict_boundary() {
    // Dict exactly 64 KiB.
    let dict_64k: Vec<u8> = b"boundary test data with repeating pattern "
        .iter()
        .cloned()
        .cycle()
        .take(65536)
        .collect();

    // Case 1: input shorter than dict.
    let short_input: Vec<u8> = b"short input".to_vec();
    let c = compress_block_with_dict(&short_input, &dict_64k);
    let d = decompress_block_with_dict(&c, &dict_64k).expect("short input round-trip");
    assert_eq!(d, short_input, "short input round-trip failed");

    // Case 2: input longer than dict.
    let long_input: Vec<u8> = b"boundary test data with repeating pattern "
        .iter()
        .cloned()
        .cycle()
        .take(131072)
        .collect();
    let c = compress_block_with_dict(&long_input, &dict_64k);
    let d = decompress_block_with_dict(&c, &dict_64k).expect("long input round-trip");
    assert_eq!(d, long_input, "long input round-trip failed");

    // Case 3: input same length as dict.
    let same_input: Vec<u8> = b"boundary test data with repeating pattern "
        .iter()
        .cloned()
        .cycle()
        .take(65536)
        .collect();
    let c = compress_block_with_dict(&same_input, &dict_64k);
    let d = decompress_block_with_dict(&c, &dict_64k).expect("same-length input round-trip");
    assert_eq!(d, same_input, "same-length input round-trip failed");
}

// ---------------------------------------------------------------------------
// Frame-level tests
// ---------------------------------------------------------------------------

/// Full round-trip using compress_frame_with_dict / decompress_frame_with_dict.
#[test]
fn test_frame_dict_roundtrip() {
    let dict: Vec<u8> =
        b"frame dictionary content: snappy compressed frame with dictionary support"
            .iter()
            .cloned()
            .cycle()
            .take(4096)
            .collect();

    // Input larger than one chunk to exercise multi-chunk framing.
    let input: Vec<u8> =
        b"frame dictionary content: snappy compressed frame with dictionary support. "
            .iter()
            .cloned()
            .cycle()
            .take(200_000)
            .collect();

    let compressed = compress_frame_with_dict(&input, &dict);
    let decompressed = decompress_frame_with_dict(&compressed, &dict)
        .expect("frame dict round-trip should succeed");

    assert_eq!(decompressed, input, "frame dict round-trip mismatch");
}

/// Verify the frame starts with the Snappy stream identifier followed by the
/// OxiArc dict-info skippable chunk (type = 0xFE).
#[test]
fn test_frame_dict_skippable_chunk() {
    let dict = b"test dictionary for verifying frame structure";
    let input = b"some test input data";

    let compressed = compress_frame_with_dict(input, dict);

    // Bytes 0-9: Snappy stream identifier.
    let expected_stream_id: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];
    assert_eq!(
        &compressed[..10],
        &expected_stream_id,
        "stream identifier not found at start of frame"
    );

    // Byte 10: chunk type must be 0xFE.
    assert_eq!(
        compressed[10], 0xFE,
        "expected OxiArc dict-info chunk type 0xFE at offset 10, got {:#04x}",
        compressed[10]
    );

    // Bytes 11-13: chunk body length (little-endian 3 bytes).
    let body_len = (compressed[11] as usize)
        | ((compressed[12] as usize) << 8)
        | ((compressed[13] as usize) << 16);
    // Body is "OXIAD" (5) + crc32c (4) + len (4) = 13 bytes.
    assert_eq!(
        body_len, 13,
        "OxiArc dict-info chunk body length should be 13, got {body_len}"
    );

    // Bytes 14-18: magic "OXIAD".
    assert_eq!(
        &compressed[14..19],
        b"OXIAD",
        "OXIAD magic not found in dict-info chunk"
    );

    // Round-trip still works.
    let decompressed =
        decompress_frame_with_dict(&compressed, dict).expect("decompress should succeed");
    assert_eq!(decompressed.as_slice(), input, "round-trip mismatch");
}

/// Supplying the wrong dict to decompress_frame_with_dict must return an error
/// (CRC32C mismatch in the dict-info chunk).
#[test]
fn test_frame_dict_wrong_dict_rejected() {
    let dict_a = b"correct dictionary AAAA";
    let dict_b = b"wrong dictionary BBBB!!";

    let input = b"some test data for frame wrong dict test";
    let compressed = compress_frame_with_dict(input, dict_a);

    let result = decompress_frame_with_dict(&compressed, dict_b);
    assert!(
        result.is_err(),
        "decompress with wrong dict should return an error"
    );
}

/// Empty dict at frame level should still produce a valid compressible frame.
#[test]
fn test_frame_dict_empty_dict() {
    let input: Vec<u8> = b"frame test with empty dict content repeating"
        .iter()
        .cloned()
        .cycle()
        .take(10_000)
        .collect();

    let compressed = compress_frame_with_dict(&input, b"");
    let decompressed =
        decompress_frame_with_dict(&compressed, b"").expect("empty dict frame round-trip");
    assert_eq!(decompressed, input, "empty dict frame round-trip mismatch");
}

/// Decompress a standard (non-dict) Snappy frame with decompress_frame_with_dict
/// should return an error (no OxiArc dict-info chunk present).
#[test]
fn test_frame_dict_rejects_standard_frame() {
    use oxiarc_snappy::FrameEncoder;
    use std::io::Write;

    let data = b"some data compressed without dict";
    let mut compressed = Vec::new();
    {
        let mut enc = FrameEncoder::new(&mut compressed);
        enc.write_all(data).expect("write should succeed");
        enc.finish().expect("finish should succeed");
    }

    let result = decompress_frame_with_dict(&compressed, b"some dict");
    assert!(
        result.is_err(),
        "standard frame must be rejected by decompress_frame_with_dict"
    );
}
