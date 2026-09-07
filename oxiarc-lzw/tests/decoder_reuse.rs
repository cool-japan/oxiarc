//! Reusing one [`LzwDecoder`] across strips (track D-verify).
//!
//! The 0.4.2 decoder caches, per learned code, the offset in the *current*
//! output at which that code's string was last written, and copies the
//! bytes back from there instead of walking the prefix chain
//! (`decoder.rs::copy_from_output`). Those offsets are only valid for the
//! buffer they were recorded against, so a decoder reused for a second
//! strip must not read one recorded during the first.
//!
//! The invariant that makes this safe is that every decode starts with a
//! table reset, after which no learned code is reachable until it has been
//! re-stored (which re-arms its offset). Nothing in the type system
//! enforces it, and getting it wrong is either a panic (`copy_within` /
//! `extend_from_within` with an out-of-range source) or, worse, silently
//! wrong bytes. So it is pinned here: every shape below decodes through a
//! reused decoder and through a freshly constructed one, and the two must
//! agree byte for byte.
//!
//! `LzwEncoder` shares the same table type, so it is exercised the same
//! way.

use oxiarc_lzw::{LzwConfig, LzwDecoder, LzwEncoder, compress_tiff, decompress_tiff_into};

/// Deterministic strip shapes that between them drive every decode path:
/// long runs (the copy-back fast path), a growing KwKwK run (the
/// parent-plus-suffix path), noise (chain walk), and a strip long enough to
/// cross the 9->10->11->12 bit-width boundaries and a mid-stream table
/// reset.
fn strips() -> Vec<Vec<u8>> {
    let mut out = vec![
        Vec::new(),
        b"A".to_vec(),
        vec![b'Z'; 4096],
        b"row0row0row1row1row2row2".to_vec(),
        b"The quick brown fox jumps over the lazy dog. ".repeat(200),
    ];

    // Growing run: "a", "aa", "aaa", ... which is the KwKwK shape.
    let mut grow = Vec::new();
    for n in 1..=120 {
        grow.extend(std::iter::repeat_n(b'a', n));
        grow.push(b'|');
    }
    out.push(grow);

    // Noise, plus noise interleaved with long runs.
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut noise = Vec::with_capacity(30_000);
    let mut mixed = Vec::with_capacity(30_000);
    while noise.len() < 30_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let byte = (seed >> 32) as u8;
        noise.push(byte);
        if byte % 7 == 0 {
            mixed.extend(std::iter::repeat_n(byte, 60));
        } else {
            mixed.push(byte);
        }
    }
    out.push(noise);
    out.push(mixed);

    // Long enough to fill the 12-bit table and force a mid-stream ClearCode.
    let mut long = Vec::with_capacity(200_000);
    let mut value = 0u8;
    while long.len() < 200_000 {
        value = value.wrapping_mul(31).wrapping_add(7);
        long.extend(std::iter::repeat_n(value, usize::from(value % 23) + 1));
    }
    out.push(long);

    out
}

#[test]
fn one_decoder_reused_across_strips_matches_fresh_decoders() {
    let payloads = strips();
    let encoded: Vec<Vec<u8>> = payloads
        .iter()
        .map(|p| compress_tiff(p).expect("compress"))
        .collect();

    let mut reused = LzwDecoder::new(LzwConfig::TIFF).expect("decoder");

    // Two passes in opposite orders, so a long strip is followed by a short
    // one (offsets recorded far past the end of the next buffer) and vice
    // versa.
    let orders: [Vec<usize>; 2] = [
        (0..payloads.len()).collect(),
        (0..payloads.len()).rev().collect(),
    ];

    for order in orders {
        for index in order {
            let payload = &payloads[index];
            let stream = &encoded[index];

            let mut via_reused = vec![0u8; payload.len()];
            let written = reused
                .decode_into(stream, &mut via_reused)
                .expect("reused decode_into");
            assert_eq!(written, payload.len(), "strip {index}");
            assert_eq!(&via_reused[..written], &payload[..], "strip {index}");

            let mut fresh = LzwDecoder::new(LzwConfig::TIFF).expect("decoder");
            let mut via_fresh = vec![0u8; payload.len()];
            let fresh_written = fresh
                .decode_into(stream, &mut via_fresh)
                .expect("fresh decode_into");
            assert_eq!(fresh_written, written, "strip {index}");
            assert_eq!(via_fresh, via_reused, "strip {index}");

            // The free function (a fresh decoder every call) is the third
            // independent path and must agree too.
            let mut via_fn = vec![0u8; payload.len()];
            let fn_written = decompress_tiff_into(stream, &mut via_fn).expect("free fn");
            assert_eq!(fn_written, written, "strip {index}");
            assert_eq!(via_fn, via_reused, "strip {index}");
        }
    }
}

#[test]
fn reuse_alternating_between_the_vec_and_slice_entry_points() {
    let payloads = strips();
    let mut reused = LzwDecoder::new(LzwConfig::TIFF).expect("decoder");

    for (index, payload) in payloads.iter().enumerate() {
        let stream = compress_tiff(payload).expect("compress");

        let via_vec = reused.decode(&stream, payload.len()).expect("decode");
        assert_eq!(&via_vec, payload, "strip {index} via Vec");

        let mut via_slice = vec![0u8; payload.len()];
        let written = reused
            .decode_into(&stream, &mut via_slice)
            .expect("decode_into");
        assert_eq!(
            &via_slice[..written],
            &payload[..],
            "strip {index} via slice"
        );

        // An explicit reset in between must change nothing.
        reused.reset();
        let after_reset = reused.decode(&stream, payload.len()).expect("decode");
        assert_eq!(&after_reset, payload, "strip {index} after reset");
    }
}

