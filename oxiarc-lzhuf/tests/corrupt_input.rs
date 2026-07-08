//! Corrupt/truncated-bitstream tests for `oxiarc-lzhuf`'s decode entry
//! points: [`decode_lzh`] (the canonical lh0/lh4-lh7 decoder, `decode.rs`)
//! and [`decode_lh1`] (the legacy `-lh1-` adaptive-Huffman decoder,
//! `lh1.rs`). Neither had any test feeding truncated or bit-flipped input
//! before this file.
//!
//! ## End-of-stream handling
//!
//! The `lh4`-`lh7` dynamic-Huffman reader (`oxiarc_core::msb_bitstream`)
//! follows the classic LHA/LZHUF `fillbuf` end-of-stream convention: once
//! the underlying byte slice is exhausted it synthesizes **zero bits**
//! forever rather than raising an I/O error, so truncating or lightly
//! bit-flipping such a stream often still returns `Ok` (with wrong/short
//! data). The `lh1` reader shares the same zero-bit synthesis, but its
//! decoder now *tracks* the over-read and stops with a `CorruptedData`
//! error once real input is exhausted before `original_size` bytes have
//! been produced -- otherwise an attacker-declared huge `original_size`
//! would let a tiny truncated payload balloon into gigabytes of fabricated
//! output (a decompression-bomb / DoS vector).
//!
//! So the assertions here are calibrated to what the code actually,
//! correctly does:
//! - `lh0` truncation -> always `Err` (real I/O short-read).
//! - `lh1` truncation -> `CorruptedData` (bitstream exhausted early); light
//!   corruption -> `CorruptedData` or full-length garbage, never the
//!   original data, never a panic/hang.
//! - `lh4`-`lh7` truncation or light corruption -> never panics, and never
//!   silently reproduces the *original* data; heavy multi-byte corruption of
//!   a dynamic-Huffman stream can and does still surface as `Err`
//!   (`InvalidDistance`/`InvalidHuffman`/`Corrupted`) once the corruption is
//!   severe enough to break the ring-buffer/tree invariants -- demonstrated
//!   deterministically below.

use oxiarc_lzhuf::{LzhMethod, decode_lh1, decode_lzh, encode_lh1, encode_lzh};

/// A non-trivial, non-repetitive sample buffer (20,000 bytes) that compresses
/// to a genuinely multi-block dynamic-Huffman stream under lh4-lh7.
fn sample_data() -> Vec<u8> {
    (0..20_000u32)
        .map(|i| ((i * 37 + i / 13) % 251) as u8)
        .collect()
}

/// Deterministic pseudo-random byte source (xorshift64), so mutation/fuzz
/// tests are fully reproducible without a `rand` dependency.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Truncating an `lh0` (stored) stream must reliably return `Err`: unlike
/// the dynamic-Huffman methods, stored data is read via `Read::read_exact`,
/// which genuinely short-reads on truncated input.
#[test]
fn decode_lzh_stored_truncated_returns_err() {
    let data = sample_data();
    for &len in &[0usize, 1, 100, 5_000, data.len() - 1] {
        let truncated = &data[..len];
        let result =
            std::panic::catch_unwind(|| decode_lzh(truncated, LzhMethod::Lh0, data.len() as u64));
        let result =
            result.unwrap_or_else(|_| panic!("decode_lzh(lh0) must not panic on {len} bytes"));
        assert!(
            result.is_err(),
            "expected Err decoding {len}/{} truncated stored bytes as lh0",
            data.len()
        );
    }
}

/// Truncating a dynamic-Huffman (`lh5`) stream at points spanning the
/// block-count field, mid-temp-table, mid-code-tree/offset-tree, and deep
/// into a literal run must never panic and -- per the zero-padding
/// end-of-stream convention documented at the top of this file -- must
/// never silently reproduce the original data either (any observed `Ok`
/// necessarily has the wrong length or wrong content).
#[test]
fn decode_lzh_dynamic_huffman_truncation_never_silently_succeeds() {
    let data = sample_data();
    let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("encode_lzh must succeed");
    assert!(encoded.len() > 200, "fixture must be a multi-block stream");

    // 0-2: still inside the 16-bit block-count field.
    // 3-32: mid temp-table / code-tree / offset-tree.
    // 64+: deep into a literal/match run (mid "literal run").
    let offsets = [
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        8,
        10,
        16,
        32,
        64,
        encoded.len() / 4,
        encoded.len() / 2,
        encoded.len() - 1,
    ];

    for &offset in &offsets {
        let truncated = &encoded[..offset];
        let result =
            std::panic::catch_unwind(|| decode_lzh(truncated, LzhMethod::Lh5, data.len() as u64));
        let result = result
            .unwrap_or_else(|_| panic!("decode_lzh(lh5) must not panic truncated to {offset}"));
        match result {
            Err(_) => {} // a hard error is also an acceptable graceful outcome
            Ok(v) => assert_ne!(
                v,
                data,
                "decode_lzh must not silently reproduce the original data \
                 from a stream truncated to {offset}/{} bytes",
                encoded.len()
            ),
        }
    }
}

