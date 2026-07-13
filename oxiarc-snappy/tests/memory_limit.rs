//! Regression tests for the bounded decoders (`decompress_with_limit` for raw
//! blocks, `decompress_frame_with_limit` / `FrameDecoder::with_max_output_size`
//! for frames).
//!
//! History: Snappy declares no *total* uncompressed size, and the crate had no
//! one-shot bounded entry point, so the OxiArc CLI's `--memory-limit` could
//! only reject a `.sz` bomb *after* decoding it. Both formats do declare a
//! per-unit size (the block header's varint; the chunk length), and the cap is
//! now checked against those *before* the offending output is decoded.
//!
//! These tests pin that:
//!
//! * a bomb under a small budget is rejected with
//!   [`SnappyError::TotalOutputExceeded`] **and never allocates anything close
//!   to its expansion** — proven with a heap-tracking global allocator;
//! * in-budget payloads still round-trip, including at the exact budget
//!   boundary (no false positives).

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use oxiarc_snappy::{
    FrameDecoder, FrameEncoder, SnappyError, compress, decompress, decompress_frame_with_limit,
    decompress_with_limit,
};

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

/// Uncompressed size of the bomb fixtures.
const BOMB_PLAIN_SIZE: usize = 8 * 1024 * 1024;

/// The budget every bomb is decoded under.
const BUDGET: usize = 64 * 1024;

/// Ceiling on what the decoder may allocate while rejecting a bomb.
///
/// The framed path decodes at most one 64 KiB chunk before the next chunk's
/// declared size trips the cap; the block path rejects straight from the
/// header. The ceiling sits far above both and two orders of magnitude below
/// the bomb's 8 MiB expansion, so a regression to "decode fully, then check"
/// fails here.
const ALLOC_CEILING: usize = 1024 * 1024;

/// A raw-block bomb: `BOMB_PLAIN_SIZE` zeros in the block format.
fn block_bomb() -> Vec<u8> {
    let plain = vec![0u8; BOMB_PLAIN_SIZE];
    compress(&plain)
}

/// A framed bomb: `BOMB_PLAIN_SIZE` zeros in the framing format.
fn frame_bomb() -> Vec<u8> {
    let plain = vec![0u8; BOMB_PLAIN_SIZE];
    let mut frame = Vec::new();
    {
        let mut encoder = FrameEncoder::new(&mut frame);
        encoder.write_all(&plain).expect("write bomb fixture");
        encoder.finish().expect("finish bomb fixture");
    }
    frame
}

/// Snapshot the allocation high-water mark; returns the baseline to subtract.
fn rebaseline_peak() -> usize {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    baseline
}

#[test]
fn block_bomb_rejected_from_the_header_without_allocating() {
    let bomb = block_bomb();
    assert!(
        bomb.len() < 512 * 1024,
        "fixture is not a bomb: {} compressed bytes",
        bomb.len()
    );

    let baseline = rebaseline_peak();
    let err = decompress_with_limit(&bomb, BUDGET).expect_err("block bomb must be rejected");
    let peak = PEAK.load(Ordering::Relaxed);

    match err {
        SnappyError::TotalOutputExceeded { produced, max } => {
            assert_eq!(
                produced, BOMB_PLAIN_SIZE as u64,
                "declared size in the error"
            );
            assert_eq!(max, BUDGET as u64);
        }
        other => panic!("expected TotalOutputExceeded, got {other:?}"),
    }

    let growth = peak.saturating_sub(baseline);
    assert!(
        growth < ALLOC_CEILING,
        "rejecting an {BOMB_PLAIN_SIZE}-byte block bomb under a {BUDGET}-byte budget allocated \
         {growth} bytes; the declared length is supposed to be checked before allocating"
    );
}

