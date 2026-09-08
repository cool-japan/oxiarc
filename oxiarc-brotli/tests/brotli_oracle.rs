//! Differential oracle tests against the reference `brotli` CLI, in BOTH
//! directions:
//!
//! 1. **Decode direction** (the primary real-world requirement):
//!    reference-`brotli`-compressed streams across the quality (0..=11) and
//!    window (10..=24) grid must decode byte-identically with OxiArc. Any
//!    `Ok` with wrong bytes (silent corruption) is an immediate failure.
//! 2. **Encode direction**: OxiArc-compressed streams must be accepted by
//!    `brotli -d` and decode back to the original bytes exactly.
//!
//! Gated behind the `brotli-oracle` feature. Each test self-skips (prints a
//! note, does not fail) if `brotli` is not found on PATH, following the
//! `lha-oracle` pattern used elsewhere in the workspace.
#![cfg(feature = "brotli-oracle")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::hand_built_dictionary_copy;
use oxiarc_brotli::{
    BrotliParams, BrotliStatus, BrotliStream, MetaBlockShape, compress_with_dictionary,
    compress_with_params, decompress, decompress_reporting_shapes, decompress_with_dictionary,
};
use oxiarc_core::traits::FlushMode;

/// Locate the `brotli` binary via `which`. Returns `None` if not found.
fn find_brotli() -> Option<PathBuf> {
    let output = Command::new("which").arg("brotli").output().ok()?;
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

/// Unique scratch directory under `std::env::temp_dir()`.
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_brotli_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Deterministic pseudo-random bytes (splitmix-style).
fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Dictionary-rich synthetic English text (words common in the RFC 7932
/// static dictionary, so reference q>=5 output contains dictionary
/// references and word transforms).
fn english_text(len: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "time",
        "down",
        "life",
        "left",
        "back",
        "code",
        "data",
        "show",
        "only",
        "site",
        "city",
        "open",
        "just",
        "like",
        "free",
        "work",
        "text",
        "year",
        "over",
        "body",
        "love",
        "form",
        "book",
        "play",
        "live",
        "line",
        "help",
        "home",
        "side",
        "more",
        "word",
        "long",
        "them",
        "view",
        "find",
        "page",
        "days",
        "full",
        "head",
        "term",
        "each",
        "area",
        "from",
        "true",
        "mark",
        "able",
        "upon",
        "high",
        "date",
        "land",
        "news",
        "even",
        "next",
        "case",
        "both",
        "post",
        "used",
        "made",
        "hand",
        "here",
        "what",
        "name",
        "the",
        "people",
        "should",
        "public",
        "information",
        "development",
        "world",
    ];
    let mut out = Vec::with_capacity(len + 16);
    let mut i = 0usize;
    while out.len() < len {
        out.extend_from_slice(WORDS[i % WORDS.len()].as_bytes());
        match i % 11 {
            10 => out.extend_from_slice(b". The "),
            4 => out.extend_from_slice(b", "),
            _ => out.push(b' '),
        }
        i += 1;
    }
    out.truncate(len);
    out
}

/// The shared corpus: diverse shapes and sizes crossing block boundaries.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0x41]),
        ("one_zero", vec![0x00]),
        ("zeros_64", vec![0u8; 64]),
        ("zeros_100k", vec![0u8; 100_000]),
        ("rep_abc", b"abc".repeat(3000)),
        (
            "rep_sentence",
            b"The quick brown fox jumps over the lazy dog. ".repeat(700),
        ),
        ("text_10k", english_text(10_000)),
        ("text_300k", english_text(300_000)),
        (
            "utf8",
            "こんにちは世界。Компрессия данных très bien. "
                .repeat(400)
                .into_bytes(),
        ),
        ("random_1k", random_bytes(1024, 42)),
        ("random_64k", random_bytes(65536, 43)),
        ("sz_65535", random_bytes(65535, 44)),
        ("sz_65537", random_bytes(65537, 45)),
        (
            "inc_u32",
            (0u32..40_000).flat_map(|i| i.to_le_bytes()).collect(),
        ),
        ("bytes_0_255", (0u8..=255).cycle().take(4096).collect()),
        ("text_1m5", english_text(1_500_000)),
    ]
}

/// Reference-compress `data` with the CLI at (quality, lgwin).
fn reference_compress(brotli: &Path, dir: &Path, data: &[u8], q: u32, w: u32) -> Vec<u8> {
    let input = dir.join("in.bin");
    let output = dir.join("in.bin.br");
    std::fs::write(&input, data).expect("write input");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .arg("-f")
        .arg("-k")
        .arg("-q")
        .arg(q.to_string())
        .arg("-w")
        .arg(w.to_string())
        .arg(&input)
        .status()
        .expect("spawn brotli");
    assert!(status.success(), "reference brotli -q {q} -w {w} failed");
    std::fs::read(&output).expect("read reference output")
}

/// Reference-decompress with the CLI; returns `None` if rejected.
fn reference_decompress(brotli: &Path, dir: &Path, compressed: &[u8]) -> Option<Vec<u8>> {
    let input = dir.join("oxi.br");
    let output = dir.join("oxi");
    std::fs::write(&input, compressed).expect("write compressed");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .arg("-d")
        .arg("-f")
        .arg("-k")
        .arg(&input)
        .status()
        .expect("spawn brotli -d");
    if !status.success() {
        return None;
    }
    Some(std::fs::read(&output).expect("read decompressed output"))
}

