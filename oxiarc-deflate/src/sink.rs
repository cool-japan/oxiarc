//! Output sinks for the resumable DEFLATE decoder.
//!
//! The one structural difference from [`crate::window`]'s `DecodeSink` is
//! that an [`InflateSink`] can say **how much room is left**. That single
//! addition is what lets [`crate::stream::InflateStream`] stop mid-block and
//! resume later instead of running a block to completion into a growable
//! `Vec`.
//!
//! Two implementations cover every front end in the crate:
//!
//! * [`GrowSink`] wraps the existing [`InflateWindow`], reports
//!   `usize::MAX` of space, and therefore monomorphises the decoder's fast
//!   loop back into exactly the shape the one-shot `inflate()` path has
//!   today (the output half of the loop guard constant-folds away).
//! * [`BoundedSink`] writes into a caller-supplied slice and resolves
//!   back-references that reach behind it out of a [`History`] — a linear
//!   32 KiB window updated once per `inflate()` call rather than once per
//!   symbol.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::ringbuffer::MAX_COPY_LENGTH;

#[cfg(test)]
use crate::window::{DecodeSink, InflateWindow};

/// DEFLATE sliding-window size (RFC 1951 §3.2.1).
pub(crate) const WINDOW: usize = 32768;

/// Destination for decoded DEFLATE symbols that can refuse further writes.
///
/// Every method except [`InflateSink::space`] and
/// [`InflateSink::history_len`] is called only after the state machine has
/// checked that `space()` covers the write, so implementations may treat an
/// overflow as a programming error (they still report it rather than
/// panicking).
pub(crate) trait InflateSink {
    /// Bytes that may still be written before the sink is full.
    ///
    /// `usize::MAX` for a growable sink, which lets the decoder's fast-loop
    /// guard fold away entirely.
    fn space(&self) -> usize;

    /// Direct access to the writable slice, for the fast loop.
    ///
    /// A slice-backed sink hands out its buffer, its cursor and the history
    /// that precedes the buffer, so the fast loop can keep the cursor in a
    /// register and write literals and matches without a method call per
    /// symbol. `None` asks the caller to use the per-symbol methods
    /// instead.
    fn fast_region(&mut self) -> Option<FastRegion<'_>> {
        None
    }

    /// Publish a cursor the fast loop advanced.
    ///
    /// Only ever called with a value a [`InflateSink::fast_region`] loop
    /// produced from this sink's own buffer.
    fn commit(&mut self, _position: usize) {}

    /// Bytes of history a back-reference may address.
    fn history_len(&self) -> usize;

    /// Append one literal byte.
    fn write_literal(&mut self, byte: u8) -> Result<()>;

    /// Append several literal bytes.
    fn write_literals(&mut self, bytes: &[u8]) -> Result<()>;

    /// Copy `length` bytes from `distance` bytes back in the history.
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()>;

    /// Bytes written to this sink since it was created.
    fn written(&self) -> usize;
}

/// The writable slice of a slice-backed [`InflateSink`], its cursor, and
/// the history that precedes it.
///
/// `dst[..pos]` is already-decoded output a back-reference may address;
/// `history` is the window that precedes `dst[0]` (a preset dictionary, or
/// output already handed to the caller). Together they are exactly what
/// `copy_match` resolves distances against.
pub(crate) struct FastRegion<'a> {
    /// The whole writable buffer.
    pub(crate) dst: &'a mut [u8],
    /// Bytes of `dst` already written.
    pub(crate) pos: usize,
    /// History preceding `dst[0]`.
    pub(crate) history: &'a [u8],
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// The bytes preceding the caller's current output buffer that a
/// back-reference may still name.
///
/// Linear, not a ring: a ring forces the *inner* copy loop to handle
/// wrap-around on every match, which is precisely why `OutputRingBuffer` was
/// replaced by [`InflateWindow`] in the first place. The cost is one
/// bounded `copy_within` + `extend_from_slice` per `inflate()` call, not per
/// symbol.
#[derive(Debug, Default)]
pub(crate) struct History {
    buf: Vec<u8>,
    capacity: usize,
}

