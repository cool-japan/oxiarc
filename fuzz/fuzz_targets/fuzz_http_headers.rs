//! Fuzz target for `oxiarc_http`'s header layer: `parse_content_encoding`,
//! `parse_content_encoding_all`, `parse_accept_encoding`, `QValue::parse` /
//! `parse_param`, `negotiate`, and `AcceptEncoding`'s builder must never
//! panic on arbitrary header text — this is the very first code any HTTP
//! response or request touches before a single byte is decoded.
//!
//! Also pins one round-trip property: every header value this crate's own
//! `AcceptEncoding::to_header_value` emits must parse cleanly with its own
//! `parse_accept_encoding` — a header generator that writes something its
//! own parser rejects is a real (if quiet) interoperability bug.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_http::{
    AcceptEncoding, ContentCoding, QValue, negotiate, parse_accept_encoding,
    parse_content_encoding, parse_content_encoding_all,
};

const CANDIDATES: [ContentCoding; 6] = [
    ContentCoding::Identity,
    ContentCoding::Deflate,
    ContentCoding::Gzip,
    ContentCoding::Brotli,
    ContentCoding::Zstd,
    ContentCoding::Compress,
];

fuzz_target!(|data: &[u8]| {
    // Every byte sequence, valid UTF-8 or not, becomes *some* string —
    // header values are logically byte sequences, and lossy conversion
    // still exercises the parser's handling of stray high bytes without
    // throwing half the corpus away for not being valid UTF-8.
    let text = String::from_utf8_lossy(data);
    let text: &str = &text;

    let _ = parse_content_encoding(text);
    let _ = parse_accept_encoding(text);

    // `parse_content_encoding_all` over a small set of lines split on a
    // byte that never appears inside one token, so multi-header-instance
    // parsing (`Content-Encoding` sent as several header lines) is covered
    // too.
    let lines: Vec<&str> = text.split('\u{0}').collect();
    let _ = parse_content_encoding_all(lines.iter().copied());

    let _ = QValue::parse(text);
    let _ = QValue::parse_param(text);

    let mut unstructured = Unstructured::new(data);
    let available_count = unstructured.arbitrary::<u8>().unwrap_or(0) % 4;
    let mut available = Vec::with_capacity(available_count as usize);
    for _ in 0..available_count {
        let Ok(pick) = unstructured.arbitrary::<u8>() else {
            break;
        };
        available.push(CANDIDATES[(pick as usize) % CANDIDATES.len()].clone());
    }
    let _ = negotiate(Some(text), &available);
    let _ = negotiate(None, &available);

    // Build an `AcceptEncoding` from a handful of arbitrary (coding,
    // q-value) pairs, then prove the emitted header round-trips through
    // this crate's own parser.
    let mut accept = AcceptEncoding::new();
    let pair_count = unstructured.arbitrary::<u8>().unwrap_or(0) % 5;
    for _ in 0..pair_count {
        let Ok(pick) = unstructured.arbitrary::<u8>() else {
            break;
        };
        let Ok(q_raw) = unstructured.arbitrary::<u16>() else {
            break;
        };
        // `QValue` is thousandths, 0..=1000; wrap into range rather than
        // rejecting so every input byte still contributes entropy.
        let q_thousandths = q_raw % 1001;
        let Some(q) = QValue::parse(&format!(
            "{}.{:03}",
            q_thousandths / 1000,
            q_thousandths % 1000
        )) else {
            continue;
        };
        let coding = CANDIDATES[(pick as usize) % CANDIDATES.len()].clone();
        accept = accept.with_q(coding, q);
    }

    if let Some(header) = accept.to_header_value() {
        let reparsed = parse_accept_encoding(&header);
        assert!(
            reparsed.is_ok(),
            "AcceptEncoding::to_header_value produced a header its own \
             parse_accept_encoding rejects: {header:?} ({reparsed:?})"
        );
    }
    let _ = accept.to_header_value_or_empty();
});
