//! The LZ77 sliding window used by the incremental decoder.
//!
//! The one-shot decoder in [`crate::decompress`] resolves backward references
//! against the output `Vec` itself, which forces the whole decompressed body to
//! stay resident. A push decoder cannot do that: it hands every byte to the
//! caller and must still be able to look `window_size` bytes back. This module
//! provides that store as a power-of-two ring.
//!
//! Two properties matter and are both tested below:
//!
//! * **Bounded.** Memory is `min(1 << WBITS, grown-to-need)` bytes, never a
//!   function of the stream length.
//! * **Bulk.** Matches and dictionary words are moved with `copy_within` /
//!   `copy_from_slice` in runs, never byte at a time. Overlapping matches
//!   (`distance < length`, the LZ77 repeat idiom) are handled by capping each
//!   run at `distance`, which keeps every individual run non-overlapping while
//!   reproducing the byte-at-a-time semantics exactly.

/// Smallest ring allocation. Brotli streams routinely declare `lgwin = 22`
/// (a 4 MiB window) for a payload of a few dozen bytes, so the ring starts
/// small and doubles on demand instead of allocating the declared size up
/// front.
const MIN_RING_CAPACITY: usize = 4096;

/// Backward distances below this get the periodic-tiling path in
/// [`BrotliWindow::copy_match`]. Above it, one run of `distance` bytes is
/// already a worthwhile bulk copy.
const SMALL_DISTANCE: usize = 64;

/// Target size of the tiled pattern block. Rounded down to a whole number of
/// periods, so `TILE / distance` copies replace `TILE` byte-at-a-time steps.
const TILE: usize = 256;

/// Once a stream has proved it needs more than this, the ring jumps straight
/// to the declared window instead of doubling again.
///
/// Doubling is what keeps a 40-byte stream that declares `lgwin = 22` from
/// allocating 4 MiB; past this point the stream has demonstrably earned its
/// declared window, and further doubling only buys repeated reallocation and
/// page-fault churn on the copy.
const GROW_TO_TARGET_ABOVE: usize = 64 * 1024;

/// A Brotli LZ77 sliding window.
///
/// Invariants:
///
/// * `capacity` is a power of two and `mask == capacity - 1`;
/// * `capacity <= target` (the declared `1 << WBITS`);
/// * while `filled < capacity` the ring has never wrapped, so its bytes are
///   exactly `buf[..filled]` and `pos == filled` — which is what makes growth
///   a plain `resize`;
/// * `filled == capacity` implies `capacity == target`, so the ring only ever
///   evicts bytes once it has reached the declared window size.
#[derive(Debug)]
pub(crate) struct BrotliWindow {
    buf: Vec<u8>,
    /// Allocated size; a power of two.
    capacity: usize,
    /// `capacity - 1`.
    mask: usize,
    /// Write cursor, always in `0..capacity`.
    pos: usize,
    /// Number of valid bytes held, capped at `capacity`.
    filled: usize,
    /// The largest capacity this ring may grow to (`1 << WBITS`).
    target: usize,
}

impl BrotliWindow {
    /// Create a window that may grow to `target` bytes (`1 << WBITS`).
    ///
    /// `target` must be a power of two; the allocation starts at
    /// `min(target, 4096)`.
    pub(crate) fn with_target(target: usize) -> Self {
        let target = target.max(1).next_power_of_two();
        let capacity = MIN_RING_CAPACITY.min(target);
        BrotliWindow {
            buf: vec![0u8; capacity],
            capacity,
            mask: capacity - 1,
            pos: 0,
            filled: 0,
            target,
        }
    }

