//! Hermetic interoperability tests against real bzip2 (libbz2) streams.
//!
//! The `tests/data/python_*.bz2` golden vectors were generated ONCE at
//! development time with CPython's `bz2` module (libbz2); the committed
//! test suite never invokes external tools. The corresponding plaintexts
//! are regenerated deterministically in this file, so the vectors stay
//! small. To regenerate the vectors, evaluate with Python 3 (the
//! `xorshift_data` / `text_data` generators below have exact Python
//! equivalents: xorshift32 with state `0x9E3779B9`, shifts `<<13`, `>>17`,
//! `<<5` masked to 32 bits, 4 little-endian bytes per step; and
//! `"line %07d: the quick brown fox jumps over the lazy dog 0123456789\n"`
//! lines truncated to the target size):
//!
//! * `python_hello100_l9.bz2`   = `bz2.compress(b"hello bzip2 world\n" * 100, 9)`
//! * `python_xorshift64k_l9.bz2`= `bz2.compress(xorshift_data(65536), 9)`
//! * `python_text1m2_l9.bz2`    = `bz2.compress(text_data(1_200_000), 9)`
//! * `python_text250k_l1.bz2`   = `bz2.compress(text_data(250_000), 1)`
//! * `python_empty_l9.bz2`      = `bz2.compress(b"", 9)`
//!
//! The `tests/data/oxiarc_*.bz2` vectors are blessed outputs of this
//! crate's encoder that were verified byte-for-byte decodable by CPython's
//! `bz2.decompress` and the system `bzip2 -d` at development time. They
//! pin the encoder to a known interoperable serialization: any change to
//! these bytes must be re-verified against a real bzip2 implementation
//! before re-blessing.

use oxiarc_bzip2::{CompressionLevel, compress, decompress};

/// `bz2.compress(b"hello bzip2 world\n" * 100, 9)`
const PY_HELLO100_L9: &[u8] = include_bytes!("data/python_hello100_l9.bz2");
/// `bz2.compress(xorshift_data(65536), 9)`
const PY_XORSHIFT64K_L9: &[u8] = include_bytes!("data/python_xorshift64k_l9.bz2");
/// `bz2.compress(text_data(1_200_000), 9)` — two blocks (>900 KB boundary).
const PY_TEXT1M2_L9: &[u8] = include_bytes!("data/python_text1m2_l9.bz2");
/// `bz2.compress(text_data(250_000), 1)` — several level-1 blocks.
const PY_TEXT250K_L1: &[u8] = include_bytes!("data/python_text250k_l1.bz2");
/// `bz2.compress(b"", 9)`
const PY_EMPTY_L9: &[u8] = include_bytes!("data/python_empty_l9.bz2");

/// Blessed output of `compress(b"hello bzip2 world\n" * 100, level 9)`.
const OXIARC_HELLO100_L9: &[u8] = include_bytes!("data/oxiarc_hello100_l9.bz2");
/// Blessed output of `compress(text_data(250_000), level 1)`.
const OXIARC_TEXT250K_L1: &[u8] = include_bytes!("data/oxiarc_text250k_l1.bz2");

/// Deterministic xorshift32 byte stream; must match `gen_bz2_golden.py`.
fn xorshift_data(size: usize) -> Vec<u8> {
    let mut state: u32 = 0x9E37_79B9;
    let mut out = Vec::with_capacity(size + 4);
    while out.len() < size {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// Deterministic text stream; must match `gen_bz2_golden.py`.
fn text_data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 80);
    let mut i = 0usize;
    while out.len() < size {
        out.extend_from_slice(
            format!("line {i:07}: the quick brown fox jumps over the lazy dog 0123456789\n")
                .as_bytes(),
        );
        i += 1;
    }
    out.truncate(size);
    out
}

// ---------------------------------------------------------------------------
// Decompression of real libbz2 streams
// ---------------------------------------------------------------------------

#[test]
fn decodes_real_libbz2_text_stream() {
    let decoded = decompress(PY_HELLO100_L9).expect("decode libbz2 text stream");
    assert_eq!(decoded, b"hello bzip2 world\n".repeat(100));
}

#[test]
fn decodes_real_libbz2_binary_stream() {
    let decoded = decompress(PY_XORSHIFT64K_L9).expect("decode libbz2 binary stream");
    assert_eq!(decoded, xorshift_data(65536));
}

