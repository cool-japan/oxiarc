//! `dcb` and `dcz`: RFC 9842 Compression Dictionary Transport.
//!
//! Both are ordinary bodies (Zstandard, Brotli) preceded by a preamble that
//! names the dictionary they were compressed against — a zstd skippable
//! frame's payload for `dcz`, `oxiarc_brotli::dcb`'s own 36-byte header for
//! `dcb` — so both are decodable exactly when the caller can supply that
//! dictionary. Neither ever falls back to decoding the coding underneath
//! without checking it (`br` for a `dcb` body missing its preamble, `zstd`
//! for a `dcz` body missing its); a wrong or absent dictionary is a
//! `MissingDictionary`/`Corrupt` error, never a silently-accepted guess.
//!
//! This crate does not implement RFC 9842's own `Available-Dictionary` /
//! `Use-As-Dictionary` / `Dictionary-ID` request and response headers —
//! obtaining, matching, and advertising a dictionary by content-address is
//! the caller's business (a browser cache, a CDN's own bookkeeping); this
//! crate only takes the dictionary *bytes*, once the caller already has
//! them, and verifies the wire body against exactly those bytes.

mod common;

#[cfg(any(feature = "brotli", feature = "zstd"))]
use oxiarc_http::DecodedBody;
// Only `make_dcz` (the `zstd` fixtures) goes through the crate's own encode
// API; the `dcb` fixtures use `oxiarc_brotli::compress_dcb` directly (a
// sibling crate's own public API, not gated on anything here).
#[cfg(feature = "zstd")]
use oxiarc_http::EncodeOptions;
#[cfg(feature = "brotli")]
use oxiarc_http::decode_body;
#[cfg(feature = "zstd")]
use oxiarc_http::encode_body;
use oxiarc_http::{ContentCoding, DecodeLimits, Decoder, HttpCodingError};

/// A dictionary and a body that shares long substrings with it, which is the
/// whole point of dictionary transport.
#[cfg(feature = "zstd")]
fn fixture() -> (Vec<u8>, Vec<u8>) {
    let dictionary = common::json(64 * 1024);
    let mut body = common::json(48 * 1024);
    body.extend_from_slice(&dictionary[..8 * 1024]);
    (dictionary, body)
}

/// Build a real `dcz` wire body (RFC 9842 preamble + dictionary-referencing
/// `zstd` frame) through this crate's own public encode API — never by
/// hand-rolling the frame, so these tests exercise the exact bytes a real
/// server would send.
#[cfg(feature = "zstd")]
fn make_dcz(dictionary: &[u8], plain: &[u8]) -> Vec<u8> {
    encode_body(
        &ContentCoding::Dcz,
        plain,
        EncodeOptions::new().with_dictionary(Some(dictionary)),
    )
    .expect("dcz encode")
}

#[cfg(feature = "zstd")]
#[test]
fn dcz_decodes_against_a_supplied_dictionary() {
    let (dictionary, plain) = fixture();
    let wire = make_dcz(&dictionary, &plain);

    let mut decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcz], &DecodeLimits::default(), &dictionary)
            .expect("dcz with a dictionary");
    let mut out = Vec::new();
    decoder.feed_into(&wire, &mut out).expect("feed");
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
    assert_eq!(decoder.codings(), &[ContentCoding::Dcz]);
}

#[cfg(feature = "zstd")]
#[test]
fn dcz_survives_byte_at_a_time_feeding() {
    let (dictionary, plain) = fixture();
    let wire = make_dcz(&dictionary, &plain);

    let mut decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcz], &DecodeLimits::default(), &dictionary)
            .expect("dcz");
    let mut out = Vec::new();
    for chunk in wire.chunks(3) {
        decoder.feed_into(chunk, &mut out).expect("feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
}

#[cfg(feature = "zstd")]
#[test]
fn a_decoded_body_can_carry_a_dictionary() {
    let (dictionary, plain) = fixture();
    let wire = make_dcz(&dictionary, &plain);

    let decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcz], &DecodeLimits::default(), &dictionary)
            .expect("dcz");
    let mut body = DecodedBody::with_decoder(&wire[..], decoder);
    assert_eq!(body.read_to_vec().expect("read"), plain);
}

