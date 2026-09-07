//! Hermetic coverage of the `.Z` (UNIX `compress`) container: every code
//! width, both block modes, every entry point, and the robustness sweeps.
//!
//! Nothing here shells out — `tests/z_oracle.rs` owns the reference-tool
//! comparisons and `tests/z_fixtures.rs` owns the committed real-`compress`
//! bytes. This file is what still runs on a machine with no tools at all.

use std::io::{self, Read, Write};

use oxiarc_lzw::LzwError;
use oxiarc_lzw::z::{
    MAGIC, MAX_MAX_BITS, MIN_MAX_BITS, ZHeader, ZReader, ZWriter, compress,
    compress_with_block_mode, decompress, decompress_into, decompress_with_limit,
};

// ---------------------------------------------------------------------------
// Payload shapes
// ---------------------------------------------------------------------------

/// Deterministic xorshift bytes (incompressible).
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// `n` bytes of `vocab` words picked by `(i*i + 3*i) % vocab.len()`.
fn words(n: usize, vocab: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 8);
    let mut i: u32 = 0;
    while out.len() < n {
        let index = (i.wrapping_mul(i).wrapping_add(3u32.wrapping_mul(i))) as usize % vocab.len();
        out.extend_from_slice(vocab[index]);
        i = i.wrapping_add(1);
    }
    out.truncate(n);
    out
}

const VOCAB_A: [&[u8]; 6] = [
    b"alpha ",
    b"beta ",
    b"gamma ",
    b"delta ",
    b"epsilon ",
    b"zeta ",
];
const VOCAB_B: [&[u8]; 7] = [
    b"one ", b"two ", b"three ", b"four ", b"five ", b"six ", b"seven ",
];

/// The shapes every width/mode combination is exercised over.
fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    let mut mixed = words(2_000, &VOCAB_A);
    mixed.extend_from_slice(&noise(2_000, 0x1234_5678));
    mixed.extend_from_slice(&words(2_000, &VOCAB_A));

    let mut regime_change = words(10_000, &VOCAB_A);
    regime_change.extend_from_slice(&words(11_000, &VOCAB_B));

    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0xA5]),
        ("two_same", vec![7u8; 2]),
        ("every_byte", (0..=255u8).collect()),
        ("zeros_64k", vec![0u8; 64 * 1024]),
        ("text", b"TOBEORNOTTOBEORTOBEORNOT".repeat(400)),
        ("words", words(30_000, &VOCAB_A)),
        ("mixed", mixed),
        ("regime_change", regime_change),
        ("noise", noise(20_000, 0x9E37_79B9)),
        ("kwkwk", b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".repeat(64)),
    ]
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn round_trips_at_every_width_in_both_block_modes() {
    let mut cases = 0usize;
    for (name, payload) in payloads() {
        for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
            for block_mode in [true, false] {
                let label = format!("{name}/b{max_bits}/block={block_mode}");
                let stream = compress_with_block_mode(&payload, max_bits, block_mode)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));

                let header = ZHeader::parse(&stream).unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(header.max_bits, max_bits, "{label}");
                assert_eq!(header.block_mode, block_mode, "{label}");
                assert_eq!(&stream[..2], &MAGIC, "{label}");

                let decoded = decompress(&stream).unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(decoded, payload, "{label}");
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 11 * 8 * 2, "every shape x width x mode is covered");
}

