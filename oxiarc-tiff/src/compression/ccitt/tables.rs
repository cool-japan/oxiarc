//! The ITU-T T.4 run-length code tables and the T.4/T.6 two-dimensional mode
//! codes, plus the lookup tables that decode them in one step.
//!
//! Every entry is `(bit length, code word, run length)`. The code word is
//! right-aligned in the `u16`; the bit length says how many of its bits are on
//! the wire, most significant first. The tables are the ones printed in T.4
//! Tables 1, 2 and 3 — terminating codes for runs 0..=63, make-up codes for
//! multiples of 64 up to 1728 per colour, and the colour-independent extended
//! make-up codes 1792..=2560.
//!
//! Decoding walks a 13-bit lookup table (the longest run code is 13 bits) so a
//! run costs one array index rather than a bit-by-bit tree walk. The tables are
//! built once, on first use.

use std::sync::OnceLock;

/// White terminating run-length codes (runs 0..=63).
pub(super) const WHITE_TERMINATING: [(u8, u16, u16); 64] = [
    (8, 0x35, 0),
    (6, 0x07, 1),
    (4, 0x07, 2),
    (4, 0x08, 3),
    (4, 0x0B, 4),
    (4, 0x0C, 5),
    (4, 0x0E, 6),
    (4, 0x0F, 7),
    (5, 0x13, 8),
    (5, 0x14, 9),
    (5, 0x07, 10),
    (5, 0x08, 11),
    (6, 0x08, 12),
    (6, 0x03, 13),
    (6, 0x34, 14),
    (6, 0x35, 15),
    (6, 0x2A, 16),
    (6, 0x2B, 17),
    (7, 0x27, 18),
    (7, 0x0C, 19),
    (7, 0x08, 20),
    (7, 0x17, 21),
    (7, 0x03, 22),
    (7, 0x04, 23),
    (7, 0x28, 24),
    (7, 0x2B, 25),
    (7, 0x13, 26),
    (7, 0x24, 27),
    (7, 0x18, 28),
    (8, 0x02, 29),
    (8, 0x03, 30),
    (8, 0x1A, 31),
    (8, 0x1B, 32),
    (8, 0x12, 33),
    (8, 0x13, 34),
    (8, 0x14, 35),
    (8, 0x15, 36),
    (8, 0x16, 37),
    (8, 0x17, 38),
    (8, 0x28, 39),
    (8, 0x29, 40),
    (8, 0x2A, 41),
    (8, 0x2B, 42),
    (8, 0x2C, 43),
    (8, 0x2D, 44),
    (8, 0x04, 45),
    (8, 0x05, 46),
    (8, 0x0A, 47),
    (8, 0x0B, 48),
    (8, 0x52, 49),
    (8, 0x53, 50),
    (8, 0x54, 51),
    (8, 0x55, 52),
    (8, 0x24, 53),
    (8, 0x25, 54),
    (8, 0x58, 55),
    (8, 0x59, 56),
    (8, 0x5A, 57),
    (8, 0x5B, 58),
    (8, 0x4A, 59),
    (8, 0x4B, 60),
    (8, 0x32, 61),
    (8, 0x33, 62),
    (8, 0x34, 63),
];

/// White make-up codes (runs 64..=1728, multiples of 64).
pub(super) const WHITE_MAKEUP: [(u8, u16, u16); 27] = [
    (5, 0x1B, 64),
    (5, 0x12, 128),
    (6, 0x17, 192),
    (7, 0x37, 256),
    (8, 0x36, 320),
    (8, 0x37, 384),
    (8, 0x64, 448),
    (8, 0x65, 512),
    (8, 0x68, 576),
    (8, 0x67, 640),
    (9, 0xCC, 704),
    (9, 0xCD, 768),
    (9, 0xD2, 832),
    (9, 0xD3, 896),
    (9, 0xD4, 960),
    (9, 0xD5, 1024),
    (9, 0xD6, 1088),
    (9, 0xD7, 1152),
    (9, 0xD8, 1216),
    (9, 0xD9, 1280),
    (9, 0xDA, 1344),
    (9, 0xDB, 1408),
    (9, 0x98, 1472),
    (9, 0x99, 1536),
    (9, 0x9A, 1600),
    (6, 0x18, 1664),
    (9, 0x9B, 1728),
];

