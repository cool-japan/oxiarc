//! Chunk invariance: **any** split of the wire bytes must yield identical
//! decoded output.
//!
//! This is the property that separates a real streaming decoder from one
//! that quietly buffers to EOF, and it is the class of bug the 0.3.6
//! campaign found across `oxiarc-deflate`'s stream wrappers (silent
//! truncation past 32 KiB, broken bit-continuity across calls). The
//! corresponding fuzz target — `fuzz_http_decoder_chunked` — is seeded from
//! `examples/fuzz_seeds.rs`.

mod common;

use oxiarc_http::{ContentCoding, DecodeLimits, DecodedBody, Decoder, FlushMode, decode_body};
use proptest::prelude::*;

fn encode(coding: &ContentCoding, plain: &[u8]) -> Vec<u8> {
    match coding {
        ContentCoding::Gzip => oxiarc_deflate::gzip_compress(plain, 6).expect("gzip"),
        ContentCoding::Deflate => oxiarc_deflate::zlib_compress(plain, 6).expect("zlib"),
        #[cfg(feature = "brotli")]
        ContentCoding::Brotli => oxiarc_brotli::compress(plain, 4).expect("brotli"),
        #[cfg(feature = "zstd")]
        ContentCoding::Zstd => oxiarc_zstd::compress(plain).expect("zstd"),
        #[cfg(feature = "compress")]
        ContentCoding::Compress => oxiarc_lzw::z::compress(plain, 16).expect("compress"),
        ContentCoding::Identity => plain.to_vec(),
        other => panic!("no encoder wired for {other}"),
    }
}

fn codings() -> Vec<ContentCoding> {
    [
        ContentCoding::Identity,
        ContentCoding::Gzip,
        ContentCoding::Deflate,
        ContentCoding::Brotli,
        ContentCoding::Zstd,
        ContentCoding::Compress,
    ]
    .into_iter()
    .filter(ContentCoding::is_decodable)
    .collect()
}

/// Feed `wire` in pieces of `chunk` bytes through the push decoder.
fn decode_in_chunks(coding: &ContentCoding, wire: &[u8], chunk: usize) -> Vec<u8> {
    let mut decoder =
        Decoder::new(std::slice::from_ref(coding), &DecodeLimits::default()).expect("decoder");
    let mut out = Vec::new();
    for piece in wire.chunks(chunk.max(1)) {
        decoder.feed_into(piece, &mut out).expect("feed");
    }
    decoder.finish_into(&mut out).expect("finish");
    out
}

/// Feed `wire` at the given split points through the push decoder.
fn decode_at_splits(coding: &ContentCoding, wire: &[u8], splits: &[usize]) -> Vec<u8> {
    let mut decoder =
        Decoder::new(std::slice::from_ref(coding), &DecodeLimits::default()).expect("decoder");
    let mut out = Vec::new();
    let mut previous = 0usize;
    for &split in splits {
        let end = split.min(wire.len()).max(previous);
        decoder
            .feed_into(&wire[previous..end], &mut out)
            .expect("feed");
        previous = end;
    }
    decoder
        .feed_into(&wire[previous..], &mut out)
        .expect("feed");
    decoder.finish_into(&mut out).expect("finish");
    out
}

#[test]
fn byte_at_a_time_matches_one_shot() {
    let plain = common::text(30_000);
    for coding in codings() {
        let wire = encode(&coding, &plain);
        let expected = decode_body(
            std::slice::from_ref(&coding),
            &wire,
            &DecodeLimits::default(),
        )
        .expect("one shot");
        assert_eq!(expected, plain, "{coding}");
        assert_eq!(
            decode_in_chunks(&coding, &wire, 1),
            plain,
            "{coding}: byte-at-a-time"
        );
    }
}

#[test]
fn every_chunk_size_yields_identical_output() {
    let plain = common::json(50_000);
    for coding in codings() {
        let wire = encode(&coding, &plain);
        let sizes = [
            1,
            2,
            3,
            7,
            16,
            255,
            4096,
            wire.len().saturating_sub(1).max(1),
            wire.len().max(1),
            wire.len() + 1,
        ];
        for size in sizes {
            assert_eq!(
                decode_in_chunks(&coding, &wire, size),
                plain,
                "{coding}: chunk size {size}"
            );
        }
    }
}

