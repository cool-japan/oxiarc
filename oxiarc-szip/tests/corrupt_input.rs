//! Corrupt/truncated-bitstream tests for the AEC/SZIP decoder.
//!
//! `oxiarc_szip::decode`'s only existing adversarial-input coverage
//! (`tests/proptest_roundtrip.rs::decode_never_panics`) throws fully random
//! bytes at arbitrary sample counts. It never exercises the much more
//! targeted scenario of a *valid, known-good* encoded buffer that has then
//! been truncated or bit-flipped -- i.e. a bitstream that starts out
//! well-formed and becomes corrupted partway through, which is the shape
//! real-world data corruption (disk/network truncation, single-bit flips)
//! actually takes. It also never covers "valid params, corrupted bitstream"
//! at all -- only invalid *params* are exercised elsewhere.
//!
//! Unlike `oxiarc-lzhuf` (which deliberately zero-pads past end-of-stream,
//! per LHA's classic `fillbuf` convention, and therefore silently produces
//! wrong-length output on truncation instead of erroring), SZIP's
//! `oxiarc_szip::bitreader::BitReader` treats running out of input as a hard
//! `UnexpectedEof` error. So for this codec, truncation of a genuine encoded
//! stream reliably surfaces as `Err`, which is the behaviour asserted below.

use oxiarc_szip::{SzipError, SzipParams, decode, encode_bytes};

/// Byte-oriented (bpp = 8) parameters sized to the given sample count,
/// mirroring the helper in `tests/proptest_roundtrip.rs`.
fn byte_params(samples: usize) -> SzipParams {
    SzipParams {
        samples,
        ..SzipParams::default()
    }
}

/// Deterministic pseudo-random byte source (xorshift64), so mutation tests
/// are fully reproducible without pulling in a `rand` dependency.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
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

/// A representative, non-trivial sample buffer that compresses to a
/// multi-block, multi-RSI stream (2000 samples, several reference-sample
/// intervals at the default `pixels_per_block = 8`).
fn sample_data() -> Vec<u8> {
    (0..2_000u32)
        .map(|i| ((i * 91 + i / 7) % 256) as u8)
        .collect()
}

/// Truncating a known-good stream at any prefix length (including all but
/// its very last byte) must return `Err`, never `Ok` with garbage/partial
/// data and never panic.
#[test]
fn truncated_prefix_returns_err_not_panic() {
    let data = sample_data();
    let params = byte_params(data.len());
    let encoded = encode_bytes(&data, &params).expect("szip encode must not fail");
    assert!(
        encoded.len() > 64,
        "test fixture must produce a non-trivial stream to truncate meaningfully"
    );

    // A representative sample of offsets: the empty stream, a handful of
    // very short prefixes (cutting off mid reference-sample / mid k-split
    // field), a few "mid literal run" offsets, and one byte short of the
    // full stream (cutting off only the final byte).
    let mut offsets: Vec<usize> = vec![0, 1, 2, 3, 4, 5, 6, 8, 10, 16, 32, 64];
    offsets.push(encoded.len() / 4);
    offsets.push(encoded.len() / 2);
    offsets.push((encoded.len() * 3) / 4);
    offsets.push(encoded.len() - 1);

    for offset in offsets {
        let truncated = &encoded[..offset];
        let result = std::panic::catch_unwind(|| decode(truncated, &params));
        let result = result.unwrap_or_else(|_| {
            panic!("decode() must not panic on a stream truncated to {offset} bytes")
        });
        match result {
            Err(SzipError::UnexpectedEof { .. }) => {}
            Err(other) => {
                panic!("expected UnexpectedEof truncating to {offset} bytes, got {other:?}")
            }
            Ok(v) => panic!(
                "decode() unexpectedly succeeded on a stream truncated to {offset}/{} \
                 bytes (produced {} bytes); truncated SZIP input should be rejected",
                encoded.len(),
                v.len()
            ),
        }
    }
}

