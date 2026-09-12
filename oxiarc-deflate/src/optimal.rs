//! Graph-based optimal DEFLATE parser (token level).
//!
//! [`OptimalParser`] computes a shortest path through the LZ77 token graph
//! instead of deciding one position at a time. It is fed the **whole**
//! candidate set from the hash chains — every improving `(length, distance)`
//! pair, plus each length-code boundary below the longest match with the
//! nearest distance that reaches it — and it prices symbols with Huffman code
//! lengths refined from the previous pass's token histogram.
//!
//! Both details matter: a DP fed only the single longest match per position
//! has no cheaper alternative to pick and loses to plain lazy matching, and a
//! DP priced with a fixed cost table cannot see that a rare distance code is
//! expensive. The candidate set is also filtered by zlib's `TOO_FAR` rule, so
//! the parser is never offered a 3-byte match beyond 4096 bytes — buying a rare
//! long-distance code for three bytes makes every distance code in the block
//! more expensive, and leaving that filter out cost 2 % on noisy image rows.
//! As a final guard the parser scores a lazy parse of the same candidates under
//! the same model and returns whichever is cheaper.
//!
//! [`crate::Deflater::with_optimal_parsing`] makes the same choice on the
//! *real* block cost (tree description included) rather than on the model, so
//! that path is the one with the measured "never larger than level 8" guard.
//!
//! [`crate::Deflater::with_optimal_parsing`] runs the same algorithm directly
//! against its block buffer; this type exists for callers that want the token
//! sequence itself.

use crate::encoder::optimal::{
    Candidates, CostModel, Dp, MAX_SPAN, Step, histogram, lazy_path, score, solve,
};
use crate::encoder::tables::{MAX_MATCH, MIN_MATCH};
use crate::lz77::{Lz77Encoder, Lz77Token};

/// Maximum number of refinement passes (hard ceiling).
const MAX_PASSES: u8 = 8;

/// Optimal DEFLATE parser using iterative cost refinement.
///
/// # Example
///
/// ```rust
/// use oxiarc_deflate::{OptimalParser, lz77::Lz77Encoder};
///
/// let data: Vec<u8> = (0..8192u32).map(|i| (i % 61) as u8).collect();
/// let mut encoder = Lz77Encoder::with_level(9);
/// let tokens = OptimalParser::new().parse(&data, &mut encoder);
///
/// // The tokens reproduce the input exactly.
/// let mut out: Vec<u8> = Vec::new();
/// for t in &tokens {
///     match *t {
///         oxiarc_deflate::Lz77Token::Literal(b) => out.push(b),
///         oxiarc_deflate::Lz77Token::Match { length, distance } => {
///             for _ in 0..length {
///                 let p = out.len() - distance as usize;
///                 out.push(out[p]);
///             }
///         }
///     }
/// }
/// assert_eq!(out, data);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct OptimalParser {
    passes: u8,
}

impl OptimalParser {
    /// Create a new `OptimalParser` with the default two refinement passes.
    pub fn new() -> Self {
        Self { passes: 2 }
    }

    /// Create a new `OptimalParser` with a custom number of passes
    /// (clamped to `1..=8`).
    pub fn with_passes(passes: u8) -> Self {
        Self {
            passes: passes.clamp(1, MAX_PASSES),
        }
    }