#[test]
fn a_one_byte_output_slice_still_makes_progress() {
    // The slice API with a pathologically small output buffer: every call
    // must report `NeedOutput` and hand back exactly one byte, and the
    // stream must still terminate.
    let plain = common::text(4_096);
    for coding in codings() {
        let wire = encode(&coding, &plain);
        let mut decoder =
            Decoder::new(std::slice::from_ref(&coding), &DecodeLimits::default()).expect("decoder");
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        let mut consumed = 0usize;
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds < 10_000_000,
                "{coding}: decoding a 4 KiB body one byte at a time did not terminate"
            );
            let progress = decoder
                .decode(&wire[consumed..], &mut one, FlushMode::Finish)
                .expect("decode");
            consumed += progress.consumed;
            out.extend_from_slice(&one[..progress.produced]);
            if progress.status == oxiarc_http::DecodeStatus::StreamEnd {
                break;
            }
            assert!(
                progress.consumed > 0 || progress.produced > 0,
                "{coding}: no progress with a one-byte output slice"
            );
        }
        decoder.close().expect("close");
        assert_eq!(out, plain, "{coding}: one-byte output slice");
    }
}

#[test]
fn a_decoded_body_read_one_byte_at_a_time_matches() {
    struct OneByte<'a> {
        data: &'a [u8],
        pos: usize,
    }
    impl std::io::Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    let plain = common::text(20_000);
    for coding in codings() {
        let wire = encode(&coding, &plain);
        let mut body = DecodedBody::with_codings(
            OneByte {
                data: &wire,
                pos: 0,
            },
            std::slice::from_ref(&coding),
            &DecodeLimits::default(),
        )
        .expect("decoder");
        assert_eq!(body.read_to_vec().expect("read"), plain, "{coding}");
    }
}

/// A chained body decoded through output slices far smaller than the 64 KiB
/// buffer that feeds the next stage.
///
/// This is the only shape that exercises the pump's intermediate-buffer
/// compaction branch: the last stage drains a few bytes per pass, so the
/// stage buffer sits at `end == buf.len()` with free space stranded at the
/// front. An off-by-one there would corrupt the intermediate stream — which
/// the outer coding's checksum would then blame on the wire.
#[cfg(all(feature = "gzip", feature = "brotli"))]
#[test]
fn a_chain_survives_output_slices_smaller_than_the_stage_buffer() {
    let plain = common::text(40_000);
    let inner = oxiarc_brotli::compress(&plain, 4).expect("brotli");
    let wire = oxiarc_deflate::gzip_compress(&inner, 6).expect("gzip");
    let codings = [ContentCoding::Brotli, ContentCoding::Gzip];

    for out_len in [1usize, 2, 3, 7, 1024] {
        let mut decoder = Decoder::new(&codings, &DecodeLimits::default()).expect("decoder");
        let mut out = Vec::new();
        let mut scratch = vec![0u8; out_len];
        let mut consumed = 0usize;
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            assert!(
                rounds < 10_000_000,
                "a {out_len}-byte output slice did not terminate"
            );
            let progress = decoder
                .decode(&wire[consumed..], &mut scratch, FlushMode::Finish)
                .expect("decode");
            consumed += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.status == oxiarc_http::DecodeStatus::StreamEnd {
                break;
            }
            assert!(
                progress.consumed > 0 || progress.produced > 0,
                "no progress with a {out_len}-byte output slice at input offset {consumed}"
            );
        }
        decoder.close().expect("close");
        assert_eq!(out, plain, "{out_len}-byte output slice");
    }
}

