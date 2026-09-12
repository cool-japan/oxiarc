//! Regression tests for the bounded decoder ([`decompress_with_limit`] and
//! [`BrotliDecompressor::with_max_output`]).
//!
//! History: Brotli declares no uncompressed size, and the crate exposed no
//! bounded decoder, so the OxiArc CLI's `--memory-limit` could only reject a
//! `.br` bomb *after* decoding it in full (bounded only by the crate's 256 MB
//! guard). The limit is now enforced per meta-block, before the offending
//! block is decoded.
//!
//! These tests pin that:
//!
//! * a bomb (small input, huge expansion) under a small budget is rejected
//!   with [`BrotliError::MemoryBudgetExceeded`] **and never allocates
//!   anything close to its expansion** — proven with a heap-tracking global
//!   allocator, not just by looking at the error;
//! * an in-budget payload still round-trips byte-for-byte, including at the
//!   exact budget boundary (no false positives).

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxiarc_brotli::streaming::BrotliDecompressor;
use oxiarc_brotli::{BrotliError, compress, decompress, decompress_with_limit};

/// Live heap bytes, and the high-water mark of the same.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// A pass-through allocator that records the peak live heap size, so a test
/// can assert that rejecting a bomb never *materialises* the bomb.
struct PeakTrackingAlloc;