    /// Run the optimal parser on `data`, returning an LZ77 token sequence.
    ///
    /// `encoder` supplies the sliding window, the hash chains and the search
    /// depth of its level; it is reset first, so history from earlier calls is
    /// not carried in.
    pub fn parse(&mut self, data: &[u8], encoder: &mut Lz77Encoder) -> Vec<Lz77Token> {
        if data.is_empty() {
            return Vec::new();
        }
        let chain = usize::from(encoder.params().max_chain);
        encoder.reset();
        let win = encoder.window_mut();

        let mut tokens: Vec<Lz77Token> = Vec::with_capacity(data.len() / 2 + 8);
        let mut cands = Candidates::default();
        let mut dp = Dp::default();
        let mut alt: Vec<Step> = Vec::new();

        let mut pos = 0usize;
        loop {
            let mut slid = 0usize;
            pos += win.fill(data.get(pos..).unwrap_or_default(), &mut slid);
            if win.lookahead == 0 {
                break;
            }
            let more_input = pos < data.len();
            let guard = if more_input && win.lookahead > MAX_MATCH {
                MAX_MATCH
            } else {
                0
            };
            let span_min = (win.lookahead - guard).min(MAX_SPAN);
            if span_min == 0 {
                break;
            }
            let positions = (span_min + MAX_MATCH).min(win.lookahead);

            // Insert every position and record its candidates.
            cands.flat.clear();
            cands.starts.clear();
            let start = win.strstart;
            let data_end = win.strstart + win.lookahead;
            let mut undo: Vec<(usize, u16)> = Vec::new();
            win.restart_hash();
            for i in 0..positions {
                cands.starts.push(cands.flat.len() as u32);
                let p = start + i;
                if data_end - p < MIN_MATCH {
                    continue;
                }
                let head = win.insert_string(p);
                if i >= span_min {
                    undo.push((win.last_hash(), head as u16));
                }
                let max_len = (data_end - p).min(MAX_MATCH);
                if max_len < MIN_MATCH {
                    continue;
                }
                win.match_candidates(p, head, max_len, chain, &mut cands.flat);
            }
            cands.starts.push(cands.flat.len() as u32);

            // The lazy parse fixes the span length so both candidate paths
            // cover exactly the same bytes. Positions the span does not reach
            // are rolled back out of the chains so the next span inserts them
            // itself, in order.
            let span = lazy_path(&cands, span_min, positions, &mut alt);
            alt.truncate(span);
            for &(h, prev_head) in undo.iter().skip(span.saturating_sub(span_min)).rev() {
                win.undo_insert(h, prev_head);
            }

            let mut model = CostModel::fixed();
            solve(&win.buf, start, &cands, span, &model, &mut dp);
            for _ in 1..self.passes {
                let (lfreq, dfreq) = histogram(&win.buf, start, &dp.path, span);
                model = CostModel::from_histogram(&lfreq, &dfreq);
                solve(&win.buf, start, &cands, span, &model, &mut dp);
            }
            if score(&win.buf, start, &alt, span, &model)
                < score(&win.buf, start, &dp.path, span, &model)
            {
                std::mem::swap(&mut dp.path, &mut alt);
            }

            let mut i = 0usize;
            while i < span {
                let step = dp.path[i];
                let len = usize::from(step.len).max(1);
                if len >= MIN_MATCH && step.dist != 0 {
                    tokens.push(Lz77Token::Match {
                        length: len as u16,
                        distance: step.dist,
                    });
                } else {
                    tokens.push(Lz77Token::Literal(win.buf[win.strstart]));
                }
                win.strstart += len;
                win.lookahead -= len;
                i += len;
            }
        }
        tokens
    }
}

impl Default for OptimalParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::{Deflater, deflate};
    use crate::inflate::inflate;

    fn decode(tokens: &[Lz77Token]) -> Vec<u8> {
        let mut out = Vec::new();
        for token in tokens {
            match *token {
                Lz77Token::Literal(b) => out.push(b),
                Lz77Token::Match { length, distance } => {
                    for _ in 0..length {
                        let pos = out.len() - distance as usize;
                        out.push(out[pos]);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn optimal_tokens_reproduce_the_input() {
        for size in [1usize, 2, 3, 300, 5000, 70_000] {
            let data: Vec<u8> = (0..size as u32)
                .map(|i| ((i * 31 % 199) ^ (i / 313)) as u8)
                .collect();
            let mut encoder = Lz77Encoder::with_level(9);
            let tokens = OptimalParser::new().parse(&data, &mut encoder);
            assert_eq!(decode(&tokens), data, "size {size}");
        }
    }

    #[test]
    fn optimal_parsing_round_trips_through_the_deflater() {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(400);
        let mut d = Deflater::with_optimal_parsing(9);
        let out = d.compress_to_vec(&text).expect("deflate");
        assert_eq!(inflate(&out).expect("inflate"), text);
    }

    #[test]
    fn optimal_parsing_is_never_larger_than_the_default_ladder() {
        let corpora: Vec<Vec<u8>> = vec![
            b"the quick brown fox jumps over the lazy dog. ".repeat(500),
            (0..60_000u32).map(|i| (i % 251) as u8).collect(),
            (0..60_000u32)
                .map(|i| ((i.wrapping_mul(2_654_435_761u32)) >> 24) as u8)
                .collect(),
            vec![0u8; 40_000],
        ];
        for (n, data) in corpora.iter().enumerate() {
            let mut d = Deflater::with_optimal_parsing(9);
            let opt = d.compress_to_vec(data).expect("deflate");
            assert_eq!(inflate(&opt).expect("inflate"), *data, "corpus {n}");
            for level in [8u8, 9] {
                let plain = deflate(data, level).expect("deflate");
                assert!(
                    opt.len() <= plain.len(),
                    "corpus {n}: optimal {} > level {level} {}",
                    opt.len(),
                    plain.len()
                );
            }
        }
    }
}
