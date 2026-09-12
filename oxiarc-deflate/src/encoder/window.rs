//! Sliding window, hash chains and the longest-match search.
//!
//! A faithful port of zlib's `deflate.c` window machinery: a `2 * 32 KiB`
//! window with a 15-bit rolling hash over three bytes, `head`/`prev` chains
//! that are slid (not rebuilt) when the window moves, and the exact
//! `longest_match` early-reject sequence. Reproducing the chain-walk
//! termination rule (`cur_match > limit`, so the `NIL == 0` tail always ends
//! the walk) is what removes the old encoder's chain spin at window index 0.

use super::config::{
    HASH_MASK, HASH_SHIFT, HASH_SIZE, MAX_DIST, MIN_LOOKAHEAD, TOO_FAR, W_MASK, W_SIZE,
};
use super::tables::{MAX_MATCH, MIN_MATCH};

/// Extra slack past `2 * W_SIZE` so the match scan can read a whole 8-byte
/// group without a bounds check on the last comparison.
pub(crate) const WINDOW_PADDING: usize = 16;
/// Total window allocation.
pub(crate) const WINDOW_SIZE: usize = 2 * W_SIZE;
/// Length of the backing `Vec<u8>` (window plus scan slack).
pub(crate) const WINDOW_BUF_LEN: usize = WINDOW_SIZE + WINDOW_PADDING;
/// Bytes past the end of valid data that `longest_match` may read; zeroed by
/// [`Window::update_high_water`] so the encoder stays deterministic.
const WIN_INIT: usize = MAX_MATCH;

/// Read eight bytes as a little-endian `u64` regardless of host endianness.
#[inline(always)]
fn read_u64(buf: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(b)
}

/// The encoder's sliding window plus its hash chains.
#[derive(Debug)]
pub(crate) struct Window {
    /// `2 * W_SIZE + WINDOW_PADDING` bytes; only the first `WINDOW_SIZE` are
    /// addressable data, the rest is read-only scan slack.
    pub buf: Vec<u8>,
    /// Most recent position for each hash value (`0` = none).
    pub head: Vec<u16>,
    /// Previous position with the same hash, indexed by `pos & W_MASK`.
    pub prev: Vec<u16>,
    /// Rolling hash of the three bytes at `strstart`.
    pub ins_h: usize,
    /// Position of the next byte to process.
    pub strstart: usize,
    /// Valid bytes at and after `strstart`.
    pub lookahead: usize,
    /// Bytes not yet inserted into the hash chains.
    pub insert: usize,
    /// Highest window offset ever written (zlib's `high_water`).
    pub high_water: usize,
    /// Start of the match found by [`Window::longest_match`].
    pub match_start: usize,
}

impl Window {
    /// Allocate a zeroed window and empty hash chains.
    pub(crate) fn new() -> Self {
        Self {
            buf: vec![0; WINDOW_BUF_LEN],
            head: vec![0; HASH_SIZE],
            prev: vec![0; W_SIZE],
            ins_h: 0,
            strstart: 0,
            lookahead: 0,
            insert: 0,
            high_water: 0,
            match_start: 0,
        }
    }

    /// Reuse caller-supplied (pool) buffers. Buffers of the wrong size are
    /// resized; contents are always cleared.
    pub(crate) fn from_buffers(mut buf: Vec<u8>, mut head: Vec<u16>, mut prev: Vec<u16>) -> Self {
        buf.clear();
        buf.resize(WINDOW_BUF_LEN, 0);
        head.clear();
        head.resize(HASH_SIZE, 0);
        prev.clear();
        prev.resize(W_SIZE, 0);
        Self {
            buf,
            head,
            prev,
            ins_h: 0,
            strstart: 0,
            lookahead: 0,
            insert: 0,
            high_water: 0,
            match_start: 0,
        }
    }

    /// Give the buffers back for pooling.
    pub(crate) fn into_buffers(self) -> (Vec<u8>, Vec<u16>, Vec<u16>) {
        (self.buf, self.head, self.prev)
    }

    /// Forget all state (zlib `lm_init` + `CLEAR_HASH`).
    pub(crate) fn reset(&mut self) {
        self.head.fill(0);
        self.prev.fill(0);
        self.strstart = 0;
        self.lookahead = 0;
        self.insert = 0;
        self.ins_h = 0;
        self.match_start = 0;
        self.high_water = 0;
    }

    /// Drop the match history but keep the bytes already in the window
    /// (zlib `CLEAR_HASH`, used by `Z_FULL_FLUSH`).
    pub(crate) fn clear_hash(&mut self) {
        self.head.fill(0);
    }