#[test]
fn every_entry_point_agrees_at_every_width() {
    for (name, payload) in payloads() {
        for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
            for block_mode in [true, false] {
                let label = format!("{name}/b{max_bits}/block={block_mode}");
                let stream = compress_with_block_mode(&payload, max_bits, block_mode)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));

                // decompress_into with an exact buffer
                let mut exact = vec![0u8; payload.len()];
                let written = decompress_into(&stream, &mut exact)
                    .unwrap_or_else(|e| panic!("{label}: into: {e}"));
                assert_eq!(written, payload.len(), "{label}");
                assert_eq!(exact, payload, "{label}");

                // ... and with a generous one: the return value is the truth
                let mut roomy = vec![0u8; payload.len() + 97];
                let written = decompress_into(&stream, &mut roomy)
                    .unwrap_or_else(|e| panic!("{label}: roomy: {e}"));
                assert_eq!(written, payload.len(), "{label}");
                assert_eq!(&roomy[..written], &payload[..], "{label}");

                // one byte short is an error, never a silent truncation
                if !payload.is_empty() {
                    let mut short = vec![0u8; payload.len() - 1];
                    let error = decompress_into(&stream, &mut short)
                        .expect_err("a short buffer must be rejected");
                    assert!(
                        matches!(error, LzwError::BufferTooSmall { .. }),
                        "{label}: {error}"
                    );
                }

                // decompress_with_limit at, and one under, the true size
                let bounded = decompress_with_limit(&stream, payload.len())
                    .unwrap_or_else(|e| panic!("{label}: limit: {e}"));
                assert_eq!(bounded, payload, "{label}");
                if !payload.is_empty() {
                    let error = decompress_with_limit(&stream, payload.len() - 1)
                        .expect_err("one byte under the true size must be rejected");
                    assert!(
                        matches!(error, LzwError::OutputLimitExceeded { .. }),
                        "{label}: {error}"
                    );
                }

                // ZReader
                let mut streamed = Vec::new();
                ZReader::new(&stream[..])
                    .read_to_end(&mut streamed)
                    .unwrap_or_else(|e| panic!("{label}: reader: {e}"));
                assert_eq!(streamed, payload, "{label}");
            }
        }
    }
}

#[test]
fn the_writer_reproduces_the_one_shot_encoder_for_every_write_pattern() {
    for (name, payload) in payloads() {
        for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
            for block_mode in [true, false] {
                let label = format!("{name}/b{max_bits}/block={block_mode}");
                let expected = compress_with_block_mode(&payload, max_bits, block_mode)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                let header = ZHeader::new(max_bits, block_mode).expect("header");

                for chunk in [1usize, 3, 997, 64 * 1024] {
                    let mut writer = ZWriter::with_header(Vec::new(), header).expect("writer");
                    assert_eq!(writer.header(), header, "{label}");
                    for piece in payload.chunks(chunk.max(1)) {
                        writer.write_all(piece).expect("write");
                    }
                    let produced = writer.finish().expect("finish");
                    assert_eq!(
                        produced, expected,
                        "{label}: chunked writes ({chunk}) must not change the bytes"
                    );
                }

                // An interleaved flush must not change the output either:
                // `.Z` has no sync point, so flush may only push completed
                // groups out early.
                let mut writer = ZWriter::with_header(Vec::new(), header).expect("writer");
                let half = payload.len() / 2;
                writer.write_all(&payload[..half]).expect("write");
                writer.flush().expect("flush");
                writer.write_all(&payload[half..]).expect("write");
                let produced = writer.finish().expect("finish");
                assert_eq!(produced, expected, "{label}: flush changed the bytes");
            }
        }
    }
}

/// A reader that hands out at most `step` bytes per call, to prove the
/// decoder resumes across arbitrary chunk boundaries.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let take = self.data.len().min(buf.len()).min(self.step);
        buf[..take].copy_from_slice(&self.data[..take]);
        self.data = &self.data[take..];
        Ok(take)
    }
}

#[test]
fn the_reader_survives_trickled_input_and_tiny_output_buffers() {
    let payload = payloads()
        .into_iter()
        .find(|(name, _)| *name == "mixed")
        .map(|(_, data)| data)
        .expect("the mixed payload");

    for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
        let stream = compress(&payload, max_bits).expect("compress");
        for step in [1usize, 2, 3, 5, 17] {
            let mut reader = ZReader::new(Trickle {
                data: &stream,
                step,
            });
            let mut out = Vec::new();
            let mut scratch = [0u8; 7];
            loop {
                let read = reader.read(&mut scratch).expect("read");
                if read == 0 {
                    break;
                }
                out.extend_from_slice(&scratch[..read]);
            }
            assert_eq!(out, payload, "b{max_bits}, {step}-byte input steps");
            assert_eq!(
                reader.header().map(|h| h.max_bits),
                Some(max_bits),
                "the header is available once decoding started"
            );
        }
    }
}

