//! Dynamic-programming ("optimal") parser, opt-in at level 9.
//!
//! The lazy parser decides one position at a time; this parser computes a true
//! shortest path through the LZ77 token graph of a span, so it can pay for a
//! shorter match now to reach a much longer one later. Two things make it
//! actually win rather than starve:
//!
//! 1. **It is fed the whole candidate set.** Every *improving* `(length,
//!    distance)` pair on the hash chain is kept, not just the longest match,
//!    and each length-code boundary below the longest match is offered with the
//!    nearest distance that reaches it. A DP fed only the single longest match
//!    per position has no cheaper alternative to choose and reliably loses to
//!    lazy matching — that is exactly what an earlier experiment measured.
//!    The set is still filtered by zlib's `TOO_FAR` rule; without that filter
//!    the parser buys rare long-distance codes for 3-byte matches, which makes
//!    *every* distance code more expensive and cost 2 % on noisy PNG rows.
//! 2. **The cost model is refined.** Pass 1 prices symbols with the fixed
//!    Huffman lengths; the token histogram it produces builds real
//!    length-limited codes, and pass 2 re-runs the DP against those.
//!
//! The parser then scores its own path *and* a lazy path built from the same
//! candidate arrays under the final model and emits whichever is cheaper, so
//! turning optimal parsing on can cost encode time but never compression.

use super::tables::{
    BASE_LENGTH, D_CODES, EXTRA_DBITS, EXTRA_LBITS, L_CODES, LENGTH_CODE, LITERALS, MAX_MATCH,
    MIN_MATCH, STATIC_DTREE_LEN, STATIC_LTREE_LEN, d_code,
};
use super::trees::{BlockTrees, Sym};
use super::{DeflateEncoder, Flush};
use crate::huffman::HuffmanBuilder;

/// Positions parsed per dynamic-programming span.
pub(crate) const MAX_SPAN: usize = 16384;
/// Cost charged to a symbol the current model never saw (so the parser does
/// not treat an unused symbol as free).
const UNSEEN_COST: u32 = 30;

/// First match length covered by each length code.
const LEN_CODE_FIRST: [u16; 29] = build_len_code_first();

const fn build_len_code_first() -> [u16; 29] {
    let mut out = [0u16; 29];
    let mut c = 0;
    while c < 29 {
        out[c] = BASE_LENGTH[c] + MIN_MATCH as u16;
        c += 1;
    }
    out
}

/// Bit cost of every literal/length and distance symbol.
pub(crate) struct CostModel {
    lit: [u32; L_CODES],
    dist: [u32; D_CODES],
}

impl CostModel {
    /// Prices from the fixed (static) Huffman trees.
    pub(crate) fn fixed() -> Self {
        let mut lit = [0u32; L_CODES];
        for (i, slot) in lit.iter_mut().enumerate() {
            *slot = u32::from(STATIC_LTREE_LEN[i]);
        }
        let mut dist = [0u32; D_CODES];
        for (i, slot) in dist.iter_mut().enumerate() {
            *slot = u32::from(STATIC_DTREE_LEN[i]);
        }
        Self { lit, dist }
    }

    /// Prices from length-limited codes built for the given histogram.
    pub(crate) fn from_histogram(lfreq: &[u32; L_CODES], dfreq: &[u32; D_CODES]) -> Self {
        let mut lb = HuffmanBuilder::new(L_CODES, 15);
        for (sym, &f) in lfreq.iter().enumerate() {
            if f > 0 {
                lb.add_count(sym as u16, f);
            }
        }
        lb.add_count(256, 1);
        let llen = lb.build_lengths();

        let mut db = HuffmanBuilder::new(D_CODES, 15);
        let mut any_dist = false;
        for (sym, &f) in dfreq.iter().enumerate() {
            if f > 0 {
                db.add_count(sym as u16, f);
                any_dist = true;
            }
        }
        if !any_dist {
            db.add_count(0, 1);
        }
        let dlen = db.build_lengths();

        let mut lit = [UNSEEN_COST; L_CODES];
        for (i, slot) in lit.iter_mut().enumerate() {
            match llen.get(i).copied() {
                Some(0) | None => {}
                Some(l) => *slot = u32::from(l),
            }
        }
        let mut dist = [UNSEEN_COST; D_CODES];
        for (i, slot) in dist.iter_mut().enumerate() {
            match dlen.get(i).copied() {
                Some(0) | None => {}
                Some(l) => *slot = u32::from(l),
            }
        }
        Self { lit, dist }
    }

