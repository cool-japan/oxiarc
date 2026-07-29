//! Decode output buffer with an implicit LZ77 history window.
//!
//! [`InflateWindow`] replaces the generic
//! [`oxiarc_core::OutputRingBuffer`] inside the DEFLATE decoder. The ring
//! buffer keeps the 32 KiB history in a *separate* circular array, so every
//! decoded byte is written twice — once into the ring and once into the
//! output vector — and every back-reference is resolved a byte at a time
//! through a masked index.
//!
//! Here the accumulated output *is* the history: the last 32 KiB of
//! `output` are exactly the bytes a DEFLATE back-reference may name, so a
//! match becomes a `Vec::extend_from_within` (a `memcpy`) and a literal
//! becomes a single `Vec::push`. History that predates the current output —
//! a preset dictionary, or bytes already handed out by
//! [`InflateWindow::drain`] — lives in `prefix`, which is kept trimmed to
//! the 32 KiB window.
//!
//! Semantics are otherwise identical to `OutputRingBuffer`, including the
//! `MAX_COPY_LENGTH` guard and the snapshot/restore used to roll back a
//! partially decoded sync-flush unit.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::ringbuffer::MAX_COPY_LENGTH;

/// DEFLATE sliding-window size (RFC 1951 §3.2.1).
const WINDOW: usize = 32768;

/// Destination for decoded DEFLATE symbols.
///
/// Implemented by [`InflateWindow`] (growable `Vec` output) and
/// [`SliceSink`] (fixed caller-supplied buffer) so the block decoder — and
/// in particular its inner symbol loop — exists exactly once.
pub(crate) trait DecodeSink {
    /// Append one literal byte.
    fn write_literal(&mut self, byte: u8) -> Result<()>;
    /// Append several literal bytes.
    fn write_literals(&mut self, bytes: &[u8]) -> Result<()>;
    /// Copy `length` bytes from `distance` bytes back.
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()>;
}

/// Fixed-size decode destination backed by a caller-supplied slice.
///
/// Nothing is allocated: literals and matches are written straight into
/// `dst`. A stream that decodes to more than `dst.len()` bytes, or a
/// back-reference that reaches behind the start of `dst`, is rejected with
/// an error — never a panic, never a silent truncation.
#[derive(Debug)]
pub(crate) struct SliceSink<'a> {
    dst: &'a mut [u8],
    pos: usize,
}

impl<'a> SliceSink<'a> {
    /// Wrap a destination buffer.
    pub(crate) fn new(dst: &'a mut [u8]) -> Self {
        Self { dst, pos: 0 }
    }

    /// Number of bytes written so far.
    pub(crate) fn written(&self) -> usize {
        self.pos
    }

    #[inline]
    fn overflow(&self, need: usize) -> OxiArcError {
        OxiArcError::buffer_too_small(self.pos.saturating_add(need), self.dst.len())
    }
}

impl DecodeSink for SliceSink<'_> {
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
        if distance == 0 || distance > self.pos {
            return Err(OxiArcError::invalid_distance(distance, self.pos));
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

        if distance == 1 {
            let byte = self.dst.get(self.pos - 1).copied().unwrap_or(0);
            if let Some(run) = self.dst.get_mut(self.pos..end) {
                run.fill(byte);
            }
            self.pos = end;
            return Ok(());
        }

        // Chunks are capped at `distance` because only bytes already
        // materialised may serve as the source; that also gives the standard
        // LZ77 pattern repetition when `length > distance`.
        let mut copied = 0usize;
        while copied < length {
            let n = (length - copied).min(distance);
            let src = self.pos + copied - distance;
            self.dst.copy_within(src..src + n, self.pos + copied);
            copied += n;
        }
        self.pos = end;
        Ok(())
    }
}

/// Accumulated decoder output plus the history needed to resolve
/// back-references that reach behind it.
#[derive(Debug)]
pub(crate) struct InflateWindow {
    /// History preceding `output`: preset dictionary and/or bytes already
    /// drained. Never longer than [`WINDOW`].
    prefix: Vec<u8>,
    /// Decoded bytes not yet drained.
    output: Vec<u8>,
}

/// Snapshot of an [`InflateWindow`] for rollback after a partial decode.
#[derive(Debug, Clone)]
pub(crate) struct WindowSnapshot {
    prefix: Vec<u8>,
    output_len: usize,
}

impl InflateWindow {
    /// Create an empty window with the given initial output capacity.
    pub(crate) fn with_capacity(output_capacity: usize) -> Self {
        Self {
            prefix: Vec::new(),
            output: Vec::with_capacity(output_capacity),
        }
    }

