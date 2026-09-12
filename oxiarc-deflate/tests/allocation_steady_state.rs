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

/// A stream whose Huffman trees change shape from block to block.
///
/// The JSON fixture's blocks all look alike, so a decoder that sized its
/// buffers for the first block would still pass. This one alternates between
/// four populations with very different alphabets and match profiles, forcing
/// `HLIT`/`HDIST` and the deepest code length to move between blocks — the
/// case where a `resize` that grows would show up.
fn shape_shifting_body(target: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut out = Vec::with_capacity(target + 4096);
    let mut phase = 0usize;
    while out.len() < target {
        // ~40 KiB per phase: comfortably more than one 16 383-symbol block.
        let end = out.len() + 40 * 1024;
        match phase % 4 {
            // Full 8-bit alphabet, incompressible: deep literal code, no
            // distances.
            0 => {
                while out.len() < end {
                    out.extend_from_slice(&next().to_le_bytes());
                }
            }
            // Two symbols: a 1-bit literal code, HLIT at its minimum.
            1 => {
                while out.len() < end {
                    out.push(if next() & 1 == 0 { b'A' } else { b'B' });
                }
            }
            // Long runs: distance code 0 dominates, lengths spread wide.
            2 => {
                while out.len() < end {
                    let run = (next() % 300) as usize + 3;
                    let byte = (next() % 251) as u8;
                    out.extend(std::iter::repeat_n(byte, run));
                }
            }
            // Text with far-apart repeats: the whole distance alphabet.
            _ => {
                let words: [&[u8]; 8] = [
                    b"alpha ",
                    b"bravo ",
                    b"charlie ",
                    b"delta ",
                    b"echo ",
                    b"foxtrot ",
                    b"golf ",
                    b"hotel ",
                ];
                while out.len() < end {
                    out.extend_from_slice(words[(next() % 8) as usize]);
                }
            }
        }
        phase += 1;
    }
    out.truncate(target);
    out
}

/// Drive one decoder over one wire, in `chunk`-sized pieces, and return the
/// number of allocations made after `warmup` chunks.
fn steady_state_allocations(
    wire: &[u8],
    chunk: usize,
    out_len: usize,
    warmup: usize,
    rounds: usize,
) -> (usize, usize) {
    let mut stream = InflateStream::new();
    let mut out = vec![0u8; out_len];
    let mut offset = 0usize;
    let mut decoded = 0usize;

    let step = |stream: &mut InflateStream,
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

    for _ in 0..warmup {
        assert!(
            !step(&mut stream, &mut out, &mut offset, &mut decoded),
            "the fixture must be far longer than the warm-up"
        );
    }

    let before = allocations();
    let mut done = 0usize;
    for _ in 0..rounds {
        if step(&mut stream, &mut out, &mut offset, &mut decoded) {
            break;
        }
        done += 1;
    }
    let per_run = allocations() - before;
    assert_eq!(done, rounds, "the fixture must supply {rounds} more chunks");

    while !step(&mut stream, &mut out, &mut offset, &mut decoded) {}
    (per_run, decoded)
}

#[test]
fn steady_state_inflate_allocates_nothing() {
    // ── Fixture ──────────────────────────────────────────────────────────
    // 16 MiB of JSON compresses to ~2 MiB of wire in ~80 dynamic blocks, so
    // the measured window (1 MiB of wire) crosses ~40 block headers rather
    // than none. Measured: body 16777216, wire 2038448, 78 blocks, 78 dynamic.
    let cases: [(&str, Vec<u8>); 2] = [
        ("json", json_body(16 * 1024 * 1024)),
        ("shape-shifting", shape_shifting_body(8 * 1024 * 1024)),
    ];

    for (name, body) in cases {
        let mut encoder = Deflater::new(6);
        let wire = encoder.compress_to_vec(&body).expect("deflate");
        let walked = blocks::walk_blocks(&wire).expect("the fixture must be walkable");
        let dynamic = walked
            .iter()
            .filter(|b| b.btype == blocks::BlockType::Dynamic)
            .count();
        eprintln!(
            "{name}: body {} wire {} blocks {} dynamic {}",
            body.len(),
            wire.len(),
            walked.len(),
            dynamic
        );
        assert!(
            dynamic >= 40,
            "{name}: the fixture must cross many dynamic-block headers, \
             got {dynamic} of {}",
            walked.len()
        );

        // Several (chunk, output-buffer) shapes: the history buffer's
        // eviction path is only reached when the produced size varies, and a
        // 1-byte output buffer forces the per-symbol resumable path rather
        // than the bulk one.
        // Average wire bytes per dynamic block: the measured window of every
        // shape below must span several of these, or the gate would pass
        // vacuously by never crossing a Huffman header — which is the exact
        // thing it exists to catch.
        let per_block = wire.len() / dynamic;

        for (chunk, out_len, warmup_bytes) in [
            // `warmup_bytes` must cover every tree shape the fixture uses (the
            // shape-shifting body cycles through four populations), because a
            // deeper tree than any seen so far legitimately grows the decode
            // table once. What must never happen is a growth *per block*.
            (4096usize, 64 * 1024usize, 800 * 1024usize),
            (64 * 1024, 4096, 768 * 1024),
            (997, 1, 800 * 1024),
            (1, 64 * 1024, 800 * 1024),
        ] {
            let warmup = warmup_bytes.div_ceil(chunk);
            // Leave one chunk unmeasured so the end-of-stream call (which ends
            // the loop early) never lands inside the measured window.
            let chunks = wire.len() / chunk;
            assert!(
                chunks > warmup + 2,
                "{name}: {chunk}-byte chunks give only {chunks} of them"
            );
            let rounds = chunks - warmup - 1;
            assert!(
                rounds * chunk >= 3 * per_block,
                "{name}: a measured window of {} bytes ({rounds} x {chunk}) does \
                 not span 3 of the fixture's {per_block}-byte blocks, so the \
                 gate would never cross a Huffman header",
                rounds * chunk
            );
            let (per_run, decoded) =
                steady_state_allocations(&wire, chunk, out_len, warmup, rounds);
            assert_eq!(
                per_run, 0,
                "{name}: InflateStream::inflate allocated {per_run} time(s) over \
                 {rounds} steady-state calls of {chunk} bytes into a \
                 {out_len}-byte buffer"
            );
            assert_eq!(
                decoded,
                body.len(),
                "{name}: the whole body must have been decoded"
            );
        }
    }
}