    #[inline(always)]
    fn lit_cost(&self, b: u8) -> u32 {
        self.lit[b as usize]
    }

    #[inline(always)]
    fn match_cost(&self, len: usize, dist: usize) -> u32 {
        let lc = len - MIN_MATCH;
        let code = LENGTH_CODE[lc] as usize;
        let dcode = d_code(dist - 1);
        self.lit[LITERALS + 1 + code]
            + u32::from(EXTRA_LBITS[code])
            + self.dist[dcode]
            + u32::from(EXTRA_DBITS[dcode])
    }
}

/// One parsed token in a span.
#[derive(Clone, Copy, Default)]
pub(crate) struct Step {
    /// Token length in bytes (`1` means a literal).
    pub len: u16,
    /// Match distance, or `0` for a literal.
    pub dist: u16,
}

/// A back-pointer in the shortest-path table: the span offset the token starts
/// at, plus the token itself. The start is stored explicitly because the last
/// token of a span is allowed to run *past* the span end (a match may reach
/// into the held-back tail), so `end - len` is not always the start.
#[derive(Clone, Copy, Default)]
pub(crate) struct Back {
    from: u32,
    step: Step,
}

/// Candidate storage for one span: a flat `(len, dist)` array plus per-position
/// start offsets.
#[derive(Default)]
pub(crate) struct Candidates {
    /// Flat `(len, dist)` pairs for every position, in span order.
    pub flat: Vec<(u16, u16)>,
    /// `starts[i]..starts[i + 1]` slices `flat` for span offset `i`.
    pub starts: Vec<u32>,
}

impl Candidates {
    /// Candidates recorded for span offset `i`.
    #[inline]
    pub(crate) fn at(&self, i: usize) -> &[(u16, u16)] {
        let a = self.starts[i] as usize;
        let b = self.starts[i + 1] as usize;
        self.flat.get(a..b).unwrap_or_default()
    }
}

/// Everything the parser needs between spans, owned by the encoder so a call
/// that produces nothing costs nothing.
///
/// [`BlockTrees`] alone holds a 573-entry heap, a 573-byte depth array and
/// twelve `Vec`s; building it per `deflate()` call made a parse fed one byte at
/// a time cost **38 us per call** against 1.5 us of real work — a 851x
/// per-call cliff (measured: 64 KiB of random bytes at level 9, 2.497 s in
/// 1-byte calls versus 2.93 ms in one call). The default ladder never had this
/// cliff, which is why it only ever showed up on the opt-in path.
#[derive(Default)]
pub(crate) struct OptimalScratch {
    cands: Candidates,
    dp: Dp,
    alt: Vec<Step>,
    trees: Option<Box<BlockTrees>>,
    syms_a: Vec<Sym>,
    syms_b: Vec<Sym>,
    undo: Vec<(usize, u16)>,
}

impl std::fmt::Debug for OptimalScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptimalScratch").finish_non_exhaustive()
    }
}

/// Run the optimal parser until the input is exhausted or the flush is served.
///
/// The scratch is moved out of `enc` for the duration (the parser needs
/// `&mut enc` at the same time) and moved back before returning, on every path.
pub(crate) fn run(enc: &mut DeflateEncoder, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
    let mut scratch = enc.take_optimal_scratch();
    let done = run_with(enc, &mut scratch, input, pos, flush);
    enc.put_optimal_scratch(scratch);
    done
}

