//! Shared (custom LZ77) dictionary round trips: `compress_with_dictionary`,
//! `decompress_with_dictionary`, `BrotliStream::with_dictionary`, the `Read`
//! adapter, and the RFC 9842 `dcb` framing.
//!
//! The reference-differential half of this lives in `brotli_oracle.rs` behind
//! the `brotli-oracle` feature; everything here is hermetic.

mod common;

use common::{InputSchedule, call_budget, drive};
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

/// Hand-build a one-meta-block stream whose single command copies more bytes
/// than the dictionary can supply, so the copy continues in the produced
/// output.
///
/// Neither this crate's encoder nor `brotli 1.1.0 -D` emits such a command —
/// both stop a dictionary match at the dictionary's end — so the decoders' path
/// for it can only be reached by a stream written by hand. The stream is legal
/// RFC 7932: one literal block type, one insert-and-copy type and one distance
/// type, each with a single-symbol *simple* prefix code (which costs zero bits
/// per symbol), so the whole meta-block is a handful of header fields plus two
/// extra-bit fields.
///
/// Layout produced below, in bit order:
///
/// ```text
/// WBITS=16                      0
/// ISLAST=1, ISLASTEMPTY=0       1 0
/// MNIBBLES=4, MLEN-1=14         00, 14 in 16 bits
/// NBLTYPES L/I/D = 1            0 0 0
/// NPOSTFIX=0, NDIRECT=0         00, 0000
/// context mode LSB6             00
/// NTREESL=1, NTREESD=1          0 0
/// literal code: simple, 'A'     01, 00, 8 bits
/// insert-and-copy code          01, 00, 10 bits
/// distance code: symbol 19      01, 00, 6 bits
/// copy extra bit (copy_len 10)  0
/// distance extra bits (dist 9)  00
/// padding                       zeros to the byte boundary
/// ```
fn hand_built_straddle_stream() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use oxiarc_brotli::bit_writer::BitWriter;
    use oxiarc_brotli::huffman::alphabet_bits;
    use oxiarc_brotli::tables::{COPY_LENGTH_CODES, INSERT_LENGTH_CODES, decompose_command};

    let insert_len = 5u32;
    let copy_len = 10u32;
    let distance = 9usize;

    // The insert-and-copy symbol for (insert 5, copy 10) with an explicit
    // distance. Found by inverting the crate's own decomposition, so the test
    // cannot drift from the table it is testing against.
    let mut ic_symbol = None;
    for sym in 0u16..704 {
        let (ins_code, cpy_code, implicit) = decompose_command(sym);
        if implicit {
            continue;
        }
        let (ins_base, ins_extra) = INSERT_LENGTH_CODES[ins_code as usize];
        let (cpy_base, cpy_extra) = COPY_LENGTH_CODES[cpy_code as usize];
        if ins_base == insert_len && ins_extra == 0 && cpy_base == copy_len && cpy_extra == 1 {
            ic_symbol = Some(sym);
            break;
        }
    }
    let ic_symbol = ic_symbol.expect("an insert-5 copy-10 command symbol must exist");

    let mut w = BitWriter::with_capacity(64);
    w.write_bit(false).expect("wbits"); // WBITS = 16
    w.write_bit(true).expect("islast");
    w.write_bit(false).expect("islastempty");
    w.write_bits(0, 2).expect("mnibbles"); // 4 nibbles
    w.write_bits(14, 16).expect("mlen"); // MLEN = 15
    for _ in 0..3 {
        w.write_bit(false).expect("nbltypes"); // L, I, D = 1
    }
    w.write_bits(0, 2).expect("npostfix");
    w.write_bits(0, 4).expect("ndirect");
    w.write_bits(0, 2).expect("context mode"); // LSB6
    w.write_bit(false).expect("ntreesl");
    w.write_bit(false).expect("ntreesd");
    // Three single-symbol simple prefix codes.
    for (alphabet, symbol) in [
        (256u32, u32::from(b'A')),
        (704, u32::from(ic_symbol)),
        (64, 19),
    ] {
        w.write_bits(1, 2).expect("hskip"); // hskip == 1 -> simple
        w.write_bits(0, 2).expect("nsym-1"); // one symbol
        w.write_bits(symbol, alphabet_bits(alphabet))
            .expect("symbol");
    }
    // The command: symbols cost no bits, only the extra-bit fields remain.
    w.write_bits(0, 1).expect("copy extra"); // copy length 10 = base 10 + 0
    w.write_bits(0, 2).expect("distance extra"); // distance 9 = base 9 + 0
    w.flush();
    let stream = w.finish();

    let dict: Vec<u8> = (0..100u32).map(|i| b'a' + (i % 26) as u8).collect();
    // 5 literals, then 4 bytes from the dictionary's tail (all it can supply),
    // then 6 more at the same distance, which land back at the start of the
    // output.
    let mut expected = vec![b'A'; insert_len as usize];
    expected.extend_from_slice(&dict[dict.len() - 4..]);
    for _ in 0..(copy_len as usize - 4) {
        let src = expected.len() - distance;
        expected.push(expected[src]);
    }
    assert_eq!(expected.len(), 15);
    (stream, dict, expected)
}

#[test]
fn a_copy_running_past_the_dictionary_continues_in_the_output() {
    let (stream, dict, expected) = hand_built_straddle_stream();

    assert_eq!(
        decompress_with_dictionary(&stream, &dict).expect("one-shot straddle"),
        expected
    );

    // The push decoder must agree, at every chunking — this is the path where
    // the copy is split across `CmdState::SharedCopy` and `CmdState::Copy`.
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
            .expect("push straddle");
            assert_eq!(got, expected, "in {in_chunk} out {out_chunk}");
        }
    }

    // Anti-vacuity: without the dictionary the same distance is an Appendix A
    // reference, so the stream must not produce these bytes.
    assert_ne!(decompress(&stream).ok(), Some(expected));
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