#[test]
fn the_reader_enforces_its_output_bound_during_decoding() {
    let payload = vec![0u8; 4 * 1024 * 1024];
    let bomb = compress(&payload, 16).expect("compress");
    assert!(bomb.len() < 32 * 1024, "the bomb must actually be small");

    let mut out = Vec::new();
    let error = ZReader::new(&bomb[..])
        .with_max_output(64 * 1024)
        .read_to_end(&mut out)
        .expect_err("the bound must stop the expansion");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(
        out.len() <= 64 * 1024 + 1024,
        "{} bytes materialised past a 64 KiB bound",
        out.len()
    );

    // The same stream inside the bound decodes fully.
    let mut ok = Vec::new();
    ZReader::new(&bomb[..])
        .with_max_output(payload.len() as u64)
        .read_to_end(&mut ok)
        .expect("within the bound");
    assert_eq!(ok, payload);
}

#[test]
fn an_empty_payload_is_a_header_only_stream() {
    for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
        for block_mode in [true, false] {
            let stream =
                compress_with_block_mode(&[], max_bits, block_mode).expect("compress empty");
            assert_eq!(stream.len(), ZHeader::LEN, "b{max_bits}");
            assert_eq!(decompress(&stream).expect("decompress"), Vec::<u8>::new());

            let mut writer = ZWriter::new(Vec::new(), max_bits).expect("writer");
            writer.write_all(&[]).expect("write nothing");
            let produced = writer.finish().expect("finish");
            assert_eq!(produced.len(), ZHeader::LEN, "b{max_bits}: writer");
        }
    }
}

// ---------------------------------------------------------------------------
// Header validation
// ---------------------------------------------------------------------------

#[test]
fn malformed_headers_are_rejected_with_the_right_error() {
    assert!(matches!(
        decompress(&[]).expect_err("empty"),
        LzwError::ZTruncatedHeader { len: 0 }
    ));
    assert!(matches!(
        decompress(&[0x1F]).expect_err("one byte"),
        LzwError::ZTruncatedHeader { len: 1 }
    ));
    assert!(matches!(
        decompress(&[0x1F, 0x9D]).expect_err("two bytes"),
        LzwError::ZTruncatedHeader { len: 2 }
    ));
    assert!(matches!(
        decompress(&[0x1F, 0x8B, 0x08]).expect_err("gzip magic"),
        LzwError::ZInvalidMagic {
            magic: [0x1F, 0x8B]
        }
    ));
    for bad in [0u8, 1, 8, 17, 24, 31] {
        let stream = [0x1F, 0x9D, bad];
        assert!(
            matches!(
                decompress(&stream).expect_err("bad width"),
                LzwError::ZUnsupportedMaxBits(width) if width == bad
            ),
            "width {bad} must be rejected"
        );
        assert!(ZWriter::new(Vec::new(), bad).is_err(), "width {bad}");
        assert!(compress(b"x", bad).is_err(), "width {bad}");
    }

    // Bits 5-6 are reserved and ignored, as `gzip` and `ncompress` do.
    let payload = b"reserved flag bits must be ignored".repeat(4);
    let mut stream = compress(&payload, 16).expect("compress");
    stream[2] |= 0x60;
    let header = ZHeader::parse(&stream).expect("reserved bits ignored");
    assert_eq!(header.max_bits, 16);
    assert!(header.block_mode);
    assert_eq!(decompress(&stream).expect("decompress"), payload);
}

// ---------------------------------------------------------------------------
// Robustness sweeps (every call bounded)
// ---------------------------------------------------------------------------

