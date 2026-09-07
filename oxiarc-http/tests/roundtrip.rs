//! Every coding, every payload, through every entry point — and all four
//! must agree byte for byte.
//!
//! The four are deliberately different shapes: the one-shot
//! [`decode_body`], the push [`Decoder`] fed in one piece, the push
//! `Decoder` fed in small chunks, and the pull [`DecodedBody`]. A bug that
//! lives in the chain pump shows up in all four; a bug that lives in one
//! adapter's end-of-stream handling shows up in exactly one.

mod common;

use oxiarc_http::{ContentCoding, DecodeLimits, DecodedBody, Decoder, decode_body};

/// Encode `plain` with `coding` using the matching `oxiarc` encoder.
fn encode(coding: &ContentCoding, plain: &[u8]) -> Vec<u8> {
    match coding {
        ContentCoding::Gzip => oxiarc_deflate::gzip_compress(plain, 6).expect("gzip"),
        ContentCoding::Deflate => oxiarc_deflate::zlib_compress(plain, 6).expect("zlib"),
        #[cfg(feature = "brotli")]
        ContentCoding::Brotli => oxiarc_brotli::compress(plain, 4).expect("brotli"),
        #[cfg(feature = "zstd")]
        ContentCoding::Zstd => oxiarc_zstd::compress(plain).expect("zstd"),
        ContentCoding::Identity => plain.to_vec(),
        other => panic!("no encoder wired for {other}"),
    }
}

/// The codings this build can exercise end to end.
fn codings() -> Vec<ContentCoding> {
    let mut list = vec![ContentCoding::Identity];
    if ContentCoding::Gzip.is_decodable() {
        list.push(ContentCoding::Gzip);
    }
    if ContentCoding::Deflate.is_decodable() {
        list.push(ContentCoding::Deflate);
    }
    if ContentCoding::Brotli.is_decodable() {
        list.push(ContentCoding::Brotli);
    }
    if ContentCoding::Zstd.is_decodable() {
        list.push(ContentCoding::Zstd);
    }
    list
}

