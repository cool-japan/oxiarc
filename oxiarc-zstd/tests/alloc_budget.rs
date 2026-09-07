//! Allocation budget for the bounded [`ZstdStream`] push decoder.
//!
//! A counting global allocator can only be installed once per test binary, so
//! this suite is deliberately its own binary and nothing else in the crate
//! installs one.
//!
//! Two claims are pinned here:
//!
//! 1. **Zero steady-state allocations.** Once a stream is warmed up (window
//!    grown, buffers sized), further `decode` calls over `Raw`/`RLE` blocks
//!    allocate nothing at all.
//! 2. **Allocations are per-block, not per-call.** For `Compressed` blocks the
//!    decoder still allocates when a block ships a *new* Huffman tree or a new
//!    FSE table description — that is inherent, one small `Vec` per table. What
//!    must never happen is an allocation that scales with how the caller
//!    chunked the input, which is what a decoder that rebuilt state per call
//!    would show. Feeding the same frame in 1-byte and in 64 KiB pieces must
//!    therefore allocate exactly the same number of times.

use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream, compress_with_level};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Number of allocations made while counting is armed.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// Total bytes requested while counting is armed.
static BYTES: AtomicU64 = AtomicU64::new(0);
/// Whether counting is armed.
static ARMED: AtomicBool = AtomicBool::new(false);

/// A pass-through allocator that counts allocations while armed.
struct Counting;

// SAFETY: every method forwards unchanged to the system allocator; the counters
// are plain atomics and do not affect the returned pointers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` with allocation counting armed; returns `(allocations, bytes, r)`.
///
/// The counters are process-global, so this binary deliberately exposes a
/// **single** `#[test]`: a second test running concurrently would have its own
/// allocations charged to whichever measurement happened to be armed.
fn measure<R>(f: impl FnOnce() -> R) -> (u64, u64, R) {
    ALLOCS.store(0, Ordering::SeqCst);
    BYTES.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    let r = f();
    ARMED.store(false, Ordering::SeqCst);
    (
        ALLOCS.load(Ordering::SeqCst),
        BYTES.load(Ordering::SeqCst),
        r,
    )
}

