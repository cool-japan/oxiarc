//! Encoder behaviour pins that do **not** need an external reference codec.
//!
//! `tests/zlib_encoder_oracle.rs` proves byte-identity with CPython's `zlib`
//! when `python3` is present; these tests run in the default configuration and
//! pin the properties that identity is *made of*, so a regression localises to
//! a mechanism rather than to "the bytes changed":
//!
//! * the per-level `configuration_table` is zlib's,
//! * lazy matching defers exactly when zlib defers, and only above `max_lazy`,
//! * the `TOO_FAR` rule rejects distant length-3 matches,
//! * the block type is chosen on real bit cost (stored for random, dynamic for
//!   structured, never fixed for a large structured block, never reserved),
//! * the encoder is bit-continuous: the call size never changes the bytes,
//! * the opt-in optimal parser round-trips every corpus and is never larger
//!   than the default ladder at the same level.

#[path = "common/blocks.rs"]
mod blocks;
#[path = "common/corpus.rs"]
mod corpus;

use blocks::{BlockType, walk_blocks};
use oxiarc_deflate::lz77::{Lz77Encoder, Lz77Params, Lz77Token};
use oxiarc_deflate::{Deflater, deflate, inflate};

/// Cap applied to the shared corpora in the (much slower) optimal-parser
/// tests so the suite stays fast; every corpus is still represented.
const OPTIMAL_CAP: usize = 96 * 1024;

fn feed_in_chunks(level: u8, optimal: bool, data: &[u8], chunk: usize) -> Vec<u8> {
    let mut d = if optimal {
        Deflater::with_optimal_parsing(level)
    } else {
        Deflater::new(level)
    };
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + chunk < data.len() {
        d.deflate(&data[i..i + chunk], &mut out, false)
            .expect("deflate chunk");
        i += chunk;
    }
    d.deflate(&data[i..], &mut out, true)
        .expect("deflate final");
    out
}

/// Walk `tokens` and return the one that starts at byte offset `offset`.
fn token_at(tokens: &[Lz77Token], offset: usize) -> Option<Lz77Token> {
    let mut pos = 0usize;
    for t in tokens {
        if pos == offset {
            return Some(*t);
        }
        pos += match *t {
            Lz77Token::Literal(_) => 1,
            Lz77Token::Match { length, .. } => usize::from(length),
        };
        if pos > offset {
            return None;
        }
    }
    None
}

fn tokens(level: u8, data: &[u8]) -> Vec<Lz77Token> {
    Lz77Encoder::with_level(level).compress(data)
}

// ---------------------------------------------------------------------------
// Level configuration table
// ---------------------------------------------------------------------------

/// zlib's `configuration_table` rows for levels 1..=9 (`deflate.c`):
/// `(good_length, max_lazy, nice_length, max_chain)`.
const ZLIB_CONFIGURATION_TABLE: [(u16, u16, u16, u16); 9] = [
    (4, 4, 8, 4),
    (4, 5, 16, 8),
    (4, 6, 32, 32),
    (4, 4, 16, 16),
    (8, 16, 32, 32),
    (8, 16, 128, 128),
    (8, 32, 128, 256),
    (32, 128, 258, 1024),
    (32, 258, 258, 4096),
];

#[test]
fn level_parameters_are_zlibs_configuration_table() {
    for (i, &(good, lazy, nice, chain)) in ZLIB_CONFIGURATION_TABLE.iter().enumerate() {
        let level = i as u32 + 1;
        let p = Lz77Params::for_level(level);
        assert_eq!(
            (p.good_length, p.max_lazy, p.nice_length, p.max_chain),
            (good, lazy, nice, chain),
            "level {level}"
        );
        // The `Deflater` must actually install the row it reports.
        let d = Deflater::new(level as u8);
        assert_eq!(d.lz77_params(), p, "level {level} deflater row");
    }
}