/// Black terminating run-length codes (runs 0..=63).
pub(super) const BLACK_TERMINATING: [(u8, u16, u16); 64] = [
    (10, 0x37, 0),
    (3, 0x02, 1),
    (2, 0x03, 2),
    (2, 0x02, 3),
    (3, 0x03, 4),
    (4, 0x03, 5),
    (4, 0x02, 6),
    (5, 0x03, 7),
    (6, 0x05, 8),
    (6, 0x04, 9),
    (7, 0x04, 10),
    (7, 0x05, 11),
    (7, 0x07, 12),
    (8, 0x04, 13),
    (8, 0x07, 14),
    (9, 0x18, 15),
    (10, 0x17, 16),
    (10, 0x18, 17),
    (10, 0x08, 18),
    (11, 0x67, 19),
    (11, 0x68, 20),
    (11, 0x6C, 21),
    (11, 0x37, 22),
    (11, 0x28, 23),
    (11, 0x17, 24),
    (11, 0x18, 25),
    (12, 0xCA, 26),
    (12, 0xCB, 27),
    (12, 0xCC, 28),
    (12, 0xCD, 29),
    (12, 0x68, 30),
    (12, 0x69, 31),
    (12, 0x6A, 32),
    (12, 0x6B, 33),
    (12, 0xD2, 34),
    (12, 0xD3, 35),
    (12, 0xD4, 36),
    (12, 0xD5, 37),
    (12, 0xD6, 38),
    (12, 0xD7, 39),
    (12, 0x6C, 40),
    (12, 0x6D, 41),
    (12, 0xDA, 42),
    (12, 0xDB, 43),
    (12, 0x54, 44),
    (12, 0x55, 45),
    (12, 0x56, 46),
    (12, 0x57, 47),
    (12, 0x64, 48),
    (12, 0x65, 49),
    (12, 0x52, 50),
    (12, 0x53, 51),
    (12, 0x24, 52),
    (12, 0x37, 53),
    (12, 0x38, 54),
    (12, 0x27, 55),
    (12, 0x28, 56),
    (12, 0x58, 57),
    (12, 0x59, 58),
    (12, 0x2B, 59),
    (12, 0x2C, 60),
    (12, 0x5A, 61),
    (12, 0x66, 62),
    (12, 0x67, 63),
];

/// Black make-up codes (runs 64..=1728, multiples of 64).
pub(super) const BLACK_MAKEUP: [(u8, u16, u16); 27] = [
    (10, 0x0F, 64),
    (12, 0xC8, 128),
    (12, 0xC9, 192),
    (12, 0x5B, 256),
    (12, 0x33, 320),
    (12, 0x34, 384),
    (12, 0x35, 448),
    (13, 0x6C, 512),
    (13, 0x6D, 576),
    (13, 0x4A, 640),
    (13, 0x4B, 704),
    (13, 0x4C, 768),
    (13, 0x4D, 832),
    (13, 0x72, 896),
    (13, 0x73, 960),
    (13, 0x74, 1024),
    (13, 0x75, 1088),
    (13, 0x76, 1152),
    (13, 0x77, 1216),
    (13, 0x52, 1280),
    (13, 0x53, 1344),
    (13, 0x54, 1408),
    (13, 0x55, 1472),
    (13, 0x5A, 1536),
    (13, 0x5B, 1600),
    (13, 0x64, 1664),
    (13, 0x65, 1728),
];

/// Extended (colour-independent) make-up codes, runs 1792..=2560.
pub(super) const EXTENDED_MAKEUP: [(u8, u16, u16); 13] = [
    (11, 0x08, 1792),
    (11, 0x0C, 1856),
    (11, 0x0D, 1920),
    (12, 0x12, 1984),
    (12, 0x13, 2048),
    (12, 0x14, 2112),
    (12, 0x15, 2176),
    (12, 0x16, 2240),
    (12, 0x17, 2304),
    (12, 0x1C, 2368),
    (12, 0x1D, 2432),
    (12, 0x1E, 2496),
    (12, 0x1F, 2560),
];

/// The end-of-line code: eleven zero bits and a one.
pub(super) const EOL_CODE: u16 = 0x0001;
/// Bit length of [`EOL_CODE`].
pub(super) const EOL_BITS: u8 = 12;
/// Longest run code word, and therefore the lookup window.
pub(super) const LOOKUP_BITS: u8 = 13;
/// Largest run a single make-up code can carry.
pub(super) const MAX_MAKEUP_RUN: u16 = 2560;