    /// Total number of history bytes a back-reference may address.
    #[inline(always)]
    pub(crate) fn history_len(&self) -> usize {
        self.prefix.len() + self.output.len()
    }

    /// Copy `length` bytes from `distance` bytes back in the history.
    ///
    /// `length` may exceed `distance` (the standard LZ77 overlapping copy,
    /// which repeats the pattern).
    ///
    /// # Errors
    ///
    /// [`OxiArcError::InvalidDistance`] when `distance` is zero or reaches
    /// past the available history, and [`OxiArcError::MemoryBudgetExceeded`]
    /// when `length` exceeds [`MAX_COPY_LENGTH`] or the output cannot be
    /// grown — a crafted length fails gracefully instead of aborting the
    /// process on a capacity overflow.
    fn copy_match_impl(&mut self, distance: usize, length: usize) -> Result<()> {
        let history = self.history_len();
        if distance == 0 || distance > history {
            return Err(OxiArcError::invalid_distance(distance, history));
        }
        if length > MAX_COPY_LENGTH {
            return Err(OxiArcError::memory_budget_exceeded(MAX_COPY_LENGTH, length));
        }
        // `try_reserve` turns an allocation failure into a recoverable error
        // instead of an abort, but it is a real call; skip it whenever the
        // spare capacity already covers the copy (the overwhelmingly common
        // case, and always so when the caller supplied a size hint).
        if self.output.capacity() - self.output.len() < length {
            self.output
                .try_reserve(length)
                .map_err(|_| OxiArcError::memory_budget_exceeded(MAX_COPY_LENGTH, length))?;
        }

        if distance <= self.output.len() {
            self.copy_within_output(distance, length);
            return Ok(());
        }

        // The match starts inside `prefix`. Copy the part that comes from the
        // prefix first, then let the in-output path finish any remainder
        // (which by then refers to freshly written bytes).
        let from_prefix = distance - self.output.len();
        let start = self.prefix.len() - from_prefix;
        let take = from_prefix.min(length);
        if let Some(src) = self.prefix.get(start..start + take) {
            self.output.extend_from_slice(src);
        }
        if take < length {
            self.copy_within_output(distance, length - take);
        }
        Ok(())
    }

    /// Overlapping copy entirely inside `output`.
    ///
    /// Each chunk is bounded by `distance` because only bytes already
    /// materialised may serve as the source; for very short distances the
    /// pattern is first tiled into a small buffer so the copy still proceeds
    /// in useful-sized `memcpy`s instead of one byte at a time.
    #[inline]
    fn copy_within_output(&mut self, distance: usize, length: usize) {
        let start = self.output.len() - distance;

        if distance >= length {
            // Non-overlapping: one memcpy.
            self.output.extend_from_within(start..start + length);
            return;
        }

        if distance == 1 {
            // Run of a single byte: one memset.
            let byte = self.output.last().copied().unwrap_or(0);
            self.output.resize(self.output.len() + length, byte);
            return;
        }

        if distance < 16 {
            // Tile the pattern up to a multiple of `distance` that is at
            // least 16 bytes, then emit whole tiles: the phase realigns at
            // every tile boundary because the tile length is a multiple of
            // the period.
            let tile_len = distance * (32 / distance);
            let mut tile = [0u8; 32];
            for (i, slot) in tile.iter_mut().take(tile_len).enumerate() {
                *slot = self.output.get(start + i % distance).copied().unwrap_or(0);
            }
            let mut remaining = length;
            while remaining > 0 {
                let n = remaining.min(tile_len);
                if let Some(src) = tile.get(..n) {
                    self.output.extend_from_slice(src);
                }
                remaining -= n;
            }
            return;
        }

        // Overlapping with a usable period: repeat `distance`-sized memcpys.
        let mut copied = 0usize;
        while copied < length {
            let n = (length - copied).min(distance);
            self.output
                .extend_from_within(start + copied..start + copied + n);
            copied += n;
        }
    }

    /// Number of decoded bytes not yet drained.
    pub(crate) fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Decoded bytes not yet drained.
    pub(crate) fn output(&self) -> &[u8] {
        &self.output
    }

    /// Consume the window and return the decoded bytes.
    pub(crate) fn into_output(self) -> Vec<u8> {
        self.output
    }

    /// Drop all state, including the history.
    pub(crate) fn clear(&mut self) {
        self.prefix.clear();
        self.output.clear();
    }

    /// Preload history from a preset dictionary (not part of the output).
    ///
    /// Only the trailing [`WINDOW`] bytes are retained, matching zlib.
    pub(crate) fn preload_dictionary(&mut self, dictionary: &[u8]) {
        let start = dictionary.len().saturating_sub(WINDOW);
        self.prefix.clear();
        if let Some(tail) = dictionary.get(start..) {
            self.prefix.extend_from_slice(tail);
        }
    }