/// Decode direction: every reference stream must decode byte-identically.
/// Zero tolerance for errors AND for silent mismatches.
#[test]
fn test_oracle_reference_encode_oxiarc_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("dec");

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        // Full quality sweep at the default window; window sweep at two
        // representative qualities. Big inputs use a reduced grid to keep
        // the test fast.
        let big = data.len() > 200_000;
        let qualities: &[u32] = if big {
            &[1, 5, 9, 11]
        } else {
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        };
        for &q in qualities {
            let windows: &[u32] = if big || !(q == 5 || q == 11) {
                &[22]
            } else {
                &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24]
            };
            for &w in windows {
                let compressed = reference_compress(&brotli, &dir, &data, q, w);
                total += 1;
                match decompress(&compressed) {
                    Ok(ref decoded) if *decoded == data => {}
                    Ok(decoded) => failures.push(format!(
                        "SILENT MISMATCH {name} q{q} w{w}: {} != {} bytes",
                        decoded.len(),
                        data.len()
                    )),
                    Err(e) => failures.push(format!("ERROR {name} q{q} w{w}: {e}")),
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "decode direction: {}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("[brotli-oracle] decode direction: {total}/{total} reference streams byte-identical");
}

/// Encode direction: every OxiArc stream must be accepted by `brotli -d`
/// and decode back to the original bytes.
#[test]
fn test_oracle_oxiarc_encode_reference_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("enc");

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        let big = data.len() > 200_000;
        let qualities: &[u32] = if big {
            &[0, 5, 11]
        } else {
            &[0, 1, 2, 5, 6, 9, 11]
        };
        for &q in qualities {
            let windows: &[u32] = if big { &[22] } else { &[10, 16, 22, 24] };
            for &w in windows {
                let params = BrotliParams {
                    quality: q,
                    lgwin: w,
                    lgblock: 0,
                };
                let compressed = compress_with_params(&data, &params)
                    .unwrap_or_else(|e| panic!("compress {name} q{q} w{w}: {e}"));
                total += 1;
                match reference_decompress(&brotli, &dir, &compressed) {
                    Some(ref decoded) if *decoded == data => {}
                    Some(decoded) => failures.push(format!(
                        "MISMATCH {name} q{q} w{w}: reference decoded {} != {} bytes",
                        decoded.len(),
                        data.len()
                    )),
                    None => failures.push(format!("REJECTED {name} q{q} w{w} by brotli -d")),
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "encode direction: {}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "[brotli-oracle] encode direction: {total}/{total} OxiArc streams accepted by brotli -d"
    );
}

/// Multi-meta-block boundary: force small lgblock so multiple compressed
/// meta-blocks are emitted, and verify the reference decoder agrees.
#[test]
fn test_oracle_multiblock_streams() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("multi");

    let data = english_text(300_000);
    for q in [1u32, 5, 9] {
        let params = BrotliParams {
            quality: q,
            lgwin: 22,
            lgblock: 16, // 64 KiB meta-blocks -> 5 compressed blocks
        };
        let compressed = compress_with_params(&data, &params).expect("compress");
        let decoded = reference_decompress(&brotli, &dir, &compressed)
            .unwrap_or_else(|| panic!("multi-block q{q} rejected by brotli -d"));
        assert_eq!(decoded, data, "multi-block q{q} content mismatch");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Literal block splitting (NBLTYPESL > 1)
// ---------------------------------------------------------------------------

/// Minimal LSB-first bit reader for walking a Brotli stream header.
///
/// Deliberately an independent re-reading of RFC 7932 Sections 9.1/9.2 rather
/// than a reuse of the crate's decoder, so a shared misunderstanding cannot
/// make the assertions below agree with the encoder for the wrong reason.
struct HeaderBits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> HeaderBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        HeaderBits { data, position: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.position / 8)?;
        let value = u32::from((byte >> (self.position % 8)) & 1);
        self.position += 1;
        Some(value)
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for index in 0..count {
            value |= self.bit()? << index;
        }
        Some(value)
    }
}

/// Read the Section 9.2 `NBLTYPES` variable-length code.
fn read_nbltypes(bits: &mut HeaderBits<'_>) -> Option<u32> {
    if bits.bit()? == 0 {
        return Some(1);
    }
    let n = bits.bits(3)?;
    if n == 0 {
        return Some(2);
    }
    Some((1 << n) + 1 + bits.bits(n)?)
}

/// Return `NBLTYPESL` for the first compressed meta-block of `stream`.
///
/// `None` means the stream had no compressed meta-block (all stored/metadata),
/// which the callers treat as "no evidence either way".
fn first_meta_block_literal_types(stream: &[u8]) -> Option<u32> {
    let mut bits = position_at_meta_block_header(stream)?;
    read_nbltypes(&mut bits)
}

/// Advance a fresh reader to the first byte of the first *compressed*
/// meta-block's block-type fields, skipping the stream header and any
/// stored/metadata meta-blocks.
fn position_at_meta_block_header(stream: &[u8]) -> Option<HeaderBits<'_>> {
    let mut bits = HeaderBits::new(stream);

    // Stream header: WBITS (Section 9.1).
    if bits.bit()? == 1 {
        let n = bits.bits(3)?;
        if n == 0 {
            bits.bits(3)?;
        }
    }

    loop {
        let is_last = bits.bit()? == 1;
        if is_last && bits.bit()? == 1 {
            return None; // empty last meta-block
        }
        let mnibbles_code = bits.bits(2)?;
        if mnibbles_code == 3 {
            // Metadata meta-block: reserved bit, MSKIPBYTES, skip length, then
            // byte-aligned payload.
            bits.bit()?;
            let mskipbytes = bits.bits(2)?;
            let mut skip = 0usize;
            for index in 0..mskipbytes {
                skip |= (bits.bits(8)? as usize) << (index * 8);
            }
            if mskipbytes > 0 {
                skip += 1;
            }
            bits.position = bits.position.div_ceil(8) * 8 + skip * 8;
            continue;
        }
        let mlen = bits.bits((mnibbles_code + 4) * 4)? as usize + 1;
        // ISUNCOMPRESSED is only present when ISLAST is 0.
        if !is_last && bits.bit()? == 1 {
            // Uncompressed meta-block: byte-aligned payload of MLEN bytes.
            bits.position = bits.position.div_ceil(8) * 8 + mlen * 8;
            continue;
        }
        return Some(bits);
    }
}

