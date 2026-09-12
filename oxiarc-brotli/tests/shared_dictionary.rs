//! Shared (custom LZ77) dictionary round trips: `compress_with_dictionary`,
//! `decompress_with_dictionary`, `BrotliStream::with_dictionary`, the `Read`
//! adapter, and the RFC 9842 `dcb` framing.
//!
//! The reference-differential half of this lives in `brotli_oracle.rs` behind
//! the `brotli-oracle` feature; everything here is hermetic.

mod common;

use common::{InputSchedule, call_budget, drive, hand_built_dictionary_copy};
use oxiarc_brotli::shared_dict::MAX_SHARED_DICTIONARY;
use oxiarc_brotli::{
    BrotliDecompressor, BrotliError, BrotliParams, BrotliStream, compress_with_dictionary,
    compress_with_params, dcb, decompress, decompress_with_dictionary,
    decompress_with_dictionary_and_limit,
};
use std::io::Read;

/// A dictionary with real internal structure: line-oriented text whose lines
/// differ only in a counter, so both long and short matches are available.
fn dictionary(lines: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        out.extend_from_slice(
            format!("line {i:06}: the quick brown fox jumps over the lazy dog\n").as_bytes(),
        );
    }
    out
}

/// The payload shapes worth testing against a dictionary.
fn payloads(dict: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![b'x']),
        (
            "wholly_in_dictionary",
            dict[dict.len() / 4..dict.len() / 2].to_vec(),
        ),
        (
            "prefix_of_dictionary",
            dict[..dict.len().min(3000)].to_vec(),
        ),
        (
            "suffix_of_dictionary",
            dict[dict.len().saturating_sub(5000)..].to_vec(),
        ),
        ("half_novel", {
            let mut v = dict[100..3000.min(dict.len())].to_vec();
            v.extend_from_slice(
                b"completely novel content that the dictionary has never seen. "
                    .repeat(120)
                    .as_slice(),
            );
            v
        }),
        ("no_overlap", b"zzzz qqqq wwww vvvv ".repeat(500)),
    ]
}

/// Every dictionary size the contract names: none, 1 KiB, 64 KiB, and one
/// larger than the declared window.
fn dictionary_sizes(dict_len: usize) -> Vec<usize> {
    vec![0, 1024, 64 * 1024, dict_len]
}