// ---------------------------------------------------------------------------
// Lazy matching decisions on crafted inputs
// ---------------------------------------------------------------------------

/// 16 distinct bytes, so no crafted match source ever sits at window position
/// 0 (zlib's `NIL`, which is unreachable through a hash chain by construction).
const PREFIX: &[u8] = b"0123456789abcdef";
/// Fillers with no internal repeat of length 3, so they never produce matches
/// of their own and never shift the offsets the assertions use.
const F1: &[u8] = b"ghijklmn";
const F2: &[u8] = b"opqrstuv";
const F3: &[u8] = b"wxyz{|}~";

/// A crafted input where the greedy choice at the marked offset is a 4-byte
/// match but deferring one byte buys a 9-byte match.
///
/// Layout: `PREFIX | "ABCD" | F1 | "PBCDEFGHIJ" | F2 | "ABCDEFGHIJ" | F3`.
/// Returns the buffer and the offset of the final `"ABCDEFGHIJ"`.
fn lazy_bait() -> (Vec<u8>, usize) {
    let mut v = Vec::new();
    v.extend_from_slice(PREFIX);
    v.extend_from_slice(b"ABCD");
    v.extend_from_slice(F1);
    v.extend_from_slice(b"PBCDEFGHIJ");
    v.extend_from_slice(F2);
    let mark = v.len();
    v.extend_from_slice(b"ABCDEFGHIJ");
    v.extend_from_slice(F3);
    (v, mark)
}

#[test]
fn lazy_matching_defers_to_a_longer_match_at_the_next_byte() {
    let (data, mark) = lazy_bait();
    // Levels 5-9 (`max_lazy` 16, 16, 32, 128, 258 — all above the greedy
    // match's length 4) run the lazy search and must defer.
    for level in 5u8..=9 {
        let toks = tokens(level, &data);
        assert_eq!(
            token_at(&toks, mark),
            Some(Lz77Token::Literal(b'A')),
            "level {level}: lazy matching must emit the literal and defer"
        );
        assert_eq!(
            token_at(&toks, mark + 1),
            Some(Lz77Token::Match {
                length: 9,
                distance: 18
            }),
            "level {level}: the deferred match must be the 9-byte one"
        );
    }
}

#[test]
fn max_lazy_suppresses_the_lazy_search() {
    // Level 4's `max_lazy` is 4, and zlib only runs the lazy search while
    // `prev_length < max_lazy`. The greedy match here is exactly 4 long, so
    // level 4 must take it even though a 9-byte match starts one byte later —
    // the same input on which levels 5-9 defer.
    let (data, mark) = lazy_bait();
    let toks = tokens(4, &data);
    assert_eq!(
        token_at(&toks, mark),
        Some(Lz77Token::Match {
            length: 4,
            distance: 30
        }),
        "level 4: max_lazy = 4 must suppress the lazy search"
    );
}

#[test]
fn greedy_levels_never_defer_a_match() {
    let (data, mark) = lazy_bait();
    for level in 1u8..=3 {
        let toks = tokens(level, &data);
        assert_eq!(
            token_at(&toks, mark),
            Some(Lz77Token::Match {
                length: 4,
                distance: 30
            }),
            "level {level}: deflate_fast must take the match immediately"
        );
    }
}

#[test]
fn a_match_at_the_next_byte_that_is_not_longer_is_not_taken() {
    // Layout: `PREFIX | "ABCDEFGH" | F1 | "PBCDEFGHQ" | F2 | "ABCDEFGHQ" | F3`.
    // At the mark the greedy match is 8 bytes; one byte later "BCDEFGHQ" is
    // *also* 8 bytes. zlib's rule is `match_length <= prev_length` → emit the
    // previous match, i.e. only a strictly longer successor wins.
    let mut data = Vec::new();
    data.extend_from_slice(PREFIX);
    data.extend_from_slice(b"ABCDEFGH");
    data.extend_from_slice(F1);
    data.extend_from_slice(b"PBCDEFGHQ");
    data.extend_from_slice(F2);
    let mark = data.len();
    data.extend_from_slice(b"ABCDEFGHQ");
    data.extend_from_slice(F3);

    // Levels 5-9 have `max_lazy` above 8, so the lazy search really runs and
    // really finds the equal-length alternative.
    for level in 5u8..=9 {
        let toks = tokens(level, &data);
        assert_eq!(
            token_at(&toks, mark),
            Some(Lz77Token::Match {
                length: 8,
                distance: 33
            }),
            "level {level}: an equal-length successor must not win"
        );
    }
}