impl History {
    /// A history that retains at most `capacity` bytes (clamped to the
    /// 32 KiB window; zero is raised to one so a stream with any
    /// back-reference still has somewhere to look).
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity: capacity.clamp(1, WINDOW),
        }
    }

    /// Bytes currently retained.
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// The retained bytes, oldest first.
    #[inline(always)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Drop every retained byte.
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    /// Replace the history with the trailing window of `dictionary`.
    pub(crate) fn set_dictionary(&mut self, dictionary: &[u8]) {
        self.buf.clear();
        self.append(dictionary);
    }

    /// Roll freshly produced output into the history, keeping the newest
    /// `capacity` bytes.
    ///
    /// The oldest bytes are evicted **before** the new ones are appended, so
    /// `buf` never exceeds `capacity` even transiently. Appending first and
    /// truncating afterwards — the obvious order — lets the buffer reach
    /// `capacity + produced.len()`, which both reallocates in the steady state
    /// (the caller's output buffer size varies from call to call, so the
    /// high-water mark keeps moving) and holds up to three times the window in
    /// live memory. Measured on a 16 MiB gzip body fed in 4 KiB chunks through
    /// a 64 KiB output buffer: one `realloc` to 125 068 bytes inside the
    /// steady state, against a 32 KiB window.
    pub(crate) fn append(&mut self, produced: &[u8]) {
        if produced.is_empty() {
            return;
        }
        // One allocation for the lifetime of the history.
        if self.buf.capacity() < self.capacity {
            let additional = self.capacity - self.buf.len().min(self.capacity);
            self.buf.reserve_exact(additional);
        }
        if produced.len() >= self.capacity {
            let start = produced.len() - self.capacity;
            self.buf.clear();
            if let Some(tail) = produced.get(start..) {
                self.buf.extend_from_slice(tail);
            }
            return;
        }
        let room = self.capacity - self.buf.len().min(self.capacity);
        if produced.len() > room {
            let evict = produced.len() - room;
            let keep = self.buf.len().saturating_sub(evict);
            self.buf.copy_within(evict.., 0);
            self.buf.truncate(keep);
        }
        self.buf.extend_from_slice(produced);
    }
}

// ---------------------------------------------------------------------------
// GrowSink
// ---------------------------------------------------------------------------

/// Growable sink over the crate's existing [`InflateWindow`].
///
/// `space()` is `usize::MAX` and [`InflateSink::fast_region`] is `None`, so
/// a decoder driven through this sink writes one symbol at a time through
/// the trait.
///
/// Test-only since 0.4.2: every production front end decodes into a slice
/// (the growable one into the tail of the buffer it is filling), which is
/// what lets the fast loop hold the output cursor in a register. `GrowSink`
/// is retained as the independent reference the copy-semantics differential
/// in this module's tests compares [`BoundedSink`] against.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct GrowSink<'a> {
    window: &'a mut InflateWindow,
    start_len: usize,
}

#[cfg(test)]
impl<'a> GrowSink<'a> {
    /// Wrap a window. `written()` counts from zero, not from the window's
    /// current length, and is derived from the window rather than tracked
    /// separately so the fast loop keeps no counter of its own.
    pub(crate) fn new(window: &'a mut InflateWindow) -> Self {
        let start_len = window.output_len();
        Self { window, start_len }
    }
}

#[cfg(test)]
impl InflateSink for GrowSink<'_> {
    #[inline(always)]
    fn space(&self) -> usize {
        usize::MAX
    }

    #[inline(always)]
    fn history_len(&self) -> usize {
        self.window.history_len()
    }

    #[inline(always)]
    fn write_literal(&mut self, byte: u8) -> Result<()> {
        DecodeSink::write_literal(self.window, byte)
    }

    #[inline]
    fn write_literals(&mut self, bytes: &[u8]) -> Result<()> {
        DecodeSink::write_literals(self.window, bytes)
    }

    #[inline]
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        DecodeSink::copy_match(self.window, distance, length)
    }

    #[inline(always)]
    fn written(&self) -> usize {
        self.window.output_len() - self.start_len
    }
}

// ---------------------------------------------------------------------------
// BoundedSink
// ---------------------------------------------------------------------------