    /// Take every decoded byte while preserving the sliding window, so
    /// back-references across a sync-flush boundary stay valid
    /// (RFC 4978 §3).
    pub(crate) fn drain(&mut self) -> Vec<u8> {
        let out = std::mem::take(&mut self.output);
        // Roll the tail of the drained data into the history prefix.
        let keep_from = out.len().saturating_sub(WINDOW);
        if let Some(tail) = out.get(keep_from..) {
            if tail.len() >= WINDOW {
                self.prefix.clear();
                self.prefix.extend_from_slice(tail);
            } else {
                self.prefix.extend_from_slice(tail);
                let excess = self.prefix.len().saturating_sub(WINDOW);
                if excess > 0 {
                    self.prefix.drain(..excess);
                }
            }
        }
        out
    }

    /// Snapshot the history for rollback on a partial-input failure.
    pub(crate) fn snapshot(&self) -> WindowSnapshot {
        WindowSnapshot {
            prefix: self.prefix.clone(),
            output_len: self.output.len(),
        }
    }

    /// Undo every side effect since `snap` was taken.
    pub(crate) fn restore(&mut self, snap: &WindowSnapshot, output_len: usize) {
        self.prefix.clear();
        self.prefix.extend_from_slice(&snap.prefix);
        self.output.truncate(output_len.min(snap.output_len));
    }
}

impl DecodeSink for InflateWindow {
    #[inline(always)]
    fn write_literal(&mut self, byte: u8) -> Result<()> {
        self.output.push(byte);
        Ok(())
    }

    fn write_literals(&mut self, bytes: &[u8]) -> Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    #[inline]
    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        self.copy_match_impl(distance, length)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Byte-at-a-time LZ77 reference for the copy semantics both sinks must
    /// reproduce exactly.
    fn reference_copy(history: &[u8], distance: usize, length: usize) -> Vec<u8> {
        let mut out = history.to_vec();
        for _ in 0..length {
            let byte = out[out.len() - distance];
            out.push(byte);
        }
        out[history.len()..].to_vec()
    }

    fn window_with(prefix: &[u8], output: &[u8]) -> InflateWindow {
        let mut w = InflateWindow::with_capacity(64);
        if !prefix.is_empty() {
            w.preload_dictionary(prefix);
        }
        w.write_literals(output).expect("literals");
        w
    }

    #[test]
    fn literals_and_simple_match() {
        let mut w = window_with(b"", b"Hello");
        w.copy_match(5, 5).expect("copy");
        assert_eq!(w.output(), b"HelloHello");
    }

    #[test]
    fn overlapping_match_repeats_pattern() {
        let mut w = window_with(b"", b"AB");
        w.copy_match(2, 6).expect("copy");
        assert_eq!(w.output(), b"ABABABAB");
    }

    #[test]
    fn single_byte_run() {
        let mut w = window_with(b"", b"X");
        w.copy_match(1, 5).expect("copy");
        assert_eq!(w.output(), b"XXXXXX");
    }

    /// Every short distance and every length must reproduce the byte-at-a-time
    /// reference — this is where the tiling / chunked `memcpy` fast paths of
    /// both sinks could diverge.
    #[test]
    fn copy_paths_match_reference() {
        for distance in 1..=40usize {
            for length in 1..=300usize {
                let seed: Vec<u8> = (0..distance).map(|i| (i as u8).wrapping_mul(37)).collect();
                let expected = reference_copy(&seed, distance, length);

                let mut w = window_with(b"", &seed);
                w.copy_match(distance, length).expect("vec copy");
                assert_eq!(
                    &w.output()[seed.len()..],
                    &expected[..],
                    "InflateWindow d={distance} l={length}"
                );

                let mut buf = vec![0u8; seed.len() + length];
                let mut sink = SliceSink::new(&mut buf);
                sink.write_literals(&seed).expect("seed");
                sink.copy_match(distance, length).expect("slice copy");
                assert_eq!(sink.written(), seed.len() + length);
                assert_eq!(
                    &buf[seed.len()..],
                    &expected[..],
                    "SliceSink d={distance} l={length}"
                );
            }
        }
    }

    #[test]
    fn dictionary_backed_match() {
        let mut w = window_with(b"DICTIONARY", b"xy");
        // distance 12 reaches 10 bytes into the dictionary.
        w.copy_match(12, 4).expect("copy");
        assert_eq!(w.output(), b"xyDICT");
    }

