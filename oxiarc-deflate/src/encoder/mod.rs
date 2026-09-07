//! The DEFLATE encoder core.
//!
//! [`DeflateEncoder`] is a faithful reimplementation of zlib's `deflate()`
//! state machine: a persistent 32 KiB window with hash chains, zlib's
//! per-level `configuration_table`, `deflate_fast` (levels 1-3) and
//! `deflate_slow` (levels 4-9) with lazy matching and the `TOO_FAR` rule, a
//! 16 383-symbol block buffer, and per-block stored/fixed/dynamic selection on
//! real bit costs.
//!
//! Because the state (window, chains, symbol buffer, bit accumulator) persists
//! across calls, feeding the same bytes as one 1 MiB call or as a thousand
//! 1 KiB calls costs the same and produces the same stream: there is no
//! per-call window rehash, tree rebuild or forced block boundary.

pub(crate) mod config;
pub(crate) mod optimal;
pub(crate) mod stored;
pub(crate) mod tables;
pub(crate) mod trees;
pub(crate) mod window;

use config::{MAX_DIST, MIN_LOOKAHEAD, Method, SYM_END, TOO_FAR, W_SIZE, config_for, method_for};
use tables::{MAX_MATCH, MIN_MATCH};
use trees::{BitSink, BlockTrees, Sym};
use window::Window;

pub use config::LevelConfig;
use config::LevelConfig as CfgRow;
pub use trees::Strategy;

/// How much of the pending input a call must push out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flush {
    /// Buffer freely; emit only completed blocks.
    None,
    /// Emit everything fed so far, then an empty fixed block (zlib
    /// `Z_PARTIAL_FLUSH`).
    Partial,
    /// Emit everything fed so far, then an empty stored block (zlib
    /// `Z_SYNC_FLUSH`).
    Sync,
    /// Like [`Flush::Sync`], and forget the match history.
    Full,
    /// Emit everything and terminate the stream.
    Finish,
}

/// A resumable DEFLATE compressor.
#[derive(Debug)]
pub(crate) struct DeflateEncoder {
    win: Window,
    trees: Box<BlockTrees>,
    sink: BitSink,
    syms: Vec<Sym>,
    level: u8,
    strategy: Strategy,
    cfg: CfgRow,
    method: Method,
    /// Start of the current block, as a window offset. Goes negative when a
    /// block outlives a window slide (zlib's `long block_start`).
    block_start: i64,
    match_length: usize,
    prev_length: usize,
    prev_match: usize,
    match_available: bool,
    finished: bool,
    /// Level-9 opt-in: run the dynamic-programming parser instead of the lazy
    /// one. See [`optimal`].
    optimal: bool,
    /// Persistent scratch for the optimal parser, so a call that produces
    /// nothing costs nothing. `None` until the parser first runs, so the
    /// default ladder never pays for it.
    optimal_scratch: Option<Box<optimal::OptimalScratch>>,
}

impl DeflateEncoder {
    /// A fresh encoder at `level` with the default strategy.
    pub(crate) fn new(level: u8) -> Self {
        Self::with_window(level, Strategy::Default, Window::new())
    }

    /// A fresh encoder reusing pooled buffers.
    pub(crate) fn with_buffers(level: u8, buf: Vec<u8>, head: Vec<u16>, prev: Vec<u16>) -> Self {
        Self::with_window(
            level,
            Strategy::Default,
            Window::from_buffers(buf, head, prev),
        )
    }

    fn with_window(level: u8, strategy: Strategy, win: Window) -> Self {
        let level = level.min(9);
        Self {
            win,
            trees: Box::new(BlockTrees::new()),
            sink: BitSink {
                out: Vec::new(),
                bit_buf: 0,
                bit_valid: 0,
            },
            syms: Vec::with_capacity(SYM_END),
            level,
            strategy,
            cfg: config_for(level),
            method: method_for(level),
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            finished: false,
            optimal: false,
            optimal_scratch: None,
        }
    }

    /// Borrow the optimal parser's scratch out of `self` (the parser needs
    /// `&mut self` at the same time). Always paired with
    /// [`DeflateEncoder::put_optimal_scratch`].
    pub(crate) fn take_optimal_scratch(&mut self) -> Box<optimal::OptimalScratch> {
        self.optimal_scratch
            .take()
            .unwrap_or_else(|| Box::new(optimal::OptimalScratch::default()))
    }