/// Truncating mid-way through a single reference-sample-interval block (i.e.
/// well inside the "literal run" of coded residuals, not at a block/RSI
/// boundary) must also fail gracefully rather than return incomplete data.
#[test]
fn truncated_mid_block_returns_err() {
    let data = sample_data();
    let params = byte_params(data.len());
    let encoded = encode_bytes(&data, &params).expect("szip encode must not fail");

    // A third of the way through, plus a small odd offset so we land inside
    // a block rather than exactly on its start.
    let mid_block_offset = (encoded.len() / 3) + 7;
    assert!(mid_block_offset < encoded.len());

    let truncated = &encoded[..mid_block_offset];
    match decode(truncated, &params) {
        Err(SzipError::UnexpectedEof { .. }) => {}
        Err(other) => panic!("expected UnexpectedEof, got {other:?}"),
        Ok(v) => panic!(
            "decode() unexpectedly succeeded on mid-block-truncated input (produced {} bytes)",
            v.len()
        ),
    }
}

/// Single-bit mutations of a valid stream must never panic. SZIP's
/// no-compression option layout means most single-bit flips just perturb
/// one decoded sample value rather than the stream's length/framing, so we
/// don't assert `Err` here (many flips legitimately still decode, just to
/// the "wrong" value) -- only that the decoder never panics on out-of-range
/// shifts/indices caused by a flipped bit.
#[test]
fn bit_flip_mutations_never_panic() {
    let data = sample_data();
    let params = byte_params(data.len());
    let encoded = encode_bytes(&data, &params).expect("szip encode must not fail");

    let mut rng = Xorshift64::new(0xC0FF_EE15_5211_2024);
    let total_bits = encoded.len() * 8;
    for _ in 0..500 {
        let bit = (rng.next_u64() as usize) % total_bits;
        let mut mutated = encoded.clone();
        mutated[bit / 8] ^= 1 << (7 - (bit % 8));

        let result = std::panic::catch_unwind(|| decode(&mutated, &params));
        assert!(
            result.is_ok(),
            "decode() must not panic on single-bit-flip mutation at bit {bit}"
        );
    }
}

/// Denser multi-bit corruption (several flipped bits per trial, still on an
/// otherwise-valid stream) must also never panic. `encode_bytes` always uses
/// the fixed-width "no compression" option (see its doc comment), so most
/// bit flips just perturb a sample value in place without changing how many
/// bits any given block consumes -- meaning `Err` is *not* guaranteed here
/// (unlike straight truncation, which always is; see
/// `truncated_prefix_returns_err_not_panic`). What must hold unconditionally
/// is that the decoder only ever returns `Ok` or a well-typed `Err`, and
/// never panics on the out-of-range shifts/indices dense corruption could in
/// principle trigger.
#[test]
fn multi_bit_flip_mutations_never_panic() {
    let data = sample_data();
    let params = byte_params(data.len());
    let encoded = encode_bytes(&data, &params).expect("szip encode must not fail");

    let mut rng = Xorshift64::new(0x5EED_1234_ABCD_EF01);
    let total_bits = encoded.len() * 8;
    for _ in 0..500 {
        let mut mutated = encoded.clone();
        let flips = 1 + (rng.next_u64() as usize % 12);
        for _ in 0..flips {
            let bit = (rng.next_u64() as usize) % total_bits;
            mutated[bit / 8] ^= 1 << (7 - (bit % 8));
        }

        let result = std::panic::catch_unwind(|| decode(&mutated, &params));
        let _ = result.expect("decode() must not panic on multi-bit-flip mutation");
    }
}

/// A stream that's a single opaque byte (far too short to contain even one
/// full reference sample at 8 bits per pixel) must error, not panic.
#[test]
fn single_byte_stream_returns_err() {
    let params = byte_params(64);
    for byte in 0u8..=255 {
        let result = std::panic::catch_unwind(|| decode(&[byte], &params));
        let result =
            result.unwrap_or_else(|_| panic!("decode() must not panic on single byte {byte:#04x}"));
        assert!(
            result.is_err(),
            "expected Err decoding a lone byte {byte:#04x} as 64 samples, got Ok"
        );
    }
}

/// Empty input requesting a non-zero sample count must error, not panic or
/// hang.
#[test]
fn empty_stream_nonzero_samples_returns_err() {
    let params = byte_params(128);
    let result = decode(&[], &params);
    assert!(
        matches!(result, Err(SzipError::UnexpectedEof { .. })),
        "expected UnexpectedEof decoding an empty stream as 128 samples, got {result:?}"
    );
}
