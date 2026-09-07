//! LZ77 token production for DEFLATE.
//!
//! [`Lz77Encoder`] exposes the match finder that [`crate::Deflater`] drives, as
//! a standalone producer of [`Lz77Token`]s. It uses the same machinery: a
//! 32 KiB sliding window, zlib's 15-bit rolling hash with `head`/`prev`
//! chains, the per-level [`Lz77Params`] row, greedy matching at levels 1-3 and
//! lazy matching with the "too far" rule at levels 4-9.
//!
//! Use it when you want the token stream itself (analysis, a different entropy
//! coder, tooling). To produce a DEFLATE stream use [`crate::Deflater`], which
//! tallies directly into its block buffer and never materialises tokens.

use crate::encoder::config::{MAX_DIST, MIN_LOOKAHEAD, Method, TOO_FAR, config_for, method_for};
use crate::encoder::window::Window;

/// Maximum window size for DEFLATE (32KB).
pub const WINDOW_SIZE: usize = crate::encoder::config::W_SIZE;

/// Minimum match length.
pub const MIN_MATCH: usize = crate::encoder::tables::MIN_MATCH;

/// Maximum match length.
pub const MAX_MATCH: usize = crate::encoder::tables::MAX_MATCH;

/// LZ77 match-finding parameters — one row of zlib's `configuration_table`.
///
/// [`Lz77Params::for_level`] returns the row zlib uses for that level, so the
/// defaults reproduce zlib's match decisions exactly. Overriding them trades
/// ratio for speed (or the other way round) in the same directions zlib does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lz77Params {
    /// Once the deferred match is at least this long, the remaining hash-chain
    /// budget is cut to 25 % (zlib's `good_match`).
    pub good_length: u16,
    /// Above this deferred-match length no further search is attempted. In the
    /// greedy levels (1-3) it is instead the longest match whose interior
    /// positions still get inserted into the hash chains.
    pub max_lazy: u16,
    /// Stop searching as soon as a match of this length is found.
    ///
    /// Range: [`MIN_MATCH`, `MAX_MATCH`].
    pub nice_length: u16,
    /// Maximum hash-chain steps per encoded position.
    pub max_chain: u16,
}

impl Lz77Params {
    /// The parameters zlib uses for `level` (0–9).
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::lz77::Lz77Params;
    ///
    /// // zlib's level-6 row: good 8, lazy 16, nice 128, chain 128.
    /// let p = Lz77Params::for_level(6);
    /// assert_eq!((p.good_length, p.max_lazy, p.nice_length, p.max_chain), (8, 16, 128, 128));
    /// ```
    pub fn for_level(level: u32) -> Self {
        let cfg = config_for(level.min(9) as u8);
        Self {
            good_length: cfg.good_length,
            max_lazy: cfg.max_lazy,
            nice_length: cfg.nice_length,
            max_chain: cfg.max_chain,
        }
    }
}

impl Default for Lz77Params {
    fn default() -> Self {
        Self::for_level(6)
    }
}

impl From<Lz77Params> for crate::encoder::LevelConfig {
    fn from(p: Lz77Params) -> Self {
        Self {
            good_length: p.good_length,
            max_lazy: p.max_lazy,
            nice_length: p.nice_length,
            max_chain: p.max_chain,
        }
    }
}

/// Named presets for LZ77 match-finding heuristics.
///
/// Each preset is one of zlib's level rows, so a preset always corresponds to
/// a search effort a real encoder ships with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lz77Preset {
    /// Level 1's row: fastest search, no lazy matching.
    Fast,
    /// Level 6's row: the default balance.
    Default,
    /// Level 8's row: much deeper chains.
    Best,
    /// Level 9's row: maximum search effort.
    Ultra,
}

impl Lz77Preset {
    /// Convert the preset to [`Lz77Params`].
    pub fn params(self) -> Lz77Params {
        match self {
            Lz77Preset::Fast => Lz77Params::for_level(1),
            Lz77Preset::Default => Lz77Params::for_level(6),
            Lz77Preset::Best => Lz77Params::for_level(8),
            Lz77Preset::Ultra => Lz77Params::for_level(9),
        }
    }
}

