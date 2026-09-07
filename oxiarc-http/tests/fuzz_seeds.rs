//! The invariants the seven fuzz targets assert, run against the committed
//! seed corpus.
//!
//! The targets live in the workspace `fuzz/` crate; the seeds come from
//! `examples/fuzz_seeds.rs`. This file re-derives the same seed families and
//! checks the same invariants, so:
//!
//! * an invariant that stops holding fails here, in `cargo test`, rather
//!   than only under a fuzzer someone remembers to run;
//! * a seed that stops reaching its interesting state (a "valid" fixture
//!   that no longer decodes, say) is caught immediately.
//!
//! The invariants themselves are deliberately weak — no panic, and bounds
//! that must hold for *any* input — because that is what a fuzz target can
//! assert. The strong assertions live in the other test files.

#![cfg(all(feature = "gzip", feature = "deflate"))]

mod common;

use oxiarc_http::{
    AcceptEncoding, ContentCoding, DecodeLimits, Decoder, QValue, negotiate, parse_accept_encoding,
    parse_content_encoding,
};

const HEADERS: &[&str] = &[
    "compress, gzip",
    "",
    "*",
    "compress;q=0.5, gzip;q=1.0",
    "gzip;q=1.0, identity; q=0.5, *;q=0",
    "gzip,,,br",
    "foo , ,bar,charlie",
    "GZIP, Br, X-GZIP",
    "gzip;Q=0.5",
    "gzip;q=0.0001",
    "gzip;q=1.5",
    "gzip;q=abc",
    "*;q=0, identity;q=0",
    "*;q=0.5, gzip;q=0.1",
    "gzip;q=0.5, gzip;q=0.9",
    "br;q=1, zstd;q=1, gzip;q=1, deflate;q=1, identity;q=0",
    "\u{feff}gzip",
    "gzip;q=0.5;extra=1",
];

const CONTENT_ENCODINGS: &[&str] = &[
    "gzip",
    "x-gzip",
    "deflate",
    "br",
    "zstd",
    "dcb",
    "dcz",
    "identity",
    "",
    "gzip, br",
    "br, gzip",
    "gzip, gzip",
    "identity, gzip",
    "gzip, unknown",
    "gzip, gzip, gzip, gzip, gzip",
    "compress",
    "GZIP , Br",
    "gzip;charset=utf-8",
];

const QVALUES: &[&str] = &[
    "0",
    "0.0",
    "0.000",
    "1",
    "1.0",
    "1.000",
    "0.5",
    "0.001",
    "0.999",
    "1.001",
    "1.5",
    "0.0001",
    "-0.5",
    "abc",
    "",
    ".",
    "0.",
    "q=0.5",
    "Q=0.5",
    " q = 0.5 ",
];

/// `fuzz_http_parse_accept_encoding`: never panics, and every q value is in
/// range.
#[test]
fn accept_encoding_seeds_hold_their_invariants() {
    for header in HEADERS {
        if let Ok(entries) = parse_accept_encoding(header) {
            for entry in entries {
                assert!(entry.qvalue.as_thousandths() <= 1000, "{header}");
            }
        }
    }
    // The DoS bound must fire rather than allocate.
    let hostile = ",".repeat(65);
    parse_accept_encoding(&hostile).expect_err("65 elements must be refused");
}

/// `fuzz_http_parse_content_encoding`: never panics, and never yields
/// `identity` (which RFC 9110 §8.4 says must not appear and this crate
/// drops).
#[test]
fn content_encoding_seeds_hold_their_invariants() {
    for header in CONTENT_ENCODINGS {
        if let Ok(codings) = parse_content_encoding(header) {
            assert!(codings.len() <= 4, "{header}: {} codings", codings.len());
            for coding in &codings {
                assert_ne!(*coding, ContentCoding::Identity, "{header}");
            }
        }
    }
}

/// `fuzz_http_qvalue`: never panics, result always in `0..=1000`.
#[test]
fn qvalue_seeds_hold_their_invariants() {
    for value in QVALUES {
        if let Some(q) = QValue::parse(value) {
            assert!(q.as_thousandths() <= 1000, "{value}");
        }
        if let Some(q) = QValue::parse_param(value) {
            assert!(q.as_thousandths() <= 1000, "{value}");
        }
    }
}

/// Drive a body through the decoder, asserting the two invariants a decode
/// fuzz target can check: it terminates, and it never exceeds `max_output`.
fn decode_bounded(coding: ContentCoding, wire: &[u8], cap: u64) {
    let limits = DecodeLimits::default()
        .with_max_output(cap)
        .with_max_ratio(None);
    let Ok(mut decoder) = Decoder::new(&[coding], &limits) else {
        return;
    };
    let mut out = Vec::new();
    let mut calls = 0usize;
    for chunk in wire.chunks(64) {
        calls += 1;
        assert!(calls < 1_000_000, "decoder did not terminate");
        if decoder.feed_into(chunk, &mut out).is_err() {
            return;
        }
        assert!(out.len() as u64 <= cap, "output exceeded the cap mid-body");
    }
    let _ = decoder.finish_into(&mut out);
    assert!(out.len() as u64 <= cap, "output exceeded the cap");
}

