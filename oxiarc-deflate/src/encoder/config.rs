//! Window geometry and the per-level match-finder configuration.
//!
//! The numbers below are zlib's `deflateInit2(level, Z_DEFLATED, 15, 8,
//! Z_DEFAULT_STRATEGY)` defaults — the exact configuration CPython's
//! `zlib.compress` uses — so the encoder can be compared against it directly.

/// Window size in bytes (`windowBits == 15`).
pub(crate) const W_SIZE: usize = 32768;
/// Mask for wrapping a position into the `prev` chain array.
pub(crate) const W_MASK: usize = W_SIZE - 1;
/// Hash table size (`memLevel == 8` → `hash_bits == 15`).
pub(crate) const HASH_SIZE: usize = 32768;
/// Mask applied after every hash update.
pub(crate) const HASH_MASK: usize = HASH_SIZE - 1;
/// Per-byte shift of the rolling hash (`(hash_bits + MIN_MATCH - 1) / MIN_MATCH`).
pub(crate) const HASH_SHIFT: usize = 5;
/// Lookahead the match finder needs before it can run without more input.
pub(crate) const MIN_LOOKAHEAD: usize = 262;
/// Largest match distance the encoder will emit.
pub(crate) const MAX_DIST: usize = W_SIZE - MIN_LOOKAHEAD;
/// Symbol-buffer capacity (`1 << (memLevel + 6)`).
pub(crate) const LIT_BUFSIZE: usize = 16384;
/// Block flush threshold: the number of tallied symbols that fills the buffer.
pub(crate) const SYM_END: usize = LIT_BUFSIZE - 1;
/// zlib's `pending_buf_size`, used only to size stored blocks.
pub(crate) const PENDING_BUF_SIZE: usize = LIT_BUFSIZE * 4;
/// Length-3 matches farther than this are rejected (zlib `TOO_FAR`).
pub(crate) const TOO_FAR: usize = 4096;

/// Which inner loop a level uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// Level 0: stored blocks only.
    Stored,
    /// Levels 1-3: greedy matching, no lazy evaluation.
    Fast,
    /// Levels 4-9: lazy matching.
    Slow,
}

/// One row of zlib's `configuration_table`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelConfig {
    /// Once a match this long is found, the remaining chain budget is quartered.
    pub good_length: u16,
    /// Above this match length no lazy search is attempted; in `deflate_fast`
    /// it is instead the largest match whose interior positions get hashed.
    pub max_lazy: u16,
    /// Stop searching as soon as a match this long is found.
    pub nice_length: u16,
    /// Maximum number of hash-chain steps per position.
    pub max_chain: u16,
}

/// zlib's `configuration_table`, indexed by level 0..=9.
///
/// Laid out one row per line, in `deflate.c`'s column order, so it can be
/// diffed against the C table by eye; `rustfmt` would otherwise explode each
/// row over five lines.
#[rustfmt::skip]
pub(crate) const CONFIG: [LevelConfig; 10] = [
    LevelConfig { good_length:  0, max_lazy:   0, nice_length:   0, max_chain:    0 },
    LevelConfig { good_length:  4, max_lazy:   4, nice_length:   8, max_chain:    4 },
    LevelConfig { good_length:  4, max_lazy:   5, nice_length:  16, max_chain:    8 },
    LevelConfig { good_length:  4, max_lazy:   6, nice_length:  32, max_chain:   32 },
    LevelConfig { good_length:  4, max_lazy:   4, nice_length:  16, max_chain:   16 },
    LevelConfig { good_length:  8, max_lazy:  16, nice_length:  32, max_chain:   32 },
    LevelConfig { good_length:  8, max_lazy:  16, nice_length: 128, max_chain:  128 },
    LevelConfig { good_length:  8, max_lazy:  32, nice_length: 128, max_chain:  256 },
    LevelConfig { good_length: 32, max_lazy: 128, nice_length: 258, max_chain: 1024 },
    LevelConfig { good_length: 32, max_lazy: 258, nice_length: 258, max_chain: 4096 },
];

/// The configuration for `level` (clamped to 0..=9).
pub(crate) fn config_for(level: u8) -> LevelConfig {
    CONFIG[usize::from(level.min(9))]
}

/// Which inner loop `level` uses.
pub(crate) fn method_for(level: u8) -> Method {
    match level.min(9) {
        0 => Method::Stored,
        1..=3 => Method::Fast,
        _ => Method::Slow,
    }
}
