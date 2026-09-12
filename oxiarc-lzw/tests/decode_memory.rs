//! Heap-budget regression tests for the TIFF/GIF LZW decode paths.
//!
//! Three claims made in the 0.4.2 docs are load-bearing and are pinned here
//! with a heap-tracking global allocator, because none of them can be seen
//! from the return value:
//!
//! 1. **`expected_size` is untrusted.** `LzwDecoder::decode` takes a
//!    caller-supplied output size that, for a TIFF strip, comes from the
//!    file's own geometry tags. `decoder.rs`'s `VecSink` documents that it
//!    zero-fills a block at a time rather than reserving the declared size,
//!    so a two-byte stream that claims to expand to 2^63 bytes must not be
//!    able to force a large allocation.
//! 2. **`decompress_tiff_into` allocates nothing but the code table.** The
//!    crate README calls it "zero-allocation strip decode"; that is only
//!    true if the per-strip cost is the table and nothing that scales with
//!    the output.
//! 3. **Reusing one `LzwDecoder` across strips costs no allocation.** The
//!    rustdoc on `LzwDecoder` tells callers with many strips to reuse one
//!    decoder because "every decode begins with a table reset, which is
//!    O(1)". If a reset reallocated, that advice would be wrong.
//!
//! The harness mirrors `tests/z_memory.rs` (one `#[global_allocator]` per
//! test binary, a mutex so two measurements never overlap).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use oxiarc_lzw::{
    LzwConfig, LzwDecoder, compress_tiff, decompress_tiff, decompress_tiff_into, gif_compress,
    gif_decompress,
};

/// Live heap bytes, and the high-water mark of the same.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Serialises the measurements: `#[global_allocator]` is process-wide, so
/// two tests measuring at once would see each other's allocations under
/// `cargo test` (nextest gives every test its own process, but this suite
/// has to be right under both).
static MEASURING: Mutex<()> = Mutex::new(());

/// A pass-through allocator that records the peak live heap size.
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

/// Take the measurement lock for the whole test body, fixture construction
/// included.
fn exclusive() -> MutexGuard<'static, ()> {
    MEASURING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Reset the high-water mark to "now" and return the baseline to subtract.
fn rebaseline() -> usize {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    baseline
}

/// Peak growth since [`rebaseline`].
fn peak_growth(baseline: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}

/// The 12-bit TIFF code table: 4 096 entries of eight bytes.
const TIFF_TABLE_BYTES: usize = 4_096 * 8;

/// A tiny valid strip must stay tiny no matter what `expected_size` claims.
///
/// This is the decompression-bomb shape for a TIFF reader: `StripByteCounts`
/// says the strip is a few bytes, but `ImageWidth * ImageLength * ...` says
/// it expands to petabytes. The decoder must charge for bytes it actually
/// produced.
#[test]
fn a_lying_expected_size_cannot_force_a_large_allocation() {
    let _guard = exclusive();
    let raw = b"a small strip that decodes to well under a kilobyte".to_vec();
    let strip = compress_tiff(&raw).expect("compress a small strip");

    for declared in [
        1_usize << 20,
        1 << 30,
        1 << 40,
        usize::MAX / 4,
        usize::MAX / 2,
    ] {
        let baseline = rebaseline();
        let decoded = decompress_tiff(&strip, declared).expect("decode with a huge declared size");
        let peak = peak_growth(baseline);
        assert_eq!(decoded, raw, "declared {declared}: wrong bytes");
        // The table (32 KiB) plus one 8 KiB output block, with room for the
        // returned `Vec` still being alive. Nothing here may scale with
        // `declared`.
        assert!(
            peak < TIFF_TABLE_BYTES + 128 * 1024,
            "declared {declared}: peak heap {peak} bytes — the declared size \
             is being pre-allocated"
        );
    }
}

