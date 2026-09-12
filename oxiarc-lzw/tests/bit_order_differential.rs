//! Differential tests for [`LzwBitOrder`] against `weezl`, an independent
//! Rust LZW implementation.
//!
//! `LzwConfig::bit_order` is new in 0.4.2 and it is the one part of the
//! generic engine that had no external witness: the TIFF (MSB) path is
//! covered by the Pillow/libtiff `tiff-oracle`, and the `.Z` (LSB) path by
//! the `compress(1)` `z-oracle`, but nothing checked
//! `LzwConfig::GIF` / `LzwConfig::TIFF_COMPAT_LSB` — the LSB-first *generic*
//! configurations — against anything but this crate itself. A self-consistent
//! round trip proves nothing about bit order: swap both ends and it still
//! passes.
//!
//! `weezl` is already a dev-dependency (it is the comparison baseline in
//! `benches/lzw_bench.rs`) and it models exactly the two dimensions that
//! matter here:
//!
//! | this crate | `weezl` decoder |
//! |---|---|
//! | [`LzwConfig::TIFF`] (MSB, early change) | `with_tiff_size_switch(Msb, 8)` |
//! | [`LzwConfig::TIFF_OLD_STYLE`] (MSB, late change) | `new(Msb, 8)` |
//! | [`LzwConfig::GIF`] (LSB, late change) | `new(Lsb, 8)` |
//! | [`LzwConfig::TIFF_COMPAT_LSB`] (LSB, late change) | `new(Lsb, 8)` |
//!
//! These tests always run — `weezl` is a library, not a system tool, so
//! there is nothing to skip.

use oxiarc_lzw::{LzwConfig, compress, compress_tiff, decompress, gif_compress};
use weezl::BitOrder;
use weezl::decode::Decoder as WeezlDecoder;
use weezl::encode::Encoder as WeezlEncoder;

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

/// Records: a shape whose dictionary long outlives the 4 096-entry table, so
/// the reset path runs on both sides.
fn records(rows: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..rows {
        out.extend_from_slice(format!("rec,{i:06},alpha,beta,gamma,{}\n", i % 977).as_bytes());
    }
    out
}

/// Payload shapes. The first four never fill the table; the last three do.
fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("one_byte", vec![0x07]),
        ("every_byte", (0..=255u8).collect()),
        (
            "text",
            b"the quick brown fox jumps over the lazy dog. ".repeat(60),
        ),
        ("zeros", vec![0u8; 3_000]),
        ("noise", noise(3_000, 0x1234_5678)),
        (
            "big_text",
            b"lorem ipsum dolor sit amet consectetur ".repeat(4_000),
        ),
        ("records", records(20_000)),
    ]
}

/// The shapes small enough that neither encoder ever fills the table.
const SMALL_SHAPES: usize = 5;

/// `weezl`'s decoder for one of this crate's configurations.
fn weezl_decode(config: LzwConfig, stream: &[u8]) -> Result<Vec<u8>, String> {
    let order = match config.bit_order {
        oxiarc_lzw::LzwBitOrder::Msb => BitOrder::Msb,
        _ => BitOrder::Lsb,
    };
    let mut decoder = if config.early_change {
        WeezlDecoder::with_tiff_size_switch(order, 8)
    } else {
        WeezlDecoder::new(order, 8)
    };
    decoder.decode(stream).map_err(|error| error.to_string())
}

