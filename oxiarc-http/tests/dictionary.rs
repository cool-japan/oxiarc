//! `dcb` and `dcz`: RFC 9842 Compression Dictionary Transport.
//!
//! `dcz` is ordinary Zstandard decoded against a dictionary the client
//! already holds, so it is supported exactly when the caller can supply
//! that dictionary. `dcb` is the Brotli variant and needs shared-dictionary
//! Brotli, which `oxiarc-brotli` does not implement (Phase 8 owner decision
//! #8) — it parses and refuses, and never silently decodes as plain `br`.

mod common;

#[cfg(feature = "zstd")]
use oxiarc_http::DecodedBody;
#[cfg(feature = "brotli")]
use oxiarc_http::decode_body;
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

#[cfg(feature = "zstd")]
#[test]
fn dcz_decodes_against_a_supplied_dictionary() {
    let (dictionary, plain) = fixture();
    let mut encoder = oxiarc_zstd::ZstdEncoder::new();
    encoder.set_dictionary(&dictionary);
    let wire = encoder.compress(&plain).expect("zstd with dictionary");

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
    let mut encoder = oxiarc_zstd::ZstdEncoder::new();
    encoder.set_dictionary(&dictionary);
    let wire = encoder.compress(&plain).expect("zstd with dictionary");

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
    let mut encoder = oxiarc_zstd::ZstdEncoder::new();
    encoder.set_dictionary(&dictionary);
    let wire = encoder.compress(&plain).expect("zstd with dictionary");

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

#[test]
fn dcb_is_refused_with_or_without_a_dictionary() {
    let error = Decoder::new(&[ContentCoding::Dcb], &DecodeLimits::default())
        .expect_err("dcb needs a dictionary");
    assert!(matches!(error, HttpCodingError::MissingDictionary { .. }));

    let error = Decoder::with_dictionary(&[ContentCoding::Dcb], &DecodeLimits::default(), b"d")
        .expect_err("dcb is unsupported even with a dictionary");
    assert!(matches!(error, HttpCodingError::UnsupportedCoding { .. }));
}

#[cfg(feature = "brotli")]
#[test]
fn dcb_never_silently_decodes_as_plain_brotli() {
    // The failure mode this rules out: a `dcb` body decoded as `br` against
    // no dictionary would either fail confusingly or, worse, produce a
    // plausible-looking wrong body.
    let plain = common::text(4_000);
    let wire = oxiarc_brotli::compress(&plain, 4).expect("brotli");
    decode_body(&[ContentCoding::Dcb], &wire, &DecodeLimits::default())
        .expect_err("dcb must never fall back to br");
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