#[test]
fn too_far_rejects_a_length_three_match_beyond_4096() {
    fn build(gap: usize) -> (Vec<u8>, usize) {
        let mut v = Vec::new();
        v.extend_from_slice(PREFIX);
        v.extend_from_slice(b"QZX");
        let mut rng = corpus::Rng::new(0x51ee_d0f1);
        for _ in 0..gap {
            v.push(b"abcd"[(rng.next_u32() & 3) as usize]);
        }
        let mark = v.len();
        v.extend_from_slice(b"QZX");
        v.extend_from_slice(F3);
        (v, mark)
    }

    // Near: distance 1003 <= TOO_FAR, so the length-3 match is taken.
    let (near, mark) = build(1000);
    assert_eq!(
        token_at(&tokens(9, &near), mark),
        Some(Lz77Token::Match {
            length: 3,
            distance: 1003
        }),
        "a length-3 match inside TOO_FAR must be taken"
    );

    // Far: distance 5003 > TOO_FAR, so the same match must be refused.
    let (far, mark) = build(5000);
    assert_eq!(
        token_at(&tokens(9, &far), mark),
        Some(Lz77Token::Literal(b'Q')),
        "a length-3 match beyond TOO_FAR must be refused"
    );

    // And the rule is global: no corpus may contain such a match at any level.
    for s in corpus::all_samples() {
        for level in [4u8, 6, 9] {
            for t in tokens(level, &s.data) {
                if let Lz77Token::Match { length, distance } = t {
                    assert!(
                        !(length == 3 && distance > 4096),
                        "{} level {level}: length 3 at distance {distance}",
                        s.name
                    );
                }
            }
        }
    }
}

#[test]
fn a_long_run_is_emitted_as_maximum_length_matches() {
    // 100 000 identical bytes: after the first literal every token must be a
    // distance-1 match, and the shortcut must reach MAX_MATCH (258).
    let data = vec![0x5au8; 100_000];
    let toks = tokens(6, &data);
    assert_eq!(toks[0], Lz77Token::Literal(0x5a));
    let max_matches = toks
        .iter()
        .filter(|t| {
            matches!(
                **t,
                Lz77Token::Match {
                    length: 258,
                    distance: 1
                }
            )
        })
        .count();
    assert!(
        max_matches >= 380,
        "expected ~387 maximal runs, got {max_matches} (tokens {})",
        toks.len()
    );
    assert!(
        toks.len() < 400,
        "a pure run must not produce {} tokens",
        toks.len()
    );
}

// ---------------------------------------------------------------------------
// Block type selection
// ---------------------------------------------------------------------------

#[test]
fn incompressible_data_is_stored() {
    let data = corpus::random(200 * 1024);
    for level in 1u8..=9 {
        let raw = deflate(&data, level).expect("deflate");
        let blocks = walk_blocks(&raw).expect("walk");
        assert!(
            blocks.iter().all(|b| b.btype == BlockType::Stored),
            "level {level}: random data must be stored, got {blocks:?}"
        );
        // A stored block costs its payload plus a 5-byte header, and nothing
        // else: the choice is made on real bit cost, so it can never lose to
        // a compressed block by more than the header.
        assert_eq!(
            blocks.iter().map(|b| b.uncompressed).sum::<usize>(),
            data.len(),
            "level {level}: stored blocks do not cover the input"
        );
        assert!(
            raw.len() <= data.len() + 5 * blocks.len() + 1,
            "level {level}: {} bytes for {} of input in {} blocks",
            raw.len(),
            data.len(),
            blocks.len()
        );
    }
}