#[test]
fn round_trips_at_every_dictionary_size_quality_and_window() {
    let dict = dictionary(2000);
    assert!(dict.len() > 64 * 1024, "dictionary must exceed 64 KiB");
    for size in dictionary_sizes(dict.len()) {
        let d = &dict[dict.len() - size.min(dict.len())..];
        for (name, data) in payloads(&dict) {
            for quality in [0u32, 1, 5, 9, 11] {
                for lgwin in [10u32, 16, 22] {
                    // lgwin 10 gives a 1008-byte window, far smaller than the
                    // 64 KiB and 78 KB dictionaries: the "dictionary larger
                    // than the window" case the contract calls for.
                    let params = BrotliParams {
                        quality,
                        lgwin,
                        lgblock: 0,
                    };
                    let compressed = compress_with_dictionary(&data, d, &params).expect("compress");
                    let label = format!("{name} q{quality} w{lgwin} dict{size}");
                    assert_eq!(
                        decompress_with_dictionary(&compressed, d).expect(&label),
                        data,
                        "one-shot {label}"
                    );
                    let mut stream = BrotliStream::new().with_dictionary(d.to_vec());
                    assert_eq!(
                        drive(
                            &mut stream,
                            &compressed,
                            InputSchedule::Whole,
                            1 << 16,
                            call_budget(&compressed),
                        )
                        .expect(&label),
                        data,
                        "streaming {label}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_streaming_decoder_is_chunk_invariant_with_a_dictionary() {
    let dict = dictionary(1500);
    let data = {
        let mut v = dict[2000..30_000].to_vec();
        v.extend_from_slice(
            b"novel tail that is not in the dictionary at all. "
                .repeat(60)
                .as_slice(),
        );
        v
    };
    let params = BrotliParams {
        quality: 9,
        lgwin: 22,
        lgblock: 0,
    };
    let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
    for in_chunk in [1usize, 2, 3, 7, 61, 4096] {
        for out_chunk in [1usize, 5, 251, 1 << 16] {
            let mut stream = BrotliStream::new().with_dictionary(dict.clone());
            let got = drive(
                &mut stream,
                &compressed,
                InputSchedule::Fixed(in_chunk),
                out_chunk,
                call_budget(&compressed),
            )
            .expect("decode");
            assert_eq!(got, data, "in {in_chunk} out {out_chunk}");
        }
    }
}

#[test]
fn an_empty_dictionary_is_exactly_the_dictionary_free_encoder() {
    let data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
    for quality in 0..=11u32 {
        let params = BrotliParams {
            quality,
            ..BrotliParams::default()
        };
        assert_eq!(
            compress_with_dictionary(&data, &[], &params).expect("dict"),
            compress_with_params(&data, &params).expect("plain"),
            "q{quality}: an empty dictionary must not change a single bit"
        );
    }
}

#[test]
fn attaching_a_dictionary_never_costs_compression_ratio() {
    // Each meta-block is encoded both with and without the dictionary and the
    // smaller kept, so this holds even where the payload is far more
    // self-similar than it is dictionary-similar.
    let dict = dictionary(2000);
    for size in [1024usize, 64 * 1024, dict.len()] {
        let d = &dict[dict.len() - size..];
        for (name, data) in payloads(&dict) {
            if data.is_empty() {
                continue;
            }
            for quality in [1u32, 5, 9, 11] {
                for lgwin in [10u32, 22] {
                    let params = BrotliParams {
                        quality,
                        lgwin,
                        lgblock: 0,
                    };
                    let with = compress_with_dictionary(&data, d, &params).expect("with");
                    let without = compress_with_params(&data, &params).expect("without");
                    assert!(
                        with.len() <= without.len(),
                        "{name} q{quality} w{lgwin} dict{size}: {} with vs {} without",
                        with.len(),
                        without.len()
                    );
                }
            }
        }
    }
}

#[test]
fn a_dictionary_pays_for_itself_on_dictionary_like_content() {
    let dict = dictionary(2000);
    let data = dict[5000..25_000].to_vec();
    let params = BrotliParams {
        quality: 9,
        lgwin: 22,
        lgblock: 0,
    };
    let with = compress_with_dictionary(&data, &dict, &params).expect("with");
    let without = compress_with_params(&data, &params).expect("without");
    assert!(
        with.len() * 4 < without.len(),
        "20 KiB of dictionary content should collapse: {} with vs {} without",
        with.len(),
        without.len()
    );
}

#[test]
fn the_wrong_dictionary_does_not_silently_decode() {
    let dict = dictionary(600);
    let other = dictionary(600)
        .iter()
        .map(|b| b ^ 0x20)
        .collect::<Vec<u8>>();
    let data = dict[1000..9000].to_vec();
    let params = BrotliParams {
        quality: 9,
        lgwin: 22,
        lgblock: 0,
    };
    let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
    // Same length, different content: the stream stays structurally valid but
    // must not reproduce the input.
    assert_ne!(
        decompress_with_dictionary(&compressed, &other).ok(),
        Some(data.clone())
    );
    // No dictionary at all: the distances now overrun the reachable history.
    assert_ne!(decompress(&compressed).ok(), Some(data));
}

#[test]
fn distances_at_the_dictionary_boundary_resolve_correctly() {
    // The three interesting distances are the dictionary's last byte
    // (`max_backward + 1`), its first byte (`max_backward + dict_len`) and the
    // first static-dictionary word (`max_backward + dict_len + 1`). Drive them
    // by compressing content taken from the very start and the very end of the
    // dictionary, plus text that provokes Appendix A words.
    let dict = dictionary(400);
    let params = BrotliParams {
        quality: 11,
        lgwin: 22,
        lgblock: 0,
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("first_bytes", dict[..64].to_vec()),
        ("last_bytes", dict[dict.len() - 64..].to_vec()),
        ("whole_dictionary", dict.clone()),
        (
            "static_dictionary_words",
            b"the time of the world is a public thing and the people should know. ".repeat(40),
        ),
        ("dictionary_then_static", {
            let mut v = dict[..2000].to_vec();
            v.extend_from_slice(
                b"the information of the development of the world. "
                    .repeat(40)
                    .as_slice(),
            );
            v
        }),
    ];
    for (name, data) in cases {
        let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
        assert_eq!(
            decompress_with_dictionary(&compressed, &dict).expect(name),
            data,
            "one-shot {name}"
        );
        let mut stream = BrotliStream::new().with_dictionary(dict.clone());
        assert_eq!(
            drive(
                &mut stream,
                &compressed,
                InputSchedule::Fixed(3),
                7,
                call_budget(&compressed)
            )
            .expect(name),
            data,
            "streaming {name}"
        );
    }
}

#[test]
fn the_output_budget_still_applies_with_a_dictionary() {
    let dict = dictionary(500);
    let data = dict[0..12_000].to_vec();
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
    assert_eq!(
        decompress_with_dictionary_and_limit(&compressed, &dict, 1 << 20).expect("generous"),
        data
    );
    assert!(matches!(
        decompress_with_dictionary_and_limit(&compressed, &dict, 128),
        Err(BrotliError::MemoryBudgetExceeded { .. })
    ));
}

#[test]
fn an_oversized_dictionary_is_refused_by_every_entry_point() {
    // A `Vec` of `MAX_SHARED_DICTIONARY + 1` bytes is 16 MiB; allocate it once
    // and share it across the three checks.
    let huge = vec![0u8; MAX_SHARED_DICTIONARY + 1];
    let params = BrotliParams::default();
    assert!(matches!(
        compress_with_dictionary(b"x", &huge, &params),
        Err(BrotliError::DictionaryError(_))
    ));
    assert!(matches!(
        decompress_with_dictionary(&compress_with_params(b"x", &params).expect("c"), &huge),
        Err(BrotliError::DictionaryError(_))
    ));
    let mut stream = BrotliStream::new().with_dictionary(huge);
    let mut out = [0u8; 16];
    assert!(matches!(
        stream.decode(&[0u8; 4], &mut out, oxiarc_core::traits::FlushMode::None),
        Err(BrotliError::DictionaryError(_))
    ));
}

#[test]
fn reset_keeps_the_dictionary() {
    let dict = dictionary(300);
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let first = compress_with_dictionary(&dict[100..2000], &dict, &params).expect("a");
    let second = compress_with_dictionary(&dict[3000..5000], &dict, &params).expect("b");
    let mut stream = BrotliStream::new().with_dictionary(dict.clone());
    assert_eq!(stream.dictionary(), &dict[..]);
    assert_eq!(
        drive(
            &mut stream,
            &first,
            InputSchedule::Whole,
            4096,
            call_budget(&first)
        )
        .expect("a"),
        dict[100..2000]
    );
    stream.reset();
    assert_eq!(stream.dictionary(), &dict[..]);
    assert_eq!(
        drive(
            &mut stream,
            &second,
            InputSchedule::Whole,
            4096,
            call_budget(&second)
        )
        .expect("b"),
        dict[3000..5000]
    );
}

#[test]
fn the_read_adapter_accepts_a_dictionary() {
    let dict = dictionary(400);
    let data = dict[500..8000].to_vec();
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
    let mut out = Vec::new();
    BrotliDecompressor::new(&compressed[..])
        .with_dictionary(dict.clone())
        .read_to_end(&mut out)
        .expect("read");
    assert_eq!(out, data);

    // Byte-at-a-time source: the adapter must not need the whole body at once.
    struct Trickle<'a> {
        data: &'a [u8],
        pos: usize,
    }
    impl std::io::Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }
    let mut trickled = Vec::new();
    BrotliDecompressor::new(Trickle {
        data: &compressed,
        pos: 0,
    })
    .with_dictionary(dict)
    .read_to_end(&mut trickled)
    .expect("trickle");
    assert_eq!(trickled, data);
}

// ─────────────────────────────────────────────────────────────────────────────
// RFC 9842 `dcb` framing
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_dcb_body_assembled_from_our_own_encoder_round_trips() {
    let dict = dictionary(800);
    let data = dict[1000..20_000].to_vec();
    let params = BrotliParams {
        quality: 11,
        lgwin: 22,
        lgblock: 0,
    };
    let body = dcb::compress(&data, &dict, &params).expect("dcb compress");

    // The wire shape the contract names: magic, then the dictionary's SHA-256.
    assert_eq!(&body[..4], &[0xFF, 0x44, 0x43, 0x42]);
    assert_eq!(body[..4], dcb::DCB_MAGIC);
    assert_eq!(&body[4..36], &dcb::dictionary_id(&dict));
    assert_eq!(dcb::DCB_HEADER_LEN, 36);

    // Full path.
    assert_eq!(dcb::decompress(&body, &dict).expect("dcb decompress"), data);

    // The path `oxiarc-http` takes: strip the header, feed the stream.
    let (id, stream) = dcb::parse_header(&body).expect("parse");
    assert_eq!(id, dcb::dictionary_id(&dict));
    let mut push = BrotliStream::new().with_dictionary(dict.clone());
    assert_eq!(
        drive(
            &mut push,
            stream,
            InputSchedule::Fixed(64),
            1 << 14,
            call_budget(stream)
        )
        .expect("push"),
        data
    );

    // Budgeted variant.
    assert_eq!(
        dcb::decompress_with_limit(&body, &dict, 1 << 20).expect("budgeted"),
        data
    );
    assert!(dcb::decompress_with_limit(&body, &dict, 64).is_err());
}

#[test]
fn a_dcb_body_naming_another_dictionary_is_refused() {
    let dict = dictionary(200);
    let other = dictionary(201);
    let params = BrotliParams {
        quality: 5,
        ..BrotliParams::default()
    };
    let body = dcb::compress(b"hello dictionary world", &dict, &params).expect("compress");
    assert!(matches!(
        dcb::decompress(&body, &other),
        Err(BrotliError::DictionaryError(_))
    ));
    // A body whose magic is wrong is framing corruption, not a dictionary
    // mismatch.
    let mut broken = body.clone();
    broken[1] ^= 0xFF;
    assert!(matches!(
        dcb::decompress(&broken, &dict),
        Err(BrotliError::CorruptedData(_))
    ));
    // Truncated to less than the header.
    assert!(dcb::decompress(&body[..20], &dict).is_err());
}

#[test]
fn dictionary_ids_are_sha256() {
    // Cross-checked against the FIPS 180-4 vector for "abc"; the digest of the
    // dictionary is what `Available-Dictionary` carries.
    let id = dcb::dictionary_id(b"abc");
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// A copy that runs past the end of the dictionary ("straddle")
// ─────────────────────────────────────────────────────────────────────────────

/// A shared-dictionary copy must stay inside the dictionary.
///
/// The dictionary is a *compound* history block, not a prefix glued to the
/// sliding window: a copy that would run past its end is a format error, not a
/// copy that continues in the produced output. `brotli 1.1.0` rejects such a
/// stream as "corrupt input" while accepting the byte-identical stream whose
/// copy stops one byte earlier — that pair is the oracle test
/// `test_oracle_reference_rejects_a_copy_past_the_dictionary_end`, and this
/// test is its hermetic half.
///
/// The regression this pins is not the rejection itself but its *uniformity*.
/// When the two decoders disagreed here, the push decoder resolved the
/// continuation against its bounded ring and the answer depended on where the
/// caller's output buffer happened to end: the same stream decoded correctly
/// at some output sizes, returned an error at others, and at yet others
/// returned `Ok` with silently wrong bytes.
#[test]
fn a_copy_running_past_the_dictionary_end_is_refused_at_every_chunking() {
    // The control: a copy that stops exactly at the dictionary's end is legal,
    // and both decoders reproduce it.
    let (stream, dict, expected) = hand_built_dictionary_copy(4);
    let expected = expected.expect("copy 4 fits inside the dictionary");
    assert_eq!(
        decompress_with_dictionary(&stream, &dict).expect("one-shot control"),
        expected
    );
    // Anti-vacuity: without the dictionary the same distance is an Appendix A
    // reference, so the stream must not produce these bytes.
    assert_ne!(decompress(&stream).ok(), Some(expected.clone()));

    for in_chunk in [1usize, 2, 3, 5, 64] {
        for out_chunk in [1usize, 2, 3, 7, 64] {
            let mut push = BrotliStream::new().with_dictionary(dict.clone());
            let got = drive(
                &mut push,
                &stream,
                InputSchedule::Fixed(in_chunk),
                out_chunk,
                call_budget(&stream),
            )
            .expect("push control");
            assert_eq!(got, expected, "control in {in_chunk} out {out_chunk}");
        }
    }

    // The overrun: only the copy length differs.
    for copy_len in [5u32, 10, 22] {
        let (stream, dict, expected) = hand_built_dictionary_copy(copy_len);
        assert!(expected.is_none());
        let one_shot = decompress_with_dictionary(&stream, &dict);
        assert!(
            matches!(one_shot, Err(BrotliError::CorruptedData(_))),
            "one-shot accepted a copy {copy_len} past the dictionary: {one_shot:?}"
        );

        // Every chunking must reach the *same* verdict. A decoder that resolved
        // the overrun against its own history would answer differently at
        // different output sizes, which is the bug this loop exists to catch.
        for in_chunk in [1usize, 2, 3, 5, 64] {
            for out_chunk in [1usize, 2, 3, 4, 5, 7, 9, 13, 64, 4096] {
                let mut push = BrotliStream::new().with_dictionary(dict.clone());
                let got = drive(
                    &mut push,
                    &stream,
                    InputSchedule::Fixed(in_chunk),
                    out_chunk,
                    call_budget(&stream),
                );
                assert!(
                    matches!(got, Err(BrotliError::CorruptedData(_))),
                    "push accepted copy {copy_len} past the dictionary \
                     at in {in_chunk} out {out_chunk}: {got:?}"
                );
            }
        }
    }
}

/// The crate-root aliases are the names a downstream `Content-Encoding: dcb`
/// implementation imports (`oxiarc-http` among them), so they get their own
/// test: the same items as the module's, reachable without a `dcb::`
/// qualifier, and covering the whole framing round trip.
#[test]
fn the_crate_root_dcb_aliases_are_the_same_api() {
    use oxiarc_brotli::{
        DCB_HEADER_LEN, DCB_MAGIC, compress_dcb, decompress_dcb, decompress_dcb_with_limit,
        dictionary_id, parse_dcb_header, verify_dcb_header, write_dcb_header,
    };

    let dict = dictionary(120);
    let data = dict[300..4000].to_vec();
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let body = compress_dcb(&data, &dict, &params).expect("compress");

    assert_eq!(body[..4], DCB_MAGIC);
    assert_eq!(body[..DCB_HEADER_LEN], write_dcb_header(&dict)[..]);
    let (id, stream) = parse_dcb_header(&body).expect("parse");
    assert_eq!(id, dictionary_id(&dict));
    assert_eq!(verify_dcb_header(&body, &dict).expect("verify"), stream);
    assert!(verify_dcb_header(&body, &dictionary(121)).is_err());

    assert_eq!(decompress_dcb(&body, &dict).expect("decompress"), data);
    assert_eq!(
        decompress_dcb_with_limit(&body, &dict, 1 << 20).expect("budgeted"),
        data
    );
    assert!(decompress_dcb_with_limit(&body, &dict, 16).is_err());

    // The aliases are re-exports, not lookalike copies.
    assert_eq!(DCB_MAGIC, dcb::DCB_MAGIC);
    assert_eq!(DCB_HEADER_LEN, dcb::DCB_HEADER_LEN);
    assert_eq!(dictionary_id(&dict), dcb::dictionary_id(&dict));
}

/// Deterministic pseudo-random bytes: incompressible filler that makes a
/// dictionary-compressed body long enough to mutate byte by byte.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Adversarial: a `dcb` body from the wire is untrusted input
// ─────────────────────────────────────────────────────────────────────────────

/// Every mutilation of a `dcb` body must produce an error or, at worst, bytes
/// that are not the original — never a panic, and never `Ok(original)` from a
/// body that no longer names or contains it.
///
/// The framing is the first thing an HTTP peer touches, so it is walked
/// exhaustively: truncation at *every* offset, a flip of *every* header byte,
/// and a flip, a drop and an insert at every offset of the compressed payload.
#[test]
fn a_mangled_dcb_body_is_refused_without_panicking() {
    let dict = dictionary(120);
    let data = {
        let mut v = dict[300..4000].to_vec();
        v.extend_from_slice(&noise(300, 0x51ED));
        v.extend_from_slice(&dict[100..900]);
        v
    };
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let body = dcb::compress(&data, &dict, &params).expect("compress");
    assert!(
        body.len() > dcb::DCB_HEADER_LEN + 200,
        "need a payload long enough to sweep: {} bytes",
        body.len()
    );
    assert_eq!(dcb::decompress(&body, &dict).expect("control"), data);

    // 1. Truncation at every offset. Nothing shorter than the whole body can
    //    reproduce the whole body.
    for cut in 0..body.len() {
        let short = &body[..cut];
        assert!(
            dcb::decompress(short, &dict).is_err(),
            "a {cut}-byte prefix of the body decoded"
        );
        // The header parse must agree with the framing rules exactly.
        let parsed = dcb::parse_header(short);
        if cut < dcb::DCB_HEADER_LEN {
            assert!(matches!(parsed, Err(BrotliError::CorruptedData(_))));
        } else {
            let (id, stream) = parsed.expect("a full header parses");
            assert_eq!(id, dcb::dictionary_id(&dict));
            assert_eq!(stream.len(), cut - dcb::DCB_HEADER_LEN);
        }
    }
    // A header with no stream at all is a header, and an empty Brotli stream.
    assert!(dcb::parse_header(&body[..dcb::DCB_HEADER_LEN]).is_ok());
    assert!(dcb::decompress(&body[..dcb::DCB_HEADER_LEN], &dict).is_err());

    // 2. Every magic byte matters, and every digest byte is checked.
    for i in 0..dcb::DCB_HEADER_LEN {
        let mut bad = body.clone();
        bad[i] ^= 0x40;
        let parsed = dcb::parse_header(&bad);
        if i < 4 {
            assert!(
                matches!(parsed, Err(BrotliError::CorruptedData(_))),
                "magic byte {i} was not checked"
            );
        } else {
            // The digest is framing, not integrity: `parse_header` hands it
            // back unchecked and `verify_header` is what rejects it.
            let (id, _) = parsed.expect("parse ignores the digest");
            assert_ne!(id, dcb::dictionary_id(&dict));
            assert!(
                matches!(
                    dcb::verify_header(&bad, &dict),
                    Err(BrotliError::DictionaryError(_))
                ),
                "digest byte {i} was not checked"
            );
        }
        assert!(dcb::decompress(&bad, &dict).is_err(), "header byte {i}");
    }

    // 3. The payload: a flipped, a dropped and an inserted byte at every
    //    offset. Brotli carries no checksum, so a mutation may legitimately
    //    still decode — even back to the original, if it lands on a bit the
    //    format does not read. What must hold is that nothing panics and that
    //    the *two* decoders answer identically: a bounded push decoder that
    //    resolved anything against its own ring rather than against the format
    //    would drift from the one-shot decoder here first.
    let mut agreed = 0usize;
    for i in dcb::DCB_HEADER_LEN..body.len() {
        for mutated in [
            {
                let mut v = body.clone();
                v[i] ^= 0x01;
                v
            },
            {
                let mut v = body.clone();
                v.remove(i);
                v
            },
            {
                let mut v = body.clone();
                v.insert(i, 0x5A);
                v
            },
        ] {
            let one_shot = dcb::decompress(&mutated, &dict).ok();
            let stream_bytes = dcb::parse_header(&mutated).expect("header survives").1;
            let mut push = BrotliStream::new().with_dictionary(dict.clone());
            let pushed = drive(
                &mut push,
                stream_bytes,
                InputSchedule::Fixed(7),
                13,
                call_budget(stream_bytes),
            )
            .ok();
            assert_eq!(
                one_shot, pushed,
                "the decoders disagree on a body mutated at {i}"
            );
            agreed += 1;
        }
    }
    assert!(
        agreed > 600,
        "the mutation sweep did not run ({agreed} cases)"
    );
    eprintln!("[dcb] {agreed} mutated bodies, both decoders agreeing");
}

/// The `dcb` output budget is enforced, and the boundary is exact.
#[test]
fn the_dcb_output_budget_is_exact() {
    let dict = dictionary(120);
    let data = dict[300..4000].to_vec();
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let body = dcb::compress(&data, &dict, &params).expect("compress");

    assert_eq!(
        dcb::decompress_with_limit(&body, &dict, data.len()).expect("exact budget"),
        data
    );
    for limit in [0usize, 1, data.len() / 2, data.len() - 1] {
        assert!(
            dcb::decompress_with_limit(&body, &dict, limit).is_err(),
            "a {limit}-byte budget accepted {} bytes",
            data.len()
        );
    }
    // The digest is checked before the budget: a wrong dictionary is a
    // dictionary error whatever the budget.
    assert!(matches!(
        dcb::decompress_with_limit(&body, &dictionary(121), 1 << 20),
        Err(BrotliError::DictionaryError(_))
    ));
}

/// A truncated dictionary stream must stop the push decoder, at every
/// truncation offset and with a one-byte output slice — no silent short read,
/// no unbounded call loop.
#[test]
fn a_truncated_dictionary_stream_stops_the_push_decoder() {
    let dict = dictionary(60);
    let data = {
        // Half from the dictionary, half novel, so the stream is long enough
        // for the truncation sweep to have somewhere to cut.
        let mut v = dict[200..1800].to_vec();
        v.extend_from_slice(&noise(200, 0xC0FFEE));
        v
    };
    let params = BrotliParams {
        quality: 9,
        ..BrotliParams::default()
    };
    let compressed = compress_with_dictionary(&data, &dict, &params).expect("compress");
    assert!(
        compressed.len() > 64,
        "stream too short to truncate meaningfully"
    );

    eprintln!("[dcb] truncation sweep over {} bytes", compressed.len());
    for cut in 0..compressed.len() {
        let short = &compressed[..cut];
        let mut stream = BrotliStream::new().with_dictionary(dict.clone());
        // `drive` asserts a call budget, so a decoder that never terminates
        // fails the test instead of hanging it.
        let got = drive(
            &mut stream,
            short,
            InputSchedule::Fixed(1),
            1,
            call_budget(&compressed),
        );
        assert!(
            got.is_err(),
            "a {cut}-byte prefix decoded to completion: {got:?}"
        );
        // The same prefix through the one-shot decoder, for agreement.
        assert!(
            decompress_with_dictionary(short, &dict).is_err(),
            "one-shot {cut}"
        );
    }
}
