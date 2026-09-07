//! Container framing: gzip members and header flags, the `deflate` sniff,
//! the trailing-data policy, and chained codings.

#![cfg(all(feature = "gzip", feature = "deflate"))]

mod common;

use common::GzipHeaderFields;
use oxiarc_http::{
    ContentCoding, DecodeLimits, Decoder, HttpCodingError, TrailingData, decode_body,
    decode_body_from_header,
};

fn decode_gzip(wire: &[u8]) -> oxiarc_http::Result<Vec<u8>> {
    decode_body(&[ContentCoding::Gzip], wire, &DecodeLimits::default())
}

fn decode_deflate(wire: &[u8]) -> oxiarc_http::Result<Vec<u8>> {
    decode_body(&[ContentCoding::Deflate], wire, &DecodeLimits::default())
}

// ── gzip ─────────────────────────────────────────────────────────────────

#[test]
fn multi_member_gzip_is_concatenated() {
    // RFC 1952 §2.2: a gzip stream is a *series* of members, and `gzip -c a
    // b`, `pigz` and oxiarc's own parallel encoder all emit several.
    let parts: Vec<Vec<u8>> = ["alpha", "beta", "gamma", ""]
        .iter()
        .map(|s| oxiarc_deflate::gzip_compress(s.as_bytes(), 6).expect("gzip"))
        .collect();
    let mut wire = Vec::new();
    for part in &parts {
        wire.extend_from_slice(part);
    }
    assert_eq!(decode_gzip(&wire).expect("multi-member"), b"alphabetagamma");
}

