//! Bounded sliding window (ring buffer) for incremental Zstandard decoding.
//!
//! The one-shot decoder in [`crate::frame`] keeps the whole frame output in a
//! `Vec<u8>` and resolves every match against it, so its memory is
//! `O(decompressed size)` — unbounded for an attacker-supplied frame. The
//! incremental decoder ([`crate::stream::ZstdStream`]) instead keeps only the
//! most recent `Window_Size` bytes in this ring, which is exactly the history
//! RFC 8878 lets a match reach into.
//!
//! Three properties make the ring usable as the *only* buffer in the decode
//! path:
//!
//! * **Matches are executed in place.** [`ZstdWindow::copy_match`] resolves a
//!   back-reference with `copy_within` runs (never a per-byte `%` loop), so an
//!   overlapping RLE-style match still runs at memcpy speed.
//! * **Newly produced bytes stay addressable until drained.** The ring tracks a
//!   `pending` count — the suffix that the caller has not yet copied out — so a
//!   caller with a one-byte output slice can take a 128 KiB block one byte at a
//!   time without the decoder re-decoding anything.
//! * **Capacity grows lazily.** A frame may declare a 128 MiB window and then
//!   produce 100 bytes; the ring starts at the frame's maximum block size and
//!   doubles only when history is about to be evicted, so the allocation is
//!   bounded by `min(declared window, max(Block_Maximum_Decompressed_Size,
//!   dictionary + bytes actually produced))`. The block-maximum floor is
//!   inherent — a whole block has to fit before the caller drains it — so a
//!   frame that declares no `Frame_Content_Size` still starts at one block.

use oxiarc_core::error::{OxiArcError, Result};

/// A bounded LZ77 history ring.
#[derive(Debug)]
pub(crate) struct ZstdWindow {
    /// Backing storage; `buf.len()` is always the live capacity.
    buf: Vec<u8>,
    /// Index at which the next byte is written.
    pos: usize,
    /// Number of valid history bytes held (`<= buf.len()`).
    filled: usize,
    /// Suffix of the history not yet handed to the caller.
    pending: usize,
    /// Hard ceiling for [`Self::grow_for`]; the frame's addressable reach.
    cap_limit: usize,
}