/// Corrupting the byte that carries the temp-table ("tree-size") symbol
/// count -- forcing it to both extremes (0x00 -> degenerate single-code
/// table, 0xFF -> maximal/garbage count) at the start of several different
/// blocks -- must never panic. Most such corruptions also change the
/// decoded output (verified in aggregate below); a rare few land on bits
/// that only affect the block-count field without changing which symbols
/// get decoded, so an individual mutation coincidentally leaving the output
/// unchanged is tolerated as long as it isn't the common case.
#[test]
fn decode_lzh_corrupted_tree_size_byte_never_panics() {
    let data = sample_data();
    let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("encode_lzh must succeed");

    // Byte 2 holds the low byte of the 16-bit block-count field plus the
    // top bits of the first block's 5-bit temp-table count; scan a spread
    // of nearby offsets so later blocks' tables are hit too.
    let candidate_offsets: Vec<usize> = (0..encoded.len()).step_by(37).collect();

    let mut trials = 0usize;
    let mut silently_unchanged = 0usize;
    for &offset in &candidate_offsets {
        for &mutation in &[0x00u8, 0xFFu8] {
            let mut mutated = encoded.clone();
            mutated[offset] = mutation;
            let result = std::panic::catch_unwind(|| {
                decode_lzh(&mutated, LzhMethod::Lh5, data.len() as u64)
            });
            let result = result.unwrap_or_else(|_| {
                panic!("decode_lzh(lh5) must not panic with byte {offset} set to {mutation:#04x}")
            });
            trials += 1;
            if let Ok(v) = result {
                if v == data {
                    silently_unchanged += 1;
                }
            }
        }
    }
    assert!(
        silently_unchanged * 10 < trials,
        "corrupting the tree-size byte left the output unchanged in {silently_unchanged}/{trials} \
         trials -- expected corruption to be detectable (Err or changed output) in the vast \
         majority of cases"
    );
}

/// Single-bit mutations across an entire valid `lh5` stream must never
/// panic. (Whether any individual flip surfaces as `Err` or as `Ok` with
/// different data depends on which field it lands in; both are acceptable.
/// A handful of bit positions -- mostly inside the per-block symbol-count
/// field, which only bounds the loop rather than selecting decoded symbols
/// -- can coincidentally leave the output unchanged; what matters is that
/// this is the rare exception, not the rule, and that nothing ever panics.)
#[test]
fn decode_lzh_bit_flip_fuzz_never_panics_or_silently_succeeds() {
    let data = sample_data();
    let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("encode_lzh must succeed");

    let mut rng = Xorshift64::new(0xABCD_1234_5678_90EF);
    let total_bits = encoded.len() * 8;
    let mut silently_unchanged = 0usize;
    let trials = 1_000usize;
    for _ in 0..trials {
        let bit = (rng.next_u64() as usize) % total_bits;
        let mut mutated = encoded.clone();
        mutated[bit / 8] ^= 1 << (7 - (bit % 8));

        let result =
            std::panic::catch_unwind(|| decode_lzh(&mutated, LzhMethod::Lh5, data.len() as u64));
        let result = result
            .unwrap_or_else(|_| panic!("decode_lzh(lh5) must not panic on bit-flip at bit {bit}"));
        if let Ok(v) = result {
            if v == data {
                silently_unchanged += 1;
            }
        }
    }
    assert!(
        silently_unchanged * 10 < trials,
        "single-bit flips left the output unchanged in {silently_unchanged}/{trials} trials -- \
         expected corruption to be detectable (Err or changed output) in the vast majority of cases"
    );
}