/// A token produced by LZ77 compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lz77Token {
    /// A literal byte.
    Literal(u8),
    /// A back-reference to previously seen data.
    Match {
        /// Number of bytes to copy (3-258).
        length: u16,
        /// Distance back into the window (1-32768).
        distance: u16,
    },
}

/// LZ77 encoder for DEFLATE compression.
#[derive(Debug)]
pub struct Lz77Encoder {
    win: Window,
    level: u8,
    params: Lz77Params,
    method: Method,
    match_length: usize,
    prev_length: usize,
    prev_match: usize,
    match_available: bool,
    /// Matches shorter than this are emitted as literals.
    min_useful_match: usize,
    dictionary_size: usize,
}

impl Lz77Encoder {
    /// Create a new LZ77 encoder with default settings (level 6).
    pub fn new() -> Self {
        Self::with_level(6)
    }

    /// Create a new LZ77 encoder with the specified compression level (0-9).
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::lz77::{Lz77Encoder, Lz77Token};
    ///
    /// let mut e = Lz77Encoder::with_level(6);
    /// let tokens = e.compress(b"abcabcabcabcabc");
    /// assert!(tokens.iter().any(|t| matches!(t, Lz77Token::Match { .. })));
    /// ```
    pub fn with_level(level: u8) -> Self {
        Self::from_window(level, Window::new())
    }

    fn from_window(level: u8, win: Window) -> Self {
        let level = level.min(9);
        Self {
            win,
            level,
            params: Lz77Params::for_level(u32::from(level)),
            method: method_for(level),
            match_length: MIN_MATCH - 1,
            prev_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            min_useful_match: MIN_MATCH,
            dictionary_size: 0,
        }
    }

    /// Override the nice match length (stops searching once a match of this
    /// length is found). Clamped to `[MIN_MATCH, MAX_MATCH]`.
    #[must_use]
    pub fn with_nice_length(mut self, nice: usize) -> Self {
        self.params.nice_length = nice.clamp(MIN_MATCH, MAX_MATCH) as u16;
        self
    }

    /// Set the minimum useful match length. Matches shorter than this are
    /// emitted as literals. Clamped to `[MIN_MATCH, MAX_MATCH]`.
    #[must_use]
    pub fn with_min_match_length(mut self, min_match: usize) -> Self {
        self.min_useful_match = min_match.clamp(MIN_MATCH, MAX_MATCH);
        self
    }

    /// Override the maximum hash-chain walk length (clamped to `u16::MAX`).
    #[must_use]
    pub fn with_max_chain(mut self, max_chain: usize) -> Self {
        self.params.max_chain = max_chain.clamp(1, usize::from(u16::MAX)) as u16;
        self
    }

    /// Set the good-length threshold for chain-budget reduction.
    ///
    /// Once the deferred match is at least `good_length` bytes long the
    /// remaining chain budget is cut to 25 %. Clamped to
    /// `[MIN_MATCH, MAX_MATCH + 1]`; `MAX_MATCH + 1` disables it.
    #[must_use]
    pub fn with_good_length(mut self, good_length: usize) -> Self {
        self.params.good_length = good_length.clamp(MIN_MATCH, MAX_MATCH + 1) as u16;
        self
    }

    /// Apply [`Lz77Params`] wholesale.
    #[must_use]
    pub fn with_lz77_params(mut self, params: &Lz77Params) -> Self {
        self.params = *params;
        self
    }

    /// The parameters currently in effect.
    pub fn params(&self) -> Lz77Params {
        self.params
    }

    /// Reset the encoder state.
    pub fn reset(&mut self) {
        self.win.reset();
        self.match_length = MIN_MATCH - 1;
        self.prev_length = MIN_MATCH - 1;
        self.prev_match = 0;
        self.match_available = false;
        self.dictionary_size = 0;
    }

