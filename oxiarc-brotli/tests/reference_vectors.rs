//! Always-run reference interop regression vectors.
//!
//! Each `*_BR` constant below is the exact output of the reference `brotli`
//! CLI (version 1.1.0) for the corresponding `*_ORIG` input at the quality
//! (`q`) and window (`w`) noted per fixture. The decoder must reproduce the
//! original bytes exactly — these tests run without any external tooling
//! and permanently pin decode-direction interoperability.
//!
//! The fixture set intentionally spans the format surface: an empty stream,
//! plain ASCII, all-zero input at the minimum window (w10), dictionary-rich
//! English text at q11 (static dictionary references + word transforms),
//! UTF-8 multibyte text (UTF8 context mode), structured binary at q11
//! (multiple block types / context maps), incompressible random bytes
//! (uncompressed meta-blocks inside a compressed stream), and mixed content.
//!
//! Regenerate with the `brotli` CLI if fixtures ever need to change:
//! `brotli -c -q <Q> -w <W> < input > output.br`.

mod common;

use common::unhex;
use common::{
    BINARY_Q11_W18_BR, BINARY_Q11_W18_ORIG, DICTTEXT_Q5_W22_BR, DICTTEXT_Q5_W22_ORIG,
    DICTTEXT_Q11_W22_BR, DICTTEXT_Q11_W22_ORIG, EMPTY_Q1_W22_BR, EMPTY_Q1_W22_ORIG,
    HELLO_Q11_W22_BR, HELLO_Q11_W22_ORIG, MIXED_Q11_W22_BR, MIXED_Q11_W22_ORIG, RANDOM_Q9_W22_BR,
    RANDOM_Q9_W22_ORIG, UTF8_Q9_W22_BR, UTF8_Q9_W22_ORIG, ZEROS256_Q5_W10_BR, ZEROS256_Q5_W10_ORIG,
};
use oxiarc_brotli::decompress;

fn check(name: &str, orig_hex: &str, br_hex: &str) {
    let original = unhex(orig_hex);
    let compressed = unhex(br_hex);
    let decoded = decompress(&compressed)
        .unwrap_or_else(|e| panic!("{name}: reference stream failed to decode: {e}"));
    assert_eq!(
        decoded, original,
        "{name}: decoded bytes differ from original"
    );
}

#[test]
fn test_reference_vector_empty() {
    check("empty_q1_w22", EMPTY_Q1_W22_ORIG, EMPTY_Q1_W22_BR);
}

#[test]
fn test_reference_vector_hello_q11() {
    check("hello_q11_w22", HELLO_Q11_W22_ORIG, HELLO_Q11_W22_BR);
}

#[test]
fn test_reference_vector_zeros_min_window() {
    check("zeros256_q5_w10", ZEROS256_Q5_W10_ORIG, ZEROS256_Q5_W10_BR);
}

#[test]
fn test_reference_vector_dictionary_text_q11() {
    check(
        "dicttext_q11_w22",
        DICTTEXT_Q11_W22_ORIG,
        DICTTEXT_Q11_W22_BR,
    );
}

#[test]
fn test_reference_vector_dictionary_text_q5() {
    check("dicttext_q5_w22", DICTTEXT_Q5_W22_ORIG, DICTTEXT_Q5_W22_BR);
}

#[test]
fn test_reference_vector_utf8_q9() {
    check("utf8_q9_w22", UTF8_Q9_W22_ORIG, UTF8_Q9_W22_BR);
}

#[test]
fn test_reference_vector_binary_q11() {
    check("binary_q11_w18", BINARY_Q11_W18_ORIG, BINARY_Q11_W18_BR);
}

#[test]
fn test_reference_vector_random_q9() {
    check("random_q9_w22", RANDOM_Q9_W22_ORIG, RANDOM_Q9_W22_BR);
}

#[test]
fn test_reference_vector_mixed_q11() {
    check("mixed_q11_w22", MIXED_Q11_W22_ORIG, MIXED_Q11_W22_BR);
}

/// Reference streams with appended garbage must be rejected (RFC 7932
/// Section 10: nothing may follow the last meta-block).
#[test]
fn test_reference_vector_trailing_garbage_rejected() {
    let mut stream = unhex(HELLO_Q11_W22_BR);
    stream.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(decompress(&stream).is_err(), "trailing garbage accepted");
}

/// Every proper prefix of a valid reference stream must fail to decode
/// (truncation can never silently succeed).
#[test]
fn test_reference_vector_truncations_rejected() {
    let stream = unhex(DICTTEXT_Q11_W22_BR);
    for cut in 1..stream.len() {
        assert!(
            decompress(&stream[..cut]).is_err(),
            "truncation to {cut} bytes decoded successfully"
        );
    }
}