/// Input built from two statistically distinct halves — the case literal block
/// splitting exists for.
///
/// Both halves are individually incompressible, so LZ77 leaves them as
/// literals; what differs is the *alphabet* each half draws from. That is
/// exactly the situation a single literal prefix code handles badly and two
/// block types handle well — an archive holding a text file next to a JPEG,
/// or a base64 blob next to packed binary.
fn two_population_input() -> Vec<u8> {
    let mut data = Vec::new();
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 40) as u8
    };
    // Half 1: printable ASCII, 64-symbol alphabet starting at 0x20.
    for _ in 0..90_000 {
        data.push(0x20 | (next() & 0x3F));
    }
    // Half 2: high bytes, a disjoint 64-symbol alphabet.
    for _ in 0..90_000 {
        data.push(0xC0 | (next() & 0x3F));
    }
    data
}

/// The walker must agree with the reference on frames the reference produced,
/// so a walker bug cannot make the assertions below pass or fail spuriously.
#[test]
fn test_oracle_header_walker_parses_reference_streams() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("walker");
    for (name, data) in corpus() {
        if data.is_empty() {
            continue;
        }
        for quality in [1u32, 5, 9, 11] {
            let stream = reference_compress(&brotli, &dir, &data, quality, 22);
            // A `None` result is legitimate (stored-only streams); a panic or a
            // wildly out-of-range value is not.
            if let Some(types) = first_meta_block_literal_types(&stream) {
                assert!(
                    (1..=256).contains(&types),
                    "walker read NBLTYPESL={types} from reference {name} q{quality}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Quality 1-9 must be byte-frozen: block splitting is a quality 10-11 feature,
/// so every lower quality must still emit `NBLTYPESL = 1`.
///
/// This is the guard that the reference-verified lower-quality output (and the
/// streaming encoder built on it) cannot regress when the splitter changes.
#[test]
fn test_block_splitting_is_confined_to_quality_10_and_11() {
    let data = two_population_input();
    for quality in 1u32..=9 {
        let params = BrotliParams {
            quality,
            ..Default::default()
        };
        let stream = compress_with_params(&data, &params).expect("compress");
        if let Some(types) = first_meta_block_literal_types(&stream) {
            assert_eq!(
                types, 1,
                "quality {quality} must not emit literal block types (got NBLTYPESL={types})"
            );
        }
    }
}

/// Encode direction with literal block splitting: oxiarc must actually emit
/// `NBLTYPESL > 1` on heterogeneous data at quality 11, and the reference
/// `brotli -d` must accept the result and reproduce the input exactly.
///
/// The `NBLTYPESL` assertion is what stops this test from passing vacuously on
/// a single-block-type stream.
#[test]
fn test_oracle_literal_block_splitting_accepted_by_reference() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("blocksplit");
    let data = two_population_input();

    let mut saw_split = false;
    for quality in [10u32, 11] {
        let params = BrotliParams {
            quality,
            ..Default::default()
        };
        let stream = compress_with_params(&data, &params).expect("compress");
        if first_meta_block_literal_types(&stream).is_some_and(|types| types > 1) {
            saw_split = true;
        }

        let decoded = reference_decompress(&brotli, &dir, &stream)
            .unwrap_or_else(|| panic!("reference brotli REJECTED oxiarc q{quality} stream"));
        assert!(
            decoded == data,
            "reference decode differs from the input at q{quality}"
        );
        assert_eq!(
            decompress(&stream).expect("oxiarc self-decode"),
            data,
            "oxiarc self-decode differs at q{quality}"
        );
    }

    assert!(
        saw_split,
        "no meta-block used NBLTYPESL > 1; the oracle check would be vacuous"
    );
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[brotli-oracle] literal block splitting accepted by reference brotli");
}

/// Every corpus entry must round-trip at the qualities on both sides of the
/// block-splitting cutoff (9 = never splits, 10 and 11 = may split).
///
/// This is a round-trip test, not a size test. The "a split can never grow the
/// stream" property is *structural*, not statistical: `write_meta_block_body`
/// is called twice — once with the split candidate and once without — and the
/// caller appends whichever wrote fewer bits, so there is no input for which
/// the split branch can produce a larger meta-block than the plain branch. A
/// size comparison across qualities could not establish that anyway, since
/// different qualities also use different LZ77 parameters.
#[test]
fn test_round_trip_across_the_block_splitting_cutoff() {
    for (name, data) in corpus() {
        if data.len() < 4096 {
            continue;
        }
        for quality in [9u32, 10, 11] {
            let params = BrotliParams {
                quality,
                ..Default::default()
            };
            let stream = compress_with_params(&data, &params)
                .unwrap_or_else(|e| panic!("compress {name} q{quality}: {e}"));
            assert_eq!(
                decompress(&stream).expect("stream decodes"),
                data,
                "round trip failed for {name} at q{quality}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Insert-and-copy / distance block splitting and context modeling
// ---------------------------------------------------------------------------

/// Input whose *command* statistics change halfway, which is what
/// insert-and-copy and distance block splitting exist for.
///
/// The first half is long, highly repetitive lines: LZ77 finds long matches at
/// short, repeating distances, so the commands cluster on large copy codes and
/// low distance symbols. The second half is short unique records separated by
/// incompressible noise: many literals, few and short copies, scattered
/// distances. One insert-and-copy code (and one distance code) has to average
/// those two regimes together.
fn two_regime_input() -> Vec<u8> {
    let mut data = Vec::new();
    let mut state = 0x5eed_1234_abcd_9876u64;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 40) as u8
    };

    // Regime A: a small set of long lines, repeated. Long copies, short
    // distances, almost no literals.
    let lines: Vec<String> = (0..8)
        .map(|i| format!("[{i:02}] the quick brown fox jumps over the lazy dog again and again\n"))
        .collect();
    for round in 0..1200 {
        data.extend_from_slice(lines[round % lines.len()].as_bytes());
    }

    // Regime B: unique short records interleaved with noise. Many literals,
    // short copies, scattered distances.
    for record in 0..4000u32 {
        data.extend_from_slice(format!("r{record:06}=").as_bytes());
        for _ in 0..12 {
            data.push(next());
        }
        data.push(b'\n');
    }
    data
}

/// Text whose byte distribution depends strongly on the preceding byte —
/// the case within-block-type context modeling exists for.
fn context_dependent_input() -> Vec<u8> {
    let mut data = Vec::new();
    let mut state = 0x1357_9bdf_0246_8aceu64;
    let mut next = |modulus: u32| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as u32 % modulus) as u8
    };
    // Strictly alternating alphabets: after a digit always comes a letter and
    // vice versa, so the LSB6 context of the previous byte predicts the next
    // byte's alphabet perfectly while the marginal distribution does not.
    for _ in 0..120_000 {
        data.push(b'0' + next(10));
        data.push(b'a' + next(26));
    }
    data
}

/// Compress at `quality` and report the shapes the crate's own (reference-
/// validated) decoder parsed back out.
fn shapes_of(data: &[u8], quality: u32) -> (Vec<u8>, Vec<oxiarc_brotli::MetaBlockShape>) {
    let params = BrotliParams {
        quality,
        ..Default::default()
    };
    let stream = compress_with_params(data, &params).expect("compress");
    let (decoded, shapes) =
        oxiarc_brotli::decompress_reporting_shapes(&stream).expect("self-decode");
    assert_eq!(decoded, data, "self-decode must reproduce the input");
    (stream, shapes)
}

/// Insert-and-copy block splitting must actually reach the wire on data whose
/// command statistics change, and the reference decoder must accept it.
#[test]
fn test_oracle_insert_and_copy_block_splitting_accepted_by_reference() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("icsplit");
    let data = two_regime_input();

    let mut saw_split = false;
    for quality in [10u32, 11] {
        let (stream, shapes) = shapes_of(&data, quality);
        if shapes.iter().any(|shape| shape.insert_and_copy_types > 1) {
            saw_split = true;
        }
        let decoded = reference_decompress(&brotli, &dir, &stream)
            .unwrap_or_else(|| panic!("reference brotli REJECTED oxiarc q{quality} stream"));
        assert_eq!(decoded, data, "reference decode differs at q{quality}");
    }

    assert!(
        saw_split,
        "no meta-block used NBLTYPESI > 1; the oracle check would be vacuous"
    );
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[brotli-oracle] insert-and-copy block splitting accepted by reference brotli");
}

/// Distance block splitting must reach the wire and be reference-accepted.
#[test]
fn test_oracle_distance_block_splitting_accepted_by_reference() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("distsplit");
    let data = two_regime_input();

    let mut saw_split = false;
    for quality in [10u32, 11] {
        let (stream, shapes) = shapes_of(&data, quality);
        if shapes.iter().any(|shape| shape.distance_types > 1) {
            saw_split = true;
        }
        let decoded = reference_decompress(&brotli, &dir, &stream)
            .unwrap_or_else(|| panic!("reference brotli REJECTED oxiarc q{quality} stream"));
        assert_eq!(decoded, data, "reference decode differs at q{quality}");
    }

    assert!(
        saw_split,
        "no meta-block used NBLTYPESD > 1; the oracle check would be vacuous"
    );
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[brotli-oracle] distance block splitting accepted by reference brotli");
}

/// Per-context histogram assignment (`NTREESL > NBLTYPESL`, i.e. two contexts
/// of one block type coded with different prefix codes) must reach the wire and
/// be reference-accepted.
#[test]
fn test_oracle_literal_context_modeling_accepted_by_reference() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("ctxmodel");
    let data = context_dependent_input();

    let mut saw_context_modeling = false;
    for quality in [10u32, 11] {
        let (stream, shapes) = shapes_of(&data, quality);
        // More literal trees than literal block types can only come from
        // context modeling: with one code per block type the two are equal.
        if shapes
            .iter()
            .any(|shape| shape.literal_trees > shape.literal_types)
        {
            saw_context_modeling = true;
        }
        let decoded = reference_decompress(&brotli, &dir, &stream)
            .unwrap_or_else(|| panic!("reference brotli REJECTED oxiarc q{quality} stream"));
        assert_eq!(decoded, data, "reference decode differs at q{quality}");
    }

    assert!(
        saw_context_modeling,
        "no meta-block bound more literal codes than block types; \
         within-block-type context modeling never fired"
    );
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[brotli-oracle] literal context modeling accepted by reference brotli");
}

