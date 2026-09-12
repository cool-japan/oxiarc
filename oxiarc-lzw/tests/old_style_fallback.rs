//! The old-style (`early_change = false`) fallback recipe documented on
//! [`oxiarc_lzw::decompress_tiff_into`].
//!
//! The recipe is copied verbatim by `oxiarc-tiff` for TIFF
//! `Compression = 5`, so its failure modes are pinned here rather than left
//! to a doctest that only proves the happy path:
//!
//! * a **short strip** (fewer decoded bytes than the geometry-sized buffer)
//!   is an ordinary outcome and must survive the fallback untouched — the
//!   earlier recipe retried on it, and the retry both failed and destroyed
//!   the bytes the standard rule had already decoded;
//! * an old-style strip that the standard rule *rejects* must be recovered;
//! * an old-style strip that the standard rule accepts with wrong bytes is
//!   a real, measured case that no return-value heuristic can catch, so the
//!   documentation must not promise otherwise.

use oxiarc_lzw::{
    LzwConfig, Result, compress, compress_tiff, decompress_into, decompress_tiff_into,
};

/// The recipe exactly as documented on `decompress_tiff_into`.
fn decode_strip(strip: &[u8], out: &mut [u8]) -> Result<usize> {
    match decompress_tiff_into(strip, out) {
        Ok(n) => Ok(n),
        Err(standard_err) => {
            let mut scratch = vec![0u8; out.len()];
            match decompress_into(strip, &mut scratch, LzwConfig::TIFF_OLD_STYLE) {
                Ok(n) => {
                    out.copy_from_slice(&scratch);
                    Ok(n)
                }
                Err(_) => Err(standard_err),
            }
        }
    }
}

fn lcg(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed;
    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        out.push((state >> 33) as u8);
    }
    out
}

#[test]
fn a_short_strip_is_not_retried_and_is_not_clobbered() {
    // A perfectly good standard-rule strip decoded into a buffer sized from
    // the image geometry, where the strip legitimately holds fewer bytes
    // (the last strip of an image, or a writer that ended a row early).
    for payload_len in [1usize, 200, 3000, 20_000] {
        let raw = lcg(payload_len, 0xC0FFEE);
        let strip = compress_tiff(&raw).expect("encode");
        let mut out = vec![0u8; raw.len() + 500];

        let written = decode_strip(&strip, &mut out).expect("a short strip must not be an error");
        assert_eq!(written, raw.len(), "len {payload_len}");
        assert_eq!(
            &out[..written],
            &raw[..],
            "len {payload_len}: bytes clobbered"
        );
    }
}

#[test]
fn old_style_strips_the_standard_rule_rejects_are_recovered() {
    let mut rejected_by_standard = 0usize;
    for len in [700usize, 1500, 4000, 9000] {
        let raw = lcg(len, 0xABCDEF);
        let strip = compress(&raw, LzwConfig::TIFF_OLD_STYLE).expect("old-style encode");
        let mut probe = vec![0u8; raw.len()];
        if decompress_tiff_into(&strip, &mut probe).is_err() {
            rejected_by_standard += 1;
            let mut out = vec![0u8; raw.len()];
            let written = decode_strip(&strip, &mut out).expect("the fallback must recover it");
            assert_eq!(written, raw.len());
            assert_eq!(out, raw);
        }
    }
    assert!(
        rejected_by_standard > 0,
        "the corpus must contain at least one old-style strip the standard rule rejects, \
         otherwise this test proves nothing"
    );
}

#[test]
fn the_documented_uncatchable_case_is_real() {
    // Documented on `decompress_tiff_into`: an old-style stream can fill the
    // buffer completely under the standard rule with wrong bytes, so no
    // return-value heuristic can route it to the fallback. Measured here so
    // the documentation stays honest if the numbers ever move.
    let mut total = 0usize;
    let mut silently_wrong = 0usize;
    for (index, len) in (200..6000).step_by(37).enumerate() {
        for raw in [
            lcg(len, 0x1234_5678 + index as u64),
            (0..len).map(|j| (j % 253) as u8).collect::<Vec<u8>>(),
        ] {
            let strip = compress(&raw, LzwConfig::TIFF_OLD_STYLE).expect("old-style encode");
            let mut out = vec![0u8; raw.len()];
            total += 1;
            if let Ok(n) = decompress_tiff_into(&strip, &mut out) {
                if n == raw.len() && out != raw {
                    silently_wrong += 1;
                }
            }
        }
    }
    assert!(
        total > 100,
        "the corpus must be big enough to be meaningful"
    );
    assert!(
        silently_wrong > 0,
        "the documented caveat claims this case exists; if it no longer does, \
         update the docs on decompress_tiff_into"
    );
    // A loose upper bound: this is a caveat, not the common case.
    assert!(
        silently_wrong * 10 < total,
        "{silently_wrong}/{total} old-style strips decoded wrongly-but-cleanly; \
         the documented '~2 %' is no longer accurate"
    );
}

#[test]
fn the_doctest_stream_really_exercises_the_fallback() {
    // Guards the runnable example on `decompress_tiff_into`: if the
    // standard rule ever accepted this stream, the example would silently
    // stop proving anything.
    let original = b"old-style TIFF LZW strip; long enough that the code \
                     width has to grow, which is the only place the two \
                     rules disagree at all. AAAAAAAAAABBBBBBBBBBCCCCCCCCCC"
        .repeat(20);
    let strip = compress(&original, LzwConfig::TIFF_OLD_STYLE).expect("old-style encode");
    let mut out = vec![0u8; original.len()];
    assert!(
        decompress_tiff_into(&strip, &mut out).is_err(),
        "the doctest fixture must be a stream the standard rule rejects"
    );
    let written = decode_strip(&strip, &mut out).expect("fallback");
    assert_eq!(written, original.len());
    assert_eq!(out, original);
}