    /// Give the scratch back so the next call reuses its buffers.
    pub(crate) fn put_optimal_scratch(&mut self, scratch: Box<optimal::OptimalScratch>) {
        self.optimal_scratch = Some(scratch);
    }

    /// Select the compression strategy (zlib's `Z_*` strategies).
    pub(crate) fn set_strategy(&mut self, strategy: Strategy) {
        self.strategy = strategy;
    }

    /// Turn on the level-9 dynamic-programming parser.
    pub(crate) fn set_optimal(&mut self, optimal: bool) {
        self.optimal = optimal;
    }

    /// The configured level.
    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    /// The configured strategy.
    pub(crate) fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Whether the dynamic-programming parser is enabled.
    pub(crate) fn is_optimal(&self) -> bool {
        self.optimal
    }

    /// Override the per-level match-finder configuration.
    pub(crate) fn set_config(&mut self, cfg: CfgRow) {
        self.cfg = cfg;
    }

    /// The active match-finder configuration.
    pub(crate) fn config(&self) -> CfgRow {
        self.cfg
    }

    /// Forget the match history without touching the window contents or the
    /// pending block (zlib `CLEAR_HASH`).
    pub(crate) fn clear_history(&mut self) {
        self.win.clear_hash();
    }

    /// Whether the final block has been written.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Give the window buffers back for pooling.
    pub(crate) fn into_buffers(self) -> (Vec<u8>, Vec<u16>, Vec<u16>) {
        self.win.into_buffers()
    }

    /// Drop all state, keeping the allocations.
    pub(crate) fn reset(&mut self) {
        self.win.reset();
        self.trees.init_block();
        self.sink.out.clear();
        self.sink.bit_buf = 0;
        self.sink.bit_valid = 0;
        self.syms.clear();
        self.block_start = 0;
        self.match_length = MIN_MATCH - 1;
        self.prev_length = MIN_MATCH - 1;
        self.prev_match = 0;
        self.match_available = false;
        self.finished = false;
    }