/// The splitting qualities must beat the frozen pre-splitting encoder on
/// context-dependent data.
///
/// **What this does and does not establish.** Quality 9 is byte-frozen to the
/// pre-splitting encoder, so this is a genuine end-to-end "before vs after"
/// from a user's point of view — but it is *not* an isolated A/B on context
/// modeling alone, because quality also feeds the LZ77 parameters, so some of
/// the difference comes from a different parse. The feature-specific claims are
/// made by the shape assertions above (context modeling provably reached the
/// wire) and by the encoder's structure (coordinate ascent accepts a plan only
/// when the fully-written meta-block gets strictly smaller, so at a *fixed*
/// quality the feature can never cost ratio). This test guards the combination
/// actually shipping as an improvement.
#[test]
fn test_splitting_qualities_beat_the_frozen_encoder() {
    let data = context_dependent_input();
    let baseline = compress_with_params(
        &data,
        &BrotliParams {
            quality: 9,
            ..Default::default()
        },
    )
    .expect("compress q9");
    let modeled = compress_with_params(
        &data,
        &BrotliParams {
            quality: 11,
            ..Default::default()
        },
    )
    .expect("compress q11");
    assert!(
        modeled.len() < baseline.len(),
        "context modeling must pay: q11 {} bytes vs q9 {} bytes",
        modeled.len(),
        baseline.len()
    );
    eprintln!(
        "[brotli-oracle] context modeling: {} -> {} bytes ({:.1}% smaller)",
        baseline.len(),
        modeled.len(),
        100.0 * (1.0 - modeled.len() as f64 / baseline.len() as f64)
    );
}