#[test]
fn frame_bomb_rejected_during_decode_without_allocating_the_expansion() {
    let bomb = frame_bomb();
    assert!(
        bomb.len() < 512 * 1024,
        "fixture is not a bomb: {} compressed bytes",
        bomb.len()
    );

    let baseline = rebaseline_peak();
    let err = decompress_frame_with_limit(&bomb, BUDGET as u64).expect_err("bomb must be rejected");
    let peak = PEAK.load(Ordering::Relaxed);

    assert!(
        matches!(err, SnappyError::TotalOutputExceeded { .. }),
        "expected TotalOutputExceeded, got {err:?}"
    );

    let growth = peak.saturating_sub(baseline);
    assert!(
        growth < ALLOC_CEILING,
        "rejecting an {BOMB_PLAIN_SIZE}-byte framed bomb under a {BUDGET}-byte budget allocated \
         {growth} bytes; the limit is supposed to be enforced *during* decoding"
    );
}

#[test]
fn frame_decoder_cap_rejects_the_same_bomb() {
    let bomb = frame_bomb();
    let mut decoder = FrameDecoder::new(&bomb[..]).with_max_output_size(BUDGET as u64);
    let mut output = Vec::new();
    let err = decoder
        .read_to_end(&mut output)
        .expect_err("FrameDecoder bomb must be rejected");
    assert!(
        err.to_string().contains("exceeds configured maximum"),
        "unexpected error: {err}"
    );
    assert!(
        output.len() <= BUDGET,
        "FrameDecoder produced {} bytes under a {BUDGET}-byte budget",
        output.len()
    );
}

#[test]
fn in_budget_payloads_still_round_trip() {
    let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();

    let block = compress(&data);
    assert_eq!(
        decompress_with_limit(&block, 1 << 20).expect("in-budget block decode"),
        data
    );
    // The unbounded entry point is unaffected.
    assert_eq!(decompress(&block).expect("unbounded block decode"), data);

    let mut frame = Vec::new();
    {
        let mut encoder = FrameEncoder::new(&mut frame);
        encoder.write_all(&data).expect("write");
        encoder.finish().expect("finish");
    }
    assert_eq!(
        decompress_frame_with_limit(&frame, 1 << 20).expect("in-budget frame decode"),
        data
    );
}

#[test]
fn budget_boundary_is_exact() {
    // Multi-chunk (>64 KiB) so the framed path exercises the cumulative cap.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();

    let block = compress(&data);
    assert_eq!(
        decompress_with_limit(&block, data.len()).expect("exact block budget must pass"),
        data
    );
    assert!(matches!(
        decompress_with_limit(&block, data.len() - 1),
        Err(SnappyError::TotalOutputExceeded { .. })
    ));

    let mut frame = Vec::new();
    {
        let mut encoder = FrameEncoder::new(&mut frame);
        encoder.write_all(&data).expect("write");
        encoder.finish().expect("finish");
    }
    assert_eq!(
        decompress_frame_with_limit(&frame, data.len() as u64).expect("exact frame budget"),
        data
    );
    let err = decompress_frame_with_limit(&frame, data.len() as u64 - 1)
        .expect_err("frame budget of len-1 must be rejected");
    match err {
        SnappyError::TotalOutputExceeded { produced, max } => {
            assert_eq!(max, data.len() as u64 - 1);
            assert!(produced > max, "produced {produced} must exceed max {max}");
        }
        other => panic!("expected TotalOutputExceeded, got {other:?}"),
    }
}

#[test]
fn empty_frame_fits_any_budget() {
    // `FrameEncoder` writes the stream identifier lazily, so an empty encode
    // is a zero-byte frame; the bounded decoder must accept it exactly as
    // `FrameDecoder` does, budget or not.
    let mut frame = Vec::new();
    {
        let encoder = FrameEncoder::new(&mut frame);
        encoder.finish().expect("finish empty frame");
    }
    assert!(frame.is_empty(), "empty encode should emit no bytes");
    assert!(
        decompress_frame_with_limit(&frame, 0)
            .expect("an empty frame produces no output")
            .is_empty()
    );

    let mut decoded = Vec::new();
    FrameDecoder::new(&frame[..])
        .read_to_end(&mut decoded)
        .expect("FrameDecoder agrees");
    assert!(decoded.is_empty());
}
