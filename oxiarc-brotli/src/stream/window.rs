//! The LZ77 sliding window used by the incremental decoder.
//!
//! The one-shot decoder in [`crate::decompress()`] resolves backward references
//! against the output `Vec` itself, which forces the whole decompressed body to
//! stay resident. A push decoder cannot do that: it hands every byte to the
//! caller and must still be able to look `window_size` bytes back. This module
//! provides that store as a power-of-two ring.
//!
//! Three properties matter and are all tested below:
//!
//! * **Bounded.** Memory is `min(1 << WBITS, grown-to-need)` bytes, never a
//!   function of the stream length.
//! * **Write-once.** The ring is *not* on the decoder's byte-producing path.
//!   Literals and matches go straight to the caller's slice, and
//!   `command::copy_into_pending` resolves a match's *source* across the
//!   boundary between this ring and the bytes produced so far in the current
//!   call; the ring is then caught up with one bulk [`BrotliWindow::push_slice`]
//!   per call. That is what keeps a bounded decoder from writing every produced
//!   byte twice.
//! * **Tail-only where a tail suffices.** A stored meta-block longer than the
//!   declared window mirrors only its last `1 << WBITS` bytes
//!   ([`BrotliWindow::push_slice_tail`]); everything before that is out of
//!   reach of every legal distance the moment the meta-block ends.

/// Smallest ring allocation. Brotli streams routinely declare `lgwin = 22`
/// (a 4 MiB window) for a payload of a few dozen bytes, so the ring starts
/// small and doubles on demand instead of allocating the declared size up
/// front.
const MIN_RING_CAPACITY: usize = 4096;

/// Runs no longer than this are moved byte at a time.
///
/// A bulk copy is a `memmove` call, and at these lengths the call costs more
/// than the bytes: real Brotli streams are dominated by literal runs and copies
/// of 2-24 bytes (RFC 7932's copy-length alphabet starts at 2), and
/// `_platform_memmove` was 12.7 % of the push decoder's profile before this
/// threshold existed — the calls, not the bytes.
pub(crate) const SHORT_MATCH: usize = 32;

