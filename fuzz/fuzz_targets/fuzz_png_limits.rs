//! Fuzz target proving `oxiarc_png::DecodeLimits` (and the sibling
//! `png`-shaped [`oxiarc_png::Limits`] budget) actually bound peak memory,
//! not merely that decoding a hostile file returns an error eventually.
//!
//! Every field of `DecodeLimits` is set to a small value — not just
//! `max_alloc_bytes` (repo-conventions §7 / critique §6.3's warning: that
//! field alone only bounds the final frame buffer via
//! `checked_output_buffer_size`; `max_chunk_len`, `max_text_bytes`,
//! `max_iccp_bytes` and `max_unknown_chunk_bytes` default to megabytes each
//! and would otherwise let a crafted file legitimately allocate well past
//! what this target's ceiling expects while still "behaving correctly").
//! The counting global allocator (`fuzz/support/counting_alloc.rs`, the
//! same pattern as `oxiarc-brotli`/`oxiarc-snappy`'s `tests/memory_limit.rs`)
//! then asserts peak growth for one decode never exceeds a ceiling derived
//! from the sum of every cap plus a fixed allowance for decoder bookkeeping
//! (the 32 KiB DEFLATE window, Huffman tables, chunk scratch) — the property
//! that matters is "far below what an unbounded decode of a claimed huge
//! image could reach", not an exact byte count.
#![no_main]

#[path = "../support/counting_alloc.rs"]
mod counting_alloc;

use libfuzzer_sys::fuzz_target;
use oxiarc_png::{DecodeLimits, DecodeOptions, Decoder, Limits};
use std::io::Cursor;

#[global_allocator]
static ALLOC: counting_alloc::PeakTrackingAlloc = counting_alloc::PeakTrackingAlloc;

/// Every `DecodeLimits` field set small, not just `max_alloc_bytes`.
fn tight_decode_limits() -> DecodeLimits {
    DecodeLimits::default()
        .with_max_width(200)
        .with_max_height(200)
        .with_max_pixels(200 * 200)
        .with_max_alloc_bytes(1 << 20) // 1 MiB: well over a 200x200 RGBA8 frame (160,000 B)
        .with_max_chunk_len(1 << 16) // 64 KiB
        .with_max_text_bytes(1 << 16)
        .with_max_iccp_bytes(1 << 16)
        .with_max_unknown_chunk_bytes(1 << 16)
        .with_max_frames(8)
        .with_max_total_frame_bytes(2 << 20) // 2 MiB across every APNG frame combined
}

/// `max_alloc_bytes` + the four 64 KiB chunk-shaped caps (`max_chunk_len`,
/// `max_text_bytes`, `max_iccp_bytes`, `max_unknown_chunk_bytes`) + a
/// generous fixed allowance for decoder bookkeeping that is not itself one
/// of the counted fields (the DEFLATE window, Huffman tables, `Vec` growth
/// overshoot, small transient copies). Deliberately excludes
/// `max_total_frame_bytes`: this harness calls `next_frame` exactly once and
/// never advances to a second APNG frame, so that budget can never bind
/// here — including it in the sum would inflate the ceiling without the
/// call shape to ever test it. Comfortably below the multi-hundred-MiB to
/// multi-GiB an *uncapped* decode of a claimed-huge `IHDR` or a zlib bomb
/// could otherwise reach, so a regression that silently drops one of the
/// caps actually in play here still trips it.
const PEAK_CEILING: usize = (1 << 20) + 4 * (1 << 16) + (1 << 20);

fuzz_target!(|data: &[u8]| {
    // Re-baseline on *every* call: libFuzzer runs this closure tens of
    // thousands of times in one process, and the harness itself allocates
    // between iterations — a `static` baseline computed once would
    // accumulate every earlier iteration's growth into this one's budget.
    let baseline = counting_alloc::baseline();
    counting_alloc::reset_peak_to_current();

    let mut options = DecodeOptions::default();
    options.set_limits(tight_decode_limits());
    let mut decoder = Decoder::new_with_options(Cursor::new(data), options);
    decoder.set_limits(Limits { bytes: 1 << 20 });

    if let Ok(mut reader) = decoder.read_info() {
        if let Ok(size) = reader.checked_output_buffer_size() {
            let mut buf = vec![0u8; size];
            let _ = reader.next_frame(&mut buf);
        }
    }

    let growth = counting_alloc::peak_growth_since(baseline);
    assert!(
        growth <= PEAK_CEILING,
        "decoding under tight DecodeLimits allocated {growth} bytes, over the \
         {PEAK_CEILING}-byte ceiling derived from those same limits — a cap is not \
         being enforced before the corresponding allocation"
    );
});