/// Fixed-size sink over a caller-supplied slice, with an optional history
/// for back-references that reach behind the start of that slice.
///
/// With `history == None` this reproduces the old `SliceSink` exactly: a
/// back-reference before `dst[0]` is [`OxiArcError::InvalidDistance`], which
/// is `inflate_into`'s documented guarantee.
#[derive(Debug)]
pub(crate) struct BoundedSink<'a, 'h> {
    dst: &'a mut [u8],
    pos: usize,
    /// Where this sink started writing. `written()` counts from here, not
    /// from `dst[0]`, so a resumed sink reports what *this* call produced —
    /// which is what the decoder's limit handling means by "has this call
    /// produced anything yet".
    start: usize,
    history: Option<&'h History>,
}

impl<'a, 'h> BoundedSink<'a, 'h> {
    /// Wrap a destination buffer and an optional history window.
    pub(crate) fn new(dst: &'a mut [u8], history: Option<&'h History>) -> Self {
        Self {
            dst,
            pos: 0,
            start: 0,
            history,
        }
    }

    /// Wrap a destination buffer whose first `position` bytes are already
    /// decoded output.
    ///
    /// This is what lets the growable front end decode into the tail of the
    /// buffer it is filling: the earlier bytes of the same buffer serve as
    /// the LZ77 window, so no byte is ever written twice and no separate
    /// history copy is kept.
    pub(crate) fn resumed(
        dst: &'a mut [u8],
        position: usize,
        history: Option<&'h History>,
    ) -> Self {
        let pos = position.min(dst.len());
        Self {
            dst,
            pos,
            start: pos,
            history,
        }
    }

    #[inline]
    fn overflow(&self, need: usize) -> OxiArcError {
        OxiArcError::buffer_too_small(self.pos.saturating_add(need), self.dst.len())
    }

    /// Bytes already written, i.e. the length of the valid prefix of `dst`.
    #[inline(always)]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Overlapping copy entirely inside `dst`, mirroring
    /// `InflateWindow::copy_within_output`'s chunking rules so both sinks
    /// reproduce the same LZ77 semantics — including its short-distance
    /// cases, without which a `distance == 1` run of 258 bytes would be 258
    /// one-byte `copy_within` calls (measured: +16.5 % on `inflate_into`
    /// over highly compressible input).
    fn copy_within_dst(&mut self, distance: usize, length: usize) {
        let end = self.pos + length;

        if distance >= length {
            // Non-overlapping: one memcpy.
            let src = self.pos - distance;
            self.dst.copy_within(src..src + length, self.pos);
            self.pos = end;
            return;
        }

        if distance == 1 {
            // Run of a single byte: one memset.
            let byte = self.dst.get(self.pos - 1).copied().unwrap_or(0);
            if let Some(run) = self.dst.get_mut(self.pos..end) {
                run.fill(byte);
            }
            self.pos = end;
            return;
        }

        if distance < 16 {
            // Tile the pattern up to a multiple of `distance` that is at
            // least 16 bytes, then emit whole tiles: the phase realigns at
            // every tile boundary because the tile length is a multiple of
            // the period.
            let tile_len = distance * (32 / distance);
            let mut tile = [0u8; 32];
            let start = self.pos - distance;
            for (i, slot) in tile.iter_mut().take(tile_len).enumerate() {
                *slot = self.dst.get(start + i % distance).copied().unwrap_or(0);
            }
            let mut written = 0usize;
            while written < length {
                let n = (length - written).min(tile_len);
                if let Some(src) = tile.get(..n) {
                    if let Some(slot) = self.dst.get_mut(self.pos + written..self.pos + written + n)
                    {
                        slot.copy_from_slice(src);
                    }
                }
                written += n;
            }
            self.pos = end;
            return;
        }

        // Overlapping with a usable period: repeat `distance`-sized memcpys.
        // Each chunk is bounded by `distance` because only bytes already
        // materialised may serve as the source.
        let mut copied = 0usize;
        while copied < length {
            let n = (length - copied).min(distance);
            let src = self.pos + copied - distance;
            self.dst.copy_within(src..src + n, self.pos + copied);
            copied += n;
        }
        self.pos = end;
    }
}