/// What a run-code lookup found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunCode {
    /// A terminating code: the run ends after adding `run`.
    Terminating {
        /// Bits to consume.
        bits: u8,
        /// Pixels to add.
        run: u16,
    },
    /// A make-up code: add `run`, then read another code.
    MakeUp {
        /// Bits to consume.
        bits: u8,
        /// Pixels to add.
        run: u16,
    },
    /// The 12-bit end-of-line code.
    Eol,
    /// These bits are not a code word.
    Invalid,
}

/// One lookup table per colour, indexed by the next [`LOOKUP_BITS`] bits.
struct RunLookup {
    white: Vec<RunCode>,
    black: Vec<RunCode>,
}

/// The lazily-built lookup tables.
fn lookup() -> &'static RunLookup {
    static LOOKUP: OnceLock<RunLookup> = OnceLock::new();
    LOOKUP.get_or_init(|| RunLookup {
        white: build_table(&WHITE_TERMINATING, &WHITE_MAKEUP),
        black: build_table(&BLACK_TERMINATING, &BLACK_MAKEUP),
    })
}

/// Expands one colour's code tables into a flat lookup table.
fn build_table(terminating: &[(u8, u16, u16)], makeup: &[(u8, u16, u16)]) -> Vec<RunCode> {
    let size = 1usize << LOOKUP_BITS;
    let mut table = vec![RunCode::Invalid; size];
    let mut place = |bits: u8, code: u16, entry: RunCode| {
        let shift = LOOKUP_BITS - bits;
        let base = (usize::from(code)) << shift;
        for slot in base..base + (1usize << shift) {
            if let Some(cell) = table.get_mut(slot) {
                *cell = entry;
            }
        }
    };
    for (bits, code, run) in terminating {
        place(
            *bits,
            *code,
            RunCode::Terminating {
                bits: *bits,
                run: *run,
            },
        );
    }
    for (bits, code, run) in makeup.iter().chain(EXTENDED_MAKEUP.iter()) {
        place(
            *bits,
            *code,
            RunCode::MakeUp {
                bits: *bits,
                run: *run,
            },
        );
    }
    place(EOL_BITS, EOL_CODE, RunCode::Eol);
    table
}

/// Decodes the next run code from a 13-bit window.
pub(super) fn run_code(window: u16, white: bool) -> RunCode {
    let table = lookup();
    let side = if white { &table.white } else { &table.black };
    side.get(usize::from(window) & ((1 << LOOKUP_BITS) - 1))
        .copied()
        .unwrap_or(RunCode::Invalid)
}

/// The two-dimensional mode codes shared by G3-2D and G4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// `0001`: the run passes over `b2`.
    Pass,
    /// `001`: two explicit run lengths follow.
    Horizontal,
    /// `1`, `011`, `000011`, `0000011`, `010`, `000010`, `0000010`: the change
    /// is `delta` pixels from `b1`.
    Vertical(i8),
}

/// Decodes a mode code from the next 7 bits, returning it and its bit length.
///
/// The caller has already ruled out an EOL (which needs a 12-bit window).
pub(super) fn mode_code(window: u8) -> Option<(Mode, u8)> {
    // `window` holds seven bits, most significant first.
    if window & 0b100_0000 != 0 {
        return Some((Mode::Vertical(0), 1));
    }
    match window >> 4 {
        0b011 => return Some((Mode::Vertical(1), 3)),
        0b010 => return Some((Mode::Vertical(-1), 3)),
        0b001 => return Some((Mode::Horizontal, 3)),
        _ => {}
    }
    if window >> 3 == 0b0001 {
        return Some((Mode::Pass, 4));
    }
    match window >> 1 {
        0b000011 => return Some((Mode::Vertical(2), 6)),
        0b000010 => return Some((Mode::Vertical(-2), 6)),
        _ => {}
    }
    match window {
        0b0000011 => Some((Mode::Vertical(3), 7)),
        0b0000010 => Some((Mode::Vertical(-3), 7)),
        _ => None,
    }
}

