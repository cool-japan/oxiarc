//! Differential interop regression tests against real `xz` (liblzma).
//!
//! ## Why this file exists
//!
//! The LZMA2 decoder used to reset its uncompressed position on every chunk,
//! desynchronizing `pos_state` on any continuation chunk whose start offset
//! was not a multiple of `2^pb` — so most real multi-chunk `.xz` streams
//! failed with "Invalid LZMA data" while oxiarc's own (reset-every-chunk)
//! output round-tripped fine. Self-round-trip passing proves nothing; these
//! tests pin interop in BOTH directions.
//!
//! ## Layout
//!
//! * Always-on hermetic tests decode committed `xz`-produced vectors under
//!   `tests/data/` (raw LZMA2 multi-chunk streams at presets 4 and 6, plus
//!   single-block and multi-block `.xz` containers) and verify byte-identical
//!   output against the deterministic fixture generator. The preset-4 stream
//!   contains continuation chunks starting at global offsets `% 4 == 2` and
//!   `% 4 == 1` (the proven pre-fix failure); the preset-6 stream's
//!   continuation chunk starts at `% 4 == 0` (the case that used to pass by
//!   luck). Fixtures were produced ONCE with `xz` (XZ Utils) 5.8.3:
//!   - `xz_preset4_multichunk.lzma2`: `xz -T1 --format=raw --lzma2=preset=4`
//!   - `xz_preset6_multichunk.lzma2`: `xz -T1 --format=raw --lzma2=preset=6`
//!   - `xz_preset6_singleblock.xz`:   `xz -T1 -6`
//!   - `xz_preset6_multiblock.xz`:    `xz -T1 -6 --block-size=262144`
//!
//!   all over `text_fixture(1_200_000)` (byte-identical to this file's
//!   generator).
//! * `xz-oracle`-gated tests (cargo feature) shell out to a live `xz` binary
//!   and self-skip (with a note, not a failure) when it is absent — the same
//!   pattern as `oxiarc-lzhuf`'s `lha-oracle`.
//!
//! The container tests use a minimal `.xz` parser local to this file (stream
//! header, block headers, LZMA2 filter) — deliberately independent of
//! `oxiarc-archive`, whose xz wiring is validated separately.

use std::io::Cursor;

use oxiarc_lzma::{
    Lzma2ChunkedEncoder, Lzma2Config, LzmaLevel, decode_lzma2, dict_size_from_props,
};

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic fixture generators
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic LCG-driven text; MUST match the generator used to produce
/// the committed fixtures byte for byte (same family as `large_fixture` in
/// tests/liblzma_golden.rs, scaled and truncated to `target_len`).
fn text_fixture(target_len: usize) -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"alpha", b"bravo", b"charlie", b"delta", b"echo", b"foxtrot", b"golf", b"hotel",
    ];
    let mut out = Vec::with_capacity(target_len + 64);
    let mut seed: u64 = 0x1234_5678;
    let mut i = 0u32;
    while out.len() < target_len {
        seed = (seed.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7FFF_FFFF;
        let word = WORDS[(seed & 7) as usize];
        let rep = ((seed >> 8) & 3) + 1;
        out.extend_from_slice(format!("line {i:06} ").as_bytes());
        for _ in 0..rep {
            out.extend_from_slice(word);
        }
        out.push(b'\n');
        i += 1;
    }
    out.truncate(target_len);
    out
}

