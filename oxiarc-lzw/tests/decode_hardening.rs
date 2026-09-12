//! Adversarial hardening tests for the shared LZW decode loop.
//!
//! `oxiarc-lzw` 0.4.2 rebuilt the decoder around a packed code table, a
//! stateless four-byte-window code reader and libtiff's entry-creation
//! order. Those changes moved every bound the decoder relies on into
//! arithmetic the optimiser is meant to see through — a masked table index,
//! a `space` counter the loop decrements itself instead of re-querying the
//! sink, a `length` field the emitter trusts instead of walking the chain,
//! and a `repeated` bit that routes a whole string to a `fill` with no
//! chain walk to disagree with it. This suite attacks exactly those bounds.
//!
//! What is covered here that the other always-run suites do not cover:
//!
//! * **Every** output-buffer length on run-heavy and table-filling payloads,
//!   not just the small fixtures — the clipped `repeated` fill and
//!   `write_chain`'s "walk up to the ancestor that fits" are only exercised
//!   when a long string is cut short.
//! * Truncation at every offset (bounded call count on the large fixtures),
//!   single-byte drops and inserts, and bit flips at every byte — on
//!   **libtiff-written** strips, not on this crate's own encoder output.
//! * The GIF codec under the same attacks. Before 0.4.2 `gif_decompress`
//!   was a separate implementation with its own `Vec<u8>`-per-code loop; it
//!   now shares the strip decoder, and nothing pinned its behaviour on
//!   hostile input.
//! * Every dialect and both bit orders through the clipping path.
//!
//! Every test here asserts one of three things and never anything weaker:
//! the call does not panic, it terminates in a bounded number of steps, and
//! any `Ok` it returns is a genuine prefix of the true output.

use oxiarc_lzw::{
    LzwBitOrder, LzwConfig, compress, compress_tiff, decompress, decompress_into, decompress_tiff,
    decompress_tiff_into, gif_compress, gif_decompress,
};

/// Deterministic pseudo-random bytes (same LCG as `tests/bench_ratios.rs`,
/// so the pinned reference fixtures reproduce bit-exactly).
fn lcg_bytes(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..len {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

/// The pinned libtiff/Pillow-produced strips (provenance in
/// `tests/tiff_ref_fixtures.rs`) with the raw bytes they decode to.
fn reference_fixtures() -> Vec<(&'static str, Vec<u8>, &'static [u8])> {
    vec![
        (
            "lcg_253",
            lcg_bytes(253),
            include_bytes!("data/lcg_253.lzw").as_slice(),
        ),
        (
            "lcg_512",
            lcg_bytes(512),
            include_bytes!("data/lcg_512.lzw").as_slice(),
        ),
        (
            "lcg_16384",
            lcg_bytes(16384),
            include_bytes!("data/lcg_16384.lzw").as_slice(),
        ),
        (
            "zeros_65536",
            vec![0u8; 65536],
            include_bytes!("data/zeros_65536.lzw").as_slice(),
        ),
        (
            "allbytes_256",
            (0..=255u8).collect(),
            include_bytes!("data/allbytes_256.lzw").as_slice(),
        ),
        (
            "fox_4500",
            b"The quick brown fox jumps over the lazy dog. ".repeat(100)[..4500].to_vec(),
            include_bytes!("data/fox_4500.lzw").as_slice(),
        ),
    ]
}

/// Payload shapes chosen for the *emitter*, not the encoder: each one drives
/// a different arm of `emit_entry`.
fn emitter_payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        // One 20 000-byte run: every learned entry is `repeated`, so every
        // emit is a `fill` and every clipped emit is a *shortened* fill.
        ("one_long_run", vec![0xA5u8; 20_000]),
        // Runs of every length 1..=200, so strings of every length are both
        // fully emitted and clipped at every offset inside them.
        ("graded_runs", {
            let mut data = Vec::new();
            for run in 1u32..=200 {
                let byte = (run % 251) as u8;
                data.extend(std::iter::repeat_n(byte, run as usize));
            }
            data
        }),
        // Two-byte strings dominate: the `want == 2 && full == 2` fast path
        // that never touches the chain.
        ("alternating_pair", b"AB".repeat(5_000)),
        // Long non-repeating strings: the chain walk, clipped mid-chain.
        ("english_text", {
            b"the quick brown fox jumps over the lazy dog "
                .repeat(500)
                .to_vec()
        }),
        // Fills the 12-bit table several times over, so mid-stream
        // ClearCodes reset the width and the cursor.
        ("table_filling_noise", lcg_bytes(30_000)),
        // A run that straddles the table-full ClearCode.
        ("run_then_noise", {
            let mut data = vec![7u8; 12_000];
            data.extend(lcg_bytes(12_000));
            data
        }),
    ]
}