    /// Set a preset dictionary, preloading it into the sliding window.
    ///
    /// Returns the Adler-32 checksum of the dictionary.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.reset();
        let dict = if dictionary.len() > WINDOW_SIZE {
            &dictionary[dictionary.len() - WINDOW_SIZE..]
        } else {
            dictionary
        };
        let mut slid = 0usize;
        let taken = self.win.fill(dict, &mut slid);
        debug_assert_eq!(taken, dict.len());
        if self.win.lookahead >= MIN_MATCH {
            self.win.restart_hash();
            let end = self.win.strstart + self.win.lookahead - (MIN_MATCH - 1);
            let mut p = self.win.strstart;
            while p < end {
                self.win.insert_string(p);
                p += 1;
            }
        }
        self.win.strstart = dict.len();
        self.win.lookahead = 0;
        self.win.insert = dict.len().min(MIN_MATCH - 1);
        self.dictionary_size = dict.len();
        adler32(dictionary)
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.dictionary_size > 0
    }

    /// Get the current dictionary size (how much of the window is pre-filled).
    pub fn dictionary_size(&self) -> usize {
        self.dictionary_size
    }

    /// Compress input data to LZ77 tokens.
    ///
    /// History carries across calls, so `compress(a)` followed by
    /// `compress(b)` can reference `a` from `b`.
    pub fn compress(&mut self, input: &[u8]) -> Vec<Lz77Token> {
        let mut tokens = Vec::with_capacity(input.len() / 2 + 8);
        let mut pos = 0usize;
        if self.level == 0 {
            // Level 0 never matches: it is the stored-block level.
            let mut slid = 0;
            while pos < input.len() {
                pos += self.win.fill(&input[pos..], &mut slid);
                while self.win.lookahead > 0 {
                    tokens.push(Lz77Token::Literal(self.win.buf[self.win.strstart]));
                    self.win.strstart += 1;
                    self.win.lookahead -= 1;
                }
            }
            return tokens;
        }
        match self.method {
            Method::Fast => self.run_fast(input, &mut pos, &mut tokens),
            _ => self.run_slow(input, &mut pos, &mut tokens),
        }
        tokens
    }

    fn fill_from(&mut self, input: &[u8], pos: &mut usize) {
        let mut slid = 0usize;
        *pos += self
            .win
            .fill(input.get(*pos..).unwrap_or_default(), &mut slid);
    }

    fn push_match(&self, tokens: &mut Vec<Lz77Token>, length: usize, distance: usize) -> bool {
        if length < self.min_useful_match {
            return false;
        }
        tokens.push(Lz77Token::Match {
            length: length as u16,
            distance: distance as u16,
        });
        true
    }

    /// Greedy matching (levels 1-3).
    fn run_fast(&mut self, input: &[u8], pos: &mut usize, tokens: &mut Vec<Lz77Token>) {
        let good = usize::from(self.params.good_length);
        let nice = usize::from(self.params.nice_length);
        let chain = usize::from(self.params.max_chain);
        let max_insert = usize::from(self.params.max_lazy);

        loop {
            if self.win.lookahead < MIN_LOOKAHEAD {
                self.fill_from(input, pos);
                if self.win.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0usize;
            if self.win.lookahead >= MIN_MATCH {
                hash_head = self.win.insert_string(self.win.strstart);
            }
            if hash_head != 0 && self.win.strstart - hash_head <= MAX_DIST {
                self.match_length =
                    self.win
                        .longest_match(hash_head, MIN_MATCH - 1, good, nice, chain);
            }
            if self.match_length >= MIN_MATCH {
                let dist = self.win.strstart - self.win.match_start;
                if self.push_match(tokens, self.match_length, dist) {
                    self.win.lookahead -= self.match_length;
                    if self.match_length <= max_insert && self.win.lookahead >= MIN_MATCH {
                        self.match_length -= 1;
                        loop {
                            self.win.strstart += 1;
                            self.win.insert_string(self.win.strstart);
                            self.match_length -= 1;
                            if self.match_length == 0 {
                                break;
                            }
                        }
                        self.win.strstart += 1;
                    } else {
                        self.win.strstart += self.match_length;
                        self.match_length = 0;
                        self.win.restart_hash();
                    }
                    continue;
                }
                self.match_length = MIN_MATCH - 1;
            }
            tokens.push(Lz77Token::Literal(self.win.buf[self.win.strstart]));
            self.win.lookahead -= 1;
            self.win.strstart += 1;
        }
    }

    /// Lazy matching (levels 4-9).
    fn run_slow(&mut self, input: &[u8], pos: &mut usize, tokens: &mut Vec<Lz77Token>) {
        let good = usize::from(self.params.good_length);
        let nice = usize::from(self.params.nice_length);
        let chain = usize::from(self.params.max_chain);
        let max_lazy = usize::from(self.params.max_lazy);

        loop {
            if self.win.lookahead < MIN_LOOKAHEAD {
                self.fill_from(input, pos);
                if self.win.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0usize;
            if self.win.lookahead >= MIN_MATCH {
                hash_head = self.win.insert_string(self.win.strstart);
            }
            self.prev_length = self.match_length;
            self.prev_match = self.win.match_start;
            self.match_length = MIN_MATCH - 1;

            if hash_head != 0
                && self.prev_length < max_lazy
                && self.win.strstart - hash_head <= MAX_DIST
            {
                self.match_length =
                    self.win
                        .longest_match(hash_head, self.prev_length, good, nice, chain);
                if self.match_length == MIN_MATCH
                    && self.win.strstart - self.win.match_start > TOO_FAR
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }

            if self.prev_length >= self.min_useful_match.max(MIN_MATCH)
                && self.match_length <= self.prev_length
            {
                let max_insert = self.win.strstart + self.win.lookahead - MIN_MATCH;
                let dist = self.win.strstart - 1 - self.prev_match;
                self.push_match(tokens, self.prev_length, dist);
                self.win.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.win.strstart += 1;
                    if self.win.strstart <= max_insert {
                        self.win.insert_string(self.win.strstart);
                    }
                    self.prev_length -= 1;
                    if self.prev_length == 0 {
                        break;
                    }
                }
                self.match_available = false;
                self.match_length = MIN_MATCH - 1;
                self.win.strstart += 1;
            } else if self.match_available {
                tokens.push(Lz77Token::Literal(self.win.buf[self.win.strstart - 1]));
                self.win.strstart += 1;
                self.win.lookahead -= 1;
            } else {
                self.match_available = true;
                self.win.strstart += 1;
                self.win.lookahead -= 1;
            }
        }
        if self.match_available {
            tokens.push(Lz77Token::Literal(self.win.buf[self.win.strstart - 1]));
            self.match_available = false;
        }
    }

    /// Compress all data at once (convenience method).
    pub fn compress_all(input: &[u8], level: u8) -> Vec<Lz77Token> {
        let mut encoder = Self::with_level(level);
        encoder.compress(input)
    }

    /// The window, for the optimal parser.
    pub(crate) fn window_mut(&mut self) -> &mut Window {
        &mut self.win
    }
}