/// The code word for a mode, as `(code, bits)`.
pub(super) const fn mode_bits(mode: Mode) -> (u16, u8) {
    match mode {
        Mode::Vertical(0) => (0b1, 1),
        Mode::Vertical(1) => (0b011, 3),
        Mode::Vertical(-1) => (0b010, 3),
        Mode::Vertical(2) => (0b000011, 6),
        Mode::Vertical(-2) => (0b000010, 6),
        Mode::Vertical(3) => (0b0000011, 7),
        Mode::Vertical(-3) => (0b0000010, 7),
        Mode::Horizontal => (0b001, 3),
        Mode::Pass => (0b0001, 4),
        // Unreachable for a well-formed `Vertical`, whose offset is -3..=3.
        Mode::Vertical(_) => (EOL_CODE, EOL_BITS),
    }
}

/// The make-up and terminating codes that spell `run` pixels of one colour.
///
/// Runs longer than [`MAX_MAKEUP_RUN`] need several make-up codes, which is
/// why this is an iterator-shaped helper rather than a single lookup.
pub(super) fn encode_run(run: u32, white: bool, mut emit: impl FnMut(u16, u8)) {
    let mut left = run;
    while left >= u32::from(MAX_MAKEUP_RUN) + 64 {
        let (bits, code, _) = EXTENDED_MAKEUP[EXTENDED_MAKEUP.len() - 1];
        emit(code, bits);
        left -= u32::from(MAX_MAKEUP_RUN);
    }
    if left >= 64 {
        let makeup_run = (left / 64) * 64;
        let entry = if makeup_run <= 1728 {
            let table = if white { &WHITE_MAKEUP } else { &BLACK_MAKEUP };
            table.get((makeup_run / 64 - 1) as usize).copied()
        } else {
            EXTENDED_MAKEUP
                .get(((makeup_run - 1792) / 64) as usize)
                .copied()
        };
        if let Some((bits, code, run)) = entry {
            emit(code, bits);
            left -= u32::from(run);
        }
    }
    let table = if white {
        &WHITE_TERMINATING
    } else {
        &BLACK_TERMINATING
    };
    if let Some((bits, code, _)) = table.get(left as usize) {
        emit(*code, *bits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code in one colour's set must be prefix-free, or the lookup
    /// table would have one code shadowing another.
    fn assert_prefix_free(terminating: &[(u8, u16, u16)], makeup: &[(u8, u16, u16)]) {
        let mut codes: Vec<(u8, u16)> = Vec::new();
        for (bits, code, _) in terminating.iter().chain(makeup.iter()) {
            codes.push((*bits, *code));
        }
        for (bits, code, _) in EXTENDED_MAKEUP.iter() {
            codes.push((*bits, *code));
        }
        codes.push((EOL_BITS, EOL_CODE));
        for (i, (bits_a, code_a)) in codes.iter().enumerate() {
            for (j, (bits_b, code_b)) in codes.iter().enumerate() {
                if i == j || bits_a > bits_b {
                    continue;
                }
                let shift = bits_b - bits_a;
                assert_ne!(
                    code_b >> shift,
                    *code_a,
                    "{code_a:b}/{bits_a} is a prefix of {code_b:b}/{bits_b}"
                );
            }
        }
    }

    #[test]
    fn both_colour_tables_are_prefix_free() {
        assert_prefix_free(&WHITE_TERMINATING, &WHITE_MAKEUP);
        assert_prefix_free(&BLACK_TERMINATING, &BLACK_MAKEUP);
    }

    #[test]
    fn the_tables_match_the_spec_spot_checks() {
        // T.4 Table 1: white 0 is 00110101, white 1 is 000111, white 63 is
        // 00110100; black 0 is 0000110111, black 1 is 010, black 2 is 11.
        assert_eq!(WHITE_TERMINATING[0], (8, 0b0011_0101, 0));
        assert_eq!(WHITE_TERMINATING[1], (6, 0b000111, 1));
        assert_eq!(WHITE_TERMINATING[63], (8, 0b0011_0100, 63));
        assert_eq!(BLACK_TERMINATING[0], (10, 0b00_0011_0111, 0));
        assert_eq!(BLACK_TERMINATING[1], (3, 0b010, 1));
        assert_eq!(BLACK_TERMINATING[2], (2, 0b11, 2));
        // T.4 Table 2: white 64 is 11011, black 64 is 0000001111.
        assert_eq!(WHITE_MAKEUP[0], (5, 0b11011, 64));
        assert_eq!(BLACK_MAKEUP[0], (10, 0b00_0000_1111, 64));
        // T.4 Table 3: the extended codes are colour-independent.
        assert_eq!(EXTENDED_MAKEUP[0], (11, 0b000_0000_1000, 1792));
        assert_eq!(EXTENDED_MAKEUP[12], (12, 0b0000_0001_1111, 2560));
        // Runs are consecutive multiples of 64 with the 1664 anomaly.
        for (index, (_, _, run)) in WHITE_MAKEUP.iter().enumerate() {
            assert_eq!(*run as usize, (index + 1) * 64);
        }
        for (index, (_, _, run)) in BLACK_MAKEUP.iter().enumerate() {
            assert_eq!(*run as usize, (index + 1) * 64);
        }
    }

    #[test]
    fn the_lookup_table_finds_every_code() {
        for (bits, code, run) in WHITE_TERMINATING {
            let window = code << (LOOKUP_BITS - bits);
            assert_eq!(run_code(window, true), RunCode::Terminating { bits, run });
        }
        for (bits, code, run) in BLACK_MAKEUP {
            let window = code << (LOOKUP_BITS - bits);
            assert_eq!(run_code(window, false), RunCode::MakeUp { bits, run });
        }
        for (bits, code, run) in EXTENDED_MAKEUP {
            let window = code << (LOOKUP_BITS - bits);
            assert_eq!(run_code(window, true), RunCode::MakeUp { bits, run });
            assert_eq!(run_code(window, false), RunCode::MakeUp { bits, run });
        }
        let eol = EOL_CODE << (LOOKUP_BITS - EOL_BITS);
        assert_eq!(run_code(eol, true), RunCode::Eol);
        assert_eq!(run_code(eol, false), RunCode::Eol);
    }

    #[test]
    fn unassigned_windows_are_invalid() {
        // The code space is not full: some windows spell nothing at all, and
        // those must be reported rather than mapped to a nearby run.
        let unassigned = (0u16..(1 << LOOKUP_BITS))
            .filter(|w| run_code(*w, true) == RunCode::Invalid)
            .count();
        assert!(unassigned > 0, "the white table cannot cover 13 bits");
        let unassigned_black = (0u16..(1 << LOOKUP_BITS))
            .filter(|w| run_code(*w, false) == RunCode::Invalid)
            .count();
        assert!(unassigned_black > 0);
        // Every 13-bit window that *is* assigned decodes to a code no longer
        // than the window.
        for window in 0u16..(1 << LOOKUP_BITS) {
            match run_code(window, true) {
                RunCode::Terminating { bits, .. } | RunCode::MakeUp { bits, .. } => {
                    assert!(bits <= LOOKUP_BITS);
                }
                RunCode::Eol | RunCode::Invalid => {}
            }
        }
    }

    #[test]
    fn mode_codes_round_trip() {
        let modes = [
            Mode::Vertical(0),
            Mode::Vertical(1),
            Mode::Vertical(-1),
            Mode::Vertical(2),
            Mode::Vertical(-2),
            Mode::Vertical(3),
            Mode::Vertical(-3),
            Mode::Horizontal,
            Mode::Pass,
        ];
        for mode in modes {
            let (code, bits) = mode_bits(mode);
            let window = (code << (7 - bits)) as u8;
            assert_eq!(mode_code(window), Some((mode, bits)), "{mode:?}");
        }
        // Seven zero bits are the extension prefix, which `mode_code` leaves
        // to the caller (it needs three more bits).
        assert_eq!(mode_code(0), None);
    }

    #[test]
    fn runs_are_spelled_as_make_up_plus_terminating() {
        let mut emitted: Vec<(u16, u8)> = Vec::new();
        encode_run(0, true, |code, bits| emitted.push((code, bits)));
        assert_eq!(emitted, vec![(0x35, 8)]);

        emitted.clear();
        encode_run(63, true, |code, bits| emitted.push((code, bits)));
        assert_eq!(emitted, vec![(0x34, 8)]);

        emitted.clear();
        encode_run(64, true, |code, bits| emitted.push((code, bits)));
        assert_eq!(emitted, vec![(0x1B, 5), (0x35, 8)]);

        emitted.clear();
        encode_run(1728 + 5, true, |code, bits| emitted.push((code, bits)));
        assert_eq!(emitted, vec![(0x9B, 9), (0x0C, 4)]);

        emitted.clear();
        encode_run(2560 + 64, true, |code, bits| emitted.push((code, bits)));
        assert_eq!(emitted.len(), 3, "a run past 2560 needs two make-ups");
    }
}
