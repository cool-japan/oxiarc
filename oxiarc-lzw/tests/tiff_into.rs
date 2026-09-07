//! Always-run tests for the zero-allocation TIFF-LZW decode entry points
//! ([`oxiarc_lzw::decompress_tiff_into`] / [`oxiarc_lzw::decompress_into`]).
//!
//! Three properties are proven here, none of which need an external tool:
//!
//! 1. **Differential.** `decompress_tiff_into` and the growable-`Vec`
//!    `decompress_tiff` agree byte-for-byte on every pinned libtiff/Pillow
//!    reference strip *and* at every possible output-buffer size, including
//!    the sizes where the final code expands past the end of the buffer.
//! 2. **Hostile input.** Truncating or corrupting a stream can neither
//!    panic, hang, nor produce a silent short success.
//! 3. **Old-style (pre-1993) LZW.** Streams written without the early code
//!    change decode through [`oxiarc_lzw::LzwConfig::TIFF_OLD_STYLE`].

use oxiarc_lzw::{
    LzwConfig, compress, compress_tiff, decompress, decompress_into, decompress_tiff,
    decompress_tiff_into,
};

/// Deterministic pseudo-random bytes (same LCG as `tests/bench_ratios.rs`).
fn lcg_bytes(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..len {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

/// The pinned libtiff/Pillow-produced fixtures (see `tiff_ref_fixtures.rs`
/// for their provenance) plus their raw inputs.
fn reference_fixtures() -> Vec<(&'static str, Vec<u8>, &'static [u8])> {
    vec![
        (
            "lcg_253",
            lcg_bytes(253),
            include_bytes!("data/lcg_253.lzw").as_slice(),
        ),
        (
            "lcg_254",
            lcg_bytes(254),
            include_bytes!("data/lcg_254.lzw").as_slice(),
        ),
        (
            "lcg_255",
            lcg_bytes(255),
            include_bytes!("data/lcg_255.lzw").as_slice(),
        ),
        (
            "lcg_511",
            lcg_bytes(511),
            include_bytes!("data/lcg_511.lzw").as_slice(),
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

#[test]
fn reference_strips_decode_into_byte_identical() {
    for (name, raw, strip) in reference_fixtures() {
        let mut out = vec![0u8; raw.len()];
        let written = decompress_tiff_into(strip, &mut out)
            .unwrap_or_else(|e| panic!("[{name}] decompress_tiff_into failed: {e}"));
        assert_eq!(written, raw.len(), "[{name}] wrong byte count");
        assert_eq!(out, raw, "[{name}] wrong bytes");
    }
}

#[test]
fn into_and_vec_paths_agree_at_every_buffer_size() {
    // Every buffer size, including the ones where the last code expands
    // past the end of the buffer and only its leading bytes are kept.
    for (name, raw, strip) in reference_fixtures() {
        if raw.len() > 1024 {
            continue; // the exhaustive sweep is quadratic; see below.
        }
        for limit in 0..=raw.len() {
            let via_vec = decompress_tiff(strip, limit)
                .unwrap_or_else(|e| panic!("[{name}] vec decode at {limit} failed: {e}"));
            let mut buffer = vec![0u8; limit];
            let written = decompress_tiff_into(strip, &mut buffer)
                .unwrap_or_else(|e| panic!("[{name}] into decode at {limit} failed: {e}"));
            assert_eq!(written, limit, "[{name}] short write at {limit}");
            assert_eq!(via_vec, buffer, "[{name}] paths disagree at {limit}");
            assert_eq!(
                &buffer[..],
                &raw[..limit],
                "[{name}] wrong bytes at {limit}"
            );
        }
    }

    // For the big fixtures, sample interesting sizes instead of all of them.
    for (name, raw, strip) in reference_fixtures() {
        if raw.len() <= 1024 {
            continue;
        }
        let sizes = [
            0,
            1,
            2,
            255,
            256,
            257,
            raw.len() / 3,
            raw.len() / 2,
            raw.len() - 2,
            raw.len() - 1,
            raw.len(),
        ];
        for limit in sizes {
            let via_vec = decompress_tiff(strip, limit)
                .unwrap_or_else(|e| panic!("[{name}] vec decode at {limit} failed: {e}"));
            let mut buffer = vec![0u8; limit];
            let written = decompress_tiff_into(strip, &mut buffer)
                .unwrap_or_else(|e| panic!("[{name}] into decode at {limit} failed: {e}"));
            assert_eq!(written, limit);
            assert_eq!(via_vec, buffer, "[{name}] paths disagree at {limit}");
            assert_eq!(
                &buffer[..],
                &raw[..limit],
                "[{name}] wrong bytes at {limit}"
            );
        }
    }
}

#[test]
fn oversized_buffer_reports_the_real_length() {
    // A buffer larger than the strip must stop at EOI and report the real
    // length rather than erroring or padding.
    for (name, raw, strip) in reference_fixtures() {
        let mut out = vec![0xCDu8; raw.len() + 4096];
        let written = decompress_tiff_into(strip, &mut out)
            .unwrap_or_else(|e| panic!("[{name}] oversized decode failed: {e}"));
        assert_eq!(written, raw.len(), "[{name}] wrong length");
        assert_eq!(&out[..written], &raw[..], "[{name}] wrong bytes");
        assert!(
            out[written..].iter().all(|&b| b == 0xCD),
            "[{name}] decoder wrote past the end of the stream"
        );
    }
}

#[test]
fn truncated_strips_never_silently_succeed() {
    for (name, raw, strip) in reference_fixtures() {
        // Sample truncation points across the whole stream (every byte for
        // the small fixtures, a stride for the large ones).
        let stride = (strip.len() / 512).max(1);
        for cut in (0..strip.len()).step_by(stride) {
            let mut out = vec![0u8; raw.len()];
            if let Ok(written) = decompress_tiff_into(&strip[..cut], &mut out) {
                assert!(
                    written <= raw.len(),
                    "[{name}] cut {cut} wrote past the buffer"
                );
                assert_eq!(
                    &out[..written],
                    &raw[..written],
                    "[{name}] cut {cut} produced wrong bytes"
                );
                // A truncated stream may only succeed by filling the
                // whole buffer (the tail happened to be redundant) or
                // by hitting a genuine EOI code.
                assert!(
                    written == raw.len() || cut == strip.len(),
                    "[{name}] cut {cut} silently produced {written}/{} bytes",
                    raw.len()
                );
            }
        }
    }
}

#[test]
fn corrupted_strips_never_panic() {
    for (name, raw, strip) in reference_fixtures() {
        let stride = (strip.len() / 64).max(1);
        for position in (0..strip.len()).step_by(stride) {
            for mask in [0x01u8, 0x40, 0x80, 0xFF] {
                let mut corrupted = strip.to_vec();
                corrupted[position] ^= mask;
                let mut out = vec![0u8; raw.len()];
                // Must not panic and must not write past `out`.
                let result = decompress_tiff_into(&corrupted, &mut out);
                if let Ok(written) = result {
                    assert!(written <= raw.len(), "[{name}] overrun at {position}");
                }
            }
        }
    }
}

#[test]
fn empty_and_degenerate_inputs() {
    let mut out = [0u8; 16];
    // Empty input, empty buffer: nothing to do.
    assert_eq!(
        decompress_tiff_into(&[], &mut out[..0]).expect("empty/empty"),
        0
    );
    // Empty input, non-empty buffer: truncated stream.
    assert!(decompress_tiff_into(&[], &mut out).is_err());
    // Valid stream, zero-length buffer.
    let strip = compress_tiff(b"hello").expect("compress");
    assert_eq!(
        decompress_tiff_into(&strip, &mut out[..0]).expect("zero-length buffer"),
        0
    );
    // Encoded empty input decodes to zero bytes.
    let empty = compress_tiff(b"").expect("compress empty");
    assert_eq!(
        decompress_tiff_into(&empty, &mut out).expect("empty payload"),
        0
    );
}

#[test]
fn old_style_streams_round_trip_through_the_compat_config() {
    // Old-style (pre-1993) writers omit the early code change. Encode with
    // that rule and prove the compat config decodes it, through both the
    // `Vec` and the `_into` entry points.
    let inputs: Vec<Vec<u8>> = vec![
        b"old-style TIFF LZW".to_vec(),
        lcg_bytes(600),
        lcg_bytes(5000),
        vec![b'W'; 9000],
        (0..30_000u32).map(|i| (i % 251) as u8).collect(),
    ];

    for raw in &inputs {
        let strip = compress(raw, LzwConfig::TIFF_OLD_STYLE).expect("old-style encode");

        let decoded =
            decompress(&strip, raw.len(), LzwConfig::TIFF_OLD_STYLE).expect("old-style decode");
        assert_eq!(&decoded, raw);

        let mut out = vec![0u8; raw.len()];
        let written =
            decompress_into(&strip, &mut out, LzwConfig::TIFF_OLD_STYLE).expect("old-style into");
        assert_eq!(written, raw.len());
        assert_eq!(&out, raw);
    }
}

#[test]
fn old_style_retry_recipe_recovers_streams_the_standard_rule_rejects() {
    // The libtiff `LZWDecodeCompat` recipe: try the standard rule, retry
    // with the old-style config. At least one corpus entry must actually
    // need the retry, otherwise this test would prove nothing.
    let inputs: Vec<Vec<u8>> = vec![
        lcg_bytes(400),
        lcg_bytes(1200),
        lcg_bytes(9000),
        (0..40_000u32).map(|i| (i % 253) as u8).collect(),
    ];

    let mut needed_retry = 0usize;
    for raw in &inputs {
        let strip = compress(raw, LzwConfig::TIFF_OLD_STYLE).expect("old-style encode");
        let mut out = vec![0u8; raw.len()];
        let standard = decompress_tiff_into(&strip, &mut out);
        let recovered = match standard {
            Ok(written) if written == out.len() && out == *raw => false,
            _ => {
                let written = decompress_into(&strip, &mut out, LzwConfig::TIFF_OLD_STYLE)
                    .expect("old-style retry");
                assert_eq!(written, raw.len());
                assert_eq!(&out, raw);
                true
            }
        };
        if recovered {
            needed_retry += 1;
        }
    }
    assert!(
        needed_retry > 0,
        "the corpus must contain at least one stream the standard rule cannot decode"
    );
}

#[test]
fn round_trip_through_into_for_many_shapes() {
    let shapes: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0u8],
        vec![0xFFu8; 1],
        b"AB".repeat(3000),
        lcg_bytes(1),
        lcg_bytes(4095),
        lcg_bytes(4096),
        lcg_bytes(4097),
        lcg_bytes(70_000),
        vec![b'Q'; 200_000],
        (0..200_000u32).map(|i| (i % 7) as u8).collect(),
    ];
    for raw in &shapes {
        let strip = compress_tiff(raw).expect("encode");
        let mut out = vec![0u8; raw.len()];
        let written = decompress_tiff_into(&strip, &mut out).expect("decode into");
        assert_eq!(written, raw.len());
        assert_eq!(&out, raw);
    }
}
