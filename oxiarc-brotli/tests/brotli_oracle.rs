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

use std::path::{Path, PathBuf};
use std::process::Command;

use oxiarc_brotli::{BrotliParams, compress_with_params, decompress};

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