/// Offsets to attack in a stream, capped so the suite stays quick: every
/// offset when the stream is small, otherwise every offset in the first and
/// last 96 bytes plus an even stride through the middle.
fn attack_offsets(len: usize) -> Vec<usize> {
    if len <= 1_200 {
        return (0..len).collect();
    }
    let mut offsets: Vec<usize> = (0..96.min(len)).collect();
    let stride = (len / 900).max(1);
    offsets.extend((96..len.saturating_sub(96)).step_by(stride));
    offsets.extend(len.saturating_sub(96)..len);
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

// ---------------------------------------------------------------------------
// 1. Clipping: every output length must yield the true prefix
// ---------------------------------------------------------------------------

/// The emitter clips a string to the space left, keeping its **leading**
/// bytes. Three separate code paths do that — the `repeated` fill, the
/// two-byte pair and `write_chain`'s "walk up to the ancestor that fits" —
/// and only the last of them has a chain to disagree with itself. Every
/// output length on every payload shape must therefore be the exact prefix.
#[test]
fn every_output_length_yields_the_true_prefix() {
    for (name, raw) in emitter_payloads() {
        let strip = compress_tiff(&raw).unwrap_or_else(|e| panic!("[{name}] encode failed: {e}"));

        // Every length up to 2 048 (dense, covers all short strings), then a
        // stride, then the last few — the interesting clip points are the
        // ones just inside a long string.
        let mut limits: Vec<usize> = (0..=2_048.min(raw.len())).collect();
        limits.extend((2_048..raw.len()).step_by(97));
        limits.extend(raw.len().saturating_sub(64)..=raw.len());
        limits.sort_unstable();
        limits.dedup();

        for limit in limits {
            let mut buffer = vec![0u8; limit];
            let written = decompress_tiff_into(&strip, &mut buffer)
                .unwrap_or_else(|e| panic!("[{name}] into decode at {limit} failed: {e}"));
            assert_eq!(written, limit, "[{name}] short write at {limit}");
            assert_eq!(
                &buffer[..],
                &raw[..limit],
                "[{name}] clipped decode at {limit} is not the true prefix"
            );

            let via_vec = decompress_tiff(&strip, limit)
                .unwrap_or_else(|e| panic!("[{name}] vec decode at {limit} failed: {e}"));
            assert_eq!(
                via_vec, buffer,
                "[{name}] the Vec and slice sinks disagree at {limit}"
            );
        }
    }
}

/// The same property on **libtiff-written** strips, at every length, for the
/// two fixtures the existing suite skips because its sweep is quadratic
/// (`tiff_into.rs` samples eleven sizes above 1 KiB). `zeros_65536` is the
/// pure-run fixture — the one whose every emit is a clipped `fill`.
#[test]
fn every_output_length_yields_the_true_prefix_on_reference_strips() {
    for (name, raw, strip) in reference_fixtures() {
        let mut limits: Vec<usize> = (0..=1_024.min(raw.len())).collect();
        limits.extend((0..raw.len()).step_by((raw.len() / 700).max(1)));
        limits.extend(raw.len().saturating_sub(64)..=raw.len());
        limits.sort_unstable();
        limits.dedup();

        for limit in limits {
            let mut buffer = vec![0u8; limit];
            let written = decompress_tiff_into(strip, &mut buffer)
                .unwrap_or_else(|e| panic!("[{name}] decode at {limit} failed: {e}"));
            assert_eq!(written, limit, "[{name}] short write at {limit}");
            assert_eq!(
                &buffer[..],
                &raw[..limit],
                "[{name}] clipped decode at {limit} is not the true prefix"
            );
        }
    }
}

/// Clipping must behave identically in every dialect and both bit orders —
/// the width rule and the packing are const-generic parameters of the decode
/// loop, so each combination is a separately compiled body.
#[test]
fn clipping_is_identical_in_every_dialect_and_bit_order() {
    let configs = [
        ("TIFF", LzwConfig::TIFF),
        ("TIFF_OLD_STYLE", LzwConfig::TIFF_OLD_STYLE),
        ("TIFF_COMPAT_LSB", LzwConfig::TIFF_COMPAT_LSB),
        ("GIF", LzwConfig::GIF),
        (
            "wide_16_msb",
            LzwConfig::new(9, 16).expect("9-16 is a valid width range"),
        ),
        (
            "wide_16_lsb",
            LzwConfig::new(9, 16)
                .expect("9-16 is a valid width range")
                .with_bit_order(LzwBitOrder::Lsb),
        ),
        // A fixed-width configuration: `min_bits == max_bits` makes the
        // decode loop's growth threshold `u32::MAX` from the first code, so
        // the width bookkeeping is a distinct path from every dialect above.
        (
            "fixed_12",
            LzwConfig::new(12, 12).expect("12-12 is a valid width range"),
        ),
        (
            "fixed_9",
            LzwConfig::new(9, 9).expect("9-9 is a valid width range"),
        ),
    ];
    let payloads = [
        ("one_long_run", vec![0x5Au8; 6_000]),
        ("graded_runs", {
            let mut data = Vec::new();
            for run in 1u32..=120 {
                data.extend(std::iter::repeat_n((run % 251) as u8, run as usize));
            }
            data
        }),
        ("noise", lcg_bytes(6_000)),
    ];

    for (config_name, config) in configs {
        for (payload_name, raw) in &payloads {
            let stream = compress(raw, config)
                .unwrap_or_else(|e| panic!("[{config_name}/{payload_name}] encode failed: {e}"));
            let mut limits: Vec<usize> = (0..=512.min(raw.len())).collect();
            limits.extend((0..raw.len()).step_by(37));
            limits.push(raw.len());
            limits.sort_unstable();
            limits.dedup();

            for limit in limits {
                let mut buffer = vec![0u8; limit];
                let written = decompress_into(&stream, &mut buffer, config).unwrap_or_else(|e| {
                    panic!("[{config_name}/{payload_name}] decode at {limit} failed: {e}")
                });
                assert_eq!(
                    written, limit,
                    "[{config_name}/{payload_name}] short write at {limit}"
                );
                assert_eq!(
                    &buffer[..],
                    &raw[..limit],
                    "[{config_name}/{payload_name}] wrong prefix at {limit}"
                );
                let via_vec = decompress(&stream, limit, config).unwrap_or_else(|e| {
                    panic!("[{config_name}/{payload_name}] vec decode at {limit} failed: {e}")
                });
                assert_eq!(
                    via_vec, buffer,
                    "[{config_name}/{payload_name}] sinks disagree at {limit}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Truncation, drops, inserts and bit flips on libtiff-written strips
// ---------------------------------------------------------------------------

/// Truncating a real libtiff strip at (almost) every offset. The forbidden
/// outcome is a silent partial success: an `Ok` shorter than the buffer that
/// the caller cannot distinguish from a complete decode.
#[test]
fn truncation_at_every_offset_never_panics_or_silently_corrupts() {
    for (name, raw, strip) in reference_fixtures() {
        for cut in attack_offsets(strip.len()) {
            let mut out = vec![0u8; raw.len()];
            match decompress_tiff_into(&strip[..cut], &mut out) {
                Err(_) => {}
                Ok(written) => {
                    assert!(
                        written <= raw.len(),
                        "[{name}] cut {cut}: wrote {written} into a {}-byte buffer",
                        raw.len()
                    );
                    assert_eq!(
                        &out[..written],
                        &raw[..written],
                        "[{name}] cut {cut}: decoded bytes are not a prefix of the truth"
                    );
                    assert_eq!(
                        written,
                        raw.len(),
                        "[{name}] cut {cut}: silent short success ({written} bytes)"
                    );
                }
            }
            // The growable sink must agree about success and about bytes.
            match decompress_tiff(&strip[..cut], raw.len()) {
                Err(_) => {}
                Ok(decoded) => {
                    assert!(
                        raw.starts_with(&decoded),
                        "[{name}] cut {cut}: Vec sink produced non-prefix bytes"
                    );
                }
            }
        }
    }
}

/// Dropping one byte and inserting one byte shift every subsequent code by
/// eight bits, which is the most effective way to produce codes the table
/// has never seen and lengths the chain cannot support.
#[test]
fn single_byte_drops_and_inserts_never_panic() {
    for (name, raw, strip) in reference_fixtures() {
        for position in attack_offsets(strip.len()) {
            let mut dropped = strip.to_vec();
            dropped.remove(position);
            let mut out = vec![0u8; raw.len()];
            if let Ok(written) = decompress_tiff_into(&dropped, &mut out) {
                assert!(written <= raw.len(), "[{name}] drop at {position} overran");
            }

            for filler in [0x00u8, 0xFF] {
                let mut inserted = strip.to_vec();
                inserted.insert(position, filler);
                let mut out = vec![0u8; raw.len()];
                if let Ok(written) = decompress_tiff_into(&inserted, &mut out) {
                    assert!(
                        written <= raw.len(),
                        "[{name}] insert {filler:#04x} at {position} overran"
                    );
                }
            }
        }
    }
}

/// A bit flip at every byte of every fixture, in four positions per byte.
/// TIFF LZW has no per-strip checksum, so wrong bytes are permitted; a
/// panic, a hang or a write past the caller's buffer are not.
#[test]
fn bit_flips_at_every_byte_never_panic_or_overrun() {
    for (name, raw, strip) in reference_fixtures() {
        for position in attack_offsets(strip.len()) {
            for mask in [0x01u8, 0x08, 0x40, 0x80] {
                let mut corrupted = strip.to_vec();
                corrupted[position] ^= mask;
                let mut out = vec![0xEEu8; raw.len() + 32];
                if let Ok(written) = decompress_tiff_into(&corrupted, &mut out) {
                    assert!(
                        written <= raw.len() + 32,
                        "[{name}] flip {mask:#04x}@{position} reported {written} bytes"
                    );
                    assert!(
                        out[written..].iter().all(|&byte| byte == 0xEE),
                        "[{name}] flip {mask:#04x}@{position} wrote past its own report"
                    );
                }
            }
        }
    }
}

/// A one-byte output buffer, for every stream shape: the emitter's
/// `want == 1` arm, which reads the entry's `first` byte instead of walking.
#[test]
fn one_byte_output_buffers_decode_exactly_the_first_byte() {
    for (name, raw, strip) in reference_fixtures() {
        let mut one = [0u8; 1];
        let written = decompress_tiff_into(strip, &mut one)
            .unwrap_or_else(|e| panic!("[{name}] one-byte decode failed: {e}"));
        assert_eq!(written, 1, "[{name}]");
        assert_eq!(one[0], raw[0], "[{name}] wrong first byte");
    }
    for (name, raw) in emitter_payloads() {
        let strip = compress_tiff(&raw).unwrap_or_else(|e| panic!("[{name}] encode failed: {e}"));
        let mut one = [0u8; 1];
        let written = decompress_tiff_into(&strip, &mut one)
            .unwrap_or_else(|e| panic!("[{name}] one-byte decode failed: {e}"));
        assert_eq!(written, 1, "[{name}]");
        assert_eq!(one[0], raw[0], "[{name}] wrong first byte");
    }
}

/// Random splices of a valid stream: a prefix of one fixture followed by a
/// suffix of another, which keeps the leading ClearCode (so decoding starts)
/// and then feeds codes from a table that was never built.
#[test]
fn spliced_streams_never_panic_or_overrun() {
    let fixtures = reference_fixtures();
    for (left_name, _, left) in &fixtures {
        for (right_name, right_raw, right) in &fixtures {
            for split in [1usize, 2, 3, 7, 16, 33, 64, 129] {
                if split >= left.len() || split >= right.len() {
                    continue;
                }
                let mut spliced = left[..split].to_vec();
                spliced.extend_from_slice(&right[split..]);
                let mut out = vec![0u8; right_raw.len().min(70_000)];
                if let Ok(written) = decompress_tiff_into(&spliced, &mut out) {
                    assert!(
                        written <= out.len(),
                        "[{left_name}+{right_name}@{split}] overran"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. The GIF codec under the same attacks
// ---------------------------------------------------------------------------

/// `gif_decompress` was rewritten onto the shared decode loop in 0.4.2 and
/// had no hostile-input coverage at all. Its documented contract is that the
/// input running out is a *normal* end of stream, so truncation must return
/// a prefix of the true output — never an error, never a panic, never bytes
/// that are not a prefix.
#[test]
fn gif_truncation_at_every_offset_returns_a_prefix() {
    let payloads: Vec<(&str, Vec<u8>, u8)> = vec![
        ("run_8", vec![0x33u8; 8_000], 8),
        ("text_8", b"the quick brown fox ".repeat(300).to_vec(), 8),
        ("noise_8", lcg_bytes(6_000), 8),
        ("nibbles_4", (0..16u8).cycle().take(4_000).collect(), 4),
        ("pairs_2", (0..4u8).cycle().take(3_000).collect(), 2),
        (
            "wide_11",
            (0..2048u16).map(|value| (value % 251) as u8).collect(),
            11,
        ),
    ];

    for (name, raw, minimum_code_size) in payloads {
        let stream = gif_compress(&raw, minimum_code_size)
            .unwrap_or_else(|e| panic!("[{name}] gif_compress failed: {e}"));
        let full = gif_decompress(&stream, minimum_code_size)
            .unwrap_or_else(|e| panic!("[{name}] gif_decompress failed: {e}"));
        assert_eq!(full, raw, "[{name}] round trip");

        for cut in attack_offsets(stream.len()) {
            match gif_decompress(&stream[..cut], minimum_code_size) {
                Err(_) => {}
                Ok(decoded) => {
                    assert!(
                        decoded.len() <= raw.len(),
                        "[{name}] cut {cut}: {} bytes from a {}-byte payload",
                        decoded.len(),
                        raw.len()
                    );
                    assert!(
                        raw.starts_with(&decoded),
                        "[{name}] cut {cut}: truncated GIF data decoded to non-prefix bytes"
                    );
                }
            }
        }
    }
}

/// Bit flips, drops and inserts in GIF image data. Output is bounded by the
/// stream, so a bit flip may change the bytes but can never make the decoder
/// run away: the 12-bit table caps a single string at 4 094 bytes and a
/// stream of `n` bytes carries fewer than `8n/3` codes.
#[test]
fn gif_corruption_never_panics_and_stays_bounded() {
    let text = b"the quick brown fox jumps over the lazy dog ".repeat(200);
    for minimum_code_size in 2u8..=11 {
        // Every byte must be a valid colour index for this code size, i.e.
        // below `1 << minimum_code_size`; anything else is not GIF image
        // data and `gif_compress` rightly rejects it.
        let modulus = 1u16 << minimum_code_size;
        let raw: Vec<u8> = text
            .iter()
            .map(|&byte| (u16::from(byte) % modulus) as u8)
            .collect();
        let stream = gif_compress(&raw, minimum_code_size)
            .unwrap_or_else(|e| panic!("[mcs {minimum_code_size}] gif_compress failed: {e}"));
        let ceiling = 4_096usize * (stream.len() * 8 / 3 + 8);

        for position in attack_offsets(stream.len()) {
            for mask in [0x01u8, 0x80] {
                let mut corrupted = stream.clone();
                corrupted[position] ^= mask;
                if let Ok(decoded) = gif_decompress(&corrupted, minimum_code_size) {
                    assert!(
                        decoded.len() <= ceiling,
                        "[mcs {minimum_code_size}] flip {mask:#04x}@{position} produced \
                         {} bytes, above the {ceiling}-byte structural ceiling",
                        decoded.len()
                    );
                }
            }

            let mut dropped = stream.clone();
            dropped.remove(position);
            if let Ok(decoded) = gif_decompress(&dropped, minimum_code_size) {
                assert!(decoded.len() <= ceiling, "[mcs {minimum_code_size}] drop");
            }

            let mut inserted = stream.clone();
            inserted.insert(position, 0xA7);
            if let Ok(decoded) = gif_decompress(&inserted, minimum_code_size) {
                assert!(decoded.len() <= ceiling, "[mcs {minimum_code_size}] insert");
            }
        }
    }
}

/// Arbitrary bytes as GIF image data, at every minimum code size: no panic,
/// bounded output, and no accepted `minimum_code_size` outside 2..=11.
#[test]
fn arbitrary_bytes_as_gif_data_never_panic() {
    for minimum_code_size in 0u8..=13 {
        let valid = (2..=11).contains(&minimum_code_size);
        for length in [0usize, 1, 2, 3, 5, 9, 17, 64, 255, 1_000] {
            let data = lcg_bytes(length);
            match gif_decompress(&data, minimum_code_size) {
                Ok(decoded) => {
                    assert!(valid, "mcs {minimum_code_size} must be rejected");
                    // Fewer than 8n/3 codes, each at most 4 094 bytes.
                    assert!(
                        decoded.len() <= 4_096 * (length * 8 / 3 + 8),
                        "mcs {minimum_code_size} len {length}: unbounded expansion"
                    );
                }
                Err(_) => {
                    // An error is always acceptable for random bytes; only
                    // an out-of-range code size is *required* to error.
                }
            }
            if !valid {
                assert!(
                    gif_decompress(&data, minimum_code_size).is_err(),
                    "mcs {minimum_code_size} must be rejected"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Degenerate framing
// ---------------------------------------------------------------------------

/// A stream that is nothing but ClearCodes, and one that is nothing but
/// EOI: the reserved-code arm of the loop, which has to reset the width, the
/// mask, the cursor and the "no previous code" sentinel together.
#[test]
fn reserved_code_only_streams_terminate() {
    // 9-bit MSB ClearCode (256) repeated: 0x80 0x40 0x20 0x10 ... in
    // practice just feed a byte pattern that decodes to 256 forever.
    // 256 = 1_0000_0000, so 0x80,0x40,0x20,0x10,0x08,0x04,0x02,0x01,0x00
    // is nine ClearCodes in nine bytes (72 bits / 9 bits).
    let clears = [0x80u8, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01, 0x00];
    let mut stream = Vec::new();
    for _ in 0..1_000 {
        stream.extend_from_slice(&clears);
    }
    let mut out = vec![0u8; 4_096];
    // Never terminates with output; must report a truncated stream rather
    // than looping or panicking.
    assert!(
        decompress_tiff_into(&stream, &mut out).is_err(),
        "a ClearCode-only stream produced output"
    );
    assert_eq!(
        decompress_tiff_into(&stream, &mut []).expect("zero-length buffer"),
        0
    );

    // EOI (257) as the first code: 1_0000_0001 -> 0x80, 0x80.
    let eoi_first = [0x80u8, 0x80, 0x00, 0x00];
    let mut out = vec![0xAAu8; 64];
    let written = decompress_tiff_into(&eoi_first, &mut out).expect("EOI-first is a valid stream");
    assert_eq!(written, 0, "EOI as the first code decodes to nothing");
    assert!(out.iter().all(|&byte| byte == 0xAA));
}

/// An enormous `expected_size` with a tiny stream must return the real
/// length, not pad, not error and not allocate for the declared size (the
/// allocation bound itself is pinned in `tests/decode_memory.rs`).
#[test]
fn a_huge_expected_size_returns_the_real_length() {
    for (name, raw, strip) in reference_fixtures() {
        let decoded = decompress_tiff(strip, usize::MAX / 2)
            .unwrap_or_else(|e| panic!("[{name}] huge expected_size failed: {e}"));
        assert_eq!(
            decoded, raw,
            "[{name}] wrong bytes with a huge declared size"
        );
    }
}

/// A first code that names an entry the table does not have must be an
/// error, at every dialect — not a zero-length entry silently emitted.
#[test]
fn a_first_code_above_the_table_is_rejected() {
    // 9-bit MSB: code 258 (the first learned slot) as the very first code,
    // with no ClearCode before it. 258 = 1_0000_0010 -> 0x81, 0x00.
    let stream = [0x81u8, 0x00, 0x00, 0x00];
    let mut out = vec![0u8; 64];
    assert!(
        decompress_tiff_into(&stream, &mut out).is_err(),
        "a code above the table decoded successfully"
    );

    // The same after a ClearCode: 256 then 258.
    // 1_0000_0000 1_0000_0010 -> 0x80 0x40 0x80 0x00 ...
    let mut bits = String::new();
    bits.push_str("100000000");
    bits.push_str("100000010");
    let mut stream = Vec::new();
    let mut accumulator = 0u16;
    let mut held = 0u32;
    for bit in bits.chars() {
        accumulator = (accumulator << 1) | u16::from(bit == '1');
        held += 1;
        if held == 8 {
            stream.push(accumulator as u8);
            accumulator = 0;
            held = 0;
        }
    }
    if held > 0 {
        stream.push(((accumulator as u32) << (8 - held)) as u8);
    }
    stream.extend_from_slice(&[0u8; 4]);
    assert!(
        decompress_tiff_into(&stream, &mut out).is_err(),
        "a code above the table after a reset decoded successfully"
    );
}
