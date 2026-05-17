//! Snappy interop tests against known wire-format vectors.
//!
//! These tests verify that our implementation produces and consumes the
//! canonical Snappy block format as described in the Google Snappy spec.
//!
//! Wire format recap:
//!   - Varint-encoded uncompressed length
//!   - Sequence of elements:
//!     - Literal: tag `0bLLLLLL_00` where LLLLLL encodes (n-1) for n≤60
//!     - Copy-1: `0bOOOLLL_01` + 1 offset byte (11-bit offset, length 4..11)
//!     - Copy-2: `0bLLLLLL_10` + 2 offset bytes (16-bit LE offset, length 1..64)
//!     - Copy-4: `0bLLLLLL_11` + 4 offset bytes (32-bit LE offset, length 1..64)

/// Verify that the empty input compresses to exactly `[0x00]` (varint 0,
/// no elements) and decompresses back to empty.
#[test]
fn test_snappy_interop_empty() {
    let compressed = oxiarc_snappy::compress(&[]);
    assert_eq!(
        compressed,
        &[0x00],
        "empty input must compress to exactly [0x00] (varint 0)"
    );
    let decompressed = oxiarc_snappy::decompress(&compressed).expect("decompress empty");
    assert!(decompressed.is_empty(), "decompressed empty must be empty");
}

/// A single byte `0xAB` must compress with varint(1) = `0x01` as the first
/// byte of the compressed stream.
#[test]
fn test_snappy_interop_single_byte() {
    let input = &[0xABu8];
    let compressed = oxiarc_snappy::compress(input);

    // Varint-encoded uncompressed length must be 0x01.
    assert_eq!(
        compressed[0], 0x01,
        "varint length header must be 0x01 for 1-byte input, got 0x{:02X}",
        compressed[0]
    );

    // Must round-trip correctly.
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress single byte should succeed");
    assert_eq!(decompressed.as_slice(), input.as_slice());
}

/// Five-byte ASCII "hello" must survive a compress/decompress round-trip.
/// The compressed stream must open with varint(5) = `0x05`.
#[test]
fn test_snappy_interop_hello() {
    let input = b"hello";
    let compressed = oxiarc_snappy::compress(input);

    // First byte: varint(5) = 0x05.
    assert_eq!(
        compressed[0], 0x05,
        "varint header must be 0x05 for 5-byte input"
    );

    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress 'hello' should succeed");
    assert_eq!(decompressed.as_slice(), input.as_slice());
}

/// 256 bytes of the same value `0xAA` should compress dramatically smaller
/// than 256 bytes (copy elements exploit the repetition).
#[test]
fn test_snappy_interop_repeated_bytes() {
    let input = vec![0xAAu8; 256];
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress repeated bytes should succeed");

    assert_eq!(decompressed, input, "decompressed must match original");
    assert!(
        compressed.len() < 100,
        "repeated bytes must compress well: expected < 100 bytes, got {}",
        compressed.len()
    );
}

/// A "run" of a single byte value for 64 bytes — exercises the minimum
/// useful copy back-reference (offset=1, repeated pattern).
#[test]
fn test_snappy_interop_single_byte_run_64() {
    let input = vec![0x5Au8; 64];
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress 64-byte run should succeed");
    assert_eq!(decompressed, input);
}

/// Exactly 64 KiB (65536 bytes) of uniform data — tests the boundary of the
/// Snappy framed format's default maximum chunk size.
#[test]
fn test_snappy_interop_64kib_boundary() {
    let input = vec![0x42u8; 65536];
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress 64KiB should succeed");
    assert_eq!(decompressed, input, "64 KiB boundary round-trip failed");
}

/// 64 KiB + 1 byte — forces the block to span exactly one byte past the
/// framed chunk boundary.
#[test]
fn test_snappy_interop_64kib_plus_one() {
    let input = vec![0x43u8; 65537];
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress 64KiB+1 should succeed");
    assert_eq!(decompressed, input, "64 KiB + 1 round-trip failed");
}

/// `max_compress_len` must always be an upper bound on the actual compressed
/// output length across a representative range of input sizes.
#[test]
fn test_snappy_interop_max_compress_len() {
    for size in [0usize, 1, 64, 1024, 65535, 65536, 65537] {
        let input = vec![0xFFu8; size];
        let compressed = oxiarc_snappy::compress(&input);
        let max_len = oxiarc_snappy::max_compress_len(size);
        assert!(
            compressed.len() <= max_len,
            "size={size}: compressed.len()={} must be <= max_compress_len={}",
            compressed.len(),
            max_len
        );
    }
}