#[test]
fn a_multi_member_stream_split_at_every_member_boundary_decodes() {
    let a = oxiarc_deflate::gzip_compress(b"first-", 6).expect("gzip");
    let b = oxiarc_deflate::gzip_compress(b"second", 6).expect("gzip");
    let mut wire = a.clone();
    wire.extend_from_slice(&b);

    // Feed one byte at a time: the member boundary lands mid-call, which is
    // exactly where a non-resumable container decoder loses bytes.
    let mut decoder =
        Decoder::new(&[ContentCoding::Gzip], &DecodeLimits::default()).expect("decoder");
    let mut out = Vec::new();
    for byte in &wire {
        decoder.feed_into(&[*byte], &mut out).expect("feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, b"first-second");
}

#[test]
fn every_optional_gzip_header_field_is_accepted() {
    let plain = b"headers with every flag set";
    let raw = common::raw_deflate(plain);
    let combinations = [
        GzipHeaderFields::default(),
        GzipHeaderFields {
            name: Some(b"body.txt".to_vec()),
            ..Default::default()
        },
        GzipHeaderFields {
            comment: Some(b"a comment".to_vec()),
            ..Default::default()
        },
        GzipHeaderFields {
            extra: Some(b"\x01\x02\x03\x04".to_vec()),
            ..Default::default()
        },
        GzipHeaderFields {
            header_crc: true,
            ..Default::default()
        },
        GzipHeaderFields {
            extra: Some(b"AB\x02\x00xy".to_vec()),
            name: Some(b"body.txt".to_vec()),
            comment: Some(b"a comment".to_vec()),
            header_crc: true,
            ..Default::default()
        },
    ];
    for (index, fields) in combinations.iter().enumerate() {
        let wire = common::gzip_member(&raw, plain, fields);
        assert_eq!(
            decode_gzip(&wire).unwrap_or_else(|e| panic!("combination {index}: {e}")),
            plain,
            "combination {index}"
        );
    }
}

#[test]
fn a_wrong_header_crc_is_rejected() {
    let plain = b"fhcrc must be verified";
    let raw = common::raw_deflate(plain);
    let fields = GzipHeaderFields {
        name: Some(b"body.txt".to_vec()),
        header_crc: true,
        ..Default::default()
    };
    let mut wire = common::gzip_member(&raw, plain, &fields);
    // The FHCRC is the two bytes immediately before the DEFLATE payload.
    let crc_at = wire.len() - raw.len() - 8 - 2;
    wire[crc_at] ^= 0xFF;
    decode_gzip(&wire).expect_err("a wrong FHCRC must be rejected, not skipped");
}

#[test]
fn reserved_flg_bits_are_rejected() {
    // RFC 1952 §2.3.1.2: FLG bits 5, 6 and 7 "must be zero".
    let plain = b"reserved flags";
    let raw = common::raw_deflate(plain);
    for bit in [0x20u8, 0x40, 0x80] {
        let fields = GzipHeaderFields {
            extra_flags: bit,
            ..Default::default()
        };
        let wire = common::gzip_member(&raw, plain, &fields);
        decode_gzip(&wire).unwrap_err();
    }
}

#[test]
fn a_wrong_trailer_crc_or_isize_is_rejected() {
    let plain = b"the trailer is load bearing";
    let wire = oxiarc_deflate::gzip_compress(plain, 6).expect("gzip");

    let mut bad_crc = wire.clone();
    let crc_at = bad_crc.len() - 8;
    bad_crc[crc_at] ^= 0x01;
    decode_gzip(&bad_crc).expect_err("a wrong CRC-32 must be rejected");

    let mut bad_size = wire.clone();
    let size_at = bad_size.len() - 4;
    bad_size[size_at] ^= 0x01;
    decode_gzip(&bad_size).expect_err("a wrong ISIZE must be rejected");
}

#[test]
fn a_body_that_is_not_gzip_at_all_is_an_error_never_an_empty_read() {
    // The legacy `GzipStreamDecoder` returns `Ok(0)` here (Phase 8 owner
    // decision #2 keeps that). Every new type, this crate included, is
    // strict: a response that claims gzip and is not gzip is a protocol
    // error, and silently returning an empty body is how a client ends up
    // parsing nothing at all.
    for body in [
        &b"<!doctype html><title>502 Bad Gateway</title>"[..],
        &b"\x1f"[..],
        &b"\x1f\x8c"[..],
    ] {
        decode_gzip(body).expect_err("non-gzip under Content-Encoding: gzip must be an error");
    }
}

// ── deflate ──────────────────────────────────────────────────────────────

#[test]
fn deflate_accepts_zlib_raw_and_a_mislabelled_gzip_stream() {
    let plain = common::text(9_000);

    // (a) the conformant spelling: RFC 1950 zlib.
    let zlib = oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib");
    assert_eq!(decode_deflate(&zlib).expect("zlib"), plain);

    // (b) raw RFC 1951, which RFC 9110 §8.4.1.2 explicitly sanctions
    // accepting ("some non-conformant implementations send the 'deflate'
    // compressed data without the zlib wrapper" — historically IIS).
    let raw = common::raw_deflate(&plain);
    assert_eq!(decode_deflate(&raw).expect("raw"), plain);

    // (c) a whole gzip stream mislabelled `deflate`, which browsers accept.
    let gz = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    assert_eq!(decode_deflate(&gz).expect("mislabelled gzip"), plain);
}

#[test]
fn the_deflate_sniff_survives_chunked_delivery_of_the_first_two_bytes() {
    let plain = common::text(5_000);
    for wire in [
        oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib"),
        common::raw_deflate(&plain),
        oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip"),
    ] {
        // One byte per feed: the sniff needs two, so it must survive being
        // asked to decide with only one in hand.
        let mut decoder =
            Decoder::new(&[ContentCoding::Deflate], &DecodeLimits::default()).expect("decoder");
        let mut out = Vec::new();
        for byte in &wire {
            decoder.feed_into(&[*byte], &mut out).expect("feed");
        }
        decoder.finish_into(&mut out).expect("finish");
        assert_eq!(out, plain);
    }
}

#[test]
fn a_zlib_stream_requiring_a_preset_dictionary_is_rejected_cleanly() {
    // FDICT (FLG bit 5) cannot be satisfied over HTTP: there is no way to
    // convey the dictionary. The failure must be a clean error, not a
    // confusing inflate failure hundreds of bytes later.
    let plain = common::text(4_000);
    let dictionary = b"the quick brown fox jumps over the lazy dog";
    let wire = oxiarc_deflate::zlib_compress_with_dict(&plain, 6, dictionary).expect("zlib+dict");
    assert_ne!(wire[1] & 0x20, 0, "the fixture must actually set FDICT");
    decode_deflate(&wire).expect_err("FDICT cannot be satisfied over HTTP");
}

#[test]
fn a_raw_stream_crafted_to_look_like_a_zlib_header_is_read_as_zlib() {
    // The documented blind spot of a two-byte sniff, pinned rather than
    // papered over. This stream is a *valid* raw DEFLATE stream — a
    // non-final stored block of 29 bytes, then a final empty stored block —
    // whose first two bytes also satisfy the zlib header test:
    //   CMF = 0x08 -> CM = 8, CINFO = 0
    //   (0x08 * 256 + 0x1D) % 31 == 0, and FDICT is clear.
    // No real encoder emits it: a non-final stored block starts `0x00`,
    // whose low nibble is 0, not the 8 a zlib CM needs. It takes a
    // hand-crafted stream, and this is one.
    let mut wire = vec![0x08, 0x1D, 0x00, 0xE2, 0xFF];
    wire.extend_from_slice(&[b'z'; 29]);
    wire.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);

    // As raw DEFLATE it decodes to the 29 payload bytes.
    assert_eq!(
        oxiarc_deflate::inflate(&wire).expect("valid raw DEFLATE"),
        vec![b'z'; 29]
    );
    // Under `Content-Encoding: deflate` the sniff reads it as zlib, so it
    // does *not* decode to the same thing. Both a clean error and a
    // different body are acceptable; silently returning the raw reading is
    // not, because that would mean the sniff had a hidden retry path whose
    // memory cost is unbounded.
    match decode_deflate(&wire) {
        Err(_) => {}
        Ok(decoded) => assert_ne!(
            decoded,
            vec![b'z'; 29],
            "the sniff must not silently re-interpret; name `gzip` when the framing is known"
        ),
    }
}

// ── trailing data ────────────────────────────────────────────────────────

fn decode_with_policy(
    coding: ContentCoding,
    wire: &[u8],
    policy: TrailingData,
) -> oxiarc_http::Result<Vec<u8>> {
    let mut decoder = Decoder::new(std::slice::from_ref(&coding), &DecodeLimits::default())?
        .trailing_data(policy);
    let mut out = Vec::new();
    decoder.feed_into(wire, &mut out)?;
    decoder.finish_into(&mut out)?;
    Ok(out)
}

#[test]
fn trailing_garbage_is_rejected_by_default_at_every_length() {
    let plain = b"a complete response body";
    let gz = oxiarc_deflate::gzip_compress(plain, 6).expect("gzip");
    // 1 and 2 bytes matter most: those are the lookahead the container needs
    // to decide whether another member starts, and a chain-level check would
    // never see them.
    for count in [1usize, 2, 3, 8, 64] {
        let mut wire = gz.clone();
        wire.extend(std::iter::repeat_n(b'X', count));
        match decode_gzip(&wire) {
            Err(error @ HttpCodingError::TrailingGarbage { .. }) => {
                assert!(error.to_string().contains("trailing"));
            }
            Err(other) => panic!("{count} trailing byte(s): wrong error: {other}"),
            Ok(_) => panic!("{count} trailing byte(s) must be rejected"),
        }
    }
}

#[test]
fn allow_zeros_tolerates_padding_but_not_junk() {
    let plain = b"cdn padded body";
    let gz = oxiarc_deflate::gzip_compress(plain, 6).expect("gzip");

    for count in [1usize, 2, 3, 16] {
        let mut wire = gz.clone();
        wire.extend(std::iter::repeat_n(0u8, count));
        assert_eq!(
            decode_with_policy(ContentCoding::Gzip, &wire, TrailingData::AllowZeros)
                .unwrap_or_else(|e| panic!("{count} zero byte(s) should be tolerated: {e}")),
            plain
        );
        // ...and rejected under the default policy.
        decode_with_policy(ContentCoding::Gzip, &wire, TrailingData::Reject)
            .expect_err("zeros are still trailing data under Reject");
    }

    // One junk byte followed by zeros must not slip through the container's
    // two-byte member lookahead.
    let mut wire = gz.clone();
    wire.extend_from_slice(&[b'X', 0, 0, 0]);
    decode_with_policy(ContentCoding::Gzip, &wire, TrailingData::AllowZeros)
        .expect_err("a non-zero trailing byte must be rejected even under AllowZeros");
}

#[test]
fn ignore_discards_anything_after_the_stream() {
    let plain = b"body followed by junk";
    let gz = oxiarc_deflate::gzip_compress(plain, 6).expect("gzip");
    for tail in [&b"X"[..], &b"XY"[..], &b"HTTP/1.1 200 OK\r\n"[..]] {
        let mut wire = gz.clone();
        wire.extend_from_slice(tail);
        assert_eq!(
            decode_with_policy(ContentCoding::Gzip, &wire, TrailingData::Ignore).expect("ignore"),
            plain
        );
    }
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_trailing_data_follows_the_policy_too() {
    let plain = b"zstd body";
    let z = oxiarc_zstd::compress(plain).expect("zstd");
    let mut wire = z.clone();
    wire.extend_from_slice(b"XYZQ!!");

    let error = decode_body(&[ContentCoding::Zstd], &wire, &DecodeLimits::default())
        .expect_err("trailing data after a zstd stream");
    assert!(matches!(error, HttpCodingError::TrailingGarbage { .. }));

    assert_eq!(
        decode_with_policy(ContentCoding::Zstd, &wire, TrailingData::Ignore).expect("ignore"),
        plain
    );
}

#[cfg(feature = "brotli")]
#[test]
fn br_rejects_trailing_data_whatever_the_policy_says() {
    // Documented on `TrailingData`: RFC 7932 gives Brotli no length field
    // and no checksum, so its final zero-padding check is the only
    // end-of-stream signal, and `oxiarc-brotli` enforces it inside the
    // codec. That is the strict direction, so no policy is weakened.
    let plain = b"brotli body";
    let br = oxiarc_brotli::compress(plain, 4).expect("brotli");
    let mut wire = br.clone();
    wire.extend_from_slice(b"X");
    for policy in [
        TrailingData::Reject,
        TrailingData::AllowZeros,
        TrailingData::Ignore,
    ] {
        decode_with_policy(ContentCoding::Brotli, &wire, policy)
            .expect_err("br always rejects trailing data");
    }
}

// ── chained codings ──────────────────────────────────────────────────────

#[cfg(all(feature = "brotli", feature = "gzip"))]
#[test]
fn chained_codings_are_applied_in_reverse_order() {
    let plain = common::text(12_000);
    // The server applied br first, then gzip: `Content-Encoding: br, gzip`.
    let inner = oxiarc_brotli::compress(&plain, 4).expect("brotli");
    let wire = oxiarc_deflate::gzip_compress(&inner, 6).expect("gzip");

    assert_eq!(
        decode_body_from_header("br, gzip", &wire, &DecodeLimits::default()).expect("br, gzip"),
        plain
    );

    // The reversed label must fail: a chain that decoded correctly under
    // both orders would prove nothing about ordering.
    decode_body_from_header("gzip, br", &wire, &DecodeLimits::default())
        .expect_err("the reversed label must not decode");
}

#[test]
fn a_repeated_coding_is_legal_and_decodes_twice() {
    let plain = common::text(20_000);
    let once = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let twice = oxiarc_deflate::gzip_compress(&once, 6).expect("gzip");
    assert_eq!(
        decode_body_from_header("gzip, gzip", &twice, &DecodeLimits::default()).expect("gzip x2"),
        plain
    );
}

#[cfg(all(feature = "brotli", feature = "gzip", feature = "zstd"))]
#[test]
fn a_three_coding_chain_survives_byte_at_a_time_feeding() {
    let plain = common::text(8_000);
    let a = oxiarc_zstd::compress(&plain).expect("zstd");
    let b = oxiarc_brotli::compress(&a, 4).expect("brotli");
    let wire = oxiarc_deflate::gzip_compress(&b, 6).expect("gzip");

    let codings = [
        ContentCoding::Zstd,
        ContentCoding::Brotli,
        ContentCoding::Gzip,
    ];
    let mut decoder = Decoder::new(&codings, &DecodeLimits::default()).expect("decoder");
    let mut out = Vec::new();
    for byte in &wire {
        decoder.feed_into(&[*byte], &mut out).expect("feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    assert_eq!(out, plain);
    assert_eq!(decoder.codings(), &codings);
}

#[test]
fn identity_in_a_chain_is_dropped_not_applied() {
    let plain = common::text(3_000);
    let wire = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    assert_eq!(
        decode_body_from_header("identity, gzip", &wire, &DecodeLimits::default())
            .expect("identity is a no-op"),
        plain
    );
    assert_eq!(
        decode_body_from_header("gzip, identity", &wire, &DecodeLimits::default())
            .expect("identity is a no-op"),
        plain
    );
}

#[test]
fn an_unknown_coding_is_refused_rather_than_passed_through() {
    // The `ureq` trap: a client that cannot decode a coding must fail
    // loudly, never hand compressed bytes to a JSON parser.
    let wire = oxiarc_deflate::gzip_compress(b"body", 6).expect("gzip");
    let error = decode_body_from_header("gzip, shrink-o-matic", &wire, &DecodeLimits::default())
        .expect_err("an unknown coding must be refused");
    assert!(matches!(error, HttpCodingError::UnsupportedCoding { .. }));

    // `compress` parses but is not implementable yet; it must also refuse.
    decode_body_from_header("compress", b"anything", &DecodeLimits::default())
        .expect_err("compress is recognised but not decodable");
}
