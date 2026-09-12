//! 13- to 16-bit code widths and the explicit bit order, through every
//! generic entry point.
//!
//! Before 0.4.2 `LzwConfig` capped `max_bits` at 12 in practice (the code
//! table index was a `u16`, so the "exhausted" state 65536 was
//! unrepresentable) and had no bit-order field at all. These tests pin both:
//! the wide widths must fill, reset and round-trip, and the two packings
//! must be genuinely different.

use std::io::{Read, Write};

use oxiarc_lzw::streaming::{LzwStreamDecoder, LzwStreamEncoder, LzwStreamMode};
use oxiarc_lzw::{
    LzwBitOrder, LzwConfig, LzwDecoder, LzwEncoder, compress, decompress, decompress_into,
};

/// Deterministic xorshift bytes.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// The payload shapes the width sweep runs over.
fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0x5A]),
        ("every_byte", (0..=255u8).collect()),
        (
            "text",
            b"the quick brown fox jumps over the lazy dog. ".repeat(200),
        ),
        ("zeros", vec![0u8; 40_000]),
        ("noise", noise(40_000, 0x5EED_1234)),
    ]
}

fn config(max_bits: u8, bit_order: LzwBitOrder) -> LzwConfig {
    LzwConfig::new(9, max_bits)
        .expect("9..=16 is a valid width range")
        .with_bit_order(bit_order)
}

#[test]
fn every_width_and_bit_order_round_trips() {
    let mut cases = 0usize;
    for (name, payload) in payloads() {
        for max_bits in 9u8..=16 {
            for bit_order in [LzwBitOrder::Msb, LzwBitOrder::Lsb] {
                let config = config(max_bits, bit_order);
                let label = format!("{name}/b{max_bits}/{bit_order:?}");

                let stream = compress(&payload, config).unwrap_or_else(|e| panic!("{label}: {e}"));
                let decoded = decompress(&stream, payload.len(), config)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(decoded, payload, "{label}");

                let mut into = vec![0u8; payload.len()];
                let written = decompress_into(&stream, &mut into, config)
                    .unwrap_or_else(|e| panic!("{label}: into: {e}"));
                assert_eq!(written, payload.len(), "{label}");
                assert_eq!(into, payload, "{label}");

                // The stateful encoder/decoder pair must agree with the
                // one-shot helpers.
                let mut encoder = LzwEncoder::new(config).expect("encoder");
                let via_encoder = encoder
                    .encode(&payload)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(via_encoder, stream, "{label}: stateful encode differs");
                let mut decoder = LzwDecoder::new(config).expect("decoder");
                let mut out = vec![0u8; payload.len()];
                let written = decoder
                    .decode_into(&stream, &mut out)
                    .unwrap_or_else(|e| panic!("{label}: decode_into: {e}"));
                assert_eq!(written, payload.len(), "{label}");
                assert_eq!(out, payload, "{label}");

                cases += 1;
            }
        }
    }
    assert_eq!(cases, 6 * 8 * 2);
}

#[test]
fn the_sixteen_bit_table_fills_resets_and_still_round_trips() {
    // 512 KiB of noise makes the encoder allocate far more than the 65 279
    // usable entries of a 16-bit table, so the "table full -> ClearCode ->
    // reset" path really runs. Before 0.4.2 the dictionary's `u16`
    // `next_code` could not represent the exhausted state (65536), so
    // `is_full()` never fired and the counter overflowed instead.
    let payload = noise(512 * 1024, 0xABCD_1234);
    for bit_order in [LzwBitOrder::Msb, LzwBitOrder::Lsb] {
        let config = config(16, bit_order);
        let stream = compress(&payload, config).expect("compress");
        let decoded = decompress(&stream, payload.len(), config).expect("decompress");
        assert_eq!(decoded, payload, "{bit_order:?}");

        let mut into = vec![0u8; payload.len()];
        let written = decompress_into(&stream, &mut into, config).expect("decompress_into");
        assert_eq!(written, payload.len());
        assert_eq!(into, payload, "{bit_order:?}");
    }
}

