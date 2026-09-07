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
    pub(crate) fn append(&mut self, produced: &[u8]) {
        if produced.is_empty() {
            return;
        }
        if produced.len() >= self.capacity {
            let start = produced.len() - self.capacity;
            self.buf.clear();
            if let Some(tail) = produced.get(start..) {
                self.buf.extend_from_slice(tail);
            }
            return;
        }
        self.buf.extend_from_slice(produced);
        let excess = self.buf.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.buf.copy_within(excess.., 0);
            self.buf.truncate(self.capacity);
        }
    }
}

// ---------------------------------------------------------------------------
// GrowSink
// ---------------------------------------------------------------------------

/// Growable sink over the crate's existing [`InflateWindow`].
///
/// `space()` is `usize::MAX`, so a decoder monomorphised over this type has
/// no output-space checks in its fast loop at all — the same instruction
/// sequence the one-shot decoder runs today.
#[derive(Debug)]
pub(crate) struct GrowSink<'a> {
    window: &'a mut InflateWindow,
    start_len: usize,
}

impl<'a> GrowSink<'a> {
    /// Wrap a window. `written()` counts from zero, not from the window's
    /// current length, and is derived from the window rather than tracked
    /// separately so the fast loop keeps no counter of its own.
    pub(crate) fn new(window: &'a mut InflateWindow) -> Self {
        let start_len = window.output_len();
        Self { window, start_len }
    }
}

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
    history: Option<&'h History>,
}

impl<'a, 'h> BoundedSink<'a, 'h> {
    /// Wrap a destination buffer and an optional history window.
    pub(crate) fn new(dst: &'a mut [u8], history: Option<&'h History>) -> Self {
        Self {
            dst,
            pos: 0,
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
    /// reproduce the same LZ77 semantics.
    fn copy_within_dst(&mut self, distance: usize, length: usize) {
        let mut copied = 0usize;
        while copied < length {
            let n = (length - copied).min(distance);
            let src = self.pos + copied - distance;
            self.dst.copy_within(src..src + n, self.pos + copied);
            copied += n;
        }
        self.pos += length;
    }
}

impl InflateSink for BoundedSink<'_, '_> {
    #[inline(always)]
    fn space(&self) -> usize {
        self.dst.len() - self.pos
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
        self.pos
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
}