    #[test]
    fn match_spanning_prefix_and_output() {
        let mut w = window_with(b"abcd", b"ef");
        // distance 6 == whole history; length 10 wraps past the prefix.
        w.copy_match(6, 10).expect("copy");
        assert_eq!(w.output(), b"efabcdefabcd");
    }

    /// A match reaching into the dictionary must agree with the reference for
    /// every split of the history.
    #[test]
    fn prefix_spanning_matches_reference() {
        let dict: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(13)).collect();
        for out_len in 0..8usize {
            let out: Vec<u8> = (0..out_len).map(|i| 200 + i as u8).collect();
            let history: Vec<u8> = dict.iter().chain(out.iter()).copied().collect();
            for distance in (out_len + 1)..=history.len() {
                for length in [1usize, 3, 17, 64, 200] {
                    let expected = reference_copy(&history, distance, length);
                    let mut w = window_with(&dict, &out);
                    w.copy_match(distance, length).expect("copy");
                    assert_eq!(
                        &w.output()[out_len..],
                        &expected[..],
                        "d={distance} l={length} out_len={out_len}"
                    );
                }
            }
        }
    }

    #[test]
    fn invalid_distance_rejected() {
        let mut w = window_with(b"", b"ab");
        assert!(w.copy_match(0, 1).is_err());
        assert!(w.copy_match(3, 1).is_err());

        let mut buf = [0u8; 8];
        let mut sink = SliceSink::new(&mut buf);
        sink.write_literals(b"ab").expect("seed");
        assert!(sink.copy_match(0, 1).is_err());
        assert!(sink.copy_match(3, 1).is_err());
    }

    #[test]
    fn oversized_length_rejected() {
        let mut w = window_with(b"", b"ab");
        let err = w.copy_match(2, usize::MAX).expect_err("must reject");
        assert!(matches!(err, OxiArcError::MemoryBudgetExceeded { .. }));
        assert_eq!(w.output(), b"ab");

        let mut buf = [0u8; 8];
        let mut sink = SliceSink::new(&mut buf);
        sink.write_literals(b"ab").expect("seed");
        assert!(sink.copy_match(2, usize::MAX).is_err());
        assert_eq!(sink.written(), 2);
    }

    #[test]
    fn slice_sink_rejects_overflow() {
        let mut buf = [0u8; 4];
        let mut sink = SliceSink::new(&mut buf);
        sink.write_literals(b"abcd").expect("fill");
        let err = sink.write_literal(b'e').expect_err("full");
        assert!(matches!(err, OxiArcError::BufferTooSmall { .. }));
        assert!(sink.write_literals(b"ef").is_err());
        assert!(sink.copy_match(4, 1).is_err());
        assert_eq!(sink.written(), 4);
        assert_eq!(&buf, b"abcd");
    }

    #[test]
    fn drain_preserves_history() {
        let mut w = window_with(b"", b"Hello");
        assert_eq!(w.drain(), b"Hello");
        assert_eq!(w.output_len(), 0);
        w.copy_match(5, 5).expect("copy after drain");
        assert_eq!(w.output(), b"Hello");
    }

    #[test]
    fn drain_trims_history_to_window() {
        let mut w = InflateWindow::with_capacity(WINDOW * 2);
        w.write_literals(&vec![7u8; WINDOW + 1000]).expect("fill");
        let _ = w.drain();
        assert_eq!(w.history_len(), WINDOW);
        assert!(w.copy_match(WINDOW, 1).is_ok());
        assert!(w.copy_match(WINDOW + 2, 1).is_err());
    }

    /// Repeated small drains must keep the window at exactly the last 32 KiB
    /// of everything produced, so cross-flush back-references stay valid.
    #[test]
    fn repeated_drains_keep_window_exact() {
        let mut w = InflateWindow::with_capacity(1024);
        let mut all = Vec::new();
        for round in 0..40u8 {
            let chunk = vec![round; 2000];
            w.write_literals(&chunk).expect("write");
            all.extend_from_slice(&chunk);
            let _ = w.drain();
        }
        assert_eq!(w.history_len(), WINDOW.min(all.len()));
        // The oldest addressable byte must be the right one.
        w.copy_match(w.history_len(), 1).expect("edge copy");
        assert_eq!(w.output()[0], all[all.len() - WINDOW]);
    }

    #[test]
    fn snapshot_restore_round_trip() {
        let mut w = window_with(b"", b"ABC");
        let snap = w.snapshot();
        let len = w.output_len();
        w.write_literals(b"XYZ").expect("write");
        w.restore(&snap, len);
        assert_eq!(w.output(), b"ABC");
        w.write_literals(b"DEF").expect("write");
        assert_eq!(w.output(), b"ABCDEF");
    }
}