impl ZstdWindow {
    /// Create an empty window with no allocation.
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            filled: 0,
            pending: 0,
            cap_limit: 1,
        }
    }

    /// Live capacity in bytes.
    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of history bytes currently held in the ring.
    #[cfg(test)]
    pub(crate) fn history_len(&self) -> usize {
        self.filled
    }

    /// How far back a match in the current frame may legitimately point.
    ///
    /// This is the history the ring holds, capped by the frame's own
    /// `cap_limit`. The cap matters because [`Self::begin_frame`] never shrinks
    /// an existing allocation: without it, a frame that follows a
    /// larger-windowed one in a concatenated stream could resolve matches
    /// further back than its own `Window_Size` allows, and the very same frame
    /// would then decode or fail depending on what preceded it.
    fn reach(&self) -> usize {
        self.filled.min(self.cap_limit)
    }

    /// Number of produced bytes not yet drained by the caller.
    pub(crate) fn pending(&self) -> usize {
        self.pending
    }

    /// Refuse to produce `more` bytes when they would not fit alongside the
    /// undrained ones.
    ///
    /// The decoder's contract is that a whole block fits: [`Self::begin_frame`]
    /// allocates at least `Block_Maximum_Decompressed_Size` and the caller
    /// drains the ring empty before the next block is decoded. If that ever
    /// stopped holding, the ring would overwrite bytes the caller has not seen
    /// — silent truncation. This turns the broken invariant into a loud error
    /// instead, in release builds as well as under `debug_assert`.
    fn reserve_pending(&self, more: usize) -> Result<()> {
        let total = self.pending.saturating_add(more);
        if total > self.buf.len() {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "zstd block would write {total} undrained bytes into a {}-byte window",
                    self.buf.len()
                ),
            ));
        }
        Ok(())
    }

    /// Clear all state but keep the allocation.
    ///
    /// Used by [`crate::ZstdStream::reset`]: a reset decoder is meant to be
    /// reused, and re-allocating a multi-megabyte ring on every reset would
    /// defeat the point.
    pub(crate) fn reset_keep_allocation(&mut self) {
        self.pos = 0;
        self.filled = 0;
        self.pending = 0;
        self.cap_limit = self.buf.len().max(1);
    }

    /// Start a new frame.
    ///
    /// Drops all history (a frame never references the previous frame's
    /// output) and sets the ceiling the ring may grow to. The existing
    /// allocation is retained, so decoding a long multi-frame stream does not
    /// reallocate per frame.
    ///
    /// `cap_limit` is the largest history the frame can address: at least the
    /// frame's `Block_Maximum_Decompressed_Size` (so one whole block always
    /// fits) and at most `min(Window_Size, dictionary + Frame_Content_Size)`.
    /// `initial` is the first allocation to make; the ring doubles from there
    /// only when history would otherwise be evicted.
    pub(crate) fn begin_frame(&mut self, cap_limit: usize, initial: usize) -> Result<()> {
        self.pos = 0;
        self.filled = 0;
        self.pending = 0;
        self.cap_limit = cap_limit.max(1);
        // Never shrink an existing allocation: a larger ring only retains more
        // history than the format guarantees, which is harmless.
        let want = initial.min(self.cap_limit).max(1);
        if self.buf.len() < want {
            self.reallocate(want)?;
        }
        Ok(())
    }

    /// Seed the window with the tail of a dictionary.
    ///
    /// Only the last `capacity()` bytes are reachable, matching the reference
    /// decoder: dictionary content further back than the window cannot be
    /// referenced.
    pub(crate) fn seed_dictionary(&mut self, dict: &[u8]) -> Result<()> {
        if dict.is_empty() {
            return Ok(());
        }
        self.grow_for(dict.len())?;
        let cap = self.buf.len();
        let tail = if dict.len() > cap {
            &dict[dict.len() - cap..]
        } else {
            dict
        };
        self.write_raw(tail);
        // Dictionary bytes are history, not output.
        self.pending = 0;
        Ok(())
    }

    /// Grow the ring so that `additional` more bytes can be written without
    /// evicting history, up to `cap_limit`.
    fn grow_for(&mut self, additional: usize) -> Result<()> {
        let needed = self.filled.saturating_add(additional);
        let cap = self.buf.len();
        if needed <= cap || cap >= self.cap_limit {
            return Ok(());
        }
        let mut new_cap = cap.max(1);
        while new_cap < needed && new_cap < self.cap_limit {
            new_cap = new_cap.saturating_mul(2);
        }
        let new_cap = new_cap.min(self.cap_limit).max(1);
        if new_cap <= cap {
            return Ok(());
        }
        self.reallocate(new_cap)
    }

    /// Re-linearise the history into a buffer of exactly `new_cap` bytes.
    fn reallocate(&mut self, new_cap: usize) -> Result<()> {
        let mut fresh: Vec<u8> = Vec::new();
        fresh
            .try_reserve_exact(new_cap)
            .map_err(|e| window_alloc_error(new_cap, &e))?;
        fresh.resize(new_cap, 0);
        let keep = self.filled.min(new_cap);
        let (a, b) = self.history_slices(keep);
        fresh[..a.len()].copy_from_slice(a);
        fresh[a.len()..a.len() + b.len()].copy_from_slice(b);
        self.filled = keep;
        self.pending = self.pending.min(keep);
        self.pos = keep % new_cap;
        self.buf = fresh;
        Ok(())
    }

    /// The two contiguous runs holding the most recent `n` history bytes,
    /// oldest run first.
    fn history_slices(&self, n: usize) -> (&[u8], &[u8]) {
        let cap = self.buf.len();
        let n = n.min(self.filled);
        if cap == 0 || n == 0 {
            return (&[], &[]);
        }
        let start = (self.pos + cap - n) % cap;
        if start + n <= cap {
            (&self.buf[start..start + n], &[])
        } else {
            let first = cap - start;
            (&self.buf[start..], &self.buf[..n - first])
        }
    }

    /// Write `data` into the ring, updating `pos`/`filled` but not `pending`.
    fn write_raw(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        debug_assert!(cap > 0);
        // More than one capacity of data: only the tail survives.
        let data = if data.len() > cap {
            &data[data.len() - cap..]
        } else {
            data
        };
        let first = (cap - self.pos).min(data.len());
        self.buf[self.pos..self.pos + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            let rest = data.len() - first;
            self.buf[..rest].copy_from_slice(&data[first..]);
        }
        self.pos = (self.pos + data.len()) % cap;
        self.filled = (self.filled + data.len()).min(cap);
    }

    /// Append literal bytes to the window as newly produced output.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.grow_for(data.len())?;
        self.reserve_pending(data.len())?;
        self.write_raw(data);
        self.pending += data.len();
        Ok(())
    }

    /// Append `len` copies of `byte` (RLE block / RLE literals).
    pub(crate) fn push_repeat(&mut self, byte: u8, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.grow_for(len)?;
        self.reserve_pending(len)?;
        let cap = self.buf.len();
        let mut written = 0usize;
        while written < len {
            let run = (cap - self.pos).min(len - written);
            self.buf[self.pos..self.pos + run].fill(byte);
            self.pos = (self.pos + run) % cap;
            written += run;
        }
        self.filled = (self.filled + len).min(cap);
        self.pending += len;
        Ok(())
    }

    /// Execute an LZ77 back-reference: append `len` bytes read from `offset`
    /// bytes before the current write position.
    ///
    /// Overlapping matches (`offset < len`) are handled by copying in runs of
    /// at most `offset` bytes, which reproduces the byte-by-byte semantics at
    /// `copy_within` speed.
    pub(crate) fn copy_match(&mut self, offset: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let reach = self.reach();
        if offset == 0 || offset > reach {
            return Err(OxiArcError::invalid_distance(offset, reach));
        }
        self.grow_for(len)?;
        self.reserve_pending(len)?;
        let cap = self.buf.len();
        let mut written = 0usize;
        while written < len {
            let src = (self.pos + cap - offset) % cap;
            let dst = self.pos;
            let run = (len - written)
                .min(cap - src)
                .min(cap - dst)
                .min(offset)
                .max(1);
            self.buf.copy_within(src..src + run, dst);
            self.pos = (self.pos + run) % cap;
            self.filled = (self.filled + run).min(cap);
            written += run;
        }
        self.pending += len;
        Ok(())
    }

    /// Copy up to `out.len()` pending bytes into `out`.
    ///
    /// `hash` receives exactly the bytes copied, in order, so a frame checksum
    /// can be computed without retaining the output.
    pub(crate) fn drain_into(&mut self, out: &mut [u8], mut hash: impl FnMut(&[u8])) -> usize {
        let n = out.len().min(self.pending);
        if n == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let start = (self.pos + cap - self.pending) % cap;
        let first = (cap - start).min(n);
        out[..first].copy_from_slice(&self.buf[start..start + first]);
        hash(&out[..first]);
        if first < n {
            let rest = n - first;
            out[first..n].copy_from_slice(&self.buf[..rest]);
            hash(&out[first..n]);
        }
        self.pending -= n;
        n
    }
}