#[test]
fn structured_data_uses_dynamic_huffman() {
    let data = corpus::html_like(200 * 1024);
    for level in 1u8..=9 {
        let raw = deflate(&data, level).expect("deflate");
        let blocks = walk_blocks(&raw).expect("walk");
        assert!(
            blocks.iter().all(|b| b.btype == BlockType::Dynamic),
            "level {level}: every full block of structured data should pay for \
             its own tree, got {blocks:?}"
        );
        assert_eq!(
            blocks.iter().map(|b| b.uncompressed).sum::<usize>(),
            data.len(),
            "level {level}: blocks do not cover the input"
        );
    }
}

#[test]
fn a_tiny_input_never_pays_for_a_dynamic_tree() {
    // Four bytes cannot amortise a dynamic header; zlib picks fixed or stored.
    for level in 1u8..=9 {
        let raw = deflate(b"abcd", level).expect("deflate");
        let blocks = walk_blocks(&raw).expect("walk");
        assert_eq!(blocks.len(), 1, "level {level}: {blocks:?}");
        assert!(
            blocks[0].btype != BlockType::Dynamic,
            "level {level}: a 4-byte input must not use a dynamic tree"
        );
    }
}

#[test]
fn a_single_repeated_byte_uses_the_cheapest_block_type() {
    // One literal plus ~388 maximal matches: a dynamic tree over three
    // literal/length symbols and one distance symbol beats both alternatives.
    let raw = deflate(&vec![7u8; 100_000], 6).expect("deflate");
    let blocks = walk_blocks(&raw).expect("walk");
    assert!(
        blocks.iter().all(|b| b.btype == BlockType::Dynamic),
        "{blocks:?}"
    );
    assert!(
        raw.len() < 200,
        "100 KB of one byte took {} bytes",
        raw.len()
    );
}