/// Output cap for every corrupted-input call: a `.Z` bit flip can turn a few
/// kilobytes into hundreds of megabytes, so no sweep below ever calls the
/// unbounded `decompress`.
const SWEEP_LIMIT: usize = 4 * 1024 * 1024;

#[test]
fn truncation_yields_a_prefix_and_never_panics() {
    // `.Z` has no end-of-information code, so a truncated stream decodes to
    // a prefix and is *not* an error — the same thing `gzip -dc` does.
    let mut calls = 0usize;
    for (name, payload) in payloads() {
        if payload.len() < 512 {
            continue;
        }
        for max_bits in [9u8, 12, 16] {
            for block_mode in [true, false] {
                let stream =
                    compress_with_block_mode(&payload, max_bits, block_mode).expect("compress");
                let step = (stream.len() / 40).max(1);
                for cut in (ZHeader::LEN..=stream.len()).step_by(step) {
                    let decoded = decompress_with_limit(&stream[..cut], SWEEP_LIMIT)
                        .unwrap_or_else(|e| {
                            panic!("{name}/b{max_bits}/block={block_mode}/cut={cut}: {e}")
                        });
                    assert!(decoded.len() <= payload.len(), "{name}: cut {cut} overran");
                    assert_eq!(
                        decoded,
                        payload[..decoded.len()],
                        "{name}: cut {cut} is not a prefix"
                    );
                    calls += 1;
                }
            }
        }
    }
    assert!(calls > 900, "only {calls} truncations exercised");
    println!("{calls} truncations decoded to prefixes, no panics");
}

#[test]
fn single_bit_flips_never_panic_and_stay_bounded() {
    let payload = words(12_000, &VOCAB_A);
    let mut calls = 0usize;
    let mut errors = 0usize;
    for max_bits in [9u8, 12, 16] {
        for block_mode in [true, false] {
            let stream =
                compress_with_block_mode(&payload, max_bits, block_mode).expect("compress");
            let step = (stream.len() / 60).max(1);
            for index in (ZHeader::LEN..stream.len()).step_by(step) {
                for bit in [0u8, 3, 7] {
                    let mut corrupt = stream.clone();
                    corrupt[index] ^= 1 << bit;
                    match decompress_with_limit(&corrupt, SWEEP_LIMIT) {
                        Ok(decoded) => assert!(decoded.len() <= SWEEP_LIMIT),
                        Err(_) => errors += 1,
                    }
                    // `decompress_into` must be equally unflappable.
                    let mut into = vec![0u8; 4096];
                    let _ = decompress_into(&corrupt, &mut into);
                    calls += 1;
                }
            }
        }
    }
    assert!(calls > 900, "only {calls} bit flips exercised");
    println!("{calls} bit flips, {errors} rejected, no panics");
}

#[test]
fn arbitrary_headers_over_random_bodies_never_panic() {
    // Every (max_bits, block_mode) header over the same pseudo-random body:
    // a body of noise is exactly what a corrupt or hostile `.Z` looks like.
    let body = noise(4_096, 0xC0FF_EE01);
    let mut calls = 0usize;
    for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
        for block_mode in [true, false] {
            let header = ZHeader::new(max_bits, block_mode).expect("header");
            let mut stream = header.to_bytes().to_vec();
            stream.extend_from_slice(&body);
            for take in [3usize, 4, 9, 64, 512, body.len() + ZHeader::LEN] {
                let slice = &stream[..take.min(stream.len())];
                let bounded = decompress_with_limit(slice, SWEEP_LIMIT);
                if let Ok(decoded) = &bounded {
                    assert!(decoded.len() <= SWEEP_LIMIT);
                }
                let mut into = [0u8; 1024];
                let _ = decompress_into(slice, &mut into);
                let mut out = Vec::new();
                let _ = ZReader::new(slice)
                    .with_max_output(SWEEP_LIMIT as u64)
                    .read_to_end(&mut out);
                calls += 1;
            }
        }
    }
    assert_eq!(calls, 8 * 2 * 6);
}