#[test]
fn reuse_with_short_buffers_after_long_ones() {
    // The nastiest ordering for a stale output offset: decode a large strip
    // (recording offsets up to ~200 KiB), then decode into a one-byte
    // buffer. A stale offset read against the short buffer would panic.
    let long = vec![b'Q'; 200_000];
    let long_stream = compress_tiff(&long).expect("compress long");
    let short = b"short".to_vec();
    let short_stream = compress_tiff(&short).expect("compress short");

    let mut reused = LzwDecoder::new(LzwConfig::TIFF).expect("decoder");
    let mut big = vec![0u8; long.len()];
    assert_eq!(
        reused
            .decode_into(&long_stream, &mut big)
            .expect("long decode"),
        long.len()
    );

    for limit in 0..=short.len() {
        let mut small = vec![0u8; limit];
        let written = reused
            .decode_into(&short_stream, &mut small)
            .expect("short decode");
        assert_eq!(written, limit, "limit {limit}");
        assert_eq!(&small[..], &short[..limit], "limit {limit}");
    }

    // And back to the long strip, which must still be exact.
    let mut big_again = vec![0u8; long.len()];
    assert_eq!(
        reused
            .decode_into(&long_stream, &mut big_again)
            .expect("long decode again"),
        long.len()
    );
    assert_eq!(big_again, long);
}

#[test]
fn one_encoder_reused_across_strips_matches_fresh_encoders() {
    // The encoder shares the dictionary type (and its open-addressed
    // `(prefix, byte) -> code` index), so the same reuse question applies.
    let payloads = strips();
    let mut reused = LzwEncoder::new(LzwConfig::TIFF).expect("encoder");

    for (index, payload) in payloads.iter().enumerate() {
        let via_reused = reused.encode(payload).expect("reused encode");
        let mut fresh = LzwEncoder::new(LzwConfig::TIFF).expect("encoder");
        let via_fresh = fresh.encode(payload).expect("fresh encode");
        assert_eq!(via_reused, via_fresh, "strip {index} encodes differently");

        let mut round_tripped = vec![0u8; payload.len()];
        let written = decompress_tiff_into(&via_reused, &mut round_tripped).expect("decode");
        assert_eq!(written, payload.len(), "strip {index}");
        assert_eq!(&round_tripped[..], &payload[..], "strip {index}");
    }
}

#[test]
fn reuse_holds_for_the_old_style_configuration_too() {
    let payloads = strips();
    let mut reused = LzwDecoder::new(LzwConfig::TIFF_OLD_STYLE).expect("decoder");

    for (index, payload) in payloads.iter().enumerate() {
        let mut encoder = LzwEncoder::new(LzwConfig::TIFF_OLD_STYLE).expect("encoder");
        let stream = encoder.encode(payload).expect("encode");

        let mut out = vec![0u8; payload.len()];
        let written = reused.decode_into(&stream, &mut out).expect("decode_into");
        assert_eq!(written, payload.len(), "strip {index}");
        assert_eq!(&out[..], &payload[..], "strip {index}");
    }
}

#[test]
fn the_gif_config_is_the_old_style_config_and_stays_msb_first() {
    // `LzwConfig` has no bit-order field, so `LzwConfig::GIF` and
    // `LzwConfig::TIFF_OLD_STYLE` are literally the same value. Passing
    // either to `decompress_into` reads MSB-first codes; GIF's LSB-first
    // packing is only reachable through `gif_decompress`. This is pinned
    // because the two constants reading identically is what the
    // `decompress_into` rustdoc warns about, and a future bit-order field
    // must update that warning rather than silently change the meaning of
    // an existing call.
    assert_eq!(LzwConfig::GIF, LzwConfig::TIFF_OLD_STYLE);

    let payload = b"gif-shaped payload, decoded MSB-first anyway".repeat(9);
    let mut encoder = LzwEncoder::new(LzwConfig::TIFF_OLD_STYLE).expect("encoder");
    let stream = encoder.encode(&payload).expect("encode");

    let mut via_gif = vec![0u8; payload.len()];
    let mut via_old = vec![0u8; payload.len()];
    let gif_written =
        oxiarc_lzw::decompress_into(&stream, &mut via_gif, LzwConfig::GIF).expect("gif config");
    let old_written = oxiarc_lzw::decompress_into(&stream, &mut via_old, LzwConfig::TIFF_OLD_STYLE)
        .expect("old-style config");
    assert_eq!(gif_written, old_written);
    assert_eq!(via_gif, via_old);
    assert_eq!(&via_gif[..gif_written], &payload[..]);

    // The real GIF path is a different bit order and a different entry
    // point, and does not decode this MSB-first stream to the same bytes.
    let gif_stream = oxiarc_lzw::gif_compress(&payload, 8).expect("gif compress");
    assert_eq!(
        oxiarc_lzw::gif_decompress(&gif_stream, 8).expect("gif decompress"),
        payload
    );
    assert_ne!(gif_stream, stream, "the two packings must not coincide");
}
