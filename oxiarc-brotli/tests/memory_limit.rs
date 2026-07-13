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