/// Pack `codes` LSB-first at the given widths, the way `.Z` does.
fn pack(codes: &[(u16, u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut held: u8 = 0;
    for (code, width) in codes {
        acc |= u32::from(*code) << held;
        held += width;
        while held >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            held -= 8;
        }
    }
    if held > 0 {
        out.push((acc & 0xFF) as u8);
    }
    out
}

#[test]
fn a_leading_clear_code_resets_to_the_initial_state_like_gzip() {
    // `compress(1)` never writes a CLEAR as the first code, so the two
    // reference decoders disagree about this stream: `gzip -dc` treats it as
    // a reset (and decodes the body exactly as if the CLEAR were absent)
    // while BSD `uncompress -c` reads the group padding as literals and
    // emits garbage. This crate follows `gzip`, which is the decoder
    // `Content-Encoding: compress` bodies meet in practice.
    //
    // Mid-stream CLEAR handling — the case real `compress` output actually
    // contains — is pinned by `tests/z_fixtures.rs::clear_b10.Z` and by the
    // `z-oracle` suite.
    let header = ZHeader::new(12, true).expect("header");

    // One full group of eight 9-bit codes: CLEAR followed by the padding a
    // writer emits after a reset.
    let mut clear_group = vec![(256u16, 9u8)];
    clear_group.extend(std::iter::repeat_n((0u16, 9u8), 7));
    let clear_group = pack(&clear_group);
    assert_eq!(clear_group.len(), 9, "a 9-bit group is 9 bytes");

    // A body encoded from the initial state: 'A', 'B', then the entry those
    // two just created, twice.
    let body = pack(&[(65, 9), (66, 9), (257, 9), (257, 9)]);

    let mut with_clear = header.to_bytes().to_vec();
    with_clear.extend_from_slice(&clear_group);
    with_clear.extend_from_slice(&body);

    let mut without_clear = header.to_bytes().to_vec();
    without_clear.extend_from_slice(&body);

    let expected = b"ABABAB";
    assert_eq!(
        decompress(&without_clear).expect("no leading clear"),
        expected,
        "the hand-built body is what it claims to be"
    );
    assert_eq!(
        decompress(&with_clear).expect("leading clear"),
        expected,
        "a leading CLEAR must leave the decoder in its initial state"
    );

    // Without block mode 256 is an ordinary code and cannot appear as the
    // first code of a stream, so the same bytes are rejected.
    let non_block = ZHeader::new(12, false).expect("header");
    let mut as_non_block = non_block.to_bytes().to_vec();
    as_non_block.extend_from_slice(&clear_group);
    as_non_block.extend_from_slice(&body);
    assert!(matches!(
        decompress(&as_non_block).expect_err("256 first in non-block mode"),
        LzwError::InvalidCode(256)
    ));
}

#[test]
fn the_kwkwk_case_matches_both_reference_decoders() {
    // The classic LZW corner: a code that names the entry the current step
    // is about to create, so its string is `string(old) ++ first(old)`.
    // These seven bytes are `1F 9D 8C 61 02 0A 04`; `gzip -dc` and
    // `uncompress -c` both turn them into `aaaaaa`, and the second and third
    // codes each take the KwKwK path (257 and 258 are `free_ent` at the
    // moment they are read).
    let mut stream = ZHeader::new(12, true).expect("header").to_bytes().to_vec();
    stream.extend_from_slice(&pack(&[(97, 9), (257, 9), (258, 9)]));
    assert_eq!(stream, [0x1F, 0x9D, 0x8C, 0x61, 0x02, 0x0A, 0x04]);
    assert_eq!(decompress(&stream).expect("kwkwk"), b"aaaaaa");

    // A code beyond `free_ent` is corrupt input, not another KwKwK.
    let mut bad = ZHeader::new(12, true).expect("header").to_bytes().to_vec();
    bad.extend_from_slice(&pack(&[(97, 9), (258, 9)]));
    assert!(matches!(
        decompress(&bad).expect_err("code past the table"),
        LzwError::InvalidCode(258)
    ));

    // And the whole-payload version: `compress` on a run of one byte is the
    // shape that exercises KwKwK on every step.
    for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
        let payload = vec![b'a'; 5_000];
        let produced = compress(&payload, max_bits).expect("compress");
        assert_eq!(decompress(&produced).expect("round trip"), payload);
    }
}