    /// Preload a dictionary (zlib `deflateSetDictionary`).
    pub(crate) fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.reset();
        let dict = if dictionary.len() > W_SIZE {
            &dictionary[dictionary.len() - W_SIZE..]
        } else {
            dictionary
        };
        let mut pos = 0usize;
        let mut slid = 0usize;
        pos += self.win.fill(&dict[pos..], &mut slid);
        while self.win.lookahead >= MIN_MATCH {
            let mut str_pos = self.win.strstart;
            let mut n = self.win.lookahead - (MIN_MATCH - 1);
            while n > 0 {
                self.win.insert_string(str_pos);
                str_pos += 1;
                n -= 1;
            }
            self.win.strstart = str_pos;
            self.win.lookahead = MIN_MATCH - 1;
            let before = pos;
            pos += self.win.fill(&dict[pos..], &mut slid);
            if pos == before && self.win.lookahead < MIN_MATCH {
                break;
            }
        }
        self.win.strstart += self.win.lookahead;
        self.block_start = self.win.strstart as i64;
        self.win.insert = self.win.lookahead;
        self.win.lookahead = 0;
        self.match_length = MIN_MATCH - 1;
        self.prev_length = MIN_MATCH - 1;
        self.match_available = false;
    }

    /// Take everything produced so far.
    pub(crate) fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sink.out)
    }

    /// The produced bytes without consuming them.
    pub(crate) fn output(&self) -> &[u8] {
        &self.sink.out
    }

    /// Discard the produced bytes.
    pub(crate) fn clear_output(&mut self) {
        self.sink.out.clear();
    }

    /// Compress `input`, honouring `flush`.
    ///
    /// Every flush mode except [`Flush::None`] guarantees that all bytes fed so
    /// far (in this and every previous call) have been written out.
    pub(crate) fn push(&mut self, input: &[u8], flush: Flush) {
        if self.finished {
            return;
        }
        self.sink.out.reserve(input.len() / 2 + 64);
        let mut pos = 0usize;

        if input.is_empty() && flush == Flush::None && self.win.lookahead == 0 {
            return;
        }

        // The strategies consume all of `input` in one call, but the stored
        // path is written against zlib's bounded-output structure and can in
        // principle stop early; loop until it stops making progress so no fed
        // byte is ever dropped.
        let mut finish;
        loop {
            let before = pos;
            finish = match self.method {
                Method::Stored => stored::run(self, input, &mut pos, flush),
                _ => match self.strategy {
                    Strategy::HuffmanOnly => self.run_huff(input, &mut pos, flush),
                    Strategy::Rle => self.run_rle(input, &mut pos, flush),
                    _ if self.optimal => optimal::run(self, input, &mut pos, flush),
                    _ if self.method == Method::Fast => self.run_fast(input, &mut pos, flush),
                    _ => self.run_slow(input, &mut pos, flush),
                },
            };
            if finish || pos >= input.len() || pos == before {
                break;
            }
        }
        debug_assert_eq!(pos, input.len(), "encoder must consume all input");

        if finish {
            self.finished = true;
            return;
        }

        match flush {
            Flush::Partial => trees::align_block(&mut self.sink),
            Flush::Sync | Flush::Full => {
                trees::stored_block(&mut self.sink, &[], false);
                if flush == Flush::Full {
                    self.win.clear_hash();
                    if self.win.lookahead == 0 {
                        self.win.strstart = 0;
                        self.block_start = 0;
                        self.win.insert = 0;
                    }
                }
            }
            _ => {}
        }
    }

    /// Refill the window from `input`, keeping `block_start` aligned with any
    /// window slide.
    fn fill_from(&mut self, input: &[u8], pos: &mut usize) {
        let mut slid = 0usize;
        let taken = self
            .win
            .fill(input.get(*pos..).unwrap_or_default(), &mut slid);
        *pos += taken;
        self.block_start -= slid as i64;
    }

    #[inline(always)]
    fn tally_lit(&mut self, lit: u8) -> bool {
        self.syms.push(Sym { dist: 0, lc: lit });
        self.trees.tally_lit(lit);
        self.syms.len() == SYM_END
    }

    #[inline(always)]
    fn tally_dist(&mut self, dist: usize, lc: usize) -> bool {
        self.syms.push(Sym {
            dist: dist as u16,
            lc: lc as u8,
        });
        self.trees.tally_dist(dist, lc);
        self.syms.len() == SYM_END
    }

    /// zlib `FLUSH_BLOCK_ONLY`: emit the current block and start a new one.
    fn emit_block(&mut self, last: bool) {
        let stored_len = (self.win.strstart as i64 - self.block_start).max(0) as usize;
        let buf = if self.block_start >= 0 {
            let start = self.block_start as usize;
            self.win.buf.get(start..start + stored_len)
        } else {
            None
        };
        trees::flush_block(
            &mut self.trees,
            &mut self.sink,
            &self.syms,
            trees::StoredView {
                buf,
                len: stored_len,
            },
            last,
            self.level,
            self.strategy,
        );
        self.block_start = self.win.strstart as i64;
        self.syms.clear();
    }

    /// Finish the block sequence for a flush or end of stream.
    ///
    /// Returns `true` when the final block has been written.
    fn finish_blocks(&mut self, flush: Flush) -> bool {
        self.win.insert = if self.win.strstart < MIN_MATCH - 1 {
            self.win.strstart
        } else {
            MIN_MATCH - 1
        };
        if flush == Flush::Finish {
            self.emit_block(true);
            return true;
        }
        if !self.syms.is_empty() {
            self.emit_block(false);
        }
        false
    }

    /// zlib `deflate_fast`: greedy matching for levels 1-3.
    fn run_fast(&mut self, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
        let good = usize::from(self.cfg.good_length);
        let nice = usize::from(self.cfg.nice_length);
        let chain = usize::from(self.cfg.max_chain);
        let max_insert = usize::from(self.cfg.max_lazy);

        loop {
            if self.win.lookahead < MIN_LOOKAHEAD {
                self.fill_from(input, pos);
                if self.win.lookahead < MIN_LOOKAHEAD && flush == Flush::None {
                    return false;
                }
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

            let bflush;
            if self.match_length >= MIN_MATCH {
                let dist = self.win.strstart - self.win.match_start;
                bflush = self.tally_dist(dist, self.match_length - MIN_MATCH);
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
            } else {
                let lit = self.win.buf[self.win.strstart];
                bflush = self.tally_lit(lit);
                self.win.lookahead -= 1;
                self.win.strstart += 1;
            }
            if bflush {
                self.emit_block(false);
            }
        }
        self.finish_blocks(flush)
    }

    /// zlib `deflate_slow`: lazy matching for levels 4-9.
    fn run_slow(&mut self, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
        let good = usize::from(self.cfg.good_length);
        let nice = usize::from(self.cfg.nice_length);
        let chain = usize::from(self.cfg.max_chain);
        let max_lazy = usize::from(self.cfg.max_lazy);
        let filtered = self.strategy == Strategy::Filtered;

        loop {
            if self.win.lookahead < MIN_LOOKAHEAD {
                self.fill_from(input, pos);
                if self.win.lookahead < MIN_LOOKAHEAD && flush == Flush::None {
                    return false;
                }
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
                if self.match_length <= 5
                    && (filtered
                        || (self.match_length == MIN_MATCH
                            && self.win.strstart - self.win.match_start > TOO_FAR))
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }

            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                let max_insert = self.win.strstart + self.win.lookahead - MIN_MATCH;
                let dist = self.win.strstart - 1 - self.prev_match;
                let bflush = self.tally_dist(dist, self.prev_length - MIN_MATCH);

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
                if bflush {
                    self.emit_block(false);
                }
            } else if self.match_available {
                let lit = self.win.buf[self.win.strstart - 1];
                let bflush = self.tally_lit(lit);
                if bflush {
                    self.emit_block(false);
                }
                self.win.strstart += 1;
                self.win.lookahead -= 1;
            } else {
                self.match_available = true;
                self.win.strstart += 1;
                self.win.lookahead -= 1;
            }
        }

        if self.match_available {
            let lit = self.win.buf[self.win.strstart - 1];
            self.tally_lit(lit);
            self.match_available = false;
        }
        self.finish_blocks(flush)
    }

    /// zlib `deflate_rle`: match only against the previous byte.
    fn run_rle(&mut self, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
        loop {
            if self.win.lookahead <= MAX_MATCH {
                self.fill_from(input, pos);
                if self.win.lookahead <= MAX_MATCH && flush == Flush::None {
                    return false;
                }
                if self.win.lookahead == 0 {
                    break;
                }
            }
            self.match_length = self.win.rle_match_length();

            let bflush;
            if self.match_length >= MIN_MATCH {
                bflush = self.tally_dist(1, self.match_length - MIN_MATCH);
                self.win.lookahead -= self.match_length;
                self.win.strstart += self.match_length;
                self.match_length = 0;
            } else {
                let lit = self.win.buf[self.win.strstart];
                bflush = self.tally_lit(lit);
                self.win.lookahead -= 1;
                self.win.strstart += 1;
            }
            if bflush {
                self.emit_block(false);
            }
        }
        self.win.insert = 0;
        if flush == Flush::Finish {
            self.emit_block(true);
            return true;
        }
        if !self.syms.is_empty() {
            self.emit_block(false);
        }
        false
    }

    /// zlib `deflate_huff`: Huffman coding with no string matching.
    fn run_huff(&mut self, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
        loop {
            if self.win.lookahead == 0 {
                self.fill_from(input, pos);
                if self.win.lookahead == 0 {
                    if flush == Flush::None {
                        return false;
                    }
                    break;
                }
            }
            self.match_length = 0;
            let lit = self.win.buf[self.win.strstart];
            let bflush = self.tally_lit(lit);
            self.win.lookahead -= 1;
            self.win.strstart += 1;
            if bflush {
                self.emit_block(false);
            }
        }
        self.win.insert = 0;
        if flush == Flush::Finish {
            self.emit_block(true);
            return true;
        }
        if !self.syms.is_empty() {
            self.emit_block(false);
        }
        false
    }
}

/// Buffer lengths the encoder wants from [`crate::pool::DeflatePool`]:
/// `(window bytes, hash-head entries, hash-prev entries)`.
pub(crate) const fn pool_buffer_lengths() -> (usize, usize, usize) {
    (window::WINDOW_BUF_LEN, config::HASH_SIZE, config::W_SIZE)
}