    /// Bytes currently allocated for the ring.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Grow so that `extra` more bytes can be written without evicting any
    /// byte that is still reachable, up to the declared window size.
    ///
    /// The `+ 1` keeps `filled < capacity` strictly true for every ring that
    /// has not yet reached `target`, which is what preserves the
    /// "never wrapped ⇒ `pos == filled`" invariant that makes growth a plain
    /// `resize`.
    fn reserve(&mut self, extra: usize) {
        if self.capacity == self.target {
            return;
        }
        let needed = self
            .filled
            .saturating_add(extra)
            .saturating_add(1)
            .min(self.target);
        if needed <= self.capacity {
            return;
        }
        let new_capacity = if needed > GROW_TO_TARGET_ABOVE {
            self.target
        } else {
            let mut grown = self.capacity;
            while grown < needed {
                grown = grown.saturating_mul(2);
            }
            grown.min(self.target)
        };
        self.buf.resize(new_capacity, 0);
        self.capacity = new_capacity;
        self.mask = new_capacity - 1;
        // Never wrapped, so `pos == filled` still addresses the write cursor.
    }

    /// Advance the write cursor by `n` bytes that were just written.
    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n) & self.mask;
        self.filled = (self.filled + n).min(self.capacity);
    }

    /// Append one byte.
    pub(crate) fn push(&mut self, byte: u8) {
        self.reserve(1);
        self.buf[self.pos] = byte;
        self.advance(1);
    }

    /// Append `src` verbatim (uncompressed meta-block bytes and transformed
    /// dictionary words), in at most two bulk copies.
    pub(crate) fn push_slice(&mut self, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        self.reserve(src.len());
        if src.len() >= self.capacity {
            // Only the last `capacity` bytes survive.
            let tail = &src[src.len() - self.capacity..];
            self.buf.copy_from_slice(tail);
            self.pos = 0;
            self.filled = self.capacity;
            return;
        }
        let first = (self.capacity - self.pos).min(src.len());
        self.buf[self.pos..self.pos + first].copy_from_slice(&src[..first]);
        if first < src.len() {
            let rest = src.len() - first;
            self.buf[..rest].copy_from_slice(&src[first..]);
        }
        self.advance(src.len());
    }

    /// Copy an LZ77 match of `count` bytes from `distance` back, appending it
    /// to the ring and writing the same bytes into `out`.
    ///
    /// Returns the number of bytes produced, which is `min(count, out.len())`.
    /// The caller must have validated `1 <= distance <= filled`.
    pub(crate) fn copy_match(&mut self, distance: usize, count: usize, out: &mut [u8]) -> usize {
        let total = count.min(out.len());
        if total == 0 {
            return 0;
        }
        self.reserve(total);
        if distance < SMALL_DISTANCE && total > distance {
            return self.copy_periodic(distance, total, out);
        }
        let mut done = 0;
        while done < total {
            let dst = self.pos;
            let src = (self.pos + self.capacity - distance) & self.mask;
            let run = (total - done)
                .min(distance)
                .min(self.capacity - dst)
                .min(self.capacity - src);
            self.buf.copy_within(src..src + run, dst);
            out[done..done + run].copy_from_slice(&self.buf[dst..dst + run]);
            self.advance(run);
            done += run;
        }
        total
    }

    /// A contiguous, writable slice at the head of the ring, at most `want`
    /// bytes long.
    ///
    /// This is the ring's "linear region": bytes written here become the next
    /// output, so a decoder can produce literals straight into the window and
    /// then hand the caller one bulk copy, instead of writing every byte twice.
    /// The slice stops at the wrap point, so it may be shorter than `want`; the
    /// caller loops. Nothing is published until [`BrotliWindow::commit`].
    pub(crate) fn linear_mut(&mut self, want: usize) -> &mut [u8] {
        self.reserve(want);
        let end = (self.pos + want).min(self.capacity);
        &mut self.buf[self.pos..end]
    }

    /// Publish `n` bytes written through [`BrotliWindow::linear_mut`].
    pub(crate) fn commit(&mut self, n: usize) {
        self.advance(n);
    }

    /// The short-distance case of [`BrotliWindow::copy_match`].
    ///
    /// An LZ77 copy whose length exceeds its distance is *periodic*: the output
    /// is `pattern[i % distance]` where `pattern` is the `distance` bytes at
    /// the source. Materialising one period, tiling it into a block of whole
    /// periods and emitting the block in bulk replaces the byte-at-a-time
    /// overlap loop — which matters, because `distance == 1` runs (a repeated
    /// byte) are the single most common shape in real compressed data.
    ///
    /// `total > distance` and `distance < SMALL_DISTANCE` are the caller's
    /// preconditions; `reserve` has already run.
    fn copy_periodic(&mut self, distance: usize, total: usize, out: &mut [u8]) -> usize {
        // One period, read out of the ring (at most two copies, for the wrap).
        let mut pattern = [0u8; SMALL_DISTANCE];
        let src = (self.pos + self.capacity - distance) & self.mask;
        let first = (self.capacity - src).min(distance);
        pattern[..first].copy_from_slice(&self.buf[src..src + first]);
        if first < distance {
            pattern[first..distance].copy_from_slice(&self.buf[..distance - first]);
        }

        // Tile it into whole periods so every emitted chunk starts on a period
        // boundary and a prefix of the block is always the right continuation.
        let mut block = [0u8; TILE + SMALL_DISTANCE];
        let reps = (TILE / distance).max(1);
        let block_len = reps * distance;
        for rep in 0..reps {
            block[rep * distance..(rep + 1) * distance].copy_from_slice(&pattern[..distance]);
        }

        let mut done = 0;
        while done < total {
            let n = (total - done).min(block_len);
            out[done..done + n].copy_from_slice(&block[..n]);
            self.push_slice(&block[..n]);
            done += n;
        }
        total
    }

    /// The byte `distance` positions back from the write cursor.
    #[cfg(test)]
    pub(crate) fn back(&self, distance: usize) -> u8 {
        self.buf[(self.pos + self.capacity - distance) & self.mask]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference window: a plain growing `Vec`, i.e. what the one-shot
    /// decoder does. Every ring operation must agree with it.
    fn reference_copy(history: &mut Vec<u8>, distance: usize, count: usize) -> Vec<u8> {
        let start = history.len();
        for _ in 0..count {
            let byte = history[history.len() - distance];
            history.push(byte);
        }
        history[start..].to_vec()
    }

    #[test]
    fn grows_lazily_from_the_minimum() {
        let mut w = BrotliWindow::with_target(1 << 22);
        assert_eq!(w.capacity(), MIN_RING_CAPACITY);
        w.push_slice(&vec![7u8; MIN_RING_CAPACITY * 3]);
        assert!(w.capacity() >= MIN_RING_CAPACITY * 3);
        assert!(w.capacity() <= 1 << 22);
        assert_eq!(w.back(1), 7);
    }

    #[test]
    fn never_exceeds_the_declared_target() {
        let mut w = BrotliWindow::with_target(1 << 12);
        w.push_slice(&vec![1u8; 1 << 16]);
        assert_eq!(w.capacity(), 1 << 12);
        assert_eq!(w.back(1), 1);
    }

    #[test]
    fn non_overlapping_match_matches_reference() {
        let mut w = BrotliWindow::with_target(1 << 13);
        let mut history = Vec::new();
        let seed: Vec<u8> = (0..200u16).map(|i| (i % 251) as u8).collect();
        w.push_slice(&seed);
        history.extend_from_slice(&seed);

        let mut out = vec![0u8; 100];
        let n = w.copy_match(150, 100, &mut out);
        assert_eq!(n, 100);
        assert_eq!(out, reference_copy(&mut history, 150, 100));
    }

    #[test]
    fn overlapping_match_matches_reference() {
        let mut w = BrotliWindow::with_target(1 << 13);
        let mut history = Vec::new();
        w.push_slice(b"ab");
        history.extend_from_slice(b"ab");

        let mut out = vec![0u8; 9];
        let n = w.copy_match(2, 9, &mut out);
        assert_eq!(n, 9);
        assert_eq!(out, reference_copy(&mut history, 2, 9));
        assert_eq!(&out, b"ababababa");
    }

    #[test]
    fn distance_one_run_matches_reference() {
        let mut w = BrotliWindow::with_target(1 << 13);
        let mut history = vec![0xAA];
        w.push(0xAA);
        let mut out = vec![0u8; 300];
        assert_eq!(w.copy_match(1, 300, &mut out), 300);
        assert_eq!(out, reference_copy(&mut history, 1, 300));
    }

    #[test]
    fn match_across_the_wrap_matches_reference() {
        let target = 1 << 12;
        let mut w = BrotliWindow::with_target(target);
        let mut history = Vec::new();
        let seed: Vec<u8> = (0..target as u32 + 500).map(|i| (i % 253) as u8).collect();
        w.push_slice(&seed);
        history.extend_from_slice(&seed);

        // A distance that reaches back over the wrap point.
        let mut out = vec![0u8; 700];
        assert_eq!(w.copy_match(3000, 700, &mut out), 700);
        assert_eq!(out, reference_copy(&mut history, 3000, 700));
    }

    #[test]
    fn linear_region_writes_are_visible_as_history() {
        let mut w = BrotliWindow::with_target(1 << 12);
        let mut history = Vec::new();
        // Fill past the wrap so the linear region really does get truncated.
        for round in 0..40u32 {
            let want = 200usize;
            let mut written = 0;
            while written < want {
                let dst = w.linear_mut(want - written);
                let n = dst.len();
                for (i, slot) in dst.iter_mut().enumerate() {
                    *slot = (round as usize + i) as u8;
                }
                let produced: Vec<u8> = (0..n).map(|i| (round as usize + i) as u8).collect();
                history.extend_from_slice(&produced);
                w.commit(n);
                written += n;
            }
        }
        for d in 1..=(1usize << 12) {
            assert_eq!(w.back(d), history[history.len() - d], "distance {d}");
        }
    }

    #[test]
    fn periodic_path_agrees_with_the_reference_for_every_small_distance() {
        for distance in 1..SMALL_DISTANCE {
            for length in [distance, distance + 1, 100, 257, 1000] {
                let mut w = BrotliWindow::with_target(1 << 14);
                let mut history = Vec::new();
                let seed: Vec<u8> = (0..distance as u32)
                    .map(|i| (i.wrapping_mul(37) % 251) as u8)
                    .collect();
                w.push_slice(&seed);
                history.extend_from_slice(&seed);

                let mut out = vec![0u8; length];
                assert_eq!(w.copy_match(distance, length, &mut out), length);
                assert_eq!(
                    out,
                    reference_copy(&mut history, distance, length),
                    "distance {distance} length {length}"
                );
                // The ring must agree with the reference history too.
                for d in 1..=distance {
                    assert_eq!(
                        w.back(d),
                        history[history.len() - d],
                        "distance {distance} length {length}: ring byte at -{d}"
                    );
                }
            }
        }
    }

    #[test]
    fn periodic_path_resumes_across_short_output_slices() {
        // The tiling must stay period-aligned when the caller's slice cuts a
        // block in half.
        let mut w = BrotliWindow::with_target(1 << 14);
        let mut history = Vec::new();
        w.push_slice(b"abc");
        history.extend_from_slice(b"abc");
        let expected = reference_copy(&mut history, 3, 1000);

        let mut got = Vec::new();
        let mut remaining = 1000usize;
        let mut chunk = 1usize;
        while remaining > 0 {
            let mut out = vec![0u8; chunk.min(remaining)];
            let n = w.copy_match(3, remaining, &mut out);
            got.extend_from_slice(&out[..n]);
            remaining -= n;
            chunk = chunk * 2 + 1;
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn short_output_slice_produces_a_prefix() {
        let mut w = BrotliWindow::with_target(1 << 13);
        w.push_slice(b"0123456789");
        let mut out = vec![0u8; 3];
        assert_eq!(w.copy_match(10, 10, &mut out), 3);
        assert_eq!(&out, b"012");
        // The remaining 7 bytes are produced by a follow-up call, exactly as
        // the resumable command loop does it.
        let mut out2 = vec![0u8; 7];
        assert_eq!(w.copy_match(10, 7, &mut out2), 7);
        assert_eq!(&out2, b"3456789");
    }

    #[test]
    fn push_slice_larger_than_capacity_keeps_the_tail() {
        let mut w = BrotliWindow::with_target(1 << 12);
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 255) as u8).collect();
        w.push_slice(&data);
        for d in 1..=(1usize << 12) {
            assert_eq!(w.back(d), data[data.len() - d], "distance {d}");
        }
    }
}
