//! Steady-state allocation gate for the resumable inflate path.
//!
//! A zlib-shaped DEFLATE stream starts a new dynamic block — and therefore
//! three new Huffman decode tables — roughly every 16 383 symbols. A push
//! decoder that allocates while rebuilding those tables allocates forever, in
//! proportion to the body size, which defeats the whole point of a bounded
//! streaming decoder. This gate feeds a many-block stream through
//! [`InflateStream`] in fixed-size chunks and requires **zero** allocations
//! after warm-up.
//!
//! Everything lives in ONE `#[test]` on purpose: the counting allocator is
//! process-wide, so a second test in this binary would pollute the counters
//! under `cargo test` (which runs tests as threads rather than processes).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{Deflater, InflateStatus, InflateStream};

#[path = "common/blocks.rs"]
mod blocks;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// A pass-through allocator that counts allocation calls.
///
/// `unsafe` is unavoidable in a `GlobalAlloc` implementation and is confined
/// to this test binary; the crate itself contains no `unsafe`.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        out
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// A JSON-ish body: compressible enough that the encoder picks dynamic blocks,
/// varied enough that the three trees really differ from block to block.
fn json_body(target: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut out = Vec::with_capacity(target + 256);
    let mut id = 0u64;
    while out.len() < target {
        let r = next();
        out.extend_from_slice(
            &format!(
                "{{\"id\":{id},\"user\":\"user{}\",\"score\":{},\"tag\":\"{}\",\"ok\":{}}},\n",
                r % 100_000,
                r % 1_000,
                ["alpha", "beta", "gamma", "delta", "epsilon"][(r % 5) as usize],
                if r & 1 == 0 { "true" } else { "false" },
            )
            .into_bytes(),
        );
        id += 1;
    }
    out.truncate(target);
    out
}

#[test]
fn steady_state_inflate_allocates_nothing() {
    // ── Fixture ──────────────────────────────────────────────────────────
    // 16 MiB of JSON compresses to ~2 MiB of wire in ~80 dynamic blocks, so
    // the measured window (1 MiB of wire) crosses ~40 block headers rather
    // than none. Measured: body 16777216, wire 2061104, 80 blocks, 80 dynamic.
    let body = json_body(16 * 1024 * 1024);
    let mut encoder = Deflater::new(6);
    let wire = encoder.compress_to_vec(&body).expect("deflate");
    let walked = blocks::walk_blocks(&wire).expect("the fixture must be walkable");
    let dynamic = walked
        .iter()
        .filter(|b| b.btype == blocks::BlockType::Dynamic)
        .count();
    eprintln!(
        "fixture: body {} wire {} blocks {} dynamic {}",
        body.len(),
        wire.len(),
        walked.len(),
        dynamic
    );
    assert!(
        dynamic >= 40,
        "the fixture must cross many dynamic-block headers, got {dynamic} of {}",
        walked.len()
    );

    let mut stream = InflateStream::new();
    let mut out = vec![0u8; 64 * 1024];

    // ── Warm-up: every buffer the steady state uses is allocated here ────
    let chunk = 4096usize;
    let mut offset = 0usize;
    let mut decoded = 0usize;
    let drive = |stream: &mut InflateStream,
                 out: &mut [u8],
                 offset: &mut usize,
                 decoded: &mut usize|
     -> bool {
        let end = (*offset + chunk).min(wire.len());
        let mut input = &wire[*offset..end];
        *offset = end;
        loop {
            let progress = stream
                .inflate(input, out, FlushMode::None)
                .expect("inflate must not fail on our own encoder's output");
            *decoded += progress.produced;
            input = &input[progress.consumed..];
            if progress.status == InflateStatus::StreamEnd {
                return true;
            }
            if input.is_empty() && progress.produced == 0 {
                return false;
            }
        }
    };

    // Enough warm-up to cross several dynamic-block headers (the first block
    // header, the first sub-tabled tree, and the output window).
    for _ in 0..24 {
        assert!(
            !drive(&mut stream, &mut out, &mut offset, &mut decoded),
            "the fixture must be far longer than the warm-up"
        );
    }

    // ── Steady state ─────────────────────────────────────────────────────
    let before = allocations();
    let mut rounds = 0usize;
    for _ in 0..256 {
        if drive(&mut stream, &mut out, &mut offset, &mut decoded) {
            break;
        }
        rounds += 1;
    }
    let per_run = allocations() - before;
    assert_eq!(rounds, 256, "the fixture must supply 256 more chunks");
    assert_eq!(
        per_run,
        0,
        "InflateStream::inflate allocated {per_run} time(s) over {rounds} \
         steady-state calls covering {} wire bytes",
        rounds * chunk
    );

    // Drain the rest and check the decode was real, not a no-op that happened
    // to allocate nothing.
    while !drive(&mut stream, &mut out, &mut offset, &mut decoded) {}
    assert_eq!(decoded, body.len(), "the whole body must have been decoded");
}