/// The same chain fed in chunks either side of the 64 KiB stage-buffer size,
/// where an intermediate buffer that filled exactly would show an off-by-one.
#[cfg(all(feature = "gzip", feature = "zstd"))]
#[test]
fn a_chain_survives_input_chunks_around_the_stage_buffer_size() {
    let plain = common::text(200_000);
    let inner = oxiarc_zstd::compress(&plain).expect("zstd");
    let wire = oxiarc_deflate::gzip_compress(&inner, 6).expect("gzip");
    let codings = [ContentCoding::Zstd, ContentCoding::Gzip];
    for chunk in [1usize, 3, 13, 997, 65_535, 65_536, 65_537] {
        let mut decoder = Decoder::new(&codings, &DecodeLimits::default()).expect("decoder");
        let mut out = Vec::new();
        for piece in wire.chunks(chunk) {
            decoder.feed_into(piece, &mut out).expect("feed");
        }
        decoder.finish_into(&mut out).expect("finish");
        assert_eq!(out, plain, "{chunk}-byte input chunks");
    }
}

/// A reader that hands out one queued part per `read` call, then EOF.
struct Parts {
    parts: Vec<Vec<u8>>,
    index: usize,
}

impl std::io::Read for Parts {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.index < self.parts.len() && self.parts[self.index].is_empty() {
            self.index += 1;
        }
        if self.index >= self.parts.len() {
            return Ok(0);
        }
        let part = &mut self.parts[self.index];
        let n = part.len().min(buf.len());
        buf[..n].copy_from_slice(&part[..n]);
        part.drain(..n);
        if part.is_empty() {
            self.index += 1;
        }
        Ok(n)
    }
}