/// Build the allocation-failure error for a window of `cap` bytes.
fn window_alloc_error(cap: usize, e: &std::collections::TryReserveError) -> OxiArcError {
    OxiArcError::corrupted(0, format!("failed to allocate {cap}-byte zstd window: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLK: usize = crate::MAX_BLOCK_SIZE;

    fn window(cap_limit: usize) -> ZstdWindow {
        let mut w = ZstdWindow::new();
        w.begin_frame(cap_limit, cap_limit.min(BLK))
            .expect("begin_frame");
        w
    }

    fn drained(w: &mut ZstdWindow) -> Vec<u8> {
        let mut out = vec![0u8; w.pending()];
        let n = w.drain_into(&mut out, |_| {});
        out.truncate(n);
        out
    }

    #[test]
    fn push_and_drain_roundtrip() {
        let mut w = window(1024);
        w.push(b"hello ").expect("push");
        w.push(b"world").expect("push");
        assert_eq!(drained(&mut w), b"hello world");
        assert_eq!(w.pending(), 0);
        assert_eq!(w.history_len(), 11);
    }

    #[test]
    fn drain_one_byte_at_a_time() {
        let mut w = window(1024);
        w.push(b"abcdef").expect("push");
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        while w.pending() > 0 {
            let n = w.drain_into(&mut one, |_| {});
            assert_eq!(n, 1);
            out.push(one[0]);
        }
        assert_eq!(out, b"abcdef");
    }

    #[test]
    fn overlapping_match_repeats() {
        let mut w = window(1024);
        w.push(b"ab").expect("push");
        assert_eq!(drained(&mut w), b"ab");
        // offset 2, length 7 -> "abababa"
        w.copy_match(2, 7).expect("copy");
        assert_eq!(drained(&mut w), b"abababa");
    }

    #[test]
    fn rle_match_offset_one() {
        let mut w = window(1024);
        w.push(b"Z").expect("push");
        assert_eq!(drained(&mut w), b"Z");
        w.copy_match(1, 5).expect("copy");
        assert_eq!(drained(&mut w), b"ZZZZZ");
    }

    #[test]
    fn match_wrapping_ring_boundary() {
        // Force wrap-around: capacity is fixed at 1 KiB, write more than that.
        let mut w = window(1024);
        let chunk = vec![7u8; 1024 - 3];
        w.push(&chunk).expect("push");
        let _ = drained(&mut w);
        w.push(b"abcdef").expect("push");
        assert_eq!(drained(&mut w), b"abcdef");
        // Reference "abcdef" back through the wrap point.
        w.copy_match(6, 6).expect("copy");
        assert_eq!(drained(&mut w), b"abcdef");
    }

    #[test]
    fn match_at_exactly_the_window_distance_on_a_wrapped_ring() {
        // The boundary the reference decoder allows and one byte more must not:
        // `offset == filled == capacity` reads the oldest byte still in the
        // ring, which sits exactly at the write cursor.
        let mut w = window(8);
        w.push(b"abcdefgh").expect("push");
        assert_eq!(drained(&mut w), b"abcdefgh");
        assert_eq!(w.history_len(), 8);
        assert_eq!(w.capacity(), 8);
        // Distance 8 == the whole window: repeats it verbatim.
        w.copy_match(8, 8).expect("copy");
        assert_eq!(drained(&mut w), b"abcdefgh");
        // One byte further back is gone.
        assert!(w.copy_match(9, 1).is_err());

        // Same boundary after the ring has wrapped an odd number of bytes.
        // The wrap is produced the way the decoder produces it — one drained
        // batch at a time — because undrained output may never exceed the ring.
        let mut w = window(8);
        w.push(b"01234567").expect("push");
        let _ = drained(&mut w);
        w.push(b"89ab").expect("push");
        let _ = drained(&mut w);
        assert_eq!(w.history_len(), 8);
        w.copy_match(8, 3).expect("copy");
        // The ring holds "56789ab" plus the oldest surviving byte "4".
        assert_eq!(drained(&mut w), b"456");
        assert!(w.copy_match(9, 1).is_err());
    }

    #[test]
    fn long_run_of_window_distance_matches_stays_consistent() {
        // A 1 KiB ring producing 64 KiB: the wrap path is exercised 64 times and
        // every byte must still match a plain reference implementation.
        let cap = 1024usize;
        let mut w = window(cap);
        let seed: Vec<u8> = (0..cap).map(|i| (i % 251) as u8).collect();
        w.push(&seed).expect("push");
        let mut expected = seed.clone();
        let _ = drained(&mut w);

        for round in 1..64usize {
            let offset = cap - (round % 7);
            let len = 300 + round;
            w.copy_match(offset, len).expect("copy");
            // Reference: read `len` bytes from `offset` back, one at a time.
            for _ in 0..len {
                let byte = expected[expected.len() - offset];
                expected.push(byte);
            }
            assert_eq!(
                drained(&mut w),
                expected[expected.len() - len..],
                "round {round}"
            );
        }
        assert_eq!(w.capacity(), cap, "the ring must not have grown");
    }

    #[test]
    fn a_leftover_allocation_does_not_widen_a_later_frames_reach() {
        // Frame 1 declares (and uses) a 4 KiB window, so the ring allocates
        // 4 KiB. `begin_frame` deliberately never shrinks that allocation.
        let mut w = ZstdWindow::new();
        w.begin_frame(4096, 4096).expect("begin");
        w.push(&vec![1u8; 4096]).expect("push");
        let _ = drained(&mut w);
        assert_eq!(w.capacity(), 4096);

        // Frame 2 declares a 1 KiB window. What a match may reach is a property
        // of *this* frame, not of how much memory the previous one happened to
        // leave behind: an offset past the declared window must be rejected
        // whatever preceded it in the stream.
        w.begin_frame(1024, 1024).expect("begin");
        w.push(&vec![2u8; 3000]).expect("push");
        let _ = drained(&mut w);
        w.copy_match(1024, 4)
            .expect("a match at exactly the window distance");
        let _ = drained(&mut w);
        assert!(
            w.copy_match(1025, 4).is_err(),
            "offset past the frame's declared window must be rejected"
        );
    }

    #[test]
    fn offset_beyond_history_is_rejected() {
        let mut w = window(1024);
        w.push(b"abc").expect("push");
        assert!(w.copy_match(4, 1).is_err());
        assert!(w.copy_match(0, 1).is_err());
    }

    #[test]
    fn dictionary_seed_is_history_not_output() {
        let mut w = window(1024);
        w.seed_dictionary(b"DICTIONARY").expect("seed");
        assert_eq!(w.pending(), 0);
        assert_eq!(w.history_len(), 10);
        w.copy_match(10, 4).expect("copy");
        assert_eq!(drained(&mut w), b"DICT");
    }

    #[test]
    fn dictionary_longer_than_capacity_keeps_tail() {
        let mut w = window(1024);
        let mut dict = vec![1u8; 1024];
        dict.extend_from_slice(b"TAIL");
        w.seed_dictionary(&dict).expect("seed");
        assert_eq!(w.history_len(), 1024);
        w.copy_match(4, 4).expect("copy");
        assert_eq!(drained(&mut w), b"TAIL");
    }

    /// The dictionary re-seeds at **every** frame, including one whose window
    /// is smaller than the dictionary, and including a frame that follows a
    /// larger-windowed one in a concatenated stream.
    ///
    /// `begin_frame` never shrinks an existing allocation, so the second frame
    /// runs with a physically larger `buf` than its own `cap_limit`. The reach
    /// cap is what keeps it honest: history the frame is not entitled to must
    /// stay unreachable even though the bytes are still in the ring.
    #[test]
    fn dictionary_reseeds_every_frame_and_reach_follows_the_frame() {
        let mut dict = vec![1u8; 4096];
        dict.extend_from_slice(b"TAIL");

        let mut w = ZstdWindow::new();
        // Frame 1: a large window, so the whole dictionary is addressable.
        w.begin_frame(8192, 8192).expect("begin");
        w.seed_dictionary(&dict).expect("seed");
        assert_eq!(w.history_len(), 4100);
        w.copy_match(4, 4).expect("copy");
        assert_eq!(drained(&mut w), b"TAIL");

        // Frame 2: a 1 KiB window. The allocation is kept (8192 bytes) but the
        // frame may only reach 1 KiB back, and the dictionary is re-seeded
        // truncated to that reach.
        w.begin_frame(1024, 1024).expect("begin");
        assert_eq!(w.capacity(), 8192, "the allocation is reused, not shrunk");
        w.seed_dictionary(&dict).expect("seed");
        assert_eq!(w.pending(), 0, "dictionary bytes are history, not output");
        w.copy_match(4, 4).expect("copy");
        assert_eq!(drained(&mut w), b"TAIL");
        // Exactly at the frame's own window: allowed. One byte more: refused,
        // even though the byte is physically still in the ring from frame 1.
        w.copy_match(1024, 1).expect("copy at the frame window");
        let _ = drained(&mut w);
        assert!(
            w.copy_match(1025, 1).is_err(),
            "a frame must not reach past its own Window_Size"
        );

        // Frame 3 with no dictionary at all: the previous frames' history is
        // gone even though the buffer still physically holds it.
        w.begin_frame(1024, 1024).expect("begin");
        assert!(w.copy_match(1, 1).is_err());
    }

    #[test]
    fn capacity_grows_lazily_not_to_declared_window() {
        // Declared reach of 64 MiB with a 4-byte initial hint.
        let mut w = ZstdWindow::new();
        w.begin_frame(64 * 1024 * 1024, 4).expect("begin");
        w.push(b"tiny").expect("push");
        assert_eq!(w.capacity(), 4);
    }

    #[test]
    fn capacity_grows_when_history_would_be_evicted() {
        let mut w = ZstdWindow::new();
        w.begin_frame(4096, 1024).expect("begin");
        let chunk = vec![3u8; 1024];
        w.push(&chunk).expect("push");
        let _ = drained(&mut w);
        assert_eq!(w.capacity(), 1024);
        w.push(&chunk).expect("push");
        let _ = drained(&mut w);
        assert!(w.capacity() >= 2048);
        assert_eq!(w.history_len(), 2048);
    }

    #[test]
    fn history_never_exceeds_cap_limit() {
        let mut w = window(1024);
        for _ in 0..4 {
            let chunk = vec![9u8; 1024];
            w.push(&chunk).expect("push");
            let _ = drained(&mut w);
        }
        assert_eq!(w.capacity(), 1024);
        assert_eq!(w.history_len(), 1024);
    }

    #[test]
    fn push_repeat_matches_push_of_repeated_slice() {
        let mut a = window(1024);
        a.push_repeat(0xAB, 300).expect("repeat");
        let mut b = window(1024);
        b.push(&vec![0xABu8; 300]).expect("push");
        assert_eq!(drained(&mut a), drained(&mut b));
    }

    #[test]
    fn push_repeat_wraps_correctly() {
        let mut w = window(64);
        w.push(&[0u8; 60]).expect("push");
        let _ = drained(&mut w);
        w.push_repeat(0xEE, 10).expect("repeat");
        assert_eq!(drained(&mut w), vec![0xEEu8; 10]);
    }

    #[test]
    fn drain_hashes_exactly_the_copied_bytes() {
        let mut w = window(1024);
        w.push(b"0123456789").expect("push");
        let mut seen = Vec::new();
        let mut out = [0u8; 4];
        while w.pending() > 0 {
            let n = w.drain_into(&mut out, |c| seen.extend_from_slice(c));
            assert!(n > 0);
        }
        assert_eq!(seen, b"0123456789");
    }

    #[test]
    fn begin_frame_clears_previous_frame_history() {
        let mut w = window(1024);
        w.push(b"first frame").expect("push");
        let _ = drained(&mut w);
        w.begin_frame(1024, 1024).expect("begin");
        assert_eq!(w.history_len(), 0);
        assert!(w.copy_match(1, 1).is_err());
    }

    #[test]
    fn long_overlapping_match_larger_than_the_history() {
        // A match far longer than the history it reads from, but still inside
        // the ring: LZ77 semantics hold for every byte.
        let mut w = window(512);
        w.push(b"xy").expect("push");
        let _ = drained(&mut w);
        w.copy_match(2, 300).expect("copy");
        let out = drained(&mut w);
        assert_eq!(out.len(), 300);
        let expected: Vec<u8> = (0..300)
            .map(|i| if i % 2 == 0 { b'x' } else { b'y' })
            .collect();
        assert_eq!(out, expected);
    }

    /// Producing more than the ring can hold before the caller drains it is an
    /// **error**, never a silent overwrite of bytes the caller has not seen.
    ///
    /// The decoder's own contract keeps this unreachable — `begin_frame`
    /// allocates at least `Block_Maximum_Decompressed_Size` and every block is
    /// drained before the next one is decoded — but the ring must not
    /// quietly clamp if that ever stops holding, because the clamped bytes are
    /// output the caller never receives.
    #[test]
    fn undrained_output_larger_than_the_ring_is_refused() {
        let mut w = window(64);
        let err = w.copy_match(1, 1).expect_err("no history yet");
        assert!(err.to_string().contains("distance"), "{err}");

        let mut w = window(64);
        w.push(&[b'a'; 64]).expect("push");
        // 64 undrained bytes already; one more must not evict them.
        let err = w.push(b"z").expect_err("must refuse");
        assert!(err.to_string().contains("undrained"), "{err}");
        assert_eq!(w.pending(), 64);

        let mut w = window(64);
        w.push(b"ab").expect("push");
        let _ = drained(&mut w);
        let err = w.copy_match(2, 300).expect_err("must refuse");
        assert!(err.to_string().contains("undrained"), "{err}");

        let mut w = window(64);
        let err = w.push_repeat(b'q', 65).expect_err("must refuse");
        assert!(err.to_string().contains("undrained"), "{err}");

        // Raw literal batches larger than the ring are refused the same way.
        let mut w = window(64);
        let err = w.push(&[b'k'; 65]).expect_err("must refuse");
        assert!(err.to_string().contains("undrained"), "{err}");
    }
}