fn run_with(
    enc: &mut DeflateEncoder,
    state: &mut OptimalScratch,
    input: &[u8],
    pos: &mut usize,
    flush: Flush,
) -> bool {
    let OptimalScratch {
        cands,
        dp,
        alt,
        trees,
        syms_a,
        syms_b,
        undo,
    } = state;
    let scratch = trees.get_or_insert_with(|| Box::new(BlockTrees::new()));

    loop {
        // Refill until the window holds a whole span plus the match guard.
        //
        // The lazy ladder can profitably parse a single position, so it runs as
        // soon as it has `MIN_LOOKAHEAD` bytes. The DP cannot: a span of `n`
        // positions costs candidate collection over `n + MAX_MATCH` positions,
        // so parsing one byte at a time costs 258 chain walks per byte.
        // Measured before this loop existed: 200 KB of text at level 9 took
        // 4.17 s in 1-byte calls against 0.31 s in one call (a 13x cliff) and
        // produced 20 277 bytes against 19 920 (1.8 % worse), because every
        // tiny span also paid its own block-boundary decisions.
        //
        // `want` is a function of `strstart` alone — which is itself a
        // deterministic function of the bytes consumed so far — so the span
        // boundaries, and therefore the output, do not depend on how the
        // caller split its calls.
        loop {
            if enc.win.lookahead >= span_target(enc) || *pos >= input.len() {
                break;
            }
            let before_pos = *pos;
            let before_lookahead = enc.win.lookahead;
            enc.fill_from(input, pos);
            if *pos == before_pos && enc.win.lookahead == before_lookahead {
                // The window cannot take more at this `strstart`.
                break;
            }
        }
        if flush == Flush::None && enc.win.lookahead < span_target(enc) {
            return false;
        }
        if enc.win.lookahead == 0 {
            break;
        }

        // Hold back the tail while more input may still arrive, so matches are
        // not cut short at an artificial boundary.
        let guard = if enc.win.lookahead >= super::config::MIN_LOOKAHEAD {
            super::config::MIN_LOOKAHEAD - 1
        } else {
            0
        };
        let span_min = (enc.win.lookahead - guard).min(MAX_SPAN);
        if span_min == 0 {
            break;
        }
        let positions = (span_min + MAX_MATCH).min(enc.win.lookahead);

        collect_candidates(enc, positions, span_min, cands, undo);

        let start = enc.win.strstart;

        // The lazy parse fixes the span length: it stops on a token boundary at
        // or past `span_min`, and the DP then covers exactly the same bytes.
        let span = lazy_path(cands, span_min, positions, alt);
        alt.truncate(span);
        rollback(enc, undo, span_min, span);

        // Pass 1: fixed-Huffman prices.
        let model = CostModel::fixed();
        solve(&enc.win.buf, start, cands, span, &model, dp);

        // Pass 2: prices from the histogram pass 1 produced.
        let (lfreq, dfreq) = histogram(&enc.win.buf, start, &dp.path, span);
        let model = CostModel::from_histogram(&lfreq, &dfreq);
        solve(&enc.win.buf, start, cands, span, &model, dp);

        // Keep the cheaper of the DP path and the lazy path over the same
        // candidates and the same bytes, judged by the *real* block cost (tree
        // description included), so optimal parsing can never lose to lazy
        // parsing.
        to_symbols(&enc.win.buf, start, &dp.path, span, syms_a);
        to_symbols(&enc.win.buf, start, alt, span, syms_b);
        let cost_dp = scratch.estimate_block_bits(&enc.syms, syms_a);
        let cost_lazy = scratch.estimate_block_bits(&enc.syms, syms_b);
        if (cost_lazy, syms_b.len()) < (cost_dp, syms_a.len()) {
            std::mem::swap(&mut dp.path, alt);
        }

        emit(enc, &dp.path, span);
    }

    if enc.win.strstart > 0 {
        enc.win.insert = 0;
    }
    enc.finish_blocks(flush)
}

/// How much lookahead the parser waits for before running a span.
///
/// A whole span (`MAX_SPAN`) plus the match guard (`MIN_LOOKAHEAD`), capped by
/// what the window can physically hold at the current `strstart`. The cap is
/// computed against the *post-slide* position, because
/// [`super::window::Window::fill`] slides before it copies, so a `strstart`
/// past `W_SIZE + MAX_DIST` really does have a full window of room.
fn span_target(enc: &DeflateEncoder) -> usize {
    use super::config::{MAX_DIST, MIN_LOOKAHEAD, W_SIZE};
    use super::window::WINDOW_SIZE;
    let strstart = if enc.win.strstart >= W_SIZE + MAX_DIST {
        enc.win.strstart - W_SIZE
    } else {
        enc.win.strstart
    };
    (MAX_SPAN + MIN_LOOKAHEAD).min(WINDOW_SIZE - strstart)
}

/// Insert `positions` window positions into the hash chains and record the
/// candidate matches at each.
///
/// Positions at or past `undo_from` are recorded so they can be rolled back
/// once the span length is known: only the positions the span actually
/// consumes may stay in the chains, or the next span would insert them a
/// second time and leave forward-pointing links behind.
fn collect_candidates(
    enc: &mut DeflateEncoder,
    positions: usize,
    undo_from: usize,
    cands: &mut Candidates,
    undo: &mut Vec<(usize, u16)>,
) {
    cands.flat.clear();
    cands.starts.clear();
    cands.starts.reserve(positions + 1);

    let chain = usize::from(enc.cfg.max_chain);
    let start = enc.win.strstart;
    let data_end = enc.win.strstart + enc.win.lookahead;

    undo.clear();
    enc.win.restart_hash();
    for i in 0..positions {
        cands.starts.push(cands.flat.len() as u32);
        let p = start + i;
        if data_end - p < MIN_MATCH {
            continue;
        }
        let head = enc.win.insert_string(p);
        if i >= undo_from {
            undo.push((enc.win.last_hash(), head as u16));
        }
        let max_len = (data_end - p).min(MAX_MATCH);
        if max_len < MIN_MATCH {
            continue;
        }
        enc.win
            .match_candidates(p, head, max_len, chain, &mut cands.flat);
    }
    cands.starts.push(cands.flat.len() as u32);
}

