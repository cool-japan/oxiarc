//! Decompression-bomb and resource limits.
//!
//! The headline test is [`the_single_block_bomb_is_stopped_mid_block`]: it
//! regenerates, from a committed Rust generator, the exact stream the design
//! report measured (812 KB of DEFLATE → 123 MiB of output, **in one block**)
//! and proves that a cap of 1 MiB stops it having produced 1 MiB, not
//! 123 MiB. A decoder that checks its budget between blocks passes every
//! other test in this file and fails that one.

#![cfg(feature = "gzip")]

mod common;

use oxiarc_http::{ContentCoding, DecodeLimits, Decoder, HttpCodingError, LimitKind, decode_body};

/// The report's figures, re-derived here so the arithmetic is auditable.
const REPORT_BOMB_OUTPUT: usize = 123 * 1024 * 1024;

fn limit_kind(error: &HttpCodingError) -> Option<LimitKind> {
    match error {
        HttpCodingError::LimitExceeded { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[test]
fn the_generator_really_produces_one_block_and_the_reported_ratio() {
    // A small instance, decoded in full, proves the generator is a valid
    // DEFLATE stream and that its expansion factor is what the design rests
    // on.
    let plain_len = 258 * 400 + 1;
    let raw = common::single_block_bomb(plain_len);
    assert_eq!(common::single_block_bomb_output_len(plain_len), plain_len);

    // One block: BFINAL is the first bit and BTYPE the next two (`01`,
    // fixed Huffman), so byte 0's low three bits are 0b011 = 3.
    assert_eq!(
        raw[0] & 0b0000_0111,
        0b011,
        "not a single final fixed block"
    );

    let decoded = oxiarc_deflate::inflate(&raw).expect("the generator emits valid DEFLATE");
    assert_eq!(decoded.len(), plain_len);
    assert!(decoded.iter().all(|&b| b == 0));

    let ratio = decoded.len() as f64 / raw.len() as f64;
    assert!(
        (150.0..165.0).contains(&ratio),
        "expansion factor {ratio:.1} is not the ~158.8x the design report measured"
    );

    // And the report's own instance: 123 MiB out of ~812 KB in, one block.
    let big = common::single_block_bomb(REPORT_BOMB_OUTPUT);
    assert!(
        (780_000..840_000).contains(&big.len()),
        "the 123 MiB single-block bomb should be ~812 KB, got {}",
        big.len()
    );
}

#[test]
fn the_single_block_bomb_is_stopped_mid_block() {
    let raw = common::single_block_bomb(REPORT_BOMB_OUTPUT);
    let wire = common::gzip_wrap(&raw, &[]); // the trailer is never reached
    let cap = 1024 * 1024;
    let limits = DecodeLimits::default()
        .with_max_output(cap)
        .with_max_ratio(None); // isolate the output cap from the ratio guard

    let mut decoder = Decoder::new(&[ContentCoding::Gzip], &limits).expect("decoder");
    let mut out = Vec::new();
    let error = decoder
        .feed_into(&wire, &mut out)
        .expect_err("a 123 MiB single block under a 1 MiB cap must be refused");

    assert!(
        matches!(limit_kind(&error), Some(LimitKind::Output { .. })),
        "wrong error: {error}"
    );
    // The cap held *inside* the block: nothing beyond the budget was ever
    // materialised.
    assert!(
        out.len() as u64 <= cap,
        "materialised {} bytes under a {cap}-byte cap",
        out.len()
    );
    assert!(
        decoder.output_len() <= cap,
        "the decoder produced {} bytes under a {cap}-byte cap",
        decoder.output_len()
    );
}

#[test]
fn the_single_block_bomb_is_stopped_mid_block_through_the_read_adapter_too() {
    // The cap has to hold on the *streaming* path as well, and for the same
    // reason: `DecodedBody` hands the stage a 64 KiB slice, `LimitedSink`
    // truncates it to the remaining budget, and the block decoder never gets
    // room for the 123rd mebibyte. Driving the bomb only through `feed_into`
    // would leave the adapter every real client uses unmeasured.
    use std::io::Read;

    let raw = common::single_block_bomb(REPORT_BOMB_OUTPUT);
    let wire = common::gzip_wrap(&raw, &[]);
    let cap = 1024 * 1024;
    let limits = DecodeLimits::default()
        .with_max_output(cap)
        .with_max_ratio(None);

    let mut body =
        oxiarc_http::DecodedBody::with_codings(&wire[..], &[ContentCoding::Gzip], &limits)
            .expect("decoder");
    let mut sink = [0u8; 8192];
    let mut total = 0u64;
    let error = loop {
        match body.read(&mut sink) {
            Ok(0) => panic!("the bomb reached end of body without tripping the cap"),
            Ok(n) => {
                total += n as u64;
                assert!(
                    total <= cap,
                    "materialised {total} bytes under a {cap}-byte cap"
                );
            }
            Err(error) => break error,
        }
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let source = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<HttpCodingError>())
        .expect("the io::Error wraps the HttpCodingError");
    assert!(
        matches!(limit_kind(source), Some(LimitKind::Output { .. })),
        "wrong error: {source}"
    );
    assert!(
        body.output_len() <= cap,
        "the decoder produced {} bytes under a {cap}-byte cap",
        body.output_len()
    );
}

#[test]
fn a_classic_multi_block_bomb_is_stopped_too() {
    let plain = vec![0u8; 32 * 1024 * 1024];
    let wire = oxiarc_deflate::gzip_compress(&plain, 9).expect("gzip");
    let cap = 1024 * 1024;
    let error = decode_body(
        &[ContentCoding::Gzip],
        &wire,
        &DecodeLimits::default()
            .with_max_output(cap)
            .with_max_ratio(None),
    )
    .expect_err("a 32 MiB body under a 1 MiB cap must be refused");
    assert!(matches!(limit_kind(&error), Some(LimitKind::Output { .. })));
}

#[test]
fn max_output_exactly_at_the_body_size_succeeds_and_one_less_fails() {
    let plain = common::text(40_000);
    for coding in [
        ContentCoding::Gzip,
        ContentCoding::Deflate,
        ContentCoding::Brotli,
        ContentCoding::Zstd,
    ]
    .into_iter()
    .filter(ContentCoding::is_decodable)
    {
        let wire = match coding {
            ContentCoding::Gzip => oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip"),
            ContentCoding::Deflate => oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib"),
            #[cfg(feature = "brotli")]
            ContentCoding::Brotli => oxiarc_brotli::compress(&plain, 4).expect("brotli"),
            #[cfg(feature = "zstd")]
            ContentCoding::Zstd => oxiarc_zstd::compress(&plain).expect("zstd"),
            other => panic!("no encoder for {other}"),
        };

        let exact = DecodeLimits::default()
            .with_max_output(plain.len() as u64)
            .with_max_ratio(None);
        let decoded = decode_body(std::slice::from_ref(&coding), &wire, &exact)
            .unwrap_or_else(|e| panic!("{coding}: an exact cap must not fire: {e}"));
        assert_eq!(decoded, plain, "{coding}");

        let short = DecodeLimits::default()
            .with_max_output(plain.len() as u64 - 1)
            .with_max_ratio(None);
        let error = decode_body(std::slice::from_ref(&coding), &wire, &short)
            .expect_err("one byte under the body size must be refused");
        assert!(
            matches!(limit_kind(&error), Some(LimitKind::Output { .. })),
            "{coding}: wrong error: {error}"
        );
    }
}

#[test]
fn the_ratio_guard_trips_on_a_high_ratio_body() {
    // ~2 MiB of zeros: past the 1 MiB ratio grace, and a ratio far above 100.
    let plain = vec![0u8; 2 * 1024 * 1024];
    let wire = oxiarc_deflate::gzip_compress(&plain, 9).expect("gzip");
    let limits = DecodeLimits::default().with_max_ratio(Some(100.0));
    let error = decode_body(&[ContentCoding::Gzip], &wire, &limits)
        .expect_err("a 1000x body must trip a 100x guard");
    assert!(
        matches!(limit_kind(&error), Some(LimitKind::Ratio { .. })),
        "wrong error: {error}"
    );
}

#[test]
fn the_ratio_guard_is_silent_below_the_grace_window() {
    // A tiny body whose ratio is nominally enormous (a gzip header alone is
    // 10 bytes of input for 0 bytes of output) must not trip anything.
    let plain = common::text(600);
    let wire = oxiarc_deflate::gzip_compress(&plain, 9).expect("gzip");
    let limits = DecodeLimits::default().with_max_ratio(Some(1.5));
    let decoded = decode_body(&[ContentCoding::Gzip], &wire, &limits)
        .expect("below the 1 MiB grace window the ratio guard must stay quiet");
    assert_eq!(decoded, plain);
}

#[test]
fn a_legitimate_high_ratio_body_passes_the_defaults() {
    // The design report measured legitimate traffic at up to 411x. A 4 MiB
    // repetitive JSON body sits in that class and must decode under the
    // shipped defaults, or the defaults are too tight to use.
    let plain = common::json(4 * 1024 * 1024);
    let wire = oxiarc_deflate::gzip_compress(&plain, 9).expect("gzip");
    let ratio = plain.len() as f64 / wire.len() as f64;
    let decoded = decode_body(&[ContentCoding::Gzip], &wire, &DecodeLimits::default())
        .unwrap_or_else(|e| panic!("a legitimate {ratio:.0}x body was refused: {e}"));
    assert_eq!(decoded, plain);
}

#[test]
fn too_many_chained_codings_is_refused_before_any_decoding() {
    let limits = DecodeLimits::default().with_max_codings(4);
    let error = Decoder::new(&vec![ContentCoding::Gzip; 5], &limits)
        .expect_err("5 codings under a limit of 4");
    assert!(matches!(
        limit_kind(&error),
        Some(LimitKind::Codings { count: 5 })
    ));

    // The header parser enforces the same default independently, because it
    // takes no `DecodeLimits`.
    let error = Decoder::from_header("gzip, gzip, gzip, gzip, gzip", &DecodeLimits::default())
        .expect_err("5 codings in a header");
    assert!(matches!(
        limit_kind(&error),
        Some(LimitKind::Codings { .. })
    ));
}

#[test]
fn each_stage_of_a_chain_carries_its_own_budget() {
    // `gzip, gzip` where the *intermediate* body is the bomb: the inner
    // stream expands hugely, then re-compresses to almost nothing. A single
    // shared budget measured only on the final output would miss it.
    let inner_plain = vec![0u8; 8 * 1024 * 1024];
    let inner = oxiarc_deflate::gzip_compress(&inner_plain, 9).expect("gzip");
    let outer = oxiarc_deflate::gzip_compress(&inner, 9).expect("gzip");

    let limits = DecodeLimits::default()
        .with_max_output(64 * 1024)
        .with_max_ratio(None);
    let error = decode_body(&[ContentCoding::Gzip, ContentCoding::Gzip], &outer, &limits)
        .expect_err("an intermediate stage is a bomb too");
    assert!(matches!(limit_kind(&error), Some(LimitKind::Output { .. })));
}

#[cfg(feature = "zstd")]
#[test]
fn a_frame_declaring_an_oversized_window_is_refused_before_allocation() {
    // A hand-built zstd frame header declaring a 128 MiB window. Nothing is
    // allocated: `oxiarc-zstd` checks the descriptor against its 8 MiB
    // ceiling — the largest window an HTTP `zstd` decoder must support —
    // before the ring is sized.
    //
    // Frame_Header_Descriptor 0x00: FCS_Field_Size 0, not single-segment,
    // no checksum, no dictionary id. Window_Descriptor 0x88: exponent 17,
    // mantissa 0 => 1 << (10 + 17) = 128 MiB.
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x88];
    frame.extend_from_slice(&[0x00; 16]);

    let error = decode_body(&[ContentCoding::Zstd], &frame, &DecodeLimits::default())
        .expect_err("a 128 MiB declared window must be refused");
    assert!(
        matches!(limit_kind(&error), Some(LimitKind::Window { declared }) if declared >= 128 * 1024 * 1024),
        "wrong error: {error}"
    );
}

#[cfg(feature = "zstd")]
#[test]
fn a_declared_content_size_over_the_cap_is_refused_before_decoding() {
    let plain = vec![7u8; 4 * 1024 * 1024];
    let wire = oxiarc_zstd::compress(&plain).expect("zstd");
    let limits = DecodeLimits::default()
        .with_max_output(1024)
        .with_max_ratio(None);
    let error = decode_body(&[ContentCoding::Zstd], &wire, &limits)
        .expect_err("a declared 4 MiB frame under a 1 KiB cap must be refused");
    assert!(matches!(limit_kind(&error), Some(LimitKind::Output { .. })));
}

#[test]
fn unlimited_really_removes_the_cap() {
    let plain = vec![0u8; 96 * 1024 * 1024];
    let wire = oxiarc_deflate::gzip_compress(&plain, 9).expect("gzip");
    // 96 MiB is over the 64 MiB default and over the default 1000x ratio.
    decode_body(&[ContentCoding::Gzip], &wire, &DecodeLimits::default())
        .expect_err("the shipped defaults must refuse a 96 MiB body");
    let decoded =
        decode_body(&[ContentCoding::Gzip], &wire, &DecodeLimits::unlimited()).expect("unlimited");
    assert_eq!(decoded.len(), plain.len());
}