/// Deterministic pseudo-random (incompressible) bytes via an LCG.
fn lcg_bytes(len: usize, mut seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((seed >> 24) as u8);
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// LZMA2 chunk walker (structure assertions on fixtures and encoder output)
// ─────────────────────────────────────────────────────────────────────────────

/// One parsed LZMA2 chunk header.
#[derive(Debug)]
enum Chunk {
    /// Uncompressed chunk.
    Uncompressed {
        /// Uncompressed size.
        size: usize,
        /// Global uncompressed start position (since the last dict reset).
        start: u64,
    },
    /// LZMA chunk.
    Lzma {
        /// Reset field (0-3).
        reset: u8,
        /// Uncompressed size.
        size: usize,
        /// Global uncompressed start position (since the last dict reset).
        start: u64,
    },
}

/// Walk the chunk headers of a raw LZMA2 stream (no payload decoding).
fn walk_chunks(stream: &[u8]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut pos = 0usize;
    let mut upos = 0u64;
    while pos < stream.len() {
        let ctrl = stream[pos];
        if ctrl == 0x00 {
            break;
        }
        if ctrl == 0x01 || ctrl == 0x02 {
            let size = u16::from_be_bytes([stream[pos + 1], stream[pos + 2]]) as usize + 1;
            if ctrl == 0x01 {
                upos = 0;
            }
            chunks.push(Chunk::Uncompressed { size, start: upos });
            pos += 3 + size;
            upos += size as u64;
        } else {
            let reset = (ctrl >> 5) & 0x3;
            let size = ((((ctrl & 0x1F) as usize) << 16)
                | u16::from_be_bytes([stream[pos + 1], stream[pos + 2]]) as usize)
                + 1;
            let csize = u16::from_be_bytes([stream[pos + 3], stream[pos + 4]]) as usize + 1;
            if reset == 3 {
                upos = 0;
            }
            chunks.push(Chunk::Lzma {
                reset,
                size,
                start: upos,
            });
            pos += 5 + usize::from(reset >= 2) + csize;
            upos += size as u64;
        }
    }
    chunks
}

/// Assert the chunk sequence covers exactly `total` contiguous uncompressed
/// bytes (each chunk starts where the previous one ended).
fn assert_contiguous(chunks: &[Chunk], total: u64) {
    let mut expected_start = 0u64;
    for chunk in chunks {
        let (size, start) = match chunk {
            Chunk::Uncompressed { size, start } | Chunk::Lzma { size, start, .. } => {
                (*size, *start)
            }
        };
        assert_eq!(start, expected_start, "non-contiguous chunk: {chunk:?}");
        expected_start += size as u64;
    }
    assert_eq!(expected_start, total, "chunks must cover the whole stream");
}

// ─────────────────────────────────────────────────────────────────────────────
// Minimal .xz container reader (test-local, independent of oxiarc-archive)
// ─────────────────────────────────────────────────────────────────────────────

/// Read a multibyte varint (xz "multibyte integer"); returns (value, length).
fn read_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().enumerate().take(9) {
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Decode every block of an `.xz` file (single LZMA2 filter only) and return
/// (concatenated uncompressed bytes, number of blocks). Check fields are
/// skipped, not verified — the tests compare full output byte-for-byte
/// against the known plaintext instead.
fn decode_xz_container(xz: &[u8]) -> (Vec<u8>, usize) {
    assert!(
        xz.len() > 12 && xz[..6] == [0xFD, b'7', b'z', b'X', b'Z', 0x00],
        "not an .xz stream"
    );
    let check_type = xz[7] & 0x0F;
    // Check sizes per spec section 2.1.1.2.
    let check_size = match check_type {
        0 => 0,
        1..=3 => 4,
        4..=6 => 8,
        7..=9 => 16,
        10..=12 => 32,
        _ => 64,
    };

    let mut pos = 12usize;
    let mut out = Vec::new();
    let mut blocks = 0usize;

    loop {
        let header_size_byte = xz[pos];
        if header_size_byte == 0x00 {
            break; // index indicator: no more blocks
        }
        let header_len = (header_size_byte as usize + 1) * 4;
        let header = &xz[pos..pos + header_len];
        let flags = header[1];
        let num_filters = (flags & 0x03) as usize + 1;
        assert_eq!(num_filters, 1, "test parser supports exactly one filter");

        let mut hp = 2usize;
        if flags & 0x40 != 0 {
            let (_, n) = read_varint(&header[hp..]).expect("compressed size varint");
            hp += n;
        }
        if flags & 0x80 != 0 {
            let (_, n) = read_varint(&header[hp..]).expect("uncompressed size varint");
            hp += n;
        }
        let (filter_id, n) = read_varint(&header[hp..]).expect("filter id varint");
        hp += n;
        assert_eq!(filter_id, 0x21, "expected the LZMA2 filter");
        let (props_size, n) = read_varint(&header[hp..]).expect("props size varint");
        hp += n;
        assert_eq!(props_size, 1, "LZMA2 filter props must be 1 byte");
        let dict_size = dict_size_from_props(header[hp]);

        pos += header_len;

        // Compressed data: a self-terminating LZMA2 chunk stream. Each block
        // starts with a dictionary reset, so a fresh decoder per block has
        // identical semantics to liblzma's per-block reset.
        let mut cursor = Cursor::new(&xz[pos..]);
        let mut decoder = oxiarc_lzma::Lzma2Decoder::new(dict_size);
        let decoded = decoder.decode(&mut cursor).expect("block must decode");
        out.extend_from_slice(&decoded);
        let consumed = cursor.position() as usize;
        pos += consumed;

        // Block padding: 0-3 null bytes aligning the compressed data to 4.
        pos += (4 - (consumed % 4)) % 4;
        // Check field.
        pos += check_size;
        blocks += 1;
    }

    (out, blocks)
}

// ─────────────────────────────────────────────────────────────────────────────
// (1) Always-on: committed xz-produced vectors must decode byte-identical
// ─────────────────────────────────────────────────────────────────────────────

/// xz -T1 --format=raw --lzma2=preset=4 over text_fixture(1_200_000).
const XZ_PRESET4_MULTICHUNK: &[u8] = include_bytes!("data/xz_preset4_multichunk.lzma2");
/// xz -T1 --format=raw --lzma2=preset=6 over text_fixture(1_200_000).
const XZ_PRESET6_MULTICHUNK: &[u8] = include_bytes!("data/xz_preset6_multichunk.lzma2");
/// xz -T1 -6 over text_fixture(1_200_000) (one block).
const XZ_PRESET6_SINGLEBLOCK: &[u8] = include_bytes!("data/xz_preset6_singleblock.xz");
/// xz -T1 -6 --block-size=262144 over text_fixture(1_200_000) (5 blocks).
const XZ_PRESET6_MULTIBLOCK: &[u8] = include_bytes!("data/xz_preset6_multiblock.xz");

/// The proven pre-fix failure: continuation chunks starting at global
/// offsets that are NOT multiples of 2^pb=4 (here `% 4 == 2` and `% 4 == 1`)
/// desynchronized `pos_state` and the whole range decoder.
#[test]
fn decodes_xz_preset4_multichunk_misaligned_continuations() {
    let chunks = walk_chunks(XZ_PRESET4_MULTICHUNK);
    assert_contiguous(&chunks, 1_200_000);
    let continuation_misaligned = chunks
        .iter()
        .filter(|c| matches!(c, Chunk::Lzma { reset: 0, start, .. } if start % 4 != 0))
        .count();
    assert!(
        chunks.len() >= 3 && continuation_misaligned >= 2,
        "fixture must contain misaligned continuation chunks; layout: {chunks:?}"
    );

    let decoded = decode_lzma2(XZ_PRESET4_MULTICHUNK, 64 << 20).expect("xz preset-4 raw LZMA2");
    assert_eq!(decoded, text_fixture(1_200_000), "preset-4 output mismatch");
}

/// The counterpart that used to pass by luck: the continuation chunk starts
/// at `% 4 == 0`, so the buggy per-chunk position reset happened to align.
/// Kept so BOTH alignments of the level-4-vs-level-6 repro stay covered.
#[test]
fn decodes_xz_preset6_multichunk_aligned_continuation() {
    let chunks = walk_chunks(XZ_PRESET6_MULTICHUNK);
    assert_contiguous(&chunks, 1_200_000);
    let has_aligned_continuation = chunks
        .iter()
        .any(|c| matches!(c, Chunk::Lzma { reset: 0, start, .. } if start % 4 == 0));
    assert!(
        chunks.len() >= 2 && has_aligned_continuation,
        "fixture must contain an aligned continuation chunk; layout: {chunks:?}"
    );

    let decoded = decode_lzma2(XZ_PRESET6_MULTICHUNK, 64 << 20).expect("xz preset-6 raw LZMA2");
    assert_eq!(decoded, text_fixture(1_200_000), "preset-6 output mismatch");
}

#[test]
fn decodes_xz_singleblock_container() {
    let (decoded, blocks) = decode_xz_container(XZ_PRESET6_SINGLEBLOCK);
    assert_eq!(blocks, 1, "fixture must be a single-block .xz");
    assert_eq!(decoded, text_fixture(1_200_000), "single-block mismatch");
}

#[test]
fn decodes_xz_multiblock_container() {
    let (decoded, blocks) = decode_xz_container(XZ_PRESET6_MULTIBLOCK);
    assert!(
        blocks >= 2,
        "fixture must be a multi-block .xz, got {blocks}"
    );
    assert_eq!(decoded, text_fixture(1_200_000), "multi-block mismatch");
}

/// Truncating a real xz stream anywhere must yield `Err` — never a panic and
/// never a silent partial `Ok`.
#[test]
fn truncated_xz_streams_error_not_panic() {
    let full = XZ_PRESET4_MULTICHUNK;
    // A spread of cut points: inside headers, inside payloads, just before
    // the end-of-stream marker.
    for cut in [
        1usize,
        4,
        5,
        100,
        61_444,
        61_450,
        full.len() / 2,
        full.len() - 1,
    ] {
        let result = decode_lzma2(&full[..cut], 64 << 20);
        assert!(
            result.is_err(),
            "truncation at {cut} must be an error, got Ok({} bytes)",
            result.map(|v| v.len()).unwrap_or(0)
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// (2) Always-on: the stateful chunked encoder must emit continuation chunks
// ─────────────────────────────────────────────────────────────────────────────

/// LZMA2-chunked output must contain continuation chunks (reset field 0) and
/// still decode byte-identical (pre-fix, every chunk carried reset field 3).
#[test]
fn chunked_encoder_emits_continuation_chunks() {
    let data = text_fixture(300_000);
    let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(64 * 1024);
    let mut encoder = Lzma2ChunkedEncoder::with_config(config);
    let encoded = encoder.encode(&data).expect("encode");

    let chunks = walk_chunks(&encoded);
    let continuations = chunks
        .iter()
        .filter(|c| matches!(c, Chunk::Lzma { reset: 0, .. }))
        .count();
    assert!(
        continuations >= 2,
        "expected continuation chunks, layout: {chunks:?}"
    );

    let decoded = oxiarc_lzma::lzma2_decompress(&encoded).expect("decode");
    assert_eq!(decoded, data, "chunked round-trip mismatch");
}

/// Cross-chunk matching regression: 4 copies of a 100 KiB random block,
/// encoded in 64 KiB chunks. Back-references must reach across chunk
/// boundaries into previous copies; with the old per-chunk dictionary reset
/// this encoded to roughly the full 400 KiB.
#[test]
fn chunked_encoder_matches_across_chunk_boundaries() {
    let block = lcg_bytes(100_000, 0xC0FFEE);
    let mut data = Vec::with_capacity(400_000);
    for _ in 0..4 {
        data.extend_from_slice(&block);
    }

    let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(64 * 1024);
    let mut encoder = Lzma2ChunkedEncoder::with_config(config);
    let encoded = encoder.encode(&data).expect("encode");

    assert!(
        encoded.len() < 200_000,
        "cross-chunk matching should compress 4x-repeated random data well \
         under half its size, got {} bytes",
        encoded.len()
    );

    let decoded = oxiarc_lzma::lzma2_decompress(&encoded).expect("decode");
    assert_eq!(decoded, data, "cross-chunk round-trip mismatch");
}

/// The streaming encoder/decoder pair must carry state across write() calls
/// and decode continuation chunks with a persistent decoder.
#[test]
fn stream_encoder_decoder_cross_chunk_roundtrip() {
    use std::io::{Read, Write};

    let data = text_fixture(300_000);

    let mut compressed = Vec::new();
    {
        let mut enc = oxiarc_lzma::Lzma2StreamEncoder::new(&mut compressed, LzmaLevel::DEFAULT)
            .with_chunk_size(16 * 1024);
        for piece in data.chunks(10_000) {
            enc.write_all(piece).expect("stream write");
        }
        enc.finish().expect("stream finish");
    }

    // The stream must contain continuation chunks — state carries across
    // write() calls now.
    let continuations = walk_chunks(&compressed)
        .iter()
        .filter(|c| matches!(c, Chunk::Lzma { reset: 0, .. }))
        .count();
    assert!(continuations >= 2, "stream encoder must emit continuations");

    let mut decoder = oxiarc_lzma::Lzma2StreamDecoder::new(Cursor::new(&compressed), 8 << 20);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).expect("stream decode");
    assert_eq!(decoded, data, "stream round-trip mismatch");
}

// ─────────────────────────────────────────────────────────────────────────────
// (3) xz-oracle: live differential testing against the `xz` CLI
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "xz-oracle")]
mod xz_oracle {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// Locate the `xz` binary; `None` means the test must self-skip.
    fn find_xz() -> Option<PathBuf> {
        let output = Command::new("which").arg("xz").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }

    /// Unique scratch file path in the system temp dir.
    fn scratch_path(label: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "oxiarc_xz_oracle_{label}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        dir
    }

    /// Run `xz` with `args` on `input` (via a temp file), capturing stdout.
    fn run_xz(label: &str, args: &[&str], input: &[u8]) -> Vec<u8> {
        let path = scratch_path(label);
        std::fs::write(&path, input).expect("write scratch input");
        let output = Command::new("xz")
            .args(args)
            .arg("-c")
            .arg(&path)
            .output()
            .expect("spawn xz");
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "[{label}] xz {args:?} failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// The diverse input corpus mandated by the remediation plan: empty,
    /// tiny, all-zeros, repetitive, incompressible, text, structured binary,
    /// and sizes crossing the 64 KiB / 2 MiB chunk boundaries.
    fn corpus() -> Vec<(&'static str, Vec<u8>)> {
        let mut structured = Vec::with_capacity(512 * 1024);
        for i in 0u32.. {
            if structured.len() >= 512 * 1024 {
                break;
            }
            structured.extend_from_slice(&i.to_le_bytes());
            structured.push((i % 251) as u8);
        }
        vec![
            ("empty", Vec::new()),
            ("one_byte", vec![b'A']),
            ("zeros_1m", vec![0u8; 1 << 20]),
            ("repetitive_256k", b"ABCD".repeat(64 * 1024)),
            ("random_1m", lcg_bytes(1 << 20, 0xDEAD_BEEF)),
            ("text_2m5", text_fixture(2_500_000)),
            ("structured_512k", structured),
            ("boundary_64k", text_fixture(65_536)),
            ("boundary_64k_plus_1", text_fixture(65_537)),
            ("boundary_2m_plus_1", text_fixture((2 << 20) + 1)),
        ]
    }

    /// Decode direction: `xz -<preset>` full containers -> oxiarc must decode
    /// byte-identical, across the whole corpus.
    fn assert_xz_to_oxiarc(preset: &str) {
        if find_xz().is_none() {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let mut cases = 0usize;
        for (name, data) in corpus() {
            let label = format!("dec_{}_{}", preset.replace('-', ""), name);
            let xz_bytes = run_xz(&label, &["-T1", "-q", preset], &data);
            let (decoded, blocks) = decode_xz_container(&xz_bytes);
            assert_eq!(
                decoded, data,
                "[{label}] oxiarc decode of xz {preset} output mismatch ({blocks} blocks)"
            );
            cases += 1;
        }
        eprintln!("[xz-oracle] xz {preset} -> oxiarc: {cases}/{cases} byte-identical");
    }

    #[test]
    fn xz_to_oxiarc_preset_0() {
        assert_xz_to_oxiarc("-0");
    }

    #[test]
    fn xz_to_oxiarc_preset_1() {
        assert_xz_to_oxiarc("-1");
    }

    #[test]
    fn xz_to_oxiarc_preset_4() {
        assert_xz_to_oxiarc("-4");
    }

    #[test]
    fn xz_to_oxiarc_preset_6() {
        assert_xz_to_oxiarc("-6");
    }

    #[test]
    fn xz_to_oxiarc_preset_9() {
        assert_xz_to_oxiarc("-9");
    }

    #[test]
    fn xz_to_oxiarc_preset_9e() {
        assert_xz_to_oxiarc("-9e");
    }

    /// Decode direction: multi-block `.xz` (`--block-size`) must decode
    /// byte-identical — each block restarts the LZMA2 chunk stream.
    #[test]
    fn xz_to_oxiarc_multiblock() {
        if find_xz().is_none() {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let data = text_fixture(1_200_000);
        let mut cases = 0usize;
        for block_size in ["--block-size=131072", "--block-size=262144"] {
            let label = format!("multiblock_{}", &block_size[13..]);
            let xz_bytes = run_xz(&label, &["-T1", "-q", "-6", block_size], &data);
            let (decoded, blocks) = decode_xz_container(&xz_bytes);
            assert!(blocks >= 2, "[{label}] expected multiple blocks");
            assert_eq!(decoded, data, "[{label}] multi-block decode mismatch");
            cases += 1;
        }
        eprintln!("[xz-oracle] multi-block xz -> oxiarc: {cases}/{cases} byte-identical");
    }

    /// Encode direction: oxiarc LZMA2 streams (including multi-chunk
    /// continuation output) must be accepted and decoded byte-identical by
    /// `xz -d --format=raw`.
    #[test]
    fn oxiarc_to_xz_raw_decode() {
        if find_xz().is_none() {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }

        // (label, level, chunk_size, data)
        let cases: Vec<(&str, u8, usize, Vec<u8>)> = vec![
            ("text_small_chunks", 6, 64 * 1024, text_fixture(1_200_000)),
            ("text_default_chunks", 6, 2 << 20, text_fixture(2_500_000)),
            ("fast_level", 1, 64 * 1024, text_fixture(400_000)),
            ("best_level_small", 9, 32 * 1024, text_fixture(120_000)),
            ("random_uncompressible", 6, 64 * 1024, lcg_bytes(300_000, 7)),
            ("zeros", 6, 64 * 1024, vec![0u8; 500_000]),
            (
                "cross_chunk_repeats",
                6,
                64 * 1024,
                lcg_bytes(100_000, 0xC0FFEE).repeat(4),
            ),
        ];

        let total = cases.len();
        let mut passed = 0usize;
        for (label, level, chunk_size, data) in cases {
            let config = Lzma2Config::with_level(LzmaLevel::new(level)).chunk_size(chunk_size);
            let mut encoder = Lzma2ChunkedEncoder::with_config(config);
            let encoded = encoder.encode(&data).expect("oxiarc encode");

            // 64 MiB >= every dictionary size this crate's levels use, and a
            // larger raw-decode dictionary accepts any smaller-window stream.
            let decoded = run_xz(
                &format!("enc_{label}"),
                &["-d", "-q", "-q", "--format=raw", "--lzma2=dict=64MiB"],
                &encoded,
            );
            assert_eq!(
                decoded, data,
                "[{label}] xz -d output differs from original input"
            );
            passed += 1;
        }
        eprintln!("[xz-oracle] oxiarc -> xz -d --format=raw: {passed}/{total} byte-identical");
    }

    /// Encode direction for the streaming writer: its output (continuation
    /// chunks across `write()` calls) must decode under `xz -d` too.
    #[test]
    fn oxiarc_stream_encoder_to_xz_raw_decode() {
        use std::io::Write;

        if find_xz().is_none() {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }

        let data = text_fixture(500_000);
        let mut compressed = Vec::new();
        {
            let mut enc = oxiarc_lzma::Lzma2StreamEncoder::new(&mut compressed, LzmaLevel::DEFAULT)
                .with_chunk_size(16 * 1024);
            for piece in data.chunks(9_999) {
                enc.write_all(piece).expect("stream write");
            }
            enc.finish().expect("stream finish");
        }

        let decoded = run_xz(
            "enc_stream",
            &["-d", "-q", "-q", "--format=raw", "--lzma2=dict=64MiB"],
            &compressed,
        );
        assert_eq!(decoded, data, "stream-encoder output mismatch under xz -d");
        eprintln!("[xz-oracle] stream encoder -> xz -d: 1/1 byte-identical");
    }
}