#[test]
fn a_wider_ceiling_never_costs_ratio_on_repetitive_data() {
    // The point of 13..16-bit codes: the table keeps growing instead of
    // resetting at 4 096 entries. On data with a long-lived dictionary the
    // wider ceiling must produce a strictly smaller stream.
    let mut payload = Vec::new();
    for i in 0..8_000u32 {
        payload
            .extend_from_slice(format!("record,{i:06},alpha,beta,gamma,{}\n", i % 977).as_bytes());
    }
    let narrow = compress(&payload, config(12, LzwBitOrder::Msb)).expect("12-bit");
    let wide = compress(&payload, config(16, LzwBitOrder::Msb)).expect("16-bit");
    println!(
        "{} bytes of records: 12-bit -> {} bytes, 16-bit -> {} bytes",
        payload.len(),
        narrow.len(),
        wide.len()
    );
    assert!(
        wide.len() < narrow.len(),
        "16-bit codes produced {} bytes, 12-bit produced {}",
        wide.len(),
        narrow.len()
    );
    assert_eq!(
        decompress(&wide, payload.len(), config(16, LzwBitOrder::Msb)).expect("round trip"),
        payload
    );
}

#[test]
fn the_two_bit_orders_are_not_interchangeable_at_any_width() {
    let payload = b"bit order matters at every code width".repeat(30);
    for max_bits in 9u8..=16 {
        let msb = config(max_bits, LzwBitOrder::Msb);
        let lsb = config(max_bits, LzwBitOrder::Lsb);
        let msb_stream = compress(&payload, msb).expect("msb");
        let lsb_stream = compress(&payload, lsb).expect("lsb");
        assert_ne!(msb_stream, lsb_stream, "b{max_bits}: same bytes");
        assert_eq!(
            msb_stream.len(),
            lsb_stream.len(),
            "b{max_bits}: only the packing may differ"
        );

        // Cross-decoding must not quietly succeed.
        let mut scratch = vec![0u8; payload.len()];
        if let Ok(n) = decompress_into(&msb_stream, &mut scratch, lsb) {
            assert_ne!(
                &scratch[..n],
                &payload[..n.min(payload.len())],
                "b{max_bits}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming: `LzwStreamMode::Config` is what carries a bit order and a wide
// ceiling through the `Read`/`Write` adapters.
// ---------------------------------------------------------------------------

fn stream_round_trip(mode: LzwStreamMode, payload: &[u8]) -> Vec<u8> {
    let mut encoder = LzwStreamEncoder::new(Vec::new(), mode);
    encoder.write_all(payload).expect("write");
    let compressed = encoder.finish().expect("finish");

    let mut decoder = LzwStreamDecoder::new(&compressed[..], mode);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).expect("read");
    assert_eq!(decoder.mode(), mode);
    out
}

#[test]
fn the_streaming_config_mode_matches_the_tiff_mode_byte_for_byte() {
    let payload = b"streaming through an explicit configuration".repeat(50);

    let mut tiff = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
    tiff.write_all(&payload).expect("write");
    let via_tiff = tiff.finish().expect("finish");

    let mut explicit = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Config(LzwConfig::TIFF));
    explicit.write_all(&payload).expect("write");
    let via_config = explicit.finish().expect("finish");

    assert_eq!(
        via_config, via_tiff,
        "Config(TIFF) must not be a new dialect"
    );
    assert_eq!(
        stream_round_trip(LzwStreamMode::Config(LzwConfig::TIFF), &payload),
        payload
    );
}

#[test]
fn the_streaming_config_mode_honours_the_bit_order_and_wide_widths() {
    let payload = b"lsb-first frames, and 16-bit codes, through std::io".repeat(80);

    let msb = LzwStreamMode::Config(config(16, LzwBitOrder::Msb));
    let lsb = LzwStreamMode::Config(config(16, LzwBitOrder::Lsb));
    assert_eq!(stream_round_trip(msb, &payload), payload);
    assert_eq!(stream_round_trip(lsb, &payload), payload);

    let mut msb_encoder = LzwStreamEncoder::new(Vec::new(), msb);
    msb_encoder.write_all(&payload).expect("write");
    let msb_bytes = msb_encoder.finish().expect("finish");
    let mut lsb_encoder = LzwStreamEncoder::new(Vec::new(), lsb);
    lsb_encoder.write_all(&payload).expect("write");
    let lsb_bytes = lsb_encoder.finish().expect("finish");
    assert_ne!(msb_bytes, lsb_bytes, "the packings must differ");

    // Reading an LSB stream as MSB is an error, not silent garbage: the
    // frame header records the true length, so a mis-decode is caught.
    let mut decoder = LzwStreamDecoder::new(&lsb_bytes[..], msb);
    let mut out = Vec::new();
    assert!(
        decoder.read_to_end(&mut out).is_err(),
        "the wrong bit order must not decode"
    );

    // Every width is usable through the streaming API too.
    for max_bits in 9u8..=16 {
        for bit_order in [LzwBitOrder::Msb, LzwBitOrder::Lsb] {
            let mode = LzwStreamMode::Config(config(max_bits, bit_order));
            assert_eq!(
                stream_round_trip(mode, b"width sweep through the stream adapters"),
                b"width sweep through the stream adapters"
            );
        }
    }
}

#[test]
fn an_invalid_streaming_config_is_an_error_not_a_panic() {
    let broken = LzwConfig {
        min_bits: 9,
        max_bits: 17,
        use_clear_code: true,
        early_change: true,
        bit_order: LzwBitOrder::Msb,
    };
    assert!(broken.validate().is_err(), "the fixture must be invalid");

    let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Config(broken));
    encoder
        .write_all(b"payload")
        .expect("buffered, not encoded yet");
    assert!(
        encoder.finish().is_err(),
        "an invalid config must fail on flush"
    );

    let mut decoder = LzwStreamDecoder::new(&[0u8; 16][..], LzwStreamMode::Config(broken));
    let mut out = Vec::new();
    assert!(decoder.read_to_end(&mut out).is_err());
}

/// A 64-bit xorshift for the robustness sweep below.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn corrupt_input_never_panics_at_any_width_or_bit_order() {
    // Widening `max_bits` to 16 and making the bit order configurable both
    // change how far a code can index into the table, so the malformed-input
    // sweep has to cover every combination — not just the 12-bit MSB one the
    // crate shipped before 0.4.2. Every call is bounded (8 KiB output buffer,
    // or a declared length the decoder clamps).
    let mut configs = vec![
        LzwConfig::TIFF,
        LzwConfig::TIFF_OLD_STYLE,
        LzwConfig::GIF,
        LzwConfig::TIFF_COMPAT_LSB,
    ];
    for max_bits in 9u8..=16 {
        for bit_order in [LzwBitOrder::Msb, LzwBitOrder::Lsb] {
            configs.push(config(max_bits, bit_order));
        }
    }
    assert_eq!(configs.len(), 4 + 8 * 2);

    let seeds: Vec<Vec<u8>> = vec![
        b"alpha beta gamma delta ".repeat(200),
        vec![0u8; 5_000],
        (0..=255u8).cycle().take(6_000).collect(),
    ];
    let mut pool = Vec::new();
    for &cfg in &configs {
        for payload in &seeds {
            pool.push((
                cfg,
                compress(payload, cfg).expect("compress"),
                payload.len(),
            ));
        }
    }

    let mut state = 0xF00D_BAAD_F00D_BAADu64;
    let mut decoded = 0usize;
    for _ in 0..12_000u32 {
        let (cfg, base, len) = &pool[(xorshift(&mut state) as usize) % pool.len()];
        let mut stream = base.clone();
        match xorshift(&mut state) % 4 {
            0 => {
                let at = (xorshift(&mut state) as usize) % stream.len();
                stream[at] ^= 1u8 << (xorshift(&mut state) % 8);
            }
            1 => {
                let keep = (xorshift(&mut state) as usize) % stream.len();
                stream.truncate(keep);
            }
            2 => {
                let at = (xorshift(&mut state) as usize) % stream.len();
                stream.remove(at);
            }
            _ => {
                let at = (xorshift(&mut state) as usize) % stream.len();
                stream.insert(at, (xorshift(&mut state) >> 21) as u8);
            }
        }

        // A lying declared length must not force a large allocation either.
        let declared = if xorshift(&mut state) % 3 == 0 {
            usize::MAX / 2
        } else {
            *len
        };
        if let Ok(out) = decompress(&stream, declared, *cfg) {
            decoded += 1;
            // `expected_size` is a hard ceiling on the output, not a hint:
            // `decode_into_sink` clips each expansion to the sink's
            // remaining space. A corrupt stream may well decode to *more*
            // than the original payload, but never past what was asked for.
            assert!(
                out.len() <= declared,
                "decoded {} bytes under a {declared}-byte ceiling",
                out.len()
            );
            if declared == *len {
                assert!(out.len() <= *len);
            }
        }
        let mut dst = vec![0u8; 8 * 1024];
        if let Ok(written) = decompress_into(&stream, &mut dst, *cfg) {
            assert!(written <= dst.len());
        }
    }
    assert!(
        decoded > 500,
        "only {decoded} mutated streams still decoded"
    );
}