unsafe impl GlobalAlloc for PeakTrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_growth(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                record_growth(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

/// Add `bytes` to the live total and lift the high-water mark if needed.
fn record_growth(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: PeakTrackingAlloc = PeakTrackingAlloc;

/// Uncompressed size of the bomb fixture (expands from a few hundred bytes).
const BOMB_PLAIN_SIZE: usize = 8 * 1024 * 1024;

/// The budget every bomb is decoded under.
const BUDGET: usize = 64 * 1024;

/// Ceiling on how much the *decoder* may allocate while rejecting the bomb.
///
/// Enforcement is per meta-block and pre-emptive, so in practice the decoder
/// allocates almost nothing (a few KB of prefix-code tables). The ceiling is
/// set generously above that but two orders of magnitude below the bomb's
/// 8 MiB expansion, so a regression to "decode fully, then check" fails here.
const ALLOC_CEILING: usize = 1024 * 1024;

/// Build a bomb: `BOMB_PLAIN_SIZE` zeros, compressed. The plaintext is
/// dropped before returning so it does not pollute the allocation tracker.
fn bomb() -> Vec<u8> {
    let plain = vec![0u8; BOMB_PLAIN_SIZE];
    compress(&plain, 6).expect("compress bomb fixture")
}

#[test]
fn bomb_rejected_during_decode_without_allocating_the_expansion() {
    let bomb = bomb();
    assert!(
        bomb.len() < 64 * 1024,
        "fixture is not a bomb: {} compressed bytes for {BOMB_PLAIN_SIZE} plain",
        bomb.len()
    );

    // Re-baseline the high-water mark now that the fixture's own buffers are
    // gone: from here on, PEAK - baseline is what the decode costs.
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);

    let err = decompress_with_limit(&bomb, BUDGET).expect_err("bomb must be rejected");
    let peak = PEAK.load(Ordering::Relaxed);

    assert!(
        matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
        "expected MemoryBudgetExceeded, got {err:?}"
    );

    let growth = peak.saturating_sub(baseline);
    assert!(
        growth < ALLOC_CEILING,
        "rejecting an {BOMB_PLAIN_SIZE}-byte bomb under a {BUDGET}-byte budget allocated \
         {growth} bytes; the limit is supposed to be enforced *during* decoding"
    );
}

#[test]
fn bomb_rejected_by_the_streaming_decoder_too() {
    let bomb = bomb();
    let mut decoder = BrotliDecompressor::new(&bomb[..]).with_max_output(BUDGET);
    let mut output = Vec::new();
    let err = decoder
        .read_to_end(&mut output)
        .expect_err("streaming bomb must be rejected");
    assert!(
        err.to_string().contains("memory budget exceeded"),
        "unexpected streaming error: {err}"
    );
    assert!(
        output.len() <= BUDGET,
        "streaming decoder produced {} bytes under a {BUDGET}-byte budget",
        output.len()
    );
}

#[test]
fn in_budget_payload_still_round_trips() {
    let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let compressed = compress(&data, 6).expect("compress");

    let decoded = decompress_with_limit(&compressed, 1 << 20).expect("in-budget decode");
    assert_eq!(decoded, data, "bounded decode changed the payload");

    // The unbounded entry point is unaffected.
    assert_eq!(decompress(&compressed).expect("unbounded decode"), data);
}

#[test]
fn budget_boundary_is_exact() {
    let data: Vec<u8> = (0..12_345u32).map(|i| (i % 97) as u8).collect();
    let compressed = compress(&data, 5).expect("compress");

    // Exactly the output size: allowed.
    let decoded = decompress_with_limit(&compressed, data.len()).expect("exact budget must pass");
    assert_eq!(decoded, data);

    // One byte short: rejected, and with the memory-budget error (not a
    // corruption error, which would mask the real cause).
    let err = decompress_with_limit(&compressed, data.len() - 1)
        .expect_err("budget of len-1 must be rejected");
    match err {
        BrotliError::MemoryBudgetExceeded { budget, requested } => {
            assert_eq!(budget, data.len() - 1);
            assert!(
                requested > budget,
                "requested {requested} must exceed budget {budget}"
            );
        }
        other => panic!("expected MemoryBudgetExceeded, got {other:?}"),
    }
}

#[test]
fn empty_and_tiny_payloads_respect_the_budget() {
    let empty = compress(b"", 6).expect("compress empty");
    assert!(
        decompress_with_limit(&empty, 0)
            .expect("empty output fits any budget")
            .is_empty()
    );

    let compressed = compress(b"abc", 6).expect("compress abc");
    assert_eq!(
        decompress_with_limit(&compressed, 3).expect("exact budget"),
        b"abc"
    );
    assert!(matches!(
        decompress_with_limit(&compressed, 2),
        Err(BrotliError::MemoryBudgetExceeded { .. })
    ));
}

// ─── Bounded memory of the incremental decoder ──────────────────────────────

/// Decoding a body far larger than any buffer must not grow the heap with the
/// body.
///
/// This is the property the old read-all `BrotliDecompressor` could not have:
/// it allocated the whole compressed input *and* the whole decompressed output
/// before serving the first byte. The push decoder's peak is the declared
/// sliding window plus one meta-block's prefix codes, so a 64 MiB body decoded
/// through a 64 KiB output slice must stay far under the body size.
#[test]
fn incremental_decode_is_bounded_by_the_window_not_the_body() {
    use oxiarc_brotli::{BrotliStatus, BrotliStream};
    use oxiarc_core::traits::FlushMode;

    /// Uncompressed size of the body used here.
    const BODY: usize = 64 * 1024 * 1024;
    /// `lgwin = 22` is what this crate's encoder declares by default, so the
    /// ring settles at 4 MiB; the ceiling leaves room for that plus the
    /// prefix-code tables and the test's own buffers.
    const PEAK_CEILING: usize = 12 * 1024 * 1024;

    let compressed = {
        let plain = vec![0x2Au8; BODY];
        compress(&plain, 5).expect("compress")
    };
    assert!(
        compressed.len() < 1 << 20,
        "fixture must be small: {} bytes",
        compressed.len()
    );

    let mut stream = BrotliStream::new();
    let mut buf = vec![0u8; 64 * 1024];

    // Re-baseline now that the fixture's plaintext is gone.
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);

    let mut produced = 0u64;
    let mut pos = 0usize;
    loop {
        let end = (pos + 4096).min(compressed.len());
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&compressed[pos..end], &mut buf, flush)
            .expect("decode");
        pos += progress.consumed;
        produced += progress.produced as u64;
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    stream.finish().expect("stream must complete");

    let peak = PEAK.load(Ordering::Relaxed);
    let growth = peak.saturating_sub(baseline);
    assert_eq!(produced, BODY as u64, "decoded the wrong number of bytes");
    assert!(
        growth < PEAK_CEILING,
        "decoding a {BODY}-byte body allocated {growth} bytes; the decoder is \
         supposed to be bounded by the sliding window, not the body"
    );
}

/// The `Read` adapter inherits that bound: `read_to_end` grows only because
/// the *caller's* `Vec` grows.
#[test]
fn the_read_adapter_does_not_buffer_the_compressed_input() {
    /// A source that reports a huge body but is generated on the fly, so the
    /// test can tell "buffered the input" apart from "buffered the output".
    struct Repeating {
        chunk: Vec<u8>,
        pos: usize,
    }
    impl Read for Repeating {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos == self.chunk.len() {
                return Ok(0);
            }
            let n = buf.len().min(1024).min(self.chunk.len() - self.pos);
            buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    let compressed = {
        let plain = vec![0x77u8; 8 * 1024 * 1024];
        compress(&plain, 5).expect("compress")
    };

    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);

    let mut decoder = BrotliDecompressor::new(Repeating {
        chunk: compressed,
        pos: 0,
    });
    // Drain into a fixed buffer so the caller's own allocation is constant.
    let mut sink = vec![0u8; 32 * 1024];
    let mut total = 0usize;
    loop {
        match decoder.read(&mut sink).expect("read") {
            0 => break,
            n => total += n,
        }
    }
    let growth = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert_eq!(total, 8 * 1024 * 1024);
    assert!(
        growth < 12 * 1024 * 1024,
        "streaming 8 MiB through a fixed 32 KiB buffer allocated {growth} bytes"
    );
}
