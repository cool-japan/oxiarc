//! Regression tests for the lh5/lh6/lh7 encoder window-handling bug.
//!
//! The lh5 encoder used to corrupt data beyond its 8 KB window:
//! * CRC-16 mismatch on extraction for incompressible payloads >= 16 KB
//!   (the whole block was pre-written into the circular window, clobbering
//!   both history and lookahead), and
//! * silent corruption (decode "succeeds", bytes differ) for compressible
//!   payloads >= 64 KB (the 16-bit per-block size field overflowed).
//!
//! These tests exercise every method/size combination the bug affected and
//! verify byte-exactness plus the LHA CRC-16 of the round-tripped payload.
//! Everything is generated deterministically in-process — no external tools.

use oxiarc_core::Crc16;
use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};

/// Deterministic xorshift32 pseudo-random bytes (effectively incompressible).
fn xorshift_data(len: usize, mut state: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 4);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Highly compressible repeating text.
fn compressible_data(len: usize) -> Vec<u8> {
    b"The quick brown fox jumps over the lazy dog. 1234567890 "
        .iter()
        .cycle()
        .take(len)
        .copied()
        .collect()
}

/// Round-trip `data` through `method` and verify bytes and CRC-16.
fn roundtrip_with_crc(method: LzhMethod, data: &[u8], label: &str) {
    let crc_before = Crc16::compute(data);

    let compressed = encode_lzh(data, method)
        .unwrap_or_else(|e| panic!("{label}: encode failed for {method}: {e}"));
    let decoded = decode_lzh(&compressed, method, data.len() as u64)
        .unwrap_or_else(|e| panic!("{label}: decode failed for {method}: {e}"));

    assert_eq!(
        decoded.len(),
        data.len(),
        "{label}: length mismatch for {method}"
    );
    // Locate the first differing byte for a diagnosable failure message.
    if decoded != data {
        let first_diff = decoded
            .iter()
            .zip(data.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(usize::MAX);
        panic!(
            "{label}: silent corruption for {method}: first differing byte at offset {first_diff}"
        );
    }

    let crc_after = Crc16::compute(&decoded);
    assert_eq!(
        crc_before, crc_after,
        "{label}: CRC-16 mismatch for {method}: expected {crc_before:04X}, got {crc_after:04X}"
    );
}

const SIZES: [usize; 4] = [8 * 1024, 16 * 1024, 64 * 1024, 100 * 1024];

#[test]
fn lh5_roundtrip_incompressible_8k_16k_64k_100k() {
    for (i, &size) in SIZES.iter().enumerate() {
        let data = xorshift_data(size, 0x1234_5678u32.wrapping_add(i as u32));
        roundtrip_with_crc(LzhMethod::Lh5, &data, &format!("incompressible {size}B"));
    }
}

#[test]
fn lh5_roundtrip_compressible_8k_16k_64k_100k() {
    for &size in &SIZES {
        let data = compressible_data(size);
        roundtrip_with_crc(LzhMethod::Lh5, &data, &format!("compressible {size}B"));
    }
}

#[test]
fn lh6_roundtrip_incompressible_and_compressible() {
    for &size in &SIZES {
        let data = xorshift_data(size, 0xC001_D00D);
        roundtrip_with_crc(LzhMethod::Lh6, &data, &format!("incompressible {size}B"));
        let data = compressible_data(size);
        roundtrip_with_crc(LzhMethod::Lh6, &data, &format!("compressible {size}B"));
    }
}

#[test]
fn lh7_roundtrip_incompressible_and_compressible() {
    for &size in &SIZES {
        let data = xorshift_data(size, 0xDEAD_BEEF);
        roundtrip_with_crc(LzhMethod::Lh7, &data, &format!("incompressible {size}B"));
        let data = compressible_data(size);
        roundtrip_with_crc(LzhMethod::Lh7, &data, &format!("compressible {size}B"));
    }
}

#[test]
fn lh4_roundtrip_beyond_window() {
    // lh4 has a 4 KB window; 16 KB exceeds it fourfold.
    let data = xorshift_data(16 * 1024, 0x0BAD_F00D);
    roundtrip_with_crc(LzhMethod::Lh4, &data, "lh4 incompressible 16KB");
    let data = compressible_data(16 * 1024);
    roundtrip_with_crc(LzhMethod::Lh4, &data, "lh4 compressible 16KB");
}

#[test]
fn mixed_content_roundtrip_beyond_window() {
    // Interleave incompressible and compressible sections so matches
    // straddle window boundaries.
    let mut data = Vec::new();
    for chunk in 0..10 {
        data.extend_from_slice(&xorshift_data(7 * 1024, 0x1111_2222 + chunk));
        data.extend_from_slice(&compressible_data(6 * 1024));
    }
    for method in [LzhMethod::Lh5, LzhMethod::Lh6, LzhMethod::Lh7] {
        roundtrip_with_crc(method, &data, "mixed 130KB");
    }
}

#[test]
fn optimal_parser_roundtrip_beyond_window() {
    // The optimal (DP) parser shares the window logic; verify it too.
    use oxiarc_lzhuf::LzhEncoder;

    for &size in &[16 * 1024usize, 64 * 1024] {
        let data = compressible_data(size);
        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("optimal encode failed");
        let decoded =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decode failed");
        assert_eq!(decoded, data, "optimal parser corruption at {size} bytes");

        let data = xorshift_data(size, 0x5EED_5EED);
        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("optimal encode failed");
        let decoded =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decode failed");
        assert_eq!(
            decoded, data,
            "optimal parser corruption (incompressible) at {size} bytes"
        );
    }
}

#[test]
fn streaming_decoder_matches_serial_beyond_window() {
    use oxiarc_lzhuf::decode_lzh_streaming;

    for method in [LzhMethod::Lh5, LzhMethod::Lh6, LzhMethod::Lh7] {
        let data = compressible_data(80 * 1024);
        let compressed = encode_lzh(&data, method).expect("encode failed");
        let serial =
            decode_lzh(&compressed, method, data.len() as u64).expect("serial decode failed");
        let streaming = decode_lzh_streaming(&compressed, method, data.len() as u64)
            .expect("streaming decode failed");
        assert_eq!(serial, data, "serial mismatch for {method}");
        assert_eq!(streaming, data, "streaming mismatch for {method}");
    }
}