    #[inline(always)]
    fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << HASH_SHIFT) ^ usize::from(c)) & HASH_MASK;
    }

    /// zlib `INSERT_STRING`: hash `window[str..str + 3]`, link it into the
    /// chain and return the previous head.
    #[inline(always)]
    pub(crate) fn insert_string(&mut self, str_pos: usize) -> usize {
        self.update_hash(self.buf[str_pos + MIN_MATCH - 1]);
        let head = self.head[self.ins_h];
        self.prev[str_pos & W_MASK] = head;
        self.head[self.ins_h] = str_pos as u16;
        head as usize
    }

    /// Undo the most recent [`Window::insert_string`] for hash bucket `h`,
    /// restoring the chain head it displaced.
    ///
    /// Used by the optimal parser, which must hash positions past the span end
    /// to decide where the span ends and then put the chains back exactly as
    /// they were, so the next span inserts those positions itself, in order.
    /// Undoing must happen in reverse insertion order; `prev[pos & W_MASK]` is
    /// left stale, which is harmless because nothing reaches it any more.
    #[inline]
    pub(crate) fn undo_insert(&mut self, h: usize, prev_head: u16) {
        self.head[h] = prev_head;
    }

    /// The hash bucket the last [`Window::insert_string`] used.
    #[inline]
    pub(crate) fn last_hash(&self) -> usize {
        self.ins_h
    }

    /// Restart the rolling hash at `strstart` (zlib does this after skipping
    /// the interior of a long match, and the optimal parser does it at the
    /// start of every span).
    #[inline]
    pub(crate) fn restart_hash(&mut self) {
        self.ins_h = usize::from(self.buf[self.strstart]);
        self.update_hash(self.buf[self.strstart + 1]);
    }

    /// Collect every *improving* match at `pos`, walking the hash chain from
    /// `head`. The results are appended to `out` in ascending length order
    /// (and therefore ascending distance among ties), so the first entry whose
    /// length reaches a target is also the nearest one that can.
    ///
    /// Unlike [`Window::longest_match`] this keeps the whole candidate set,
    /// which is what feeds the dynamic-programming parser: a DP fed only the
    /// single longest match per position starves and loses to lazy matching.
    /// zlib's `TOO_FAR` rule still applies, so the set never contains a
    /// length-3 match beyond 4096 bytes.
    pub(crate) fn match_candidates(
        &self,
        pos: usize,
        head: usize,
        max_len: usize,
        max_chain: usize,
        out: &mut Vec<(u16, u16)>,
    ) {
        if max_len < MIN_MATCH || head == 0 {
            return;
        }
        let limit = pos.saturating_sub(MAX_DIST);
        if head <= limit || head >= pos {
            return;
        }
        let win = &self.buf;
        let mut best_len = MIN_MATCH - 1;
        let mut cur_match = head;
        let mut chain_length = max_chain;
        loop {
            let m = cur_match;
            debug_assert!(m < pos, "hash chain must never point forward");
            if m >= pos {
                break;
            }
            if win[m + best_len] == win[pos + best_len] && win[m] == win[pos] {
                let mut i = 0usize;
                while i < max_len {
                    let x = read_u64(win, pos + i);
                    let y = read_u64(win, m + i);
                    if x != y {
                        i += ((x ^ y).trailing_zeros() >> 3) as usize;
                        break;
                    }
                    i += 8;
                }
                let len = i.min(max_len);
                if len > best_len {
                    best_len = len;
                    // zlib's `TOO_FAR`: a length-3 match farther than 4096 is
                    // refused outright (`deflate.c` applies it to
                    // `longest_match`'s result). Offering it to the parser
                    // instead is what made an earlier optimal parser lose to
                    // lazy matching on noisy data: the rare long-distance code
                    // it buys makes every distance code more expensive.
                    if len > MIN_MATCH || pos - m <= TOO_FAR {
                        out.push((len as u16, (pos - m) as u16));
                    }
                    if len >= max_len {
                        break;
                    }
                }
            }
            cur_match = self.prev[cur_match & W_MASK] as usize;
            if cur_match <= limit {
                break;
            }
            chain_length = chain_length.saturating_sub(1);
            if chain_length == 0 {
                break;
            }
        }
    }

    /// zlib `slide_hash`: shift every chain entry down by one window.
    fn slide_hash(&mut self) {
        for m in self.head.iter_mut() {
            *m = if *m as usize >= W_SIZE {
                *m - W_SIZE as u16
            } else {
                0
            };
        }
        for m in self.prev.iter_mut() {
            *m = if *m as usize >= W_SIZE {
                *m - W_SIZE as u16
            } else {
                0
            };
        }
    }

    /// zlib `fill_window`: slide if needed, copy input in, and hash the bytes
    /// that a previous call could not finish.
    ///
    /// Returns the number of bytes consumed from `input`. `block_start` and
    /// `match_start` are adjusted by the caller-visible slide amount, reported
    /// through `slid`.
    pub(crate) fn fill(&mut self, input: &[u8], slid: &mut usize) -> usize {
        let mut consumed = 0usize;
        loop {
            let mut more = WINDOW_SIZE - self.lookahead - self.strstart;

            if self.strstart >= W_SIZE + MAX_DIST {
                let live = self.strstart + self.lookahead - W_SIZE;
                self.buf.copy_within(W_SIZE..W_SIZE + live, 0);
                self.match_start = self.match_start.saturating_sub(W_SIZE);
                self.strstart -= W_SIZE;
                *slid += W_SIZE;
                // `high_water` is deliberately NOT slid: zlib leaves it where
                // it is, so the bytes above `strstart + lookahead` keep the
                // slid-down copy of the old data instead of being re-zeroed.
                // Re-zeroing them here would change which matches the scan
                // finds past the end of the lookahead, and with it the output.
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += W_SIZE;
            }
            if consumed >= input.len() {
                break;
            }

            let n = more.min(input.len() - consumed);
            let dst = self.strstart + self.lookahead;
            self.buf[dst..dst + n].copy_from_slice(&input[consumed..consumed + n]);
            consumed += n;
            self.lookahead += n;

            if self.lookahead + self.insert >= MIN_MATCH {
                let mut str_pos = self.strstart - self.insert;
                self.ins_h = usize::from(self.buf[str_pos]);
                self.update_hash(self.buf[str_pos + 1]);
                while self.insert != 0 {
                    self.insert_string(str_pos);
                    str_pos += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }

            if self.lookahead >= MIN_LOOKAHEAD || consumed >= input.len() {
                break;
            }
        }

        self.update_high_water();
        consumed
    }

    /// Zero the `WIN_INIT` bytes past the end of the data so the match scan,
    /// which deliberately reads past `lookahead`, sees deterministic values.
    fn update_high_water(&mut self) {
        if self.high_water >= WINDOW_SIZE {
            return;
        }
        let curr = self.strstart + self.lookahead;
        if self.high_water < curr {
            let init = (WINDOW_SIZE - curr).min(WIN_INIT);
            self.buf[curr..curr + init].fill(0);
            self.high_water = curr + init;
        } else if self.high_water < curr + WIN_INIT {
            let init = (curr + WIN_INIT - self.high_water).min(WINDOW_SIZE - self.high_water);
            let hw = self.high_water;
            self.buf[hw..hw + init].fill(0);
            self.high_water += init;
        }
    }

    /// Record that `strstart` advanced past written data (used by the stored
    /// path, which writes the window directly).
    pub(crate) fn raise_high_water(&mut self) {
        if self.high_water < self.strstart {
            self.high_water = self.strstart;
        }
    }

    /// zlib `longest_match`: walk the hash chain from `cur_match` and return
    /// the length of the best match, setting [`Window::match_start`].
    ///
    /// `prev_length` is the length already secured by the lazy state (the
    /// search only reports something strictly longer), `good_match`,
    /// `nice_match` and `max_chain` come from the level configuration.
    pub(crate) fn longest_match(
        &mut self,
        cur_match_in: usize,
        prev_length: usize,
        good_match: usize,
        nice_match_in: usize,
        max_chain: usize,
    ) -> usize {
        let mut chain_length = max_chain;
        let scan = self.strstart;
        let mut best_len = prev_length;
        let mut cur_match = cur_match_in;

        let limit = self.strstart.saturating_sub(MAX_DIST);

        if prev_length >= good_match {
            chain_length >>= 2;
        }
        let nice_match = nice_match_in.min(self.lookahead);

        let win = &self.buf;
        let mut scan_end1 = win[scan + best_len - 1];
        let mut scan_end = win[scan + best_len];
        let mut match_start = self.match_start;

        loop {
            let m = cur_match;
            if win[m + best_len] == scan_end
                && win[m + best_len - 1] == scan_end1
                && win[m] == win[scan]
                && win[m + 1] == win[scan + 1]
            {
                // The first two bytes match; byte 2 is implied by the hash
                // (equal 15-bit rolling hash with equal bytes 0 and 1 forces
                // byte 2 to be equal as well), exactly as zlib assumes.
                // `MAX_MATCH - 2 == 256` is a multiple of 8, so the whole
                // span is covered by aligned 8-byte groups with no remainder.
                let mut i = 2usize;
                while i < MAX_MATCH {
                    let x = read_u64(win, scan + i);
                    let y = read_u64(win, m + i);
                    if x != y {
                        i += ((x ^ y).trailing_zeros() >> 3) as usize;
                        break;
                    }
                    i += 8;
                }
                let len = i.min(MAX_MATCH);
                if len > best_len {
                    match_start = cur_match;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = win[scan + best_len - 1];
                    scan_end = win[scan + best_len];
                }
            }
            cur_match = self.prev[cur_match & W_MASK] as usize;
            if cur_match <= limit {
                break;
            }
            chain_length = chain_length.saturating_sub(1);
            if chain_length == 0 {
                break;
            }
        }

        self.match_start = match_start;
        if best_len <= self.lookahead {
            best_len
        } else {
            self.lookahead
        }
    }

    /// The run length at `strstart` when matching against `strstart - 1`
    /// (zlib's `deflate_rle` scan), capped at `MAX_MATCH` and `lookahead`.
    pub(crate) fn rle_match_length(&self) -> usize {
        if self.lookahead < MIN_MATCH || self.strstart == 0 {
            return 0;
        }
        let prev = self.buf[self.strstart - 1];
        let win = &self.buf;
        if win[self.strstart] != prev
            || win[self.strstart + 1] != prev
            || win[self.strstart + 2] != prev
        {
            return 0;
        }
        let mut i = 3usize;
        while i < MAX_MATCH && win[self.strstart + i] == prev {
            i += 1;
        }
        i.min(self.lookahead)
    }
}