/// Feed `frame` to `stream` in `in_chunk` pieces, writing into `scratch`.
fn pump(stream: &mut ZstdStream, frame: &[u8], in_chunk: usize, scratch: &mut [u8]) -> usize {
    let mut pos = 0usize;
    let mut produced = 0usize;
    loop {
        let end = pos.saturating_add(in_chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let p = stream
            .decode(&frame[pos..end], scratch, flush)
            .expect("decode must succeed");
        pos += p.consumed;
        produced += p.produced;
        if p.status == ZstdStatus::StreamEnd {
            return produced;
        }
    }
}

/// Build a frame made entirely of `Raw` blocks by compressing incompressible
/// data at level 0 (the encoder's raw/RLE block path).
fn raw_block_frame(size: usize) -> Vec<u8> {
    let mut x: u32 = 0xDEAD_BEEF;
    let data: Vec<u8> = (0..size)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    compress_with_level(&data, 0).expect("compress")
}

/// Steady state over `Raw`/`RLE` blocks allocates nothing at all.
///
/// `reset()` keeps every buffer, so a warmed-up decoder re-run over the same
/// shape of data must not touch the allocator once.
fn steady_state_over_raw_blocks_allocates_nothing() {
    let frame = raw_block_frame(1 << 20);
    let mut joined = frame.clone();
    joined.extend_from_slice(&frame);
    joined.extend_from_slice(&frame);

    let mut scratch = vec![0u8; 64 * 1024];
    let mut stream = ZstdStream::new().with_max_window(usize::MAX);

    // Warm-up pass: grows the window and sizes the internal buffers.
    let warm = pump(&mut stream, &joined, 64 * 1024, &mut scratch);
    assert_eq!(warm, 3 << 20);

    for chunk in [64 * 1024usize, 4096, 97] {
        stream.reset();
        let (allocs, bytes, produced) = measure(|| pump(&mut stream, &joined, chunk, &mut scratch));
        assert_eq!(produced, 3 << 20);
        assert_eq!(
            allocs, 0,
            "steady state with {chunk}-byte chunks allocated {allocs} times ({bytes} bytes)"
        );
    }
}

/// Allocation count is a function of the frame, not of the chunk schedule.
fn allocations_are_per_block_not_per_call() {
    let data = "structured record: alpha=1 beta=22 gamma=333 delta=4444 "
        .repeat(20_000)
        .into_bytes();
    let frame = compress_with_level(&data, 6).expect("compress");
    let mut scratch = vec![0u8; 64 * 1024];

    let schedule = [64 * 1024usize, 4096, 64, 1];
    let mut stream = ZstdStream::new().with_max_window(usize::MAX);

    // Warm-up: run every schedule once. Small chunks force the block carry to
    // its 128 KiB high-water mark, which large chunks never touch (they decode
    // straight out of the caller's slice); measuring before that would compare
    // one-off buffer growth rather than steady-state behaviour.
    for chunk in schedule {
        stream.reset();
        let warm = pump(&mut stream, &frame, chunk, &mut scratch);
        assert_eq!(warm, data.len());
    }

    let mut counts = Vec::new();
    for chunk in schedule {
        stream.reset();
        let (allocs, _bytes, produced) = measure(|| pump(&mut stream, &frame, chunk, &mut scratch));
        assert_eq!(produced, data.len());
        counts.push((chunk, allocs));
    }

    let baseline = counts[0].1;
    for (chunk, allocs) in &counts {
        assert_eq!(
            *allocs, baseline,
            "chunk size {chunk} allocated {allocs} times but 64 KiB chunks allocated {baseline}: \
             allocation count must not depend on how the caller splits the input"
        );
    }
    // And the per-frame count is small: a handful of entropy tables per block,
    // not one allocation per byte.
    let blocks = data.len().div_ceil(oxiarc_zstd::MAX_BLOCK_SIZE);
    assert!(
        baseline <= 8 * blocks as u64 + 8,
        "{baseline} allocations for {blocks} blocks is more than a few tables per block"
    );
    eprintln!(
        "[alloc] {baseline} allocations for a {blocks}-block frame, chunk-schedule invariant"
    );
}

/// A bomb rejected by the output budget never allocates a large window.
fn rejected_bomb_stays_within_its_budget() {
    let bomb = compress_with_level(&vec![0u8; 8 << 20], 3).expect("compress");
    let mut scratch = vec![0u8; 64 * 1024];
    let mut stream = ZstdStream::new()
        .with_max_window(usize::MAX)
        .with_max_output(64 * 1024);

    let (_allocs, bytes, ()) = measure(|| {
        let mut pos = 0usize;
        loop {
            let end = pos.saturating_add(64 * 1024).min(bomb.len());
            let flush = if end == bomb.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            match stream.decode(&bomb[pos..end], &mut scratch, flush) {
                Ok(p) => {
                    pos += p.consumed;
                    if p.status == ZstdStatus::StreamEnd {
                        panic!("bomb must be rejected");
                    }
                }
                Err(_) => return,
            }
        }
    });
    // Window + carry + literals for a 64 KiB budget: well under a megabyte.
    assert!(
        bytes < 1 << 20,
        "rejected bomb allocated {bytes} bytes; the budget must bound the window"
    );
    assert!(stream.window_size() <= 64 * 1024 + oxiarc_zstd::MAX_BLOCK_SIZE + 8);
}

/// The whole allocation budget, run sequentially in one test so that no other
/// test's allocations can be charged to an armed measurement.
#[test]
fn allocation_budget() {
    steady_state_over_raw_blocks_allocates_nothing();
    allocations_are_per_block_not_per_call();
    rejected_bomb_stays_within_its_budget();
}