/// Adler-32 checksum (for dictionary identification).
fn adler32(data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    const NMAX: usize = 5552;

    let mut a: u32 = 1;
    let mut b: u32 = 0;
    let mut remaining = data;

    while remaining.len() >= NMAX {
        let (chunk, rest) = remaining.split_at(NMAX);
        remaining = rest;
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD_ADLER;
        b %= MOD_ADLER;
    }
    for &byte in remaining {
        a += u32::from(byte);
        b += a;
    }
    ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
}

impl Default for Lz77Encoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(tokens: &[Lz77Token]) -> Vec<u8> {
        let mut out = Vec::new();
        for token in tokens {
            match token {
                Lz77Token::Literal(b) => out.push(*b),
                Lz77Token::Match { length, distance } => {
                    for _ in 0..*length {
                        let pos = out.len() - *distance as usize;
                        out.push(out[pos]);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn test_literals_only() {
        let input = b"abcdefgh";
        let tokens = Lz77Encoder::compress_all(input, 6);
        assert!(tokens.iter().all(|t| matches!(t, Lz77Token::Literal(_))));
        assert_eq!(tokens.len(), 8);
    }

    #[test]
    fn test_simple_match() {
        let input = b"abcabcabcabc";
        let tokens = Lz77Encoder::compress_all(input, 6);
        assert!(
            tokens.iter().any(|t| matches!(t, Lz77Token::Match { .. })),
            "Should find at least one match"
        );
        assert_eq!(decode(&tokens), input);
    }

    #[test]
    fn test_repeated_char() {
        let input = b"aaaaaaaaaa";
        let tokens = Lz77Encoder::compress_all(input, 6);
        assert_eq!(decode(&tokens), input);
        assert!(tokens.len() < 10, "Should compress repeated chars");
    }

    #[test]
    fn test_decode_matches() {
        let input = b"Hello, Hello, Hello!";
        let tokens = Lz77Encoder::compress_all(input, 6);
        assert_eq!(decode(&tokens), input);
    }

    #[test]
    fn test_level_0_is_all_literals() {
        let input = b"test data test data";
        let tokens = Lz77Encoder::compress_all(input, 0);
        assert!(tokens.iter().all(|t| matches!(t, Lz77Token::Literal(_))));
        assert_eq!(decode(&tokens), input);
    }

    #[test]
    fn every_level_round_trips_through_the_token_stream() {
        let data: Vec<u8> = (0..40_000u32)
            .map(|i| ((i * 7 % 251) ^ (i / 997)) as u8)
            .collect();
        for level in 1..=9u8 {
            let tokens = Lz77Encoder::compress_all(&data, level);
            assert_eq!(decode(&tokens), data, "level {level} round trip");
        }
    }

    #[test]
    fn params_for_level_match_zlib_rows() {
        assert_eq!(
            Lz77Params::for_level(1),
            Lz77Params {
                good_length: 4,
                max_lazy: 4,
                nice_length: 8,
                max_chain: 4
            }
        );
        assert_eq!(
            Lz77Params::for_level(9),
            Lz77Params {
                good_length: 32,
                max_lazy: 258,
                nice_length: 258,
                max_chain: 4096
            }
        );
        assert_eq!(Lz77Preset::Default.params(), Lz77Params::for_level(6));
        assert_eq!(Lz77Preset::Ultra.params(), Lz77Params::for_level(9));
    }

    #[test]
    fn cross_call_history_is_preserved() {
        let mut e = Lz77Encoder::with_level(6);
        let first = e.compress(b"the quick brown fox jumps over the lazy dog");
        let second = e.compress(b"the quick brown fox jumps over the lazy dog");
        assert_eq!(
            decode(&first),
            b"the quick brown fox jumps over the lazy dog"
        );
        assert!(
            second.iter().any(|t| matches!(t, Lz77Token::Match { .. })),
            "second call must reference the first call's history"
        );
    }

    #[test]
    fn dictionary_lets_the_first_bytes_match() {
        let mut e = Lz77Encoder::with_level(6);
        let sum = e.set_dictionary(b"the quick brown fox jumps over the lazy dog");
        assert_ne!(sum, 0);
        assert!(e.has_dictionary());
        assert_eq!(e.dictionary_size(), 43);
        let tokens = e.compress(b"the quick brown fox jumps over the lazy dog");
        assert!(
            tokens.iter().any(|t| matches!(t, Lz77Token::Match { .. })),
            "dictionary must be matchable"
        );
    }

    #[test]
    fn min_match_length_override_suppresses_short_matches() {
        let data = b"ab_ab_xyzxyzxyzxyzxyzxyzxyzxyz";
        let mut e = Lz77Encoder::with_level(6).with_min_match_length(8);
        let tokens = e.compress(data);
        for t in &tokens {
            if let Lz77Token::Match { length, .. } = t {
                assert!(*length >= 8, "short match {length} leaked through");
            }
        }
        assert_eq!(decode(&tokens), data);
    }

    #[test]
    fn nice_length_and_chain_overrides_still_round_trip() {
        let input: Vec<u8> = (0u8..=127).cycle().take(20_000).collect();
        let mut e = Lz77Encoder::with_level(6)
            .with_nice_length(32)
            .with_max_chain(8)
            .with_good_length(16);
        let tokens = e.compress(&input);
        assert_eq!(decode(&tokens), input);
    }
}