/// `fuzz_http_decoder_gzip`: no panic, no cap violation, on every gzip seed
/// shape — and the "ok" seeds must still decode, or the corpus has rotted.
#[test]
fn gzip_seeds_hold_their_invariants() {
    let plain = common::text(2_048);
    let ok = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let small = oxiarc_deflate::gzip_compress(b"hi", 6).expect("gzip");

    let mut multi = ok.clone();
    multi.extend_from_slice(&small);
    let mut trailing = ok.clone();
    trailing.extend_from_slice(b"XX");
    let mut bad_crc = ok.clone();
    let at = bad_crc.len() - 8;
    bad_crc[at] ^= 0x01;
    let mut reserved = ok.clone();
    reserved[3] |= 0x20;
    let bomb = common::gzip_wrap(&common::single_block_bomb(4 * 1024 * 1024), &[]);

    // Valid seeds must still be valid.
    assert_eq!(
        oxiarc_http::decode_body(&[ContentCoding::Gzip], &ok, &DecodeLimits::default())
            .expect("the `ok` seed must decode"),
        plain
    );
    assert_eq!(
        oxiarc_http::decode_body(&[ContentCoding::Gzip], &multi, &DecodeLimits::default())
            .expect("the multi-member seed must decode")
            .len(),
        plain.len() + 2
    );

    for wire in [
        ok.clone(),
        small,
        multi,
        trailing,
        bad_crc,
        reserved,
        ok[..ok.len() / 2].to_vec(),
        ok[..ok.len() - 3].to_vec(),
        ok[..10].to_vec(),
        vec![0x1F, 0x8B],
        Vec::new(),
        bomb,
    ] {
        decode_bounded(ContentCoding::Gzip, &wire, 64 * 1024);
    }
}

/// `fuzz_http_decoder_deflate`: the sniffer's three spellings plus the
/// crafted collision, all bounded and panic-free.
#[test]
fn deflate_seeds_hold_their_invariants() {
    let plain = common::text(2_048);
    let zlib = oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib");
    let raw = common::raw_deflate(&plain);
    let gz = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let fdict =
        oxiarc_deflate::zlib_compress_with_dict(&plain, 6, b"the quick brown fox").expect("dict");
    let mut collision = vec![0x08, 0x1D, 0x00, 0xE2, 0xFF];
    collision.extend_from_slice(&[b'z'; 29]);
    collision.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);

    for wire in [&zlib, &raw, &gz] {
        assert_eq!(
            oxiarc_http::decode_body(&[ContentCoding::Deflate], wire, &DecodeLimits::default())
                .expect("every `deflate` spelling must decode"),
            plain
        );
    }

    for wire in [
        zlib.clone(),
        raw.clone(),
        gz,
        fdict,
        collision,
        zlib[..zlib.len() / 3].to_vec(),
        raw[..raw.len() / 3].to_vec(),
        vec![0x78, 0x9C],
        vec![0x78],
        Vec::new(),
    ] {
        decode_bounded(ContentCoding::Deflate, &wire, 64 * 1024);
    }
}

/// `fuzz_http_decoder_chunked`: the strongest streaming invariant — the
/// decoded bytes do not depend on how the wire bytes were split.
#[test]
fn chunked_seeds_hold_the_invariance_property() {
    let plain = common::text(6_000);
    for (label, wire, coding) in [
        (
            "gzip",
            oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip"),
            ContentCoding::Gzip,
        ),
        (
            "zlib",
            oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib"),
            ContentCoding::Deflate,
        ),
        ("raw", common::raw_deflate(&plain), ContentCoding::Deflate),
    ] {
        let mut reference: Option<Vec<u8>> = None;
        for plan in [vec![1usize], vec![3], vec![1, 7, 64, 2, 255], vec![9999]] {
            let mut decoder = Decoder::new(std::slice::from_ref(&coding), &DecodeLimits::default())
                .expect("decoder");
            let mut out = Vec::new();
            let mut pos = 0usize;
            let mut step = plan.iter().copied().cycle();
            while pos < wire.len() {
                let n = step.next().unwrap_or(1).max(1).min(wire.len() - pos);
                decoder
                    .feed_into(&wire[pos..pos + n], &mut out)
                    .expect("feed");
                pos += n;
            }
            decoder.finish_into(&mut out).expect("finish");
            match &reference {
                None => reference = Some(out),
                Some(expected) => {
                    assert_eq!(&out, expected, "{label}: chunking changed the output")
                }
            }
        }
        assert_eq!(reference.as_deref(), Some(&plain[..]), "{label}");
    }
}

/// `fuzz_http_negotiate`: the result is always `None` or a member of
/// `available`, and it never panics.
#[test]
fn negotiate_seeds_hold_their_invariants() {
    let sets: [Vec<ContentCoding>; 4] = [
        vec![ContentCoding::Gzip, ContentCoding::Deflate],
        Vec::new(),
        vec![
            ContentCoding::Brotli,
            ContentCoding::Zstd,
            ContentCoding::Gzip,
        ],
        vec![ContentCoding::Identity],
    ];
    for header in HEADERS {
        for available in &sets {
            match negotiate(Some(header), available) {
                Ok(None) => {}
                Ok(Some(chosen)) => assert!(
                    available.contains(&chosen),
                    "{header}: chose {chosen}, which is not available"
                ),
                Err(_) => {}
            }
        }
        // The absent-header case is a distinct rule (§12.5.3 rule 1).
        for available in &sets {
            let _ = negotiate(None, available);
        }
    }
}

/// A round-trip property no fuzz target covers but every seed depends on:
/// what `AcceptEncoding` renders, `parse_accept_encoding` accepts.
#[test]
fn the_advertised_header_parses_back() {
    let accept = AcceptEncoding::all_supported();
    let rendered = accept.to_header_value_or_empty();
    let parsed = parse_accept_encoding(&rendered)
        .unwrap_or_else(|e| panic!("{rendered:?} must parse back: {e}"));
    assert_eq!(parsed.len(), accept.codings().len(), "{rendered:?}");
}