/// Roll back the recorded inserts for every position at or past `keep`.
fn rollback(enc: &mut DeflateEncoder, undo: &[(usize, u16)], undo_from: usize, keep: usize) {
    let drop_from = keep.saturating_sub(undo_from);
    for &(h, prev_head) in undo.iter().skip(drop_from).rev() {
        enc.win.undo_insert(h, prev_head);
    }
}

/// Reusable scratch for [`solve`]: the shortest-path tables plus the forward
/// token list they produce. Kept in one value so a caller can hand the whole
/// working set to `solve` and read the answer out of [`Dp::path`].
#[derive(Default)]
pub(crate) struct Dp {
    /// `cost[i]`: cheapest modelled bit cost of reaching span offset `i`.
    cost: Vec<u32>,
    /// Back-pointer for every reached offset.
    back: Vec<Back>,
    /// The solved path: `path[i]` is the token starting at span offset `i`
    /// (`len == 1` means "literal"). Positions inside a token are unused.
    pub path: Vec<Step>,
}

/// Forward shortest path over the span, written to `dp.path`.
pub(crate) fn solve(
    win: &[u8],
    start: usize,
    cands: &Candidates,
    span: usize,
    model: &CostModel,
    dp: &mut Dp,
) {
    dp.cost.clear();
    dp.cost.resize(span + 1, u32::MAX);
    dp.back.clear();
    dp.back.resize(span + 1, Back::default());
    dp.cost[0] = 0;

    for i in 0..span {
        let base = dp.cost[i];
        if base == u32::MAX {
            continue;
        }
        let lit = win[start + i];
        let c = base + model.lit_cost(lit);
        if c < dp.cost[i + 1] {
            dp.cost[i + 1] = c;
            dp.back[i + 1] = Back {
                from: i as u32,
                step: Step { len: 1, dist: 0 },
            };
        }

        let list = cands.at(i);
        let mut prev_len = MIN_MATCH - 1;
        let mut code = 0usize;
        for &(l, d) in list {
            let l = l as usize;
            // Every length-code boundary strictly above the previous
            // candidate's reach is achievable with this (nearest) distance.
            while code < LEN_CODE_FIRST.len() {
                let b = LEN_CODE_FIRST[code] as usize;
                if b > l {
                    break;
                }
                if b > prev_len && b < l {
                    relax(dp, span, i, b, d, model, base);
                }
                code += 1;
            }
            relax(dp, span, i, l, d, model, base);
            // The clamped length keeps a path to exactly `span` available when
            // the natural match would overshoot the span end.
            let rest = span - i;
            if rest >= MIN_MATCH && rest < l {
                relax(dp, span, i, rest, d, model, base);
            }
            prev_len = l;
        }
    }

    // Walk back, then flip into a forward token list.
    dp.path.clear();
    dp.path.resize(span, Step::default());
    let mut i = span;
    while i > 0 {
        let entry = dp.back[i];
        let at = entry.from as usize;
        dp.path[at] = entry.step;
        i = at;
    }
}

/// Relax the edge `i -> i + len`, ignoring transitions that would run past the
/// span end (the span always ends on a token boundary of the lazy parse, so a
/// legal path exists to exactly `span`).
#[inline]
fn relax(dp: &mut Dp, span: usize, i: usize, len: usize, dist: u16, model: &CostModel, base: u32) {
    let j = i + len;
    if j > span {
        return;
    }
    let c = base + model.match_cost(len, usize::from(dist));
    if c < dp.cost[j] {
        dp.cost[j] = c;
        dp.back[j] = Back {
            from: i as u32,
            step: Step {
                len: len as u16,
                dist,
            },
        };
    }
}

