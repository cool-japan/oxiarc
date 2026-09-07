//! A reusable peak-tracking global allocator, for fuzz targets (and, by
//! copying this file, any test binary) that must assert a decoder's memory
//! use stays bounded by a declared limit rather than by the size of a
//! hostile input.
//!
//! This is the same shape as `oxiarc-brotli/tests/memory_limit.rs` and
//! `oxiarc-snappy/tests/memory_limit.rs` (the two existing "reject the bomb
//! without allocating it" regression tests), extracted once so a third copy
//! does not have to be typed by hand — see `critique.md` §6.3 ("Counting-
//! allocator harness is assumed by four reports ... and specified by none.
//! One `#[global_allocator]` per test binary; write it once and copy it.").
//!
//! # Why this cannot be one shared `static` across every target
//!
//! `#[global_allocator]` is a whole-binary decision, and every fuzz target in
//! this crate is its own `[[bin]]`. There is no way for a library to install
//! a global allocator on a downstream binary's behalf, so each target that
//! needs one still declares its own `#[global_allocator]` static — what this
//! module removes is having to retype `unsafe impl GlobalAlloc` and the
//! atomics behind it every time. Pull it in with:
//!
//! ```ignore
//! #[path = "../support/counting_alloc.rs"]
//! mod counting_alloc;
//!
//! #[global_allocator]
//! static ALLOC: counting_alloc::PeakTrackingAlloc = counting_alloc::PeakTrackingAlloc;
//! ```
//!
//! # Why the baseline must be reset every call, here specifically
//!
//! A `#[test]` function runs once per process, so `oxiarc-brotli`'s copy
//! re-baselines exactly once, right before the operation under test. A
//! libFuzzer `fuzz_target!` body, by contrast, runs tens of thousands of
//! times **in the same process** — the harness itself allocates through this
//! same global allocator between iterations (to hand the closure its `data`
//! slice, grow internal buffers, etc.). Call [`baseline`] then
//! [`reset_peak_to_current`] at the top of *every* `fuzz_target!` body, do the
//! work, then read [`peak_growth_since`] with that call's own baseline —
//! never a `static` baseline computed once at start-up, which would
//! accumulate every previous iteration's growth into the next one's
//! assertion.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Live heap bytes right now, tracked across every allocation/deallocation
/// that goes through [`PeakTrackingAlloc`].
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// High-water mark of [`LIVE`] since the last [`reset_peak_to_current`].
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// A pass-through [`GlobalAlloc`] that records live-byte growth and its
/// high-water mark, so a caller can assert "this operation never
/// materialised more than N bytes" instead of only checking its return
/// value.
///
/// Install with `#[global_allocator]`; see the module docs for the exact
/// `#[path]` incantation each fuzz target uses to share this one copy.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeakTrackingAlloc;

// SAFETY: every method is a thin, correctly-sized pass-through to `System`
// (the platform default allocator), with the accounting done on the sizes
// `System` itself was given — no pointer arithmetic, no manual layout
// construction, nothing that could hand back a pointer `System` did not
// itself allocate for exactly that `Layout`.
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

/// Add `bytes` to the live total and lift the high-water mark if this pushed
/// it past its previous value.
fn record_growth(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

/// The current live-byte count, to remember as a baseline before an
/// operation under test.
#[must_use]
pub fn baseline() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Pull the high-water mark down to the current live count, so growth
/// measured after this point reflects only what happens next.
///
/// Call this immediately after recording [`baseline`] and immediately before
/// the operation under test — anything allocated by fixture setup must not
/// count against the operation's own budget.
pub fn reset_peak_to_current() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// How far the high-water mark rose above `baseline` since the last
/// [`reset_peak_to_current`].
#[must_use]
pub fn peak_growth_since(baseline: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}