#[test]
fn dcz_without_a_dictionary_fails_at_construction_not_mid_body() {
    // Failing early matters: a response the client cannot possibly decode
    // should be rejected before any of it is read off the socket.
    let error = Decoder::new(&[ContentCoding::Dcz], &DecodeLimits::default())
        .expect_err("dcz needs a dictionary");
    if ContentCoding::Dcz.is_decodable() {
        assert!(matches!(error, HttpCodingError::MissingDictionary { .. }));
    } else {
        assert!(matches!(error, HttpCodingError::UnsupportedCoding { .. }));
    }
}

/// A dictionary and a body that shares long substrings with it — the
/// `dcb` twin of [`fixture`] above.
#[cfg(feature = "brotli")]
fn dcb_fixture() -> (Vec<u8>, Vec<u8>) {
    let dictionary = common::json(64 * 1024);
    let mut body = common::json(48 * 1024);
    body.extend_from_slice(&dictionary[..8 * 1024]);
    (dictionary, body)
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_decodes_against_a_supplied_dictionary() {
    let (dictionary, plain) = dcb_fixture();
    let params = oxiarc_brotli::BrotliParams::default();
    let wire = oxiarc_brotli::compress_dcb(&plain, &dictionary, &params).expect("dcb compress");

    let mut decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcb], &DecodeLimits::default(), &dictionary)
            .expect("dcb with a dictionary");
    let mut out = Vec::new();
    decoder.feed_into(&wire, &mut out).expect("feed");
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
    assert_eq!(decoder.codings(), &[ContentCoding::Dcb]);
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_survives_byte_at_a_time_feeding() {
    let (dictionary, plain) = dcb_fixture();
    let params = oxiarc_brotli::BrotliParams::default();
    let wire = oxiarc_brotli::compress_dcb(&plain, &dictionary, &params).expect("dcb compress");

    let mut decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcb], &DecodeLimits::default(), &dictionary)
            .expect("dcb");
    let mut out = Vec::new();
    for chunk in wire.chunks(3) {
        decoder.feed_into(chunk, &mut out).expect("feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
}

#[cfg(feature = "brotli")]
#[test]
fn a_decoded_body_can_carry_a_dcb_dictionary() {
    let (dictionary, plain) = dcb_fixture();
    let params = oxiarc_brotli::BrotliParams::default();
    let wire = oxiarc_brotli::compress_dcb(&plain, &dictionary, &params).expect("dcb compress");

    let decoder =
        Decoder::with_dictionary(&[ContentCoding::Dcb], &DecodeLimits::default(), &dictionary)
            .expect("dcb");
    let mut body = DecodedBody::with_decoder(&wire[..], decoder);
    assert_eq!(body.read_to_vec().expect("read"), plain);
}

#[test]
fn dcb_without_a_dictionary_fails_at_construction_not_mid_body() {
    // The `dcz` twin of this test, mirrored: failing early matters — a
    // response the client cannot possibly decode should be rejected before
    // any of it is read off the socket.
    let error = Decoder::new(&[ContentCoding::Dcb], &DecodeLimits::default())
        .expect_err("dcb needs a dictionary");
    if ContentCoding::Dcb.is_decodable() {
        assert!(matches!(error, HttpCodingError::MissingDictionary { .. }));
    } else {
        assert!(matches!(error, HttpCodingError::UnsupportedCoding { .. }));
    }
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_never_silently_decodes_as_plain_brotli() {
    // The failure mode this rules out: a `dcb` body decoded as `br` against
    // no dictionary would either fail confusingly or, worse, produce a
    // plausible-looking wrong body. `decode_body` builds its `Decoder` with
    // no dictionary (`Decoder::new`, not `with_dictionary`), so this fails
    // at construction (`MissingDictionary`) — before the missing preamble
    // would even be checked.
    let plain = common::text(4_000);
    let wire = oxiarc_brotli::compress(&plain, 4).expect("brotli");
    decode_body(&[ContentCoding::Dcb], &wire, &DecodeLimits::default())
        .expect_err("dcb must never fall back to br");
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_never_decodes_a_body_naming_a_different_dictionary() {
    let (dictionary, plain) = dcb_fixture();
    // `common::json`/`common::text` are pure functions of the requested
    // length, so `wrong` must come from a *different* generator than
    // `dictionary` (also `common::json(64 * 1024)`, via `dcb_fixture`) —
    // otherwise the two "different" dictionaries are byte-identical and
    // this test would pass for the wrong reason.
    let wrong = common::text(64 * 1024);
    let params = oxiarc_brotli::BrotliParams::default();
    let wire = oxiarc_brotli::compress_dcb(&plain, &dictionary, &params).expect("dcb compress");

    decode_body(
        &[ContentCoding::Dcb],
        &wire,
        &DecodeLimits::default().with_max_ratio(None),
    )
    .expect_err("no dictionary at all must fail first, at MissingDictionary");
    let error = Decoder::with_dictionary(&[ContentCoding::Dcb], &DecodeLimits::default(), &wrong)
        .and_then(|mut decoder| {
            let mut out = Vec::new();
            decoder.feed_into(&wire, &mut out)?;
            decoder.finish_into(&mut out)?;
            Ok(out)
        })
        .expect_err("a body naming a different dictionary must never decode");
    assert!(matches!(error, HttpCodingError::Corrupt { .. }));
}

#[cfg(feature = "zstd")]
#[test]
fn dcz_never_silently_decodes_as_plain_zstd() {
    // The `dcz` twin of `dcb_never_silently_decodes_as_plain_brotli`. This
    // one has real teeth: a `dcz` body's payload *is* an ordinary
    // dictionary-referencing Zstandard frame, so a decoder that skipped the
    // RFC 9842 preamble check would happily decode a frame that named no
    // dictionary at all — or somebody else's.
    let (dictionary, plain) = fixture();
    let mut encoder = oxiarc_zstd::ZstdEncoder::new();
    encoder.set_dictionary(&dictionary);
    let bare = encoder.compress(&plain).expect("zstd with dictionary");

    let error =
        Decoder::with_dictionary(&[ContentCoding::Dcz], &DecodeLimits::default(), &dictionary)
            .and_then(|mut decoder| {
                let mut out = Vec::new();
                decoder.feed_into(&bare, &mut out)?;
                decoder.finish_into(&mut out)?;
                Ok(out)
            })
            .expect_err("a dcz body must open with the RFC 9842 preamble");
    assert!(matches!(error, HttpCodingError::Corrupt { .. }));
}

#[cfg(feature = "zstd")]
#[test]
fn dcz_never_decodes_a_body_naming_a_different_dictionary() {
    // The `dcz` twin of `dcb_never_decodes_a_body_naming_a_different_dictionary`.
    let (dictionary, plain) = fixture();
    // `common::json`/`common::text` are pure functions of the requested
    // length, so `wrong` has to come from a *different* generator than
    // `dictionary` or the two would be byte-identical and this test would
    // pass for the wrong reason.
    let wrong = common::text(64 * 1024);
    let wire = make_dcz(&dictionary, &plain);

    let error = Decoder::with_dictionary(&[ContentCoding::Dcz], &DecodeLimits::default(), &wrong)
        .and_then(|mut decoder| {
            let mut out = Vec::new();
            decoder.feed_into(&wire, &mut out)?;
            decoder.finish_into(&mut out)?;
            Ok(out)
        })
        .expect_err("a body naming a different dictionary must never decode");
    assert!(matches!(error, HttpCodingError::Corrupt { .. }));
}

/// Decode `wire` through a dictionary-carrying `Decoder`, feeding it in one
/// piece.
#[cfg(any(feature = "brotli", feature = "zstd"))]
fn decode_with_dictionary(
    coding: &ContentCoding,
    wire: &[u8],
    dictionary: &[u8],
) -> Result<Vec<u8>, HttpCodingError> {
    let mut decoder = Decoder::with_dictionary(
        std::slice::from_ref(coding),
        &DecodeLimits::default(),
        dictionary,
    )?;
    let mut out = Vec::new();
    decoder.feed_into(wire, &mut out)?;
    decoder.finish_into(&mut out)?;
    Ok(out)
}

/// A cheap deterministic PRNG — no `rand` dependency — for split points and
/// byte edits below.
#[cfg(any(feature = "brotli", feature = "zstd"))]
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The sweep `tests/chunking.rs` and `tests/roundtrip.rs` cannot run for
/// these two codings: their `codings()` helpers are filtered to
/// dictionary-free codings, so `dcb`/`dcz` get no crate-level truncation or
/// random-split coverage from there.
///
/// Three invariants, in order of sharpness:
///
/// 1. **Truncating at any offset is an error.** Both codings carry a
///    complete-stream marker under the preamble, so a short body must never
///    decode to a plausible short one.
/// 2. **Every single-bit flip inside the preamble is an error.** The
///    preamble is a magic number plus a SHA-256 of the dictionary, so every
///    one of its bits is load-bearing — this is the RFC 9842 binding
///    itself, and the reason `dcb`/`dcz` are not merely "`br`/`zstd` with a
///    dictionary".
/// 3. **A random split never changes the output**, and a flip in the
///    *payload* never panics or hangs. Neither RFC 7932 Brotli nor an
///    unchecksummed Zstandard frame detects every flipped payload bit — a
///    flip in a padding or don't-care bit legitimately yields a different,
///    well-formed body (`tests/roundtrip.rs` records the same exception for
///    plain `br`) — so this one counts outcomes rather than demanding an
///    error, and asserts only that the detection rate is not zero.
#[cfg(any(feature = "brotli", feature = "zstd"))]
fn sweep_truncation_and_splits(
    coding: &ContentCoding,
    wire: &[u8],
    dictionary: &[u8],
    plain: &[u8],
    preamble_len: usize,
) {
    for cut in 0..wire.len() {
        match decode_with_dictionary(coding, &wire[..cut], dictionary) {
            Err(_) => {}
            Ok(out) => panic!(
                "{coding}: truncating at {cut}/{} decoded {} bytes without an error",
                wire.len(),
                out.len()
            ),
        }
    }

    for index in 0..preamble_len {
        for bit in 0..8u32 {
            let mut bad = wire.to_vec();
            bad[index] ^= 1 << bit;
            decode_with_dictionary(coding, &bad, dictionary)
                .err()
                .unwrap_or_else(|| {
                    panic!(
                        "{coding}: flipping bit {bit} of preamble byte {index} still decoded — \
                     every bit of the RFC 9842 magic and digest is load-bearing"
                    )
                });
        }
    }

    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..32 {
        let mut decoder = Decoder::with_dictionary(
            std::slice::from_ref(coding),
            &DecodeLimits::default(),
            dictionary,
        )
        .expect("decoder");
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < wire.len() {
            let step = 1 + (xorshift(&mut state) % 97) as usize;
            let end = (pos + step).min(wire.len());
            decoder.feed_into(&wire[pos..end], &mut out).expect("feed");
            pos = end;
        }
        decoder.finish_into(&mut out).expect("finish");
        assert_eq!(out, plain, "{coding}: a random split changed the output");
    }

    let payload = wire.len() - preamble_len;
    let mut caught = 0usize;
    let mut accepted = 0usize;
    for _ in 0..256 {
        let mut bad = wire.to_vec();
        let index = preamble_len + (xorshift(&mut state) as usize) % payload;
        bad[index] ^= 1 << (xorshift(&mut state) % 8);
        match decode_with_dictionary(coding, &bad, dictionary) {
            Err(_) => caught += 1,
            Ok(out) => {
                accepted += 1;
                assert!(
                    out.len() <= plain.len() * 2,
                    "{coding}: a flipped bit at {index} produced {} bytes from a {}-byte body",
                    out.len(),
                    plain.len()
                );
            }
        }
    }
    assert_eq!(caught + accepted, 256);
    assert!(
        caught > 0,
        "{coding}: not one of 256 flipped payload bits was detected"
    );
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_survives_truncation_at_every_offset_and_random_splits() {
    let dictionary = common::json(8 * 1024);
    let mut plain = common::json(6 * 1024);
    plain.extend_from_slice(&dictionary[..1024]);
    let params = oxiarc_brotli::BrotliParams::default();
    let wire = oxiarc_brotli::compress_dcb(&plain, &dictionary, &params).expect("dcb compress");
    sweep_truncation_and_splits(
        &ContentCoding::Dcb,
        &wire,
        &dictionary,
        &plain,
        oxiarc_brotli::DCB_HEADER_LEN,
    );
}

#[cfg(feature = "zstd")]
#[test]
fn dcz_survives_truncation_at_every_offset_and_random_splits() {
    let dictionary = common::json(8 * 1024);
    let mut plain = common::json(6 * 1024);
    plain.extend_from_slice(&dictionary[..1024]);
    let wire = make_dcz(&dictionary, &plain);
    // 4-byte skippable magic + 4-byte little-endian size + a 32-byte
    // SHA-256; `decode/zstd.rs` owns the constant, which is private.
    sweep_truncation_and_splits(&ContentCoding::Dcz, &wire, &dictionary, &plain, 40);
}

#[cfg(all(feature = "zstd", feature = "gzip"))]
#[test]
fn a_dictionary_is_ignored_by_codings_that_do_not_use_one() {
    let plain = common::text(4_000);
    let wire = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let mut decoder =
        Decoder::with_dictionary(&[ContentCoding::Gzip], &DecodeLimits::default(), b"unused")
            .expect("gzip ignores the dictionary");
    let mut out = Vec::new();
    decoder.feed_into(&wire, &mut out).expect("feed");
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
}