/// A 64-bit xorshift, for the mutation sweep below.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn mutated_valid_streams_never_panic() {
    // The bit-flip and truncation sweeps above keep the stream's *length*.
    // Byte deletions, insertions and splices do not, and they are what
    // desynchronises the group alignment — the part of the format most
    // likely to index something. This sweep is how the
    // `InvalidCode(512)` out-of-bounds panic (see
    // `src/z/decode.rs::tests::a_code_naming_a_slot_the_full_table_can_never_create_is_rejected`)
    // was found; it is kept so the class stays covered.
    //
    // Everything is bounded: 8 MiB of output per call, a fixed seed, and a
    // fixed number of mutations.
    const CAP: usize = 8 * 1024 * 1024;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;

    let mut pool = Vec::new();
    for (_, payload) in payloads() {
        if payload.len() < 64 {
            continue;
        }
        for max_bits in [9u8, 10, 12, 16] {
            for block_mode in [true, false] {
                pool.push(
                    compress_with_block_mode(&payload, max_bits, block_mode).expect("compress"),
                );
            }
        }
    }
    assert!(pool.len() >= 8, "only {} streams in the pool", pool.len());

    let mut decoded_ok = 0usize;
    let mut rejected = 0usize;
    for _ in 0..15_000u32 {
        let base = &pool[(xorshift(&mut state) as usize) % pool.len()];
        let mut stream = base.clone();
        let span = stream.len() - ZHeader::LEN;
        match xorshift(&mut state) % 5 {
            0 => {
                let at = ZHeader::LEN + (xorshift(&mut state) as usize) % span;
                stream[at] ^= 1u8 << (xorshift(&mut state) % 8);
            }
            1 => {
                let keep = ZHeader::LEN + (xorshift(&mut state) as usize) % span;
                stream.truncate(keep);
            }
            2 => {
                let at = ZHeader::LEN + (xorshift(&mut state) as usize) % span;
                stream.remove(at);
            }
            3 => {
                let at = ZHeader::LEN + (xorshift(&mut state) as usize) % span;
                stream.insert(at, (xorshift(&mut state) >> 23) as u8);
            }
            _ => {
                let other = &pool[(xorshift(&mut state) as usize) % pool.len()];
                let cut = ZHeader::LEN + (xorshift(&mut state) as usize) % span;
                stream.truncate(cut);
                stream.extend_from_slice(&other[ZHeader::LEN..]);
            }
        }
        if xorshift(&mut state) % 8 == 0 {
            // Flip the block-mode bit too: the two dialects read the same
            // bytes differently.
            stream[2] ^= 0x80;
        }

        match decompress_with_limit(&stream, CAP) {
            Ok(out) => {
                assert!(out.len() <= CAP);
                decoded_ok += 1;
            }
            Err(_) => rejected += 1,
        }
        let mut dst = vec![0u8; 64 * 1024];
        if let Ok(written) = decompress_into(&stream, &mut dst) {
            assert!(written <= dst.len());
        }
        let mut streamed = Vec::new();
        let _ = ZReader::new(&stream[..])
            .with_max_output(CAP as u64)
            .read_to_end(&mut streamed);
        assert!(streamed.len() <= CAP);
    }
    assert!(
        decoded_ok > 1_000 && rejected > 1_000,
        "the sweep must exercise both outcomes: {decoded_ok} ok, {rejected} rejected"
    );
}