/// A Brotli LZ77 sliding window.
///
/// Invariants:
///
/// * `capacity` is a power of two and `mask == capacity - 1`;
/// * `capacity <= target` (the declared `1 << WBITS`);
/// * every write is preceded by a [`BrotliWindow::reserve`] for exactly the
///   bytes it will append, so while `capacity < target` the ring has never
///   evicted anything: its bytes are `buf[..filled]` and `pos == filled`.
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
    /// Total bytes the ring is already known to be asked for — the current
    /// meta-block's `MLEN` added to what it holds. Sizing from this turns the
    /// growth of a large meta-block into one allocation instead of a
    /// doubling schedule, and stops a 1 MiB body from claiming a stream's
    /// declared 4 MiB window. Zero means "no announcement".
    expected: usize,
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
            expected: 0,
        }
    }

    /// Announce that the ring is about to be asked to hold `total` bytes in
    /// all (what it holds now plus the current meta-block's `MLEN`).
    ///
    /// A meta-block declares its exact length before it produces a byte, so
    /// the ring can be sized once, from the truth, instead of doubling its way
    /// there — and a body far smaller than the stream's declared window never
    /// pays for that window at all.
    pub(crate) fn expect_total(&mut self, total: usize) {
        self.expected = self.expected.max(total).min(self.target);
    }

    /// Bytes currently allocated for the ring.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of valid history bytes the ring currently holds.
    ///
    /// This is `min(1 << WBITS, bytes produced so far)` once the ring has
    /// reached its declared size, which is exactly the reach the one-shot
    /// decoder has into its output `Vec` — the two decoders therefore accept
    /// and reject the same backward distances.
    ///
    /// Only the tests read it: every distance the command loop resolves is
    /// already bounded by `min(window_size, bytes produced)`, so nothing on the
    /// decoding path has to ask the ring how much it holds.
    #[cfg(test)]
    pub(crate) fn filled(&self) -> usize {
        self.filled
    }

    /// Grow so that `extra` more bytes can be written without evicting any
    /// byte that is still reachable, up to the declared window size.
    ///
    /// The size chosen is the larger of what this write needs and what
    /// [`BrotliWindow::expect_total`] announced, so a meta-block of known
    /// length is allocated for once rather than doubled into.
    fn reserve(&mut self, extra: usize) {
        if self.capacity == self.target {
            return;
        }
        let needed = self
            .filled
            .saturating_add(extra)
            .max(self.expected)
            .min(self.target);
        if needed <= self.capacity {
            return;
        }
        self.regrow(needed.next_power_of_two().min(self.target));
    }

    /// Grow straight to the declared window.
    ///
    /// Used when the ring is *certain* to end up full — a meta-block longer
    /// than the window — so that the intermediate sizes are skipped and
    /// [`BrotliWindow::filled`] reaches the declared reach.
    fn grow_to_target(&mut self) {
        if self.capacity < self.target {
            self.regrow(self.target);
        }
    }

    /// Move the ring into a `new_capacity`-byte allocation, preserving both
    /// its contents and their order.
    ///
    /// A fresh zeroed allocation rather than `Vec::resize`: at the sizes that
    /// matter the allocator hands back lazily-zeroed pages, so the ring costs
    /// no `memset` at all, while `resize` writes zeros across the whole new
    /// tail before a single decoded byte lands in it — measured at 31.5 us for
    /// the 4 KiB → 4 MiB step a `lgwin = 22` stream used to take, against a
    /// 16.2 us *total* for one-shot-decoding a 1 MiB stored meta-block.
    fn regrow(&mut self, new_capacity: usize) {
        debug_assert!(new_capacity > self.capacity);
        let mut grown = vec![0u8; new_capacity];
        if self.filled == self.capacity {
            // Exactly full: the bytes run `pos..capacity` then `..pos`.
            let head = self.capacity - self.pos;
            grown[..head].copy_from_slice(&self.buf[self.pos..]);
            grown[head..self.capacity].copy_from_slice(&self.buf[..self.pos]);
        } else {
            debug_assert_eq!(self.pos, self.filled, "ring grew after wrapping");
            grown[..self.filled].copy_from_slice(&self.buf[..self.filled]);
        }
        self.buf = grown;
        self.pos = self.filled;
        self.capacity = new_capacity;
        self.mask = new_capacity - 1;
    }

    /// Advance the write cursor by `n` bytes that were just written.
    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n) & self.mask;
        self.filled = (self.filled + n).min(self.capacity);
    }

    /// Append `src` verbatim (a run of literals, uncompressed meta-block bytes,
    /// a transformed dictionary word), in at most two bulk copies.
    ///
    /// Short runs take a byte loop instead. A literal run is a handful of bytes
    /// on most real data, and one `memmove` call per run was 13 % of the push
    /// decoder's profile — the calls, not the bytes.
    #[inline]
    pub(crate) fn push_slice(&mut self, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        self.reserve(src.len());
        if src.len() <= SHORT_MATCH {
            let mask = self.mask;
            let mut pos = self.pos;
            for &byte in src {
                self.buf[pos] = byte;
                pos = (pos + 1) & mask;
            }
            self.pos = pos;
            self.filled = (self.filled + src.len()).min(self.capacity);
            return;
        }
        self.push_slice_bulk(src);
    }

    /// The bulk half of [`BrotliWindow::push_slice`], kept out of line so the
    /// short-run path above inlines into the command loop.
    #[inline(never)]
    fn push_slice_bulk(&mut self, src: &[u8]) {
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

    /// Append only the part of `src` that can still be reached once `future`
    /// more bytes have been appended after it.
    ///
    /// A window needs its tail and nothing else: a byte with more than
    /// `1 << WBITS` bytes behind it is out of reach of every legal distance,
    /// so mirroring it is pure memory traffic. This is what a stored
    /// (uncompressed) meta-block longer than the declared window exploits —
    /// its bytes go to the caller directly and only the last window's worth is
    /// ever written to the ring.
    ///
    /// Callers must pass the *exact* number of bytes still to come inside the
    /// run being mirrored; the last `min(run, 1 << WBITS)` bytes of that run
    /// then reach the ring, in order, which is precisely the history a decoder
    /// may address afterwards.
    pub(crate) fn push_slice_tail(&mut self, src: &[u8], future: usize) {
        let room = self.target.saturating_sub(future);
        if room >= src.len() {
            self.push_slice(src);
            return;
        }
        // More bytes follow than the window can hold, so the ring is certain
        // to end up exactly full; grow to the declared size in one step so
        // that `filled` really does reach the reach the distances assume.
        self.grow_to_target();
        if room > 0 {
            self.push_slice(&src[src.len() - room..]);
        }
    }

    /// Copy `dst.len()` bytes out of the ring, starting `distance` bytes back
    /// from the write cursor, writing nothing.
    ///
    /// This is the read half a decoder needs when the *destination* of a match
    /// is the caller's buffer rather than the ring — see
    /// `command::copy_into_pending`. The caller must have validated
    /// `dst.len() <= distance <= filled()`, so the read never runs past the
    /// write cursor into bytes that do not exist yet.
    pub(crate) fn read_back(&self, distance: usize, dst: &mut [u8]) {
        debug_assert!(dst.len() <= distance && distance <= self.filled);
        if dst.is_empty() {
            return;
        }
        let start = (self.pos + self.capacity - distance) & self.mask;
        let first = (self.capacity - start).min(dst.len());
        dst[..first].copy_from_slice(&self.buf[start..start + first]);
        if first < dst.len() {
            let rest = dst.len() - first;
            dst[first..].copy_from_slice(&self.buf[..rest]);
        }
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
    fn a_stored_run_longer_than_the_window_keeps_exactly_its_tail() {
        // What `push_slice_tail` is for: a stored meta-block bigger than the
        // declared window goes to the caller in full, but only its last
        // window's worth is ever mirrored — and the ring must then hold
        // exactly the history a decoder may address.
        for target_bits in [12u32, 14] {
            let target = 1usize << target_bits;
            for mlen in [target / 2, target, target + 1, target * 3 + 7] {
                let data: Vec<u8> = (0..mlen as u32)
                    .map(|i| (i.wrapping_mul(31)) as u8)
                    .collect();
                let mut w = BrotliWindow::with_target(target);
                w.expect_total(mlen);
                let mut done = 0usize;
                while done < mlen {
                    let n = 333.min(mlen - done);
                    w.push_slice_tail(&data[done..done + n], mlen - done - n);
                    done += n;
                }
                let kept = mlen.min(target);
                assert_eq!(w.filled(), kept, "target {target} mlen {mlen}");
                for d in 1..=kept {
                    assert_eq!(
                        w.back(d),
                        data[data.len() - d],
                        "target {target} mlen {mlen} distance {d}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_announced_meta_block_is_allocated_once_and_no_larger() {
        // A 1 MiB body on a stream that declares a 4 MiB window gets a 1 MiB
        // ring: the announcement, not the declaration, sizes the allocation.
        let mut w = BrotliWindow::with_target(1 << 22);
        w.expect_total(1 << 20);
        w.push_slice(&vec![9u8; 1 << 16]);
        assert_eq!(w.capacity(), 1 << 20);
        w.push_slice(&vec![9u8; (1 << 20) - (1 << 16)]);
        assert_eq!(w.capacity(), 1 << 20, "no growth beyond the announcement");
        // Going past it still works, and still never exceeds the declaration.
        w.push_slice(&vec![3u8; 1 << 20]);
        assert!(w.capacity() > 1 << 20 && w.capacity() <= 1 << 22);
        assert_eq!(w.back(1), 3);
    }

    #[test]
    fn growth_preserves_history_across_a_wrap() {
        // `regrow` must handle the exactly-full ring, whose bytes are split
        // around `pos`; a plain prefix copy would silently rotate history.
        let mut w = BrotliWindow::with_target(1 << 14);
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        w.push_slice(&data[..4096]);
        assert_eq!(w.capacity(), 4096);
        w.push_slice(&data[..100]);
        assert!(w.capacity() > 4096);
        for d in 1..=100 {
            assert_eq!(w.back(d), data[100 - d], "distance {d}");
        }
        for d in 101..=4196 {
            assert_eq!(w.back(d), data[4096 - (d - 100)], "distance {d}");
        }
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

    /// `read_back` is the ring's whole read interface now, so it gets its own
    /// reference test: reading `n` bytes from `distance` back must reproduce
    /// the same bytes a plain history `Vec` holds there, across the wrap and at
    /// every distance the ring can hold.
    #[test]
    fn read_back_agrees_with_a_plain_history() {
        let target = 1 << 12;
        let mut w = BrotliWindow::with_target(target);
        let mut history: Vec<u8> = Vec::new();
        let feed: Vec<u8> = (0..(target as u32 * 2 + 777))
            .map(|i| (i.wrapping_mul(97) % 251) as u8)
            .collect();
        let mut fed = 0usize;
        while fed < feed.len() {
            let n = 251.min(feed.len() - fed);
            w.push_slice(&feed[fed..fed + n]);
            history.extend_from_slice(&feed[fed..fed + n]);
            fed += n;
        }
        assert_eq!(w.filled(), target);
        for distance in [1usize, 2, 63, 64, 255, 1000, target - 1, target] {
            for len in [1usize, 2, 31, 32, 33] {
                if len > distance {
                    continue;
                }
                let mut got = vec![0u8; len];
                w.read_back(distance, &mut got);
                let start = history.len() - distance;
                assert_eq!(
                    got,
                    &history[start..start + len],
                    "distance {distance} len {len}"
                );
            }
        }
    }
}