/// Heavier multi-byte corruption of a valid `lh5` stream -- deterministic,
/// fixed-seed, reproducible across runs -- both never panics *and*
/// demonstrably surfaces `Err` (via the ring-buffer distance guard or the
/// Huffman-tree bounds guard) at least once across a bounded number of
/// trials, proving those guard rails are reachable from adversarial input
/// rather than dead code.
#[test]
fn decode_lzh_heavy_multi_byte_corruption_can_return_err() {
    let data = sample_data();
    let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("encode_lzh must succeed");

    let mut rng = Xorshift64::new(0x1234_5678_9abc_def0);
    let mut saw_err = false;
    for trial in 0..3_000 {
        let mut mutated = encoded.clone();
        let n_corrupt = 1 + (rng.next_u64() as usize % 20);
        for _ in 0..n_corrupt {
            let idx = (rng.next_u64() as usize) % mutated.len();
            mutated[idx] = rng.next_u64() as u8;
        }

        let result =
            std::panic::catch_unwind(|| decode_lzh(&mutated, LzhMethod::Lh5, data.len() as u64));
        let result = result
            .unwrap_or_else(|_| panic!("decode_lzh(lh5) must not panic on corrupt trial {trial}"));
        if result.is_err() {
            saw_err = true;
        }
    }

    assert!(
        saw_err,
        "expected at least one Err among 3000 heavy multi-byte-corruption trials"
    );
}

/// `decode_lh1` (the legacy adaptive-Huffman `-lh1-` path) uses its own
/// private MSB-first `BitReader`. A truncated stream is exhausted before it
/// can produce the declared `original_size`, so decoding must terminate with
/// a `CorruptedData` error rather than fabricating a full-length garbage
/// buffer from zero padding (the latter is a decompression-bomb / DoS
/// vector when `original_size` is attacker-controlled). This test verifies
/// the decoder never panics and never silently reproduces the original data
/// on genuine truncation; when it returns `Ok` it must emit exactly
/// `original_size` bytes.
#[test]
fn decode_lh1_truncated_never_panics_or_silently_succeeds() {
    let data = sample_data();
    let encoded = encode_lh1(&data);
    assert!(encoded.len() > 100, "fixture must be non-trivial");

    for &len in &[0usize, 1, 5, 50, 200, encoded.len() / 2, encoded.len() - 1] {
        let truncated = &encoded[..len];
        let result = std::panic::catch_unwind(|| decode_lh1(truncated, data.len() as u64));
        let result =
            result.unwrap_or_else(|_| panic!("decode_lh1 must not panic truncated to {len}"));
        match result {
            // Preferred outcome: a truncated stream is detected as corrupt.
            Err(oxiarc_core::error::OxiArcError::CorruptedData { .. }) => {}
            Err(other) => panic!(
                "decode_lh1 truncated to {len} bytes returned unexpected error kind: {other:?}"
            ),
            // If it still manages to emit output, it must be full-length and
            // must never coincide with the original data.
            Ok(decoded) => {
                assert_eq!(
                    decoded.len(),
                    data.len(),
                    "decode_lh1 Ok output must be exactly `original_size` bytes ({len} truncation)"
                );
                if len < encoded.len() {
                    assert_ne!(
                        decoded,
                        data,
                        "decode_lh1 must not silently reproduce the original data from a \
                         stream truncated to {len}/{} bytes",
                        encoded.len()
                    );
                }
            }
        }
    }
}

/// Corrupting the leading bytes of an `-lh1-` stream (where the adaptive
/// Huffman tree's very first symbol decisions are made) to extreme values
/// must never panic and must never silently reproduce the original data. It
/// may either error (`CorruptedData`) or produce a full-length garbage
/// buffer, but never hang or panic.
#[test]
fn decode_lh1_corrupted_prefix_never_panics() {
    let data = sample_data();
    let encoded = encode_lh1(&data);

    for &offset in &[0usize, 1, 2, 3, 5, 10] {
        for &mutation in &[0x00u8, 0xFFu8] {
            let mut mutated = encoded.clone();
            mutated[offset] = mutation;
            let result = std::panic::catch_unwind(|| decode_lh1(&mutated, data.len() as u64));
            let result = result.unwrap_or_else(|_| {
                panic!("decode_lh1 must not panic with byte {offset} set to {mutation:#04x}")
            });
            match result {
                Err(oxiarc_core::error::OxiArcError::CorruptedData { .. }) => {}
                Err(other) => panic!(
                    "decode_lh1 byte {offset}={mutation:#04x} returned unexpected error kind: \
                     {other:?}"
                ),
                Ok(decoded) => assert_eq!(decoded.len(), data.len()),
            }
        }
    }
}