/// The same for the growable path on a real payload: allocation must track
/// the bytes produced, not the ceiling handed in.
#[test]
fn the_growable_sink_charges_for_bytes_produced_not_bytes_declared() {
    let _guard = exclusive();
    let raw = vec![0x5Au8; 64 * 1024];
    let strip = compress_tiff(&raw).expect("compress a 64 KiB run");

    let baseline = rebaseline();
    let decoded = decompress_tiff(&strip, usize::MAX / 2).expect("decode with no real ceiling");
    let peak = peak_growth(baseline);
    assert_eq!(decoded.len(), raw.len());
    assert_eq!(decoded, raw);
    // 64 KiB of output, grown by doubling (so a 32 KiB + 64 KiB overlap
    // during the last `resize`), plus the 32 KiB table.
    assert!(
        peak < 512 * 1024,
        "peak heap {peak} bytes for 64 KiB of output — the sink is not \
         growing incrementally"
    );
}

/// `decompress_tiff_into` must allocate the code table and nothing that
/// scales with the strip: the caller already owns the output buffer.
#[test]
fn the_slice_sink_allocates_only_the_code_table() {
    let _guard = exclusive();
    let raw = vec![0x11u8; 256 * 1024];
    let strip = compress_tiff(&raw).expect("compress a 256 KiB run");
    let mut out = vec![0u8; raw.len()];

    let baseline = rebaseline();
    let written = decompress_tiff_into(&strip, &mut out).expect("decode into a caller buffer");
    let peak = peak_growth(baseline);
    assert_eq!(written, raw.len());
    assert_eq!(out, raw);
    assert!(
        peak < TIFF_TABLE_BYTES * 2,
        "peak heap {peak} bytes for a 256 KiB strip — `decompress_tiff_into` \
         is allocating more than the code table"
    );
}

/// One decoder reused across many strips must allocate once, not once per
/// strip: that is exactly what the `LzwDecoder` rustdoc promises callers
/// with many strips, and what makes a table reset O(1).
#[test]
fn reusing_one_decoder_across_strips_allocates_nothing_per_strip() {
    let _guard = exclusive();
    let strips: Vec<Vec<u8>> = (0..64u8)
        .map(|index| {
            let payload: Vec<u8> = (0..4_096u32)
                .map(|position| (position as u8).wrapping_add(index))
                .collect();
            compress_tiff(&payload).expect("compress a strip")
        })
        .collect();
    let mut out = vec![0u8; 4_096];

    let mut decoder = LzwDecoder::new(LzwConfig::TIFF).expect("build a decoder");
    // Warm the table allocation up outside the measured window.
    decoder
        .decode_into(&strips[0], &mut out)
        .expect("first decode");

    let baseline = rebaseline();
    for (index, strip) in strips.iter().enumerate() {
        let written = decoder
            .decode_into(strip, &mut out)
            .unwrap_or_else(|e| panic!("strip {index} failed: {e}"));
        assert_eq!(written, 4_096, "strip {index}");
    }
    let peak = peak_growth(baseline);
    assert!(
        peak < 4_096,
        "peak heap {peak} bytes over 64 reused decodes — a table reset is \
         allocating"
    );
}

/// GIF image data carries no uncompressed length, so `gif_decompress` grows
/// its own buffer. It must still charge for bytes produced: a crafted
/// stream is bounded by the 12-bit table's longest string, and the working
/// set must stay proportional to the output rather than jumping to a
/// speculative ceiling.
#[test]
fn gif_decompress_allocation_tracks_the_bytes_it_produces() {
    let _guard = exclusive();

    // A tiny stream: the output is a few bytes, so the heap cost must be
    // the 4 096-entry table and one small block, not a speculative ceiling.
    let small = gif_compress(b"abcabcabcabc", 8).expect("compress a tiny GIF payload");
    let baseline = rebaseline();
    let decoded = gif_decompress(&small, 8).expect("decode a tiny GIF payload");
    let peak = peak_growth(baseline);
    assert_eq!(decoded, b"abcabcabcabc");
    assert!(
        peak < TIFF_TABLE_BYTES + 128 * 1024,
        "peak heap {peak} bytes for a 12-byte GIF payload"
    );

    // A 256 KiB payload: still linear in the output.
    let raw = vec![0x77u8; 256 * 1024];
    let stream = gif_compress(&raw, 8).expect("compress a 256 KiB GIF payload");
    let baseline = rebaseline();
    let decoded = gif_decompress(&stream, 8).expect("decode a 256 KiB GIF payload");
    let peak = peak_growth(baseline);
    assert_eq!(decoded, raw);
    assert!(
        peak < 2 * 1024 * 1024,
        "peak heap {peak} bytes for 256 KiB of GIF output"
    );
}
