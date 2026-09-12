//! Adversarial sweep over the DEFLATE **encoder** (track `DEFENC-verify`).
//!
//! `tests/adversarial_verify.rs` hammers the decoder; `tests/encoder_behaviour.rs`
//! pins the encoder's structural decisions on friendly corpora. Neither reaches
//! the combination this file exists for: hostile *sizes* (every window, block
//! and match boundary) crossed with hostile *call patterns* (one byte at a
//! time, exactly one window, exactly one stored block) crossed with every
//! level, strategy, flush mode, dictionary and the optimal parser.
//!
//! The encoder indexes its window directly (`win[scan + best_len]`,
//! `read_u64(win, m + i)`) and reads deliberately past the lookahead into the
//! zeroed high-water region, so the shapes that matter are the ones that put
//! the data end near `2 * W_SIZE` with zeros on both sides of a comparison —
//! `zeros-at-window-end`, `runs`, and the period-258/259 patterns below.
//!
//! Every assertion is a property that must hold whatever the input is: no
//! panic, every fed byte comes back, and (levels 1..=9) the call pattern does
//! not change the bytes.

#![allow(clippy::expect_used)]

use oxiarc_deflate::{Deflater, Strategy, deflate, inflate, zlib_compress, zlib_decompress};

/// Deterministic xorshift64* PRNG.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

/// Every size that sits on a boundary the encoder cares about.
///
/// `MIN_MATCH` 3, `MAX_MATCH` 258, `MIN_LOOKAHEAD` 262, `SYM_END` 16 383,
/// `W_SIZE` 32 768, `MAX_STORED` 65 535, `2 * W_SIZE` 65 536, and
/// `W_SIZE + MAX_DIST` 65 274 (the window-slide trigger).
fn boundary_sizes() -> Vec<usize> {
    vec![
        0, 1, 2, 3, 4, 257, 258, 259, 261, 262, 263, 16_382, 16_383, 16_384, 32_766, 32_767,
        32_768, 32_769, 65_273, 65_274, 65_275, 65_534, 65_535, 65_536, 65_537, 98_304,
    ]
}

/// Content shapes chosen to reach the pointer arithmetic, not to compress well.
fn shapes(n: usize) -> Vec<(String, Vec<u8>)> {
    let mut rng = Rng::new(0x0bad_f00d_dead_beef);
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();

    out.push((format!("zeros-{n}"), vec![0u8; n]));
    out.push((format!("ones-{n}"), vec![0xFFu8; n]));
    out.push((
        format!("counting-{n}"),
        (0..n).map(|i| (i & 0xff) as u8).collect(),
    ));
    out.push((format!("random-{n}"), rng.bytes(n)));

    // Zeros at the *end* of the data: the match scan reads past the lookahead
    // into the zeroed high-water region, so a zero tail makes the comparison
    // run its full 258 bytes right at the window edge.
    let mut zero_tail = rng.bytes(n / 2);
    zero_tail.resize(n, 0);
    out.push((format!("random-then-zeros-{n}"), zero_tail));

    let mut zero_head = vec![0u8; n / 2];
    zero_head.extend_from_slice(&rng.bytes(n - n / 2));
    out.push((format!("zeros-then-random-{n}"), zero_head));

    // Periods around MAX_MATCH, where the match length saturates.
    for period in [1usize, 2, 3, 257, 258, 259] {
        let unit = rng.bytes(period);
        let mut v = Vec::with_capacity(n + period);
        while v.len() < n {
            v.extend_from_slice(&unit);
        }
        v.truncate(n);
        out.push((format!("period{period}-{n}"), v));
    }

    // A single far-away 3-byte repeat: the TOO_FAR rule's own shape.
    if n > 9000 {
        let mut v = Vec::with_capacity(n);
        v.extend_from_slice(b"QRS");
        v.extend_from_slice(&rng.bytes(n - 6));
        v.extend_from_slice(b"QRS");
        out.push((format!("far3-{n}"), v));
    }

    out
}

fn feed(level: u8, optimal: bool, strategy: Strategy, data: &[u8], chunk: usize) -> Vec<u8> {
    let mut d = if optimal {
        Deflater::with_optimal_parsing(level)
    } else {
        Deflater::new(level)
    }
    .with_strategy(strategy);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let end = (i + chunk).min(data.len());
        d.deflate(&data[i..end], &mut out, false).expect("deflate");
        i = end;
    }
    d.deflate(&[], &mut out, true).expect("finish");
    out
}

#[test]
fn every_boundary_size_round_trips_at_every_level() {
    for n in boundary_sizes() {
        for (name, data) in shapes(n) {
            for level in 0u8..=9 {
                let one = deflate(&data, level).expect("deflate");
                assert_eq!(
                    inflate(&one).expect("inflate"),
                    data,
                    "{name} level {level}: one-shot round trip"
                );
                let z = zlib_compress(&data, level).expect("zlib_compress");
                assert_eq!(
                    zlib_decompress(&z).expect("zlib_decompress"),
                    data,
                    "{name} level {level}: zlib round trip"
                );
            }
        }
    }
}