/// The *verdict* is chunk-invariant too, not just the bytes.
///
/// `decode_body` sees a complete stream followed by garbage in one slice and
/// rejects it (`tests/framing.rs`). The streaming adapter must reach the same
/// verdict when the garbage arrives in a **later read** — including when the
/// read boundary falls exactly at the end of the stream, which is the split
/// where a decoder that closes on `StreamEnd` and stops reading would
/// silently accept the response instead.
#[test]
fn trailing_garbage_is_rejected_at_every_read_boundary() {
    let plain = common::text(70_000);
    for coding in codings() {
        if coding == ContentCoding::Identity {
            // `identity` has no end-of-stream marker: every byte is body.
            continue;
        }
        if coding == ContentCoding::Compress {
            // `.Z` has no end-of-stream marker either, and no way to tell
            // "the stream ended" from "more codes happened to follow" — the
            // trailing `XXXX` here just decodes as (garbage) input, the same
            // as any other coding's *body* bytes would. See
            // `ContentCoding::Compress`'s doc comment.
            continue;
        }
        let wire = encode(&coding, &plain);
        let mut one_shot = wire.clone();
        one_shot.extend_from_slice(b"XXXX");
        decode_body(
            std::slice::from_ref(&coding),
            &one_shot,
            &DecodeLimits::default(),
        )
        .expect_err("the one-shot verdict is the reference: trailing garbage is an error");

        // Every split of the stream into two reads, plus the garbage as a
        // third read. `wire.len()` is the boundary that matters most.
        for k in 0..=wire.len() {
            let source = Parts {
                parts: vec![wire[..k].to_vec(), wire[k..].to_vec(), b"XXXX".to_vec()],
                index: 0,
            };
            let mut body = DecodedBody::with_codings(
                source,
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .expect("decoder");
            assert!(
                body.read_to_vec().is_err(),
                "{coding}: trailing garbage slipped through when the stream was split at {k}"
            );
        }
    }
}

/// Truncation at every offset: an error or a clean refusal, never a short
/// `Ok` that a caller would mistake for the whole body.
///
/// Both entry points, because a truncation check that lives in one adapter's
/// end-of-stream handling would pass the other.
#[test]
fn truncation_at_every_offset_is_never_a_short_body() {
    let plain = common::text(3_000);
    for coding in codings() {
        if coding == ContentCoding::Identity {
            continue;
        }
        if coding == ContentCoding::Compress {
            // `.Z` has no end-of-information code (see
            // `ContentCoding::Compress`'s doc comment): a genuinely short
            // body is the documented, correct outcome here, not the bug
            // this test hunts for. The *replacement* invariant — a truncated
            // `.Z` body always decodes to a strict prefix of the whole one,
            // through both entry points, without panicking or hanging — is
            // asserted at every offset by
            // `a_truncated_compress_body_always_decodes_to_a_prefix` below.
            continue;
        }
        let wire = encode(&coding, &plain);
        for k in 0..wire.len() {
            let cut = &wire[..k];
            if let Ok(bytes) =
                decode_body(std::slice::from_ref(&coding), cut, &DecodeLimits::default())
            {
                assert_eq!(
                    bytes, plain,
                    "{coding}: truncating at {k} produced a short body without an error"
                );
            }
            let mut body = DecodedBody::with_codings(
                cut,
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .expect("decoder");
            if let Ok(bytes) = body.read_to_vec() {
                assert_eq!(
                    bytes, plain,
                    "{coding}: DecodedBody truncated at {k} read short without an error"
                );
            }
        }
    }
}

/// `.Z` has no end-of-information code, so
/// `truncation_at_every_offset_is_never_a_short_body` cannot ask `compress`
/// for an error. The invariant that *does* hold, and that this asserts at
/// every offset through both entry points, is strictly weaker but far from
/// vacuous: whatever a truncated body decodes to is always a **prefix** of
/// what the whole body decodes to. A decoder that emitted one extra
/// speculative code from a partial final group, or that resynchronised onto
/// garbage, would break it — and so would a panic or a hang, which the
/// bounded loop below also rules out.
///
/// The reference decoders agree, byte for byte, in
/// `tests/http_oracle.rs::a_truncated_compress_body_matches_the_reference_prefix`.
#[cfg(feature = "compress")]
#[test]
fn a_truncated_compress_body_always_decodes_to_a_prefix() {
    let plain = common::text(3_000);
    let coding = ContentCoding::Compress;
    let wire = encode(&coding, &plain);
    let whole = decode_body(
        std::slice::from_ref(&coding),
        &wire,
        &DecodeLimits::default(),
    )
    .expect("the whole body must decode");
    assert_eq!(whole, plain);

    let mut short = 0usize;
    for k in 0..wire.len() {
        let cut = &wire[..k];
        if let Ok(bytes) = decode_body(std::slice::from_ref(&coding), cut, &DecodeLimits::default())
        {
            assert!(
                whole.starts_with(&bytes),
                "truncating at {k} decoded {} bytes that are not a prefix of the whole body",
                bytes.len()
            );
            if bytes.len() < whole.len() {
                short += 1;
            }
        }
        let mut body =
            DecodedBody::with_codings(cut, std::slice::from_ref(&coding), &DecodeLimits::default())
                .expect("decoder");
        if let Ok(bytes) = body.read_to_vec() {
            assert!(
                whole.starts_with(&bytes),
                "DecodedBody truncated at {k} read {} bytes that are not a prefix",
                bytes.len()
            );
        }
    }
    assert!(
        short > wire.len() / 2,
        "only {short} of {} cuts decoded short — this test would be vacuous",
        wire.len()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any set of split points produces the same bytes as one whole feed.
    #[test]
    fn arbitrary_split_points_are_invariant(
        payload_len in 0usize..9_000,
        raw_splits in prop::collection::vec(0usize..9_000, 0..12),
    ) {
        let plain = common::text(payload_len);
        let mut splits = raw_splits;
        splits.sort_unstable();
        for coding in codings() {
            let wire = encode(&coding, &plain);
            let split = decode_at_splits(&coding, &wire, &splits);
            prop_assert_eq!(split, plain.clone(), "{}", coding);
        }
    }

    /// The same, over arbitrary (incompressible) payload bytes rather than
    /// text, so the encoder's stored/uncompressed paths are exercised too.
    #[test]
    fn arbitrary_payloads_survive_chunked_feeding(
        plain in prop::collection::vec(any::<u8>(), 0..4_000),
        chunk in 1usize..64,
    ) {
        for coding in codings() {
            let wire = encode(&coding, &plain);
            prop_assert_eq!(decode_in_chunks(&coding, &wire, chunk), plain.clone(), "{}", coding);
        }
    }
}