/// Round-trip a deterministic pseudo-random byte pattern (same construction
/// used by Google's snappy unit-test corpus) to exercise the hash-table
/// code path with non-trivial data.
#[test]
fn test_snappy_interop_roundtrip_arbitrary_data() {
    let mut data = Vec::with_capacity(4096);
    for i in 0u32..4096 {
        // Knuth multiplicative hash scramble — deterministic, non-repeating.
        data.push((i.wrapping_mul(0x9E37_79B9) >> 24) as u8);
    }
    let compressed = oxiarc_snappy::compress(&data);
    let decompressed = oxiarc_snappy::decompress(&compressed)
        .expect("decompress deterministic pseudo-random data should succeed");
    assert_eq!(decompressed, data);
}

/// Alternating two-byte pattern of length 1024 — exercises 2-byte copy
/// back-references (offset=2, repeated pattern).
#[test]
fn test_snappy_interop_alternating_two_bytes() {
    let pattern = [0xDEu8, 0xAD];
    let input: Vec<u8> = pattern.iter().cycle().take(1024).cloned().collect();
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed = oxiarc_snappy::decompress(&compressed)
        .expect("decompress alternating pattern should succeed");
    assert_eq!(decompressed, input);
    // Two alternating bytes should compress much better than 1024 raw bytes.
    assert!(
        compressed.len() < 200,
        "alternating 2-byte pattern should compress well: got {} bytes",
        compressed.len()
    );
}

/// Verify that a manually crafted valid Snappy stream (empty-content) is
/// accepted by our decompressor.  Wire format: `[0x00]` is the shortest
/// valid Snappy block (varint 0, no elements).
#[test]
fn test_snappy_interop_decode_crafted_empty_stream() {
    // Manually constructed: varint(0) with no elements.
    let crafted: &[u8] = &[0x00];
    let result = oxiarc_snappy::decompress(crafted).expect("crafted empty stream must decompress");
    assert!(
        result.is_empty(),
        "crafted empty stream must decompress to empty vec"
    );
}

/// Verify that a Snappy stream containing a single literal element for 4 bytes
/// decompresses correctly.  This exercises the literal-only code path.
///
/// Wire format:
///   - `0x04` — varint(4): 4 uncompressed bytes
///   - `0x0C` — literal tag: `(3 << 2) | 0x00` = 12 = (4-1)<<2 = 12
///   - `0xDE 0xAD 0xBE 0xEF` — the 4 raw bytes
#[test]
fn test_snappy_interop_decode_crafted_literal_stream() {
    let crafted: &[u8] = &[0x04, 0x0C, 0xDE, 0xAD, 0xBE, 0xEF];
    let result =
        oxiarc_snappy::decompress(crafted).expect("crafted literal stream must decompress");
    assert_eq!(result, &[0xDE, 0xAD, 0xBE, 0xEF]);
}

/// Compress then decompress 128 KiB of a cycling byte sequence to exercise
/// multi-block code paths in the block compressor.
#[test]
fn test_snappy_interop_128kib_cycling_pattern() {
    let input: Vec<u8> = (0u8..=127).cycle().take(131072).collect();
    let compressed = oxiarc_snappy::compress(&input);
    let decompressed =
        oxiarc_snappy::decompress(&compressed).expect("decompress 128KiB cycling pattern");
    assert_eq!(
        decompressed, input,
        "128 KiB cycling pattern round-trip failed"
    );
}

/// Varint header must encode the exact uncompressed length.  We verify by
/// decoding `decompress_len` against known varint encodings.
#[test]
fn test_snappy_interop_varint_header_roundtrip() {
    // For each input size we can quickly verify: the decompressor must agree
    // on the uncompressed length without fully decompressing.
    for size in [0usize, 1, 127, 128, 255, 256, 16383, 16384] {
        let input = vec![0x7Fu8; size];
        let compressed = oxiarc_snappy::compress(&input);
        let reported_len =
            oxiarc_snappy::decompress_len(&compressed).expect("decompress_len should succeed");
        assert_eq!(
            reported_len, size,
            "varint header in compressed stream must encode uncompressed length {size}, got {reported_len}"
        );
    }
}

/// Ensure that corrupted / truncated data is rejected gracefully (no panic).
#[test]
fn test_snappy_interop_rejects_truncated_data() {
    // A valid single-byte stream is [0x01, 0x00, 0xAB].
    // Truncate after the varint — the literal element is missing.
    let truncated: &[u8] = &[0x01];
    let result = oxiarc_snappy::decompress(truncated);
    assert!(
        result.is_err(),
        "truncated stream must return an error, not Ok"
    );
}

/// Ensure that a length-field claiming an impossibly huge decompressed size
/// is rejected before any allocation attempt.
#[test]
fn test_snappy_interop_rejects_oversized_length_varint() {
    // Encode a varint claiming 512 MiB uncompressed (> MAX_DECOMPRESSED_SIZE).
    // 512 MiB = 0x2000_0000
    // LEB128 encoding: 0xA0 0x80 0x80 0x80 0x02
    let crafted: &[u8] = &[0xA0, 0x80, 0x80, 0x80, 0x02];
    let result = oxiarc_snappy::decompress(crafted);
    assert!(result.is_err(), "oversized varint length must be rejected");
}