#[test]
fn every_block_of_every_corpus_has_a_valid_type_and_the_last_flag_is_set_once() {
    for s in corpus::all_samples() {
        for level in [1u8, 6, 9] {
            let raw = deflate(&s.data, level).expect("deflate");
            let blocks = walk_blocks(&raw).expect("walk");
            assert!(!blocks.is_empty(), "{} level {level}: no blocks", s.name);
            assert!(
                blocks.iter().all(|b| b.btype != BlockType::Reserved),
                "{} level {level}: reserved block type",
                s.name
            );
            assert_eq!(
                blocks.iter().map(|b| b.uncompressed).sum::<usize>(),
                s.data.len(),
                "{} level {level}: blocks do not cover the input",
                s.name
            );
            let last: Vec<bool> = blocks.iter().map(|b| b.last).collect();
            assert!(
                last[last.len() - 1] && last[..last.len() - 1].iter().all(|f| !f),
                "{} level {level}: BFINAL set on {last:?}",
                s.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Bit continuity / per-call invariance
// ---------------------------------------------------------------------------

#[test]
fn the_call_size_never_changes_the_encoded_bytes() {
    let data = corpus::rust_sources(1024 * 1024);
    for level in [1u8, 4, 6, 9] {
        let one_shot = deflate(&data, level).expect("deflate");
        for chunk in [1024usize, 4096, 32 * 1024, 1024 * 1024, 7, 65_521] {
            let chunked = feed_in_chunks(level, false, &data, chunk);
            assert_eq!(
                chunked.len(),
                one_shot.len(),
                "level {level}, {chunk}-byte calls: {} vs one-shot {}",
                chunked.len(),
                one_shot.len()
            );
            assert!(
                chunked == one_shot,
                "level {level}, {chunk}-byte calls: same length, different bytes"
            );
        }
    }
}

/// Level 0 is deliberately excluded from call-size byte-identity, so pin what
/// *is* true there instead of leaving the behaviour untested.
///
/// zlib's `deflate_stored` cuts each stored block at whatever it has when a
/// call arrives (it cuts on `avail_out`; this encoder, writing into an
/// unbounded sink, cuts on the format maximum), so feeding the same bytes in
/// small calls can add block headers. What must hold regardless: the decoded
/// bytes are identical, every block is stored, no block exceeds 65 535 bytes,
/// `NLEN` is the complement of `LEN`, and the overhead stays at 5 bytes per
/// block — never a silent expansion.
#[test]
fn level_0_streaming_stays_stored_and_bounded_at_every_call_size() {
    for len in [0usize, 1, 5, 65_534, 65_535, 65_536, 65_537, 131_072] {
        let data: Vec<u8> = (0..len).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
        let one_shot = deflate(&data, 0).expect("deflate");
        for chunk in [1usize, 7, 4096, 65_535, 1 << 20] {
            let chunked = feed_in_chunks(0, false, &data, chunk);
            assert_eq!(
                inflate(&chunked).expect("inflate"),
                data,
                "len {len} chunk {chunk}: round trip"
            );
            let walked = blocks::walk_blocks(&chunked).expect("walkable");
            assert!(
                walked.iter().all(|b| b.btype == blocks::BlockType::Stored),
                "len {len} chunk {chunk}: level 0 emitted a coded block"
            );
            assert!(
                walked.iter().all(|b| b.uncompressed <= 65_535),
                "len {len} chunk {chunk}: a stored block exceeds the 65535-byte maximum"
            );
            assert_eq!(
                chunked.len(),
                len + 5 * walked.len(),
                "len {len} chunk {chunk}: {} bytes for {} stored blocks is not \
                 5 bytes of header each",
                chunked.len(),
                walked.len()
            );
            // Feeding in pieces may add headers; it must never add anything
            // else, and one-shot must remain the tightest layout.
            assert!(
                chunked.len() >= one_shot.len(),
                "len {len} chunk {chunk}: one-shot {} beat by chunked {}",
                one_shot.len(),
                chunked.len()
            );
            let extra = chunked.len() - one_shot.len();
            assert_eq!(extra % 5, 0, "len {len} chunk {chunk}: {extra} extra bytes");
        }
    }
}

/// The opt-in optimal parser must be call-size invariant too, and must not pay
/// a per-call cliff.
///
/// It used to fail both: a span was parsed as soon as `MIN_LOOKAHEAD` bytes
/// were buffered, so a byte-at-a-time caller ran a 259-position candidate
/// collection to emit one byte. Measured before the fix, 200 KB of text at
/// level 9: 4.17 s in 1-byte calls against 0.31 s in one call (13x), producing
/// 20 277 bytes against 19 920 (1.8 % worse). The parser now waits for a whole
/// span, and the wait threshold is a function of the window position alone —
/// never of how the caller split its input.
#[test]
fn the_optimal_parser_is_call_size_invariant() {
    // Several shapes and sizes: the wait threshold is clamped by what the
    // window can hold at the current `strstart`
    // (`min(MAX_SPAN + MIN_LOOKAHEAD, WINDOW_SIZE - strstart)`), so a corpus
    // whose spans happen to land clear of the clamp would not exercise it.
    // 200 KiB and 132 KiB straddle several 64 KiB window cycles, and the
    // incompressible buffer advances `strstart` in a completely different
    // rhythm from the text one.
    let corpora: Vec<(&str, Vec<u8>)> = vec![
        ("log-lines-200k", corpus::log_lines(200 * 1024)),
        ("random-132k", corpus::random(132 * 1024)),
        ("html-65537", corpus::html_like(65_537)),
        ("runs-131072", corpus::runs(131_072)),
    ];
    for (name, data) in &corpora {
        for level in [6u8, 9] {
            let one_shot = feed_in_chunks(level, true, data, data.len().max(1));
            for chunk in [1usize, 16, 258, 4096, 65_536] {
                let chunked = feed_in_chunks(level, true, data, chunk);
                assert_eq!(
                    inflate(&chunked).expect("inflate"),
                    *data,
                    "{name} optimal level {level}, {chunk}-byte calls: round trip"
                );
                assert!(
                    chunked == one_shot,
                    "{name} optimal level {level}, {chunk}-byte calls: {} bytes \
                     vs one-shot {}",
                    chunked.len(),
                    one_shot.len()
                );
            }
        }
    }
}

#[test]
fn matches_reach_across_call_boundaries() {
    // The second half is a copy of the first, fed in a separate call, and is
    // small enough to stay inside the 32 KiB window. A sync flush after the
    // first half makes the per-half cost observable: without a persistent
    // window the copy could not be matched at all.
    let half = corpus::log_lines(12 * 1024);
    let mut both = half.clone();
    both.extend_from_slice(&half);

    let mut d = Deflater::new(6);
    let mut out = Vec::new();
    d.deflate_sync(&half, &mut out).expect("first");
    let after_first = out.len();
    d.deflate(&half, &mut out, true).expect("second");
    assert_eq!(inflate(&out).expect("inflate"), both);

    // Measured: 2454 bytes for the first copy, 133 for the repeat.
    let second_half_cost = out.len() - after_first;
    assert!(
        after_first > 2048
            && second_half_cost * 4 < after_first
            && second_half_cost * 50 < half.len(),
        "a repeat of the first {} bytes cost {second_half_cost} bytes \
         (first half {after_first}): the window did not survive the call boundary",
        half.len()
    );
}

#[test]
fn a_match_straddling_a_call_boundary_is_still_found() {
    // The repeat starts 20 bytes before the call boundary.
    let unit = b"the quick brown fox jumps over the lazy dog. ";
    let mut data = unit.repeat(8);
    let head = data.len();
    data.extend_from_slice(&unit.repeat(8));

    let mut d = Deflater::new(6);
    let mut out = Vec::new();
    d.deflate(&data[..head + 20], &mut out, false)
        .expect("first");
    d.deflate(&data[head + 20..], &mut out, true)
        .expect("second");
    assert_eq!(inflate(&out).expect("inflate"), data);
    assert_eq!(out, deflate(&data, 6).expect("one-shot"));
}

#[test]
fn zero_length_calls_are_inert() {
    let data = corpus::html_like(40 * 1024);
    let mut d = Deflater::new(6);
    let mut out = Vec::new();
    d.deflate(&[], &mut out, false).expect("empty");
    for chunk in data.chunks(3000) {
        d.deflate(&[], &mut out, false).expect("empty");
        d.deflate(chunk, &mut out, false).expect("chunk");
        d.deflate(&[], &mut out, false).expect("empty");
    }
    d.deflate(&[], &mut out, true).expect("finish");
    assert_eq!(out, deflate(&data, 6).expect("one-shot"));
}

// ---------------------------------------------------------------------------
// Level ladder
// ---------------------------------------------------------------------------

#[test]
fn the_level_ladder_is_ordered_where_zlibs_is() {
    // zlib's output size is *not* monotone in the level, and neither is ours,
    // because we reproduce its bytes exactly. Two measured counterexamples on
    // this corpus: `runs` goes 370, 371, 380, 380 over levels 4-7 (the level-5
    // row has a shorter `nice_length` than level 4's, so a different block
    // split wins), and `png-photo-like-rgb8` goes 159783 -> 159784 from level
    // 2 to level 3. Asserting strict monotonicity would therefore be asserting
    // a bug. What *does* hold everywhere is pinned here; the exact ladder is
    // pinned by byte-identity with CPython in `tests/zlib_encoder_oracle.rs`.
    for s in corpus::all_samples() {
        let sizes: Vec<usize> = (1u8..=9)
            .map(|l| deflate(&s.data, l).expect("deflate").len())
            .collect();
        let best = *sizes.iter().min().unwrap_or(&0);
        assert_eq!(
            sizes[8], best,
            "{}: level 9 is not the smallest level {sizes:?}",
            s.name
        );
        assert!(
            sizes[8] <= sizes[7] && sizes[7] <= sizes[6],
            "{}: the top of the ladder is not ordered {sizes:?}",
            s.name
        );
        assert!(
            sizes[2] <= sizes[0],
            "{}: level 3 is larger than level 1 {sizes:?}",
            s.name
        );
        assert!(
            sizes[3] <= sizes[0],
            "{}: the lazy family loses to level 1 {sizes:?}",
            s.name
        );
    }
}

// ---------------------------------------------------------------------------
// Optimal parser (opt-in, level 9)
// ---------------------------------------------------------------------------

#[test]
fn optimal_parsing_round_trips_every_corpus() {
    for s in corpus::all_samples() {
        let data = &s.data[..s.data.len().min(OPTIMAL_CAP)];
        let mut d = Deflater::with_optimal_parsing(9);
        let out = d.compress_to_vec(data).expect("deflate");
        assert_eq!(inflate(&out).expect("inflate"), data, "{}", s.name);
    }
}

#[test]
fn optimal_parsing_round_trips_when_fed_in_small_calls() {
    // The span protocol hashes positions past the span end and rolls them back
    // out of the chains; feeding the encoder in small calls is what exercises
    // that rollback against `fill`/slide, which a one-shot call never does.
    for s in corpus::all_samples() {
        let data = &s.data[..s.data.len().min(OPTIMAL_CAP)];
        for chunk in [997usize, 8192, 40_000] {
            let out = feed_in_chunks(9, true, data, chunk);
            assert_eq!(
                inflate(&out).expect("inflate"),
                data,
                "{} in {chunk}-byte calls",
                s.name
            );
        }
        // One byte per call is quadratic in the driver, so it runs over a
        // smaller slice of every corpus rather than not at all.
        let tiny = &data[..data.len().min(6 * 1024)];
        let out = feed_in_chunks(9, true, tiny, 1);
        assert_eq!(
            inflate(&out).expect("inflate"),
            tiny,
            "{} in 1-byte calls",
            s.name
        );
    }
}

#[test]
fn optimal_parsing_handles_degenerate_inputs() {
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0u8],
        vec![0xffu8; 2],
        b"abc".to_vec(),
        vec![0x41u8; 300_000],
        (0..70_000u32).map(|i| (i % 7) as u8).collect(),
        corpus::random(40 * 1024),
    ];
    for (n, data) in cases.iter().enumerate() {
        let mut d = Deflater::with_optimal_parsing(9);
        let out = d.compress_to_vec(data).expect("deflate");
        assert_eq!(inflate(&out).expect("inflate"), *data, "case {n}");
        let plain = deflate(data, 9).expect("deflate");
        assert!(
            out.len() <= plain.len(),
            "case {n}: optimal {} > level 9 {}",
            out.len(),
            plain.len()
        );
    }
}

#[test]
fn optimal_parsing_is_never_larger_than_the_default_ladder() {
    for s in corpus::all_samples() {
        let data = &s.data[..s.data.len().min(OPTIMAL_CAP)];
        let mut d = Deflater::with_optimal_parsing(9);
        let opt = d.compress_to_vec(data).expect("deflate");
        for level in [8u8, 9] {
            let plain = deflate(data, level).expect("deflate");
            assert!(
                opt.len() <= plain.len(),
                "{}: optimal {} > level {level} {}",
                s.name,
                opt.len(),
                plain.len()
            );
        }
    }
}