#[test]
fn weezl_decodes_every_configuration_this_crate_writes() {
    let mut checked = 0usize;
    for (name, payload) in payloads() {
        // `compress_tiff` is `LzwConfig::TIFF`; spell both out so a change to
        // either shows up here.
        let tiff = compress_tiff(&payload).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            tiff,
            compress(&payload, LzwConfig::TIFF).unwrap_or_else(|e| panic!("{name}: {e}"))
        );

        for (label, config) in [
            ("TIFF", LzwConfig::TIFF),
            ("TIFF_OLD_STYLE", LzwConfig::TIFF_OLD_STYLE),
            ("GIF", LzwConfig::GIF),
            ("TIFF_COMPAT_LSB", LzwConfig::TIFF_COMPAT_LSB),
        ] {
            let stream =
                compress(&payload, config).unwrap_or_else(|e| panic!("{name}/{label}: {e}"));
            let decoded = weezl_decode(config, &stream)
                .unwrap_or_else(|e| panic!("{name}/{label}: weezl rejected the stream: {e}"));
            assert_eq!(
                decoded, payload,
                "{name}/{label}: weezl decoded differently"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, payloads().len() * 4);
    println!("weezl accepted {checked} streams across four configurations");
}

#[test]
fn this_crate_decodes_everything_weezl_writes() {
    let mut checked = 0usize;
    for (name, payload) in payloads() {
        for (label, order, config) in [
            ("msb", BitOrder::Msb, LzwConfig::TIFF_OLD_STYLE),
            ("lsb", BitOrder::Lsb, LzwConfig::GIF),
        ] {
            let stream = WeezlEncoder::new(order, 8)
                .encode(&payload)
                .unwrap_or_else(|e| panic!("{name}/{label}: weezl encode: {e}"));
            let decoded = decompress(&stream, payload.len(), config)
                .unwrap_or_else(|e| panic!("{name}/{label}: {e}"));
            assert_eq!(decoded, payload, "{name}/{label}");
            checked += 1;
        }
    }
    assert_eq!(checked, payloads().len() * 2);
    println!("this crate decoded {checked} weezl streams");
}

#[test]
fn the_bit_order_field_is_externally_observable() {
    // The point of the field: an MSB stream is not an LSB stream. Checked
    // against `weezl` rather than against this crate's own reader, so a
    // symmetrical mistake at both ends cannot hide.
    let payload = b"bit order is not an internal detail".repeat(40);

    let msb = compress(&payload, LzwConfig::TIFF_OLD_STYLE).expect("msb");
    let lsb = compress(&payload, LzwConfig::GIF).expect("lsb");
    assert_ne!(msb, lsb, "the two packings must produce different bytes");
    assert_eq!(msb.len(), lsb.len(), "only the packing may differ");

    assert_eq!(
        WeezlDecoder::new(BitOrder::Msb, 8)
            .decode(&msb)
            .expect("weezl msb"),
        payload
    );
    assert_eq!(
        WeezlDecoder::new(BitOrder::Lsb, 8)
            .decode(&lsb)
            .expect("weezl lsb"),
        payload
    );

    // Cross-reading must never quietly reproduce the payload.
    if let Ok(wrong) = WeezlDecoder::new(BitOrder::Lsb, 8).decode(&msb) {
        assert_ne!(
            wrong, payload,
            "an MSB stream read LSB-first must not match"
        );
    }
    if let Ok(wrong) = WeezlDecoder::new(BitOrder::Msb, 8).decode(&lsb) {
        assert_ne!(
            wrong, payload,
            "an LSB stream read MSB-first must not match"
        );
    }
}

#[test]
fn the_gif_config_matches_the_gif_codec_exactly_where_the_docs_say_it_does() {
    // `LzwConfig::GIF`'s rustdoc claims the generic engine and the dedicated
    // `gif_compress` codec agree byte for byte at `minimum_code_size == 8`
    // for every input that never fills the 4 096-entry table, and diverge
    // once it does. Both halves of that claim are pinned here, and both
    // encoders' output is checked against `weezl` either way.
    let shapes = payloads();
    for (index, (name, payload)) in shapes.iter().enumerate() {
        let generic = compress(payload, LzwConfig::GIF).unwrap_or_else(|e| panic!("{name}: {e}"));
        let codec = gif_compress(payload, 8).unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(
            weezl_decode(LzwConfig::GIF, &generic).unwrap_or_else(|e| panic!("{name}: {e}")),
            *payload,
            "{name}: generic GIF config"
        );
        assert_eq!(
            weezl_decode(LzwConfig::GIF, &codec).unwrap_or_else(|e| panic!("{name}: {e}")),
            *payload,
            "{name}: gif_compress"
        );

        if index < SMALL_SHAPES {
            assert_eq!(
                generic, codec,
                "{name} never fills the table, so the two encoders must agree byte for byte"
            );
        }
    }

    // And the divergence the docs promise, so the claim cannot rot into
    // "they are always the same".
    let big = records(20_000);
    assert_ne!(
        compress(&big, LzwConfig::GIF).expect("generic"),
        gif_compress(&big, 8).expect("codec"),
        "the documented divergence on table-filling input has disappeared"
    );

    // `TIFF_COMPAT_LSB` is `TIFF_OLD_STYLE` with the bit order flipped, and
    // `GIF` happens to have the same four other fields, so the two configs
    // must produce identical bytes.
    for (name, payload) in &shapes {
        assert_eq!(
            compress(payload, LzwConfig::TIFF_COMPAT_LSB).unwrap_or_else(|e| panic!("{name}: {e}")),
            compress(payload, LzwConfig::GIF).unwrap_or_else(|e| panic!("{name}: {e}")),
            "{name}"
        );
    }
}