/// Decode through the push `Decoder`, feeding `chunk` bytes at a time.
fn push_decode(coding: &ContentCoding, wire: &[u8], chunk: usize) -> Vec<u8> {
    let mut decoder = Decoder::new(std::slice::from_ref(coding), &DecodeLimits::default())
        .expect("build decoder");
    let mut out = Vec::new();
    for piece in wire.chunks(chunk.max(1)) {
        decoder.feed_into(piece, &mut out).expect("feed");
        // A decoder must tolerate an empty feed between real ones.
        decoder.feed_into(&[], &mut out).expect("empty feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    assert!(decoder.is_finished());
    out
}

#[test]
fn every_coding_and_payload_agrees_across_every_entry_point() {
    for coding in codings() {
        for (label, plain) in common::payloads() {
            let wire = encode(&coding, &plain);
            let context = format!("{coding} / {label}");

            let one_shot = decode_body(
                std::slice::from_ref(&coding),
                &wire,
                &DecodeLimits::default(),
            )
            .unwrap_or_else(|e| panic!("{context}: one-shot decode failed: {e}"));
            assert_eq!(one_shot, plain, "{context}: one-shot mismatch");

            let whole = push_decode(&coding, &wire, wire.len().max(1));
            assert_eq!(whole, plain, "{context}: push (one chunk) mismatch");

            let chunked = push_decode(&coding, &wire, 7);
            assert_eq!(chunked, plain, "{context}: push (7-byte chunks) mismatch");

            let mut body = DecodedBody::with_codings(
                &wire[..],
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .unwrap_or_else(|e| panic!("{context}: DecodedBody build failed: {e}"));
            let pulled = body
                .read_to_vec()
                .unwrap_or_else(|e| panic!("{context}: DecodedBody read failed: {e}"));
            assert_eq!(pulled, plain, "{context}: DecodedBody mismatch");
            assert!(body.is_finished(), "{context}: DecodedBody never closed");
        }
    }
}

#[test]
fn feed_and_feed_into_produce_the_same_bytes() {
    for coding in codings() {
        let plain = common::text(9_000);
        let wire = encode(&coding, &plain);

        let mut a =
            Decoder::new(std::slice::from_ref(&coding), &DecodeLimits::default()).expect("decoder");
        let mut from_feed = Vec::new();
        for piece in wire.chunks(64) {
            from_feed.extend(a.feed(piece).expect("feed"));
        }
        from_feed.extend(a.finish().expect("finish"));

        let from_feed_into = push_decode(&coding, &wire, 64);
        assert_eq!(from_feed, from_feed_into, "{coding}: feed vs feed_into");
        assert_eq!(from_feed, plain, "{coding}: feed mismatch");
    }
}

#[test]
fn output_and_input_counters_track_the_body() {
    for coding in codings() {
        let plain = common::text(5_000);
        let wire = encode(&coding, &plain);
        let mut decoder =
            Decoder::new(std::slice::from_ref(&coding), &DecodeLimits::default()).expect("decoder");
        let mut out = Vec::new();
        decoder.feed_into(&wire, &mut out).expect("feed");
        decoder.finish_into(&mut out).expect("finish");
        assert_eq!(
            decoder.output_len(),
            plain.len() as u64,
            "{coding}: output_len"
        );
        assert_eq!(
            decoder.input_len(),
            wire.len() as u64,
            "{coding}: input_len"
        );
    }
}

#[test]
fn a_response_with_no_content_encoding_header_passes_through() {
    let plain = common::pseudo_random(70_000);
    let out = decode_body(&[], &plain, &DecodeLimits::unlimited()).expect("identity");
    assert_eq!(out, plain);

    let mut body = DecodedBody::identity(&plain[..]);
    assert_eq!(body.read_to_vec().expect("identity"), plain);
}

#[test]
fn a_truncated_body_is_an_error_for_every_coding() {
    for coding in codings() {
        if coding == ContentCoding::Identity {
            continue; // identity has no framing to truncate
        }
        let plain = common::text(20_000);
        let wire = encode(&coding, &plain);
        for cut in [1usize, 3, 8] {
            let truncated = &wire[..wire.len() - cut];
            let result = decode_body(
                std::slice::from_ref(&coding),
                truncated,
                &DecodeLimits::default(),
            );
            assert!(
                result.is_err(),
                "{coding}: truncating {cut} byte(s) must be an error, not a short body"
            );
        }
    }
}

#[test]
fn an_empty_body_is_an_error_for_every_real_coding() {
    for coding in codings() {
        if coding == ContentCoding::Identity {
            continue;
        }
        let result = decode_body(std::slice::from_ref(&coding), b"", &DecodeLimits::default());
        assert!(
            result.is_err(),
            "{coding}: an empty body is not a valid stream (204/304/HEAD must not reach a decoder)"
        );
    }
}

#[test]
fn a_corrupted_payload_byte_never_decodes_to_the_original() {
    for coding in codings() {
        if coding == ContentCoding::Identity {
            continue;
        }
        let plain = common::text(20_000);
        let mut wire = encode(&coding, &plain);
        // Flip a bit in the middle of the compressed payload.
        let middle = wire.len() / 2;
        wire[middle] ^= 0x40;
        let result = decode_body(
            std::slice::from_ref(&coding),
            &wire,
            &DecodeLimits::default(),
        );
        // gzip (CRC-32), deflate (Adler-32) and zstd (XXH64) all carry an
        // integrity check, so corruption must be an error. `br` carries
        // none — RFC 7932 has no checksum and no length field — so a flipped
        // bit legitimately yields a *different, well-formed* body. What must
        // never happen for any coding is a silent return of the original.
        match result {
            Err(_) => {}
            Ok(decoded) => {
                assert_eq!(
                    coding,
                    ContentCoding::Brotli,
                    "{coding}: a flipped payload bit decoded without an error"
                );
                assert_ne!(
                    decoded, plain,
                    "br: a flipped bit reproduced the original body exactly"
                );
            }
        }
    }
}

#[cfg(feature = "gzip")]
#[test]
fn x_gzip_is_an_alias_for_gzip() {
    let plain = common::text(3_000);
    let wire = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    for header in ["gzip", "x-gzip", "X-GZIP", "GZip"] {
        let out = oxiarc_http::decode_body_from_header(header, &wire, &DecodeLimits::default())
            .unwrap_or_else(|e| panic!("{header}: {e}"));
        assert_eq!(out, plain, "{header}");
    }
}