/// Quality 1-9 must stay byte-frozen for *every* category, not just literals.
#[test]
fn test_no_category_splits_below_quality_ten() {
    for data in [two_regime_input(), context_dependent_input()] {
        for quality in 1u32..=9 {
            let (_, shapes) = shapes_of(&data, quality);
            for shape in &shapes {
                assert_eq!(shape.literal_types, 1, "q{quality} NBLTYPESL");
                assert_eq!(shape.insert_and_copy_types, 1, "q{quality} NBLTYPESI");
                assert_eq!(shape.distance_types, 1, "q{quality} NBLTYPESD");
                assert_eq!(shape.literal_trees, 1, "q{quality} NTREESL");
                assert_eq!(shape.distance_trees, 1, "q{quality} NTREESD");
            }
        }
    }
}

/// The whole corpus must round-trip through the reference decoder at the
/// splitting qualities — the broad safety net behind the targeted checks above.
#[test]
fn test_oracle_corpus_accepted_by_reference_at_splitting_qualities() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("splitcorpus");
    for (name, data) in corpus() {
        if data.is_empty() {
            continue;
        }
        for quality in [10u32, 11] {
            let params = BrotliParams {
                quality,
                ..Default::default()
            };
            let stream = compress_with_params(&data, &params).expect("compress");
            let decoded = reference_decompress(&brotli, &dir, &stream)
                .unwrap_or_else(|| panic!("reference brotli REJECTED oxiarc {name} at q{quality}"));
            assert_eq!(
                decoded, data,
                "reference decode differs for {name} q{quality}"
            );
            assert_eq!(
                decompress(&stream).expect("self-decode"),
                data,
                "self-decode differs for {name} q{quality}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("[brotli-oracle] full corpus accepted by reference brotli at q10/q11");
}

// ─── Incremental decode leg ─────────────────────────────────────────────────

/// Drive [`BrotliStream`] over `data` with fixed input/output chunk sizes.
///
/// Returns the decoded bytes together with the meta-block shapes the
/// incremental decoder observed.
fn incremental_decode(
    data: &[u8],
    in_chunk: usize,
    out_chunk: usize,
) -> Result<(Vec<u8>, Vec<MetaBlockShape>), String> {
    let mut stream = BrotliStream::new().with_shape_recording(true);
    let mut decoded = Vec::new();
    let mut buf = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0u64;
    let budget = (data.len() as u64 + 1) * 64 + 4_000_000;
    loop {
        calls += 1;
        if calls > budget {
            return Err("decoder did not terminate".to_string());
        }
        let end = (pos + in_chunk.max(1)).min(data.len());
        let flush = if end == data.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&data[pos..end], &mut buf, flush)
            .map_err(|e| e.to_string())?;
        pos += progress.consumed;
        decoded.extend_from_slice(&buf[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd && pos == data.len() {
            break;
        }
        if progress.consumed == 0 && progress.produced == 0 && end == data.len() {
            return Err("decoder stalled with all input offered".to_string());
        }
    }
    stream.finish().map_err(|e| e.to_string())?;
    let shapes = stream.recorded_shapes().to_vec();
    Ok((decoded, shapes))
}

/// Incremental decode direction: every reference stream must decode
/// byte-identically through [`BrotliStream`], under several chunkings, and
/// must observe the same meta-block shapes as the one-shot decoder.
///
/// This is the strongest interop check in the crate: the reference encoder's
/// streams carry block splits, real context maps, static-dictionary references
/// with transforms and uncompressed meta-blocks — the exact features that make
/// resumable parsing hard, and that this crate's own encoder does not emit.
#[test]
fn test_oracle_reference_encode_incremental_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("incdec");

    let mut total = 0usize;
    let mut shape_checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        let big = data.len() > 200_000;
        let qualities: &[u32] = if big { &[5, 11] } else { &[0, 1, 5, 9, 11] };
        for &q in qualities {
            let windows: &[u32] = if big || q != 11 {
                &[22]
            } else {
                &[10, 16, 22, 24]
            };
            for &w in windows {
                let compressed = reference_compress(&brotli, &dir, &data, q, w);
                let (expected, expected_shapes) = match decompress_reporting_shapes(&compressed) {
                    Ok(pair) => pair,
                    Err(e) => {
                        failures.push(format!("one-shot ERROR {name} q{q} w{w}: {e}"));
                        continue;
                    }
                };
                if expected != data {
                    failures.push(format!("one-shot MISMATCH {name} q{q} w{w}"));
                    continue;
                }
                // Big inputs use a coarse chunking to keep the test fast; the
                // small ones get the punishing one-byte-in/one-byte-out grid.
                let schedules: &[(usize, usize)] = if big {
                    &[(4096, 65536), (7, 13)]
                } else {
                    &[(1, 1), (3, 7), (127, 251), (usize::MAX / 2, 65536)]
                };
                for &(in_chunk, out_chunk) in schedules {
                    total += 1;
                    match incremental_decode(&compressed, in_chunk, out_chunk) {
                        Ok((got, shapes)) => {
                            if got != data {
                                failures.push(format!(
                                    "SILENT MISMATCH {name} q{q} w{w} in={in_chunk} out={out_chunk}: \
                                     {} != {} bytes",
                                    got.len(),
                                    data.len()
                                ));
                            } else if shapes != expected_shapes {
                                failures.push(format!(
                                    "SHAPE MISMATCH {name} q{q} w{w} in={in_chunk} out={out_chunk}: \
                                     {} shapes vs {} from the one-shot decoder",
                                    shapes.len(),
                                    expected_shapes.len()
                                ));
                            } else {
                                shape_checked += 1;
                            }
                        }
                        Err(e) => failures.push(format!(
                            "ERROR {name} q{q} w{w} in={in_chunk} out={out_chunk}: {e}"
                        )),
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "incremental decode: {}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "[brotli-oracle] incremental decode: {total}/{total} reference streams byte-identical, \
         {shape_checked} with matching meta-block shapes"
    );
}

/// Every prefix of a reference stream must be rejected by the incremental
/// decoder — no panic, no hang, no short body reported as success.
#[test]
fn test_oracle_reference_truncations_rejected_incrementally() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("inctrunc");

    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        if data.len() > 20_000 {
            continue;
        }
        for &q in &[1u32, 5, 11] {
            let compressed = reference_compress(&brotli, &dir, &data, q, 22);
            for cut in 0..compressed.len() {
                if incremental_decode(&compressed[..cut], 1, 1).is_ok() {
                    failures.push(format!(
                        "{name} q{q}: a {cut}/{}-byte prefix decoded successfully",
                        compressed.len()
                    ));
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{} truncations accepted:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("[brotli-oracle] every reference-stream prefix rejected by the incremental decoder");
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared (custom LZ77) dictionaries — `brotli -D FILE`, both directions
// ─────────────────────────────────────────────────────────────────────────────

/// Whether this `brotli` build understands `-D/--dictionary`. The option has
/// been present since 1.0, but a build without it must skip rather than fail.
fn brotli_supports_dictionary(brotli: &Path) -> bool {
    let Ok(out) = Command::new(brotli).arg("--help").output() else {
        return false;
    };
    // `brotli --help` writes to stdout on 1.1.0 and to stderr on some builds.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.contains("--dictionary")
}

/// Reference-compress `data` against `dict_path` at (quality, lgwin).
fn reference_compress_with_dictionary(
    brotli: &Path,
    dir: &Path,
    dict_path: &Path,
    data: &[u8],
    q: u32,
    w: u32,
) -> Vec<u8> {
    let input = dir.join("din.bin");
    let output = dir.join("din.bin.br");
    std::fs::write(&input, data).expect("write input");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .args(["-f", "-k", "-q"])
        .arg(q.to_string())
        .arg("-w")
        .arg(w.to_string())
        .arg("-D")
        .arg(dict_path)
        .arg(&input)
        .status()
        .expect("spawn brotli -D");
    assert!(status.success(), "reference brotli -D -q {q} -w {w} failed");
    std::fs::read(&output).expect("read reference output")
}

/// Reference-decompress `compressed` against `dict_path`; `None` if rejected.
fn reference_decompress_with_dictionary(
    brotli: &Path,
    dir: &Path,
    dict_path: &Path,
    compressed: &[u8],
) -> Option<Vec<u8>> {
    let input = dir.join("doxi.br");
    let output = dir.join("doxi");
    std::fs::write(&input, compressed).expect("write compressed");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .args(["-d", "-f", "-k", "-D"])
        .arg(dict_path)
        .arg(&input)
        .status()
        .expect("spawn brotli -d -D");
    if !status.success() {
        return None;
    }
    Some(std::fs::read(&output).expect("read decompressed output"))
}

/// A structured dictionary plus the payloads worth coding against it.
fn dictionary_corpus() -> (Vec<u8>, Vec<(&'static str, Vec<u8>)>) {
    let mut dict = Vec::new();
    for i in 0..2000u32 {
        dict.extend_from_slice(
            format!("line {i:06}: the quick brown fox jumps over the lazy dog\n").as_bytes(),
        );
    }
    let payloads: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "one_line",
            b"line 000042: the quick brown fox jumps over the lazy dog\n".to_vec(),
        ),
        ("dict_head", dict[..4096].to_vec()),
        ("dict_tail", dict[dict.len() - 4096..].to_vec()),
        ("dict_middle", dict[20_000..60_000].to_vec()),
        ("whole_dictionary", dict.clone()),
        ("dict_then_novel", {
            let mut v = dict[1000..9000].to_vec();
            v.extend_from_slice(english_text(20_000).as_slice());
            v
        }),
        ("novel_then_dict", {
            let mut v = english_text(200_000);
            v.extend_from_slice(&dict[..8000]);
            v
        }),
        ("unrelated", random_bytes(20_000, 77)),
    ];
    (dict, payloads)
}

/// Decode direction with a shared dictionary: every reference `-D` stream must
/// decode byte-identically through both this crate's decoders.
#[test]
fn test_oracle_reference_dictionary_encode_oxiarc_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    if !brotli_supports_dictionary(&brotli) {
        eprintln!("[brotli-oracle] `brotli` has no --dictionary; skipping (not a failure)");
        return;
    }
    let dir = scratch_dir("dictdec");
    let (dict, payloads) = dictionary_corpus();
    let dict_path = dir.join("dict.bin");
    std::fs::write(&dict_path, &dict).expect("write dictionary");

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in payloads {
        for q in [5u32, 9, 11] {
            // lgwin 10 is a 1008-byte window, far below the 114 KB dictionary:
            // the case that proves shared-dictionary distances legitimately
            // exceed the declared window.
            for w in [10u32, 16, 22] {
                total += 1;
                let compressed =
                    reference_compress_with_dictionary(&brotli, &dir, &dict_path, &data, q, w);
                match decompress_with_dictionary(&compressed, &dict) {
                    Ok(got) if got == data => {}
                    Ok(got) => failures.push(format!(
                        "{name} q{q} w{w}: SILENT MISMATCH ({} vs {} bytes)",
                        got.len(),
                        data.len()
                    )),
                    Err(e) => failures.push(format!("{name} q{q} w{w}: one-shot error {e}")),
                }
                // The push decoder must agree, at an awkward chunking.
                let mut stream = BrotliStream::new().with_dictionary(dict.clone());
                let mut out = vec![0u8; 4096];
                let mut got = Vec::new();
                let mut pos = 0usize;
                let mut calls = 0u64;
                let verdict = loop {
                    calls += 1;
                    if calls > (compressed.len() as u64 + 1) * 8 + 100_000 {
                        break Some("push decoder did not terminate".to_string());
                    }
                    let end = (pos + 61).min(compressed.len());
                    let flush = if end == compressed.len() {
                        FlushMode::Finish
                    } else {
                        FlushMode::None
                    };
                    match stream.decode(&compressed[pos..end], &mut out, flush) {
                        Ok(p) => {
                            pos += p.consumed;
                            got.extend_from_slice(&out[..p.produced]);
                            if p.status == BrotliStatus::StreamEnd && pos == compressed.len() {
                                break None;
                            }
                        }
                        Err(e) => break Some(format!("push error {e}")),
                    }
                };
                match verdict {
                    Some(msg) => failures.push(format!("{name} q{q} w{w}: {msg}")),
                    None if got != data => {
                        failures.push(format!("{name} q{q} w{w}: push decoder MISMATCH"));
                    }
                    None => {}
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{}/{total} reference dictionary streams failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        total >= 60,
        "dictionary decode oracle only ran {total} cases"
    );
    eprintln!("[brotli-oracle] {total} reference `-D` streams decoded byte-identically");
}

/// Encode direction with a shared dictionary: the reference decoder must
/// accept everything `compress_with_dictionary` produces.
#[test]
fn test_oracle_oxiarc_dictionary_encode_reference_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    if !brotli_supports_dictionary(&brotli) {
        eprintln!("[brotli-oracle] `brotli` has no --dictionary; skipping (not a failure)");
        return;
    }
    let dir = scratch_dir("dictenc");
    let (dict, payloads) = dictionary_corpus();
    let dict_path = dir.join("dict.bin");
    std::fs::write(&dict_path, &dict).expect("write dictionary");

    let mut total = 0usize;
    let mut smaller = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in payloads {
        for q in [1u32, 5, 9, 11] {
            for w in [10u32, 16, 22] {
                total += 1;
                let params = BrotliParams {
                    quality: q,
                    lgwin: w,
                    lgblock: 0,
                };
                let compressed =
                    compress_with_dictionary(&data, &dict, &params).expect("oxiarc compress -D");
                match reference_decompress_with_dictionary(&brotli, &dir, &dict_path, &compressed) {
                    Some(got) if got == data => {}
                    Some(got) => failures.push(format!(
                        "{name} q{q} w{w}: reference decoded {} bytes, wanted {}",
                        got.len(),
                        data.len()
                    )),
                    None => failures.push(format!(
                        "{name} q{q} w{w}: reference `brotli -d -D` rejected the stream"
                    )),
                }
                if compressed.len() < compress_with_params(&data, &params).expect("plain").len() {
                    smaller += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{}/{total} oxiarc dictionary streams failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // Anti-vacuity: the dictionary must actually be reaching the wire, not
    // merely being ignored in favour of the dictionary-free encoding.
    assert!(
        smaller * 2 >= total,
        "only {smaller}/{total} dictionary streams beat the dictionary-free encoder: \
         the dictionary is not reaching the wire"
    );
    eprintln!(
        "[brotli-oracle] {total} oxiarc `-D` streams accepted by the reference decoder \
         ({smaller} smaller than the dictionary-free encoding)"
    );
}

/// A shared-dictionary copy may not run past the end of the dictionary — and
/// this is the reference decoder saying so, not us.
///
/// Two hand-built streams differ in exactly one field, the copy length of a
/// single command that reads the dictionary's last 4 bytes:
///
/// * copy 4 stops at the dictionary's end. `brotli -d -D` accepts it and
///   reproduces the bytes this crate produces.
/// * copy 5, 10 and 22 would continue past it. `brotli -d -D` reports
///   "corrupt input" for every one of them.
///
/// So the dictionary is a compound history block, not a prefix glued in front
/// of the sliding window: a copy cannot walk out of it and carry on in the
/// produced output. Both of this crate's decoders reject the overruns
/// (`shared_dictionary.rs::a_copy_running_past_the_dictionary_end_is_refused_at_every_chunking`);
/// this test is what stops that decision from silently drifting back, because
/// it re-derives it from the reference every run.
#[test]
fn test_oracle_reference_rejects_a_copy_past_the_dictionary_end() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    if !brotli_supports_dictionary(&brotli) {
        eprintln!("[brotli-oracle] `brotli` has no --dictionary; skipping (not a failure)");
        return;
    }
    let dir = scratch_dir("dictoverrun");
    let dict_path = dir.join("dict.bin");
    let input = dir.join("in.br");
    let output = dir.join("out.bin");

    let mut checked = 0usize;
    for copy_len in [4u32, 5, 10, 22] {
        let (stream, dict, expected) = hand_built_dictionary_copy(copy_len);
        std::fs::write(&dict_path, &dict).expect("write dictionary");
        std::fs::write(&input, &stream).expect("write stream");
        let _ = std::fs::remove_file(&output);
        let status = Command::new(&brotli)
            .args(["-d", "-f", "-D"])
            .arg(&dict_path)
            .arg("-o")
            .arg(&output)
            .arg(&input)
            .status()
            .expect("spawn brotli -d -D");
        checked += 1;
        match expected {
            Some(want) => {
                assert!(
                    status.success(),
                    "reference rejected a legal dictionary copy of {copy_len} bytes"
                );
                let got = std::fs::read(&output).expect("read reference output");
                assert_eq!(got, want, "reference bytes for copy {copy_len}");
                assert_eq!(
                    decompress_with_dictionary(&stream, &dict).expect("oxiarc control"),
                    want,
                    "oxiarc bytes for copy {copy_len}"
                );
            }
            None => {
                assert!(
                    !status.success(),
                    "reference ACCEPTED a copy of {copy_len} bytes running past the \
                     dictionary's end; this crate's decoders reject it, so the two \
                     have diverged and the rejection must be revisited"
                );
                assert!(
                    decompress_with_dictionary(&stream, &dict).is_err(),
                    "oxiarc accepted a copy {copy_len} past the dictionary's end"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(checked, 4, "dictionary-overrun oracle did not run");
    eprintln!(
        "[brotli-oracle] reference accepts a dictionary copy that stops at the end \
         and rejects all 3 that run past it"
    );
}