impl InflateSink for BoundedSink<'_, '_> {
    #[inline(always)]
    fn space(&self) -> usize {
        self.dst.len() - self.pos
    }

    #[inline(always)]
    fn fast_region(&mut self) -> Option<FastRegion<'_>> {
        let history = self.history.map_or(&[][..], History::as_slice);
        Some(FastRegion {
            pos: self.pos,
            dst: self.dst,
            history,
        })
    }

    #[inline(always)]
    fn commit(&mut self, position: usize) {
        debug_assert!(position <= self.dst.len());
        self.pos = position.min(self.dst.len());
    }

    #[inline(always)]
    fn history_len(&self) -> usize {
        self.pos + self.history.map_or(0, History::len)
    }

    #[inline(always)]
    fn write_literal(&mut self, byte: u8) -> Result<()> {
        match self.dst.get_mut(self.pos) {
            Some(slot) => {
                *slot = byte;
                self.pos += 1;
                Ok(())
            }
            None => Err(self.overflow(1)),
        }
    }

    #[inline(always)]
    fn write_literals(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or_else(|| self.overflow(bytes.len()))?;
        match self.dst.get_mut(self.pos..end) {
            Some(dst) => {
                dst.copy_from_slice(bytes);
                self.pos = end;
                Ok(())
            }
            None => Err(self.overflow(bytes.len())),
        }
    }

    #[inline(always)]
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        let history = self.history_len();
        if distance == 0 || distance > history {
            return Err(OxiArcError::invalid_distance(distance, history));
        }
        if length > MAX_COPY_LENGTH {
            return Err(OxiArcError::memory_budget_exceeded(MAX_COPY_LENGTH, length));
        }
        let end = self
            .pos
            .checked_add(length)
            .ok_or_else(|| self.overflow(length))?;
        if end > self.dst.len() {
            return Err(self.overflow(length));
        }

        if distance <= self.pos {
            if distance == 1 {
                let byte = self.dst.get(self.pos - 1).copied().unwrap_or(0);
                if let Some(run) = self.dst.get_mut(self.pos..end) {
                    run.fill(byte);
                }
                self.pos = end;
                return Ok(());
            }
            self.copy_within_dst(distance, length);
            return Ok(());
        }

        // The match starts inside the history window. Copy that part first,
        // then let the in-`dst` path finish the remainder — by then it
        // refers to bytes this call has already materialised.
        let from_history = distance - self.pos;
        let hist = self.history.map_or(&[][..], History::as_slice);
        let start = hist.len() - from_history;
        let take = from_history.min(length);
        match (
            hist.get(start..start + take),
            self.dst.get_mut(self.pos..end),
        ) {
            (Some(src), Some(dst)) => {
                if let Some(head) = dst.get_mut(..take) {
                    head.copy_from_slice(src);
                }
            }
            _ => return Err(OxiArcError::invalid_distance(distance, history)),
        }
        self.pos += take;
        if take < length {
            self.copy_within_dst(distance, length - take);
        }
        Ok(())
    }

    #[inline(always)]
    fn written(&self) -> usize {
        self.pos - self.start
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Byte-at-a-time LZ77 reference for the copy semantics every sink must
    /// reproduce exactly.
    fn reference_copy(history: &[u8], distance: usize, length: usize) -> Vec<u8> {
        let mut out = history.to_vec();
        for _ in 0..length {
            let byte = out[out.len() - distance];
            out.push(byte);
        }
        out[history.len()..].to_vec()
    }

    #[test]
    fn history_keeps_the_newest_window() {
        let mut h = History::with_capacity(8);
        h.append(b"abcdef");
        assert_eq!(h.as_slice(), b"abcdef");
        h.append(b"ghij");
        assert_eq!(h.as_slice(), b"cdefghij");
        h.append(&[b'z'; 40]);
        assert_eq!(h.as_slice(), &[b'z'; 8]);
    }

    /// `append` must retain exactly the newest `capacity` bytes for every
    /// combination of prior fill and appended length, and must never let the
    /// buffer grow past `capacity` — the transient overshoot is what made the
    /// history reallocate in the steady state (measured: one `realloc` to
    /// 125 068 bytes against a 32 KiB window, on a 16 MiB gzip body fed in
    /// 4 KiB chunks through a 64 KiB output buffer).
    #[test]
    fn history_never_exceeds_its_capacity_and_matches_a_reference() {
        for capacity in [1usize, 2, 7, 8, 9, 64, 1000] {
            for schedule in [
                vec![1usize, 1, 1, 1, 1],
                vec![3, 5, 7, 11],
                vec![capacity, capacity, 1],
                vec![capacity - 1, 2, capacity + 1],
                vec![capacity + 5, 1, capacity * 3],
                vec![0, capacity / 2 + 1, 0, capacity / 2 + 1],
            ] {
                let mut h = History::with_capacity(capacity);
                let mut reference: Vec<u8> = Vec::new();
                let mut next = 0u8;
                for len in schedule {
                    let chunk: Vec<u8> = (0..len)
                        .map(|_| {
                            next = next.wrapping_add(1);
                            next
                        })
                        .collect();
                    h.append(&chunk);
                    reference.extend_from_slice(&chunk);
                    let keep = reference.len().min(capacity);
                    assert_eq!(
                        h.as_slice(),
                        &reference[reference.len() - keep..],
                        "capacity {capacity}: retained bytes differ"
                    );
                    assert!(
                        h.len() <= capacity,
                        "capacity {capacity}: history holds {} bytes",
                        h.len()
                    );
                    // The allocation, not just the length. Appending before
                    // evicting leaves `len` correct but grows the *buffer* to
                    // `capacity + produced.len()`, which is invisible to the
                    // assertion above and is what reallocated in the steady
                    // state.
                    assert!(
                        h.buf.capacity() <= capacity,
                        "capacity {capacity}: history buffer grew to {} bytes",
                        h.buf.capacity()
                    );
                }
            }
        }
    }

    #[test]
    fn history_set_dictionary_keeps_the_tail() {
        let mut h = History::with_capacity(WINDOW);
        let dict: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
        h.set_dictionary(&dict);
        assert_eq!(h.len(), WINDOW);
        assert_eq!(h.as_slice(), &dict[dict.len() - WINDOW..]);
    }

    #[test]
    fn bounded_sink_without_history_rejects_pre_buffer_distance() {
        let mut dst = [0u8; 16];
        let mut sink = BoundedSink::new(&mut dst, None);
        sink.write_literals(b"abc").expect("literals");
        let err = sink.copy_match(4, 1).expect_err("distance before dst[0]");
        assert!(matches!(err, OxiArcError::InvalidDistance { .. }));
    }

    #[test]
    fn bounded_sink_reports_space_and_overflow() {
        let mut dst = [0u8; 4];
        let mut sink = BoundedSink::new(&mut dst, None);
        assert_eq!(sink.space(), 4);
        sink.write_literals(b"abcd").expect("fill");
        assert_eq!(sink.space(), 0);
        let err = sink.write_literal(b'e').expect_err("full");
        assert!(matches!(err, OxiArcError::BufferTooSmall { .. }));
    }

    /// Every `(history split, distance, length)` combination must agree with
    /// the byte-at-a-time LZ77 reference — the case that exercises the
    /// two-part copy across the history/output seam.
    #[test]
    fn bounded_sink_copy_matches_reference_across_the_history_seam() {
        let source: Vec<u8> = (0..64u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(3))
            .collect();
        for split in 0..=40usize {
            let mut history = History::with_capacity(WINDOW);
            history.append(&source[..split]);
            let already = &source[split..40];

            for distance in 1..=40usize {
                for length in 1..=300usize {
                    if distance > split + already.len() {
                        continue;
                    }
                    let mut dst = vec![0u8; already.len() + length];
                    let mut sink = BoundedSink::new(&mut dst, Some(&history));
                    sink.write_literals(already).expect("prefill");
                    sink.copy_match(distance, length).expect("copy");
                    let produced = dst[already.len()..].to_vec();

                    let mut full = source[..split].to_vec();
                    full.extend_from_slice(already);
                    let expected = reference_copy(&full, distance, length);
                    assert_eq!(
                        produced, expected,
                        "split={split} distance={distance} length={length}"
                    );
                }
            }
        }
    }

    #[test]
    fn bounded_sink_rejects_oversized_copy() {
        let mut dst = vec![0u8; 32];
        let mut sink = BoundedSink::new(&mut dst, None);
        sink.write_literal(b'x').expect("literal");
        let err = sink
            .copy_match(1, MAX_COPY_LENGTH + 1)
            .expect_err("over the copy budget");
        assert!(matches!(err, OxiArcError::MemoryBudgetExceeded { .. }));
    }

    #[test]
    fn grow_sink_reports_unbounded_space() {
        let mut window = InflateWindow::with_capacity(16);
        let mut sink = GrowSink::new(&mut window);
        assert_eq!(sink.space(), usize::MAX);
        sink.write_literals(b"hello").expect("literals");
        sink.copy_match(5, 5).expect("copy");
        assert_eq!(sink.written(), 10);
        assert_eq!(window.output(), b"hellohello");
    }

    /// R10 at the growable sink. `BoundedSink` has its own pair above; the
    /// same two rejections must hold here, because the fast loop is
    /// monomorphised over `GrowSink` and never re-checks them itself.
    #[test]
    fn grow_sink_rejects_zero_and_out_of_range_distance() {
        let mut window = InflateWindow::with_capacity(64);
        let mut sink = GrowSink::new(&mut window);
        sink.write_literals(b"abc").expect("literals");

        let err = sink.copy_match(0, 3).expect_err("distance zero");
        assert!(matches!(err, OxiArcError::InvalidDistance { .. }));

        // One past everything ever written: there is no such history byte.
        let err = sink.copy_match(4, 1).expect_err("distance past history");
        assert!(matches!(err, OxiArcError::InvalidDistance { .. }));
    }

    #[test]
    fn grow_sink_rejects_oversized_copy() {
        let mut window = InflateWindow::with_capacity(64);
        let mut sink = GrowSink::new(&mut window);
        sink.write_literal(b'x').expect("literal");
        let err = sink
            .copy_match(1, MAX_COPY_LENGTH + 1)
            .expect_err("over the copy budget");
        assert!(matches!(err, OxiArcError::MemoryBudgetExceeded { .. }));
    }

    #[test]
    fn bounded_sink_with_history_rejects_zero_distance() {
        let mut history = History::with_capacity(WINDOW);
        history.append(b"earlier output");
        let mut dst = [0u8; 16];
        let mut sink = BoundedSink::new(&mut dst, Some(&history));
        sink.write_literals(b"abc").expect("literals");
        let err = sink.copy_match(0, 3).expect_err("distance zero");
        assert!(matches!(err, OxiArcError::InvalidDistance { .. }));
    }

    /// The two sinks must produce identical bytes for the same match, so a
    /// caller cannot observe which front end decoded a stream.
    #[test]
    fn both_sinks_agree_with_the_reference_copy() {
        let seed: Vec<u8> = (0..48u8)
            .map(|i| i.wrapping_mul(11).wrapping_add(5))
            .collect();
        for distance in 1..=48usize {
            for length in [1usize, 2, 3, 17, 48, 100, 258] {
                let expected = reference_copy(&seed, distance, length);

                let mut window = InflateWindow::with_capacity(WINDOW);
                let mut grow = GrowSink::new(&mut window);
                grow.write_literals(&seed).expect("seed");
                grow.copy_match(distance, length).expect("grow copy");
                let grown = window.output()[seed.len()..].to_vec();

                let mut dst = vec![0u8; seed.len() + length];
                let mut bounded = BoundedSink::new(&mut dst, None);
                bounded.write_literals(&seed).expect("seed");
                bounded.copy_match(distance, length).expect("bounded copy");
                let bound = dst[seed.len()..].to_vec();

                assert_eq!(grown, expected, "grow distance={distance} length={length}");
                assert_eq!(
                    bound, expected,
                    "bounded distance={distance} length={length}"
                );
            }
        }
    }
}