#[test]
fn decodes_real_libbz2_multiblock_over_900k() {
    // 1.2 MB at level 9 spans the 900 KB block boundary: two blocks whose
    // CRCs combine into the stream CRC.
    let decoded = decompress(PY_TEXT1M2_L9).expect("decode libbz2 multi-block stream");
    assert_eq!(decoded, text_data(1_200_000));
}

#[test]
fn decodes_real_libbz2_level1_blocks() {
    // 250 KB at level 1 produces several ~100 KB blocks.
    let decoded = decompress(PY_TEXT250K_L1).expect("decode libbz2 level-1 stream");
    assert_eq!(decoded, text_data(250_000));
}

#[test]
fn decodes_real_libbz2_empty_stream() {
    let decoded = decompress(PY_EMPTY_L9).expect("decode libbz2 empty stream");
    assert!(decoded.is_empty());
}

#[test]
fn rejects_corrupted_block_crc() {
    // Flip one bit inside the compressed payload; the block CRC must catch it.
    let mut corrupted = PY_HELLO100_L9.to_vec();
    let mid = corrupted.len() / 2;
    corrupted[mid] ^= 0x10;
    assert!(decompress(&corrupted[..]).is_err());
}

// ---------------------------------------------------------------------------
// Encoder output pinned to libbz2-verified serializations
// ---------------------------------------------------------------------------

#[test]
fn encoder_output_matches_blessed_interoperable_bytes_text() {
    let data = b"hello bzip2 world\n".repeat(100);
    let compressed = compress(&data, CompressionLevel::new(9)).expect("compress text");
    assert_eq!(
        compressed, OXIARC_HELLO100_L9,
        "encoder serialization drifted from the libbz2-verified blessed vector; \
         re-verify against a real bzip2 decoder before re-blessing"
    );
}

#[test]
fn encoder_output_matches_blessed_interoperable_bytes_multiblock() {
    let data = text_data(250_000);
    let compressed = compress(&data, CompressionLevel::new(1)).expect("compress level-1 blocks");
    assert_eq!(
        compressed, OXIARC_TEXT250K_L1,
        "encoder serialization drifted from the libbz2-verified blessed vector; \
         re-verify against a real bzip2 decoder before re-blessing"
    );
}

#[test]
fn encoder_empty_stream_matches_libbz2_layout() {
    // Header, EOS magic, zero combined CRC: identical to libbz2's empty
    // stream apart from the level digit.
    let compressed = compress(b"", CompressionLevel::new(9)).expect("compress empty");
    assert_eq!(compressed, PY_EMPTY_L9);
}

// ---------------------------------------------------------------------------
// Self round-trips (including the >900 KB multi-block encoder path)
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_binary_data() {
    let data = xorshift_data(300 * 1024);
    let compressed = compress(&data, CompressionLevel::new(1)).expect("compress binary");
    let decoded = decompress(&compressed[..]).expect("decode own binary stream");
    assert_eq!(decoded, data);
}

#[test]
fn roundtrip_text_and_runs() {
    let mut data = "bzip2 round-trip with mixed content \u{65e5}\u{672c}\u{8a9e}\n"
        .repeat(64)
        .into_bytes();
    data.extend_from_slice(&[0xAA; 1000]); // long RLE1 run
    data.extend_from_slice(&[0x00; 4]); // exactly four in a row
    let compressed = compress(&data, CompressionLevel::new(9)).expect("compress text");
    let decoded = decompress(&compressed[..]).expect("decode own text stream");
    assert_eq!(decoded, data);
}

#[test]
fn roundtrip_multiblock_over_900k() {
    // Forces the encoder across its per-block input limit at level 9.
    let data = text_data(1_200_000);
    let compressed = compress(&data, CompressionLevel::new(9)).expect("compress 1.2 MB");
    let decoded = decompress(&compressed[..]).expect("decode own multi-block stream");
    assert_eq!(decoded, data);
}

#[test]
fn roundtrip_all_byte_values() {
    // Exercises a full 256-symbol alphabet (all sixteen symbol-map groups).
    let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let compressed = compress(&data, CompressionLevel::new(5)).expect("compress all bytes");
    let decoded = decompress(&compressed[..]).expect("decode all-bytes stream");
    assert_eq!(decoded, data);
}