/// Lazy parse over the same candidate arrays (no extra chain walking).
///
/// The candidate set is `TOO_FAR`-filtered and its last entry at a position is
/// the same match `longest_match` would return, so at level 9 (where
/// `max_lazy` is 258 and `nice_length` is `MAX_MATCH`) this reproduces zlib's
/// `deflate_slow` decisions — it is a real fallback, not an approximation of
/// one.
///
/// Parses until at least `span_min` bytes are covered, then stops on the token
/// boundary it happens to land on, and returns that coverage. The DP is then
/// solved for **exactly** the same number of bytes, so the two candidate token
/// sequences describe the same input and their block costs are directly
/// comparable. (Comparing paths that cover different byte counts is what made
/// an earlier version pick a locally cheaper span that lost overall.)
pub(crate) fn lazy_path(
    cands: &Candidates,
    span_min: usize,
    positions: usize,
    out: &mut Vec<Step>,
) -> usize {
    out.clear();
    out.resize(positions, Step::default());
    let mut i = 0usize;
    while i < span_min {
        let here = if i < positions {
            cands.at(i).last().copied()
        } else {
            None
        };
        match here {
            Some((l, d)) if usize::from(l) >= MIN_MATCH => {
                let next = if i + 1 < positions {
                    cands.at(i + 1).last().copied()
                } else {
                    None
                };
                let defer = matches!(next, Some((l2, _)) if l2 > l);
                if defer {
                    out[i] = Step { len: 1, dist: 0 };
                    i += 1;
                } else {
                    out[i] = Step { len: l, dist: d };
                    i += usize::from(l);
                }
            }
            _ => {
                out[i] = Step { len: 1, dist: 0 };
                i += 1;
            }
        }
    }
    i
}

/// Total modelled bit cost of a token path.
pub(crate) fn score(
    win: &[u8],
    start: usize,
    path: &[Step],
    span: usize,
    model: &CostModel,
) -> u64 {
    let mut total = 0u64;
    let mut i = 0usize;
    while i < span {
        let step = path[i];
        debug_assert!(step.len > 0);
        let len = usize::from(step.len).max(1);
        if len >= MIN_MATCH && step.dist != 0 {
            total += u64::from(model.match_cost(len, usize::from(step.dist)));
        } else {
            total += u64::from(model.lit_cost(win[start + i]));
        }
        i += len;
    }
    total
}

/// Symbol histogram of a token path, for the next cost model.
pub(crate) fn histogram(
    win: &[u8],
    start: usize,
    path: &[Step],
    span: usize,
) -> ([u32; L_CODES], [u32; D_CODES]) {
    let mut lfreq = [0u32; L_CODES];
    let mut dfreq = [0u32; D_CODES];
    let mut i = 0usize;
    while i < span {
        let step = path[i];
        let len = usize::from(step.len).max(1);
        if len >= MIN_MATCH && step.dist != 0 {
            let code = LENGTH_CODE[len - MIN_MATCH] as usize;
            lfreq[LITERALS + 1 + code] += 1;
            dfreq[d_code(usize::from(step.dist) - 1)] += 1;
        } else {
            lfreq[win[start + i] as usize] += 1;
        }
        i += len;
    }
    (lfreq, dfreq)
}

/// Materialise a token path as the symbol list a block would tally.
pub(crate) fn to_symbols(win: &[u8], start: usize, path: &[Step], span: usize, out: &mut Vec<Sym>) {
    out.clear();
    let mut i = 0usize;
    while i < span {
        let step = path[i];
        let len = usize::from(step.len).max(1);
        if len >= MIN_MATCH && step.dist != 0 {
            out.push(Sym {
                dist: step.dist,
                lc: (len - MIN_MATCH) as u8,
            });
        } else {
            out.push(Sym {
                dist: 0,
                lc: win[start + i],
            });
        }
        i += len;
    }
}

/// Tally and advance the window over a parsed span.
///
/// The final token may cover more than `span` bytes (see [`relax`]); the window
/// simply advances by the real token lengths, and the caller re-derives the
/// next span from `strstart`/`lookahead`.
fn emit(enc: &mut DeflateEncoder, path: &[Step], span: usize) {
    let mut i = 0usize;
    while i < span {
        let step = path[i];
        let len = usize::from(step.len).max(1);
        let bflush = if len >= MIN_MATCH && step.dist != 0 {
            let b = enc.tally_dist(usize::from(step.dist), len - MIN_MATCH);
            enc.win.strstart += len;
            enc.win.lookahead -= len;
            b
        } else {
            let lit = enc.win.buf[enc.win.strstart];
            let b = enc.tally_lit(lit);
            enc.win.strstart += 1;
            enc.win.lookahead -= 1;
            b
        };
        if bflush {
            enc.emit_block(false);
        }
        i += len;
    }
}