#[test]
fn every_call_pattern_round_trips_and_levels_1_to_9_are_call_size_invariant() {
    // Chunk sizes that land exactly on the encoder's internal boundaries.
    let chunks = [1usize, 3, 258, 262, 16_383, 32_768, 65_535, 65_536];
    for n in boundary_sizes() {
        if n > 70_000 {
            continue;
        }
        for (name, data) in shapes(n) {
            for level in 0u8..=9 {
                let one = deflate(&data, level).expect("deflate");
                for chunk in chunks {
                    let split = feed(level, false, Strategy::Default, &data, chunk);
                    assert_eq!(
                        inflate(&split).expect("inflate"),
                        data,
                        "{name} level {level} chunk {chunk}: round trip"
                    );
                    if level > 0 {
                        assert!(
                            split == one,
                            "{name} level {level} chunk {chunk}: {} bytes vs one-shot {}",
                            split.len(),
                            one.len()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn every_strategy_round_trips_at_every_boundary_size() {
    let strategies = [
        Strategy::Default,
        Strategy::Filtered,
        Strategy::HuffmanOnly,
        Strategy::Rle,
        Strategy::Fixed,
    ];
    for n in boundary_sizes() {
        if n > 70_000 {
            continue;
        }
        for (name, data) in shapes(n) {
            for strategy in strategies {
                for level in [1u8, 4, 6, 9] {
                    for chunk in [1usize, 262, 65_536] {
                        let out = feed(level, false, strategy, &data, chunk);
                        assert_eq!(
                            inflate(&out).expect("inflate"),
                            data,
                            "{name} {strategy:?} level {level} chunk {chunk}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_optimal_parser_round_trips_at_every_boundary_size() {
    for n in boundary_sizes() {
        if n > 70_000 {
            continue;
        }
        for (name, data) in shapes(n) {
            for level in [6u8, 9] {
                for chunk in [1usize, 258, 65_536] {
                    let out = feed(level, true, Strategy::Default, &data, chunk);
                    assert_eq!(
                        inflate(&out).expect("inflate"),
                        data,
                        "{name} optimal level {level} chunk {chunk}"
                    );
                }
            }
        }
    }
}

#[test]
fn interleaved_flush_modes_never_lose_a_byte() {
    for n in [0usize, 1, 263, 16_384, 32_768, 65_536, 98_304] {
        for (name, data) in shapes(n) {
            for (level, optimal) in [
                (0u8, false),
                (1, false),
                (2, false),
                (3, false),
                (4, false),
                (5, false),
                (6, false),
                (7, false),
                (8, false),
                (9, false),
                (6, true),
                (9, true),
            ] {
                let mut d = if optimal {
                    Deflater::with_optimal_parsing(level)
                } else {
                    Deflater::new(level)
                };
                let mut out = Vec::new();
                let step = (data.len() / 5).max(1);
                let mut i = 0usize;
                let mut phase = 0usize;
                while i < data.len() {
                    let end = (i + step).min(data.len());
                    let piece = &data[i..end];
                    match phase % 4 {
                        0 => d.deflate(piece, &mut out, false).expect("none"),
                        1 => d.deflate_sync(piece, &mut out).expect("sync"),
                        2 => d.deflate_partial(piece, &mut out).expect("partial"),
                        _ => d.deflate_full(piece, &mut out).expect("full"),
                    }
                    phase += 1;
                    i = end;
                }
                d.deflate(&[], &mut out, true).expect("finish");
                assert_eq!(
                    inflate(&out).expect("inflate"),
                    data,
                    "{name} level {level} optimal={optimal}: interleaved flushes"
                );
            }
        }
    }
}

#[test]
fn a_flush_on_an_empty_call_is_idempotent_and_still_terminates() {
    for level in 0u8..=9 {
        let data = vec![b'z'; 40_000];
        let mut d = Deflater::new(level);
        let mut out = Vec::new();
        // Repeated no-input flushes must reach a fixed point rather than
        // spinning or emitting an unbounded run of empty blocks.
        for _ in 0..8 {
            d.deflate(&[], &mut out, false).expect("empty none");
        }
        d.deflate(&data, &mut out, false).expect("data");
        for _ in 0..4 {
            d.deflate_sync(&[], &mut out).expect("empty sync");
        }
        let before = out.len();
        for _ in 0..4 {
            d.deflate(&[], &mut out, false).expect("empty none");
        }
        assert_eq!(
            out.len(),
            before,
            "level {level}: an empty Flush::None call emitted bytes"
        );
        d.deflate(&[], &mut out, true).expect("finish");
        assert_eq!(inflate(&out).expect("inflate"), data, "level {level}");
    }
}

#[test]
fn dictionaries_at_every_boundary_round_trip() {
    let mut rng = Rng::new(0xfeed_face_0102_0304);
    let dicts: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"a".to_vec(),
        b"abc".to_vec(),
        vec![0u8; 32_767],
        vec![0u8; 32_768],
        rng.bytes(32_769),
        rng.bytes(40_000),
    ];
    for dict in &dicts {
        for n in [0usize, 3, 262, 32_768, 65_536] {
            for (name, data) in shapes(n) {
                for level in [1u8, 6, 9] {
                    let mut d = Deflater::with_dictionary(level, dict);
                    let out = d.compress_to_vec(&data).expect("compress_to_vec");
                    let mut stream = oxiarc_deflate::InflateStream::new();
                    if !dict.is_empty() {
                        stream.set_dictionary(dict);
                    }
                    let round = stream.inflate_to_vec(&out).expect("inflate_to_vec");
                    assert_eq!(round, data, "{name} dict {} level {level}", dict.len());
                }
            }
        }
    }
}

#[test]
fn reset_returns_the_encoder_to_a_pristine_state() {
    let mut rng = Rng::new(0x1357_9bdf_2468_ace0);
    let first = rng.bytes(70_000);
    let second = rng.bytes(3_000);
    for level in 0u8..=9 {
        let expected = deflate(&second, level).expect("deflate");

        let mut d = Deflater::new(level);
        let mut junk = Vec::new();
        d.deflate(&first, &mut junk, false).expect("first");
        d.deflate_sync(&[], &mut junk).expect("sync");
        d.reset();

        let mut out = Vec::new();
        d.deflate(&second, &mut out, true).expect("second");
        assert!(
            out == expected,
            "level {level}: reset left state behind ({} vs {} bytes)",
            out.len(),
            expected.len()
        );
    }
}
