//! Heap-budget regression tests for the `.Z` (`oxiarc_lzw::z`) codec.
//!
//! History (2026-09-08, found by the LZW3 verification pass): [`ZReader`]
//! documented a bounded working set — "at most 16 KiB of compressed input at
//! a time" — but the push decoder measured its bit position from the last
//! *code-width change*, so it retained every compressed byte since that
//! event. On a 16-bit stream the width stops changing early, and in
//! non-block mode there are no `CLEAR` codes at all, so the retained tail was
//! the whole body: **20.5 MB of live heap for a 5.2 MB stream**, re-copied on
//! every chunk (quadratic — 8 MiB of payload took 762 ms instead of 107 ms).
//! `with_max_output` did not help: it bounds *output*, not the compressed
//! carry. That is a remote-input memory amplification for anything decoding
//! `Content-Encoding: compress` off the wire.
//!
//! These tests pin the fix with a heap-tracking global allocator:
//!
//! * the reader's working set does not grow with the length of the stream;
//! * a bounded reader never materialises a bomb;
//! * the 16-bit code tables cost what the module docs claim they cost.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use std::io::Write;

use oxiarc_lzw::z::{ZReader, ZWriter, compress, compress_with_block_mode, decompress_with_limit};

/// Live heap bytes, and the high-water mark of the same.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Serialises the measurements: `#[global_allocator]` is process-wide, so two
/// tests measuring at once would see each other's allocations under
/// `cargo test` (nextest gives every test its own process, but this suite has
/// to be right under both).
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

/// Take the measurement lock for the whole test body.
///
/// It has to cover fixture construction too, not just the measured window:
/// another test allocating a 32 MiB payload on a second thread would show up
/// in this one's high-water mark.
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

/// A deterministic xorshift byte stream: incompressible, so the `.Z` stream
/// is *larger* than the payload and the code width reaches its ceiling fast.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// Ceiling on the reader's working set, whatever the stream length.
///
/// The honest cost is the 16-bit code table (~193 KiB) plus the 16 KiB input
/// chunk plus one chunk's expansion. 2 MiB leaves generous headroom and is
/// still an order of magnitude below the smallest stream measured here, so a
/// regression to "retain the whole body" cannot slip through.
const READER_CEILING: usize = 2 * 1024 * 1024;

#[test]
fn the_reader_working_set_does_not_scale_with_the_stream() {
    let _guard = exclusive();
    let mut peaks = Vec::new();
    for megabytes in [4usize, 16] {
        let payload = noise(megabytes * 1024 * 1024, 0x9E37_79B9);
        // Non-block mode is the worst case: no `CLEAR` code ever fires, so
        // nothing but the (long-finished) width growth can move the group
        // origin.
        let stream =
            compress_with_block_mode(&payload, 16, false).expect("compress the noise payload");
        assert!(
            stream.len() > megabytes * 1024 * 1024,
            "noise must not compress: {} bytes",
            stream.len()
        );

        let mut sink = std::io::sink();
        let baseline = rebaseline();
        let decoded = std::io::copy(&mut ZReader::new(&stream[..]), &mut sink).expect("decode");
        let growth = peak_growth(baseline);

        assert_eq!(decoded, payload.len() as u64);
        assert!(
            growth < READER_CEILING,
            "a {}-byte stream made ZReader allocate {growth} bytes; the reader is supposed to \
             hold one 16 KiB chunk, not the body",
            stream.len()
        );
        peaks.push((stream.len(), growth));
    }

    // The 16 MiB stream is 4x the 4 MiB one; the working set must not be.
    let (small_len, small_peak) = peaks[0];
    let (large_len, large_peak) = peaks[1];
    assert!(
        large_peak < small_peak.saturating_mul(2).saturating_add(64 * 1024),
        "working set grew with the stream: {small_peak} bytes for {small_len} compressed bytes, \
         {large_peak} for {large_len}"
    );
}

#[test]
fn a_bounded_reader_never_materialises_a_bomb() {
    let _guard = exclusive();
    // 32 MiB of one byte is a few kilobytes of 16-bit codes.
    let payload = vec![b'q'; 32 * 1024 * 1024];
    let bomb = compress(&payload, 16).expect("compress the bomb");
    assert!(
        bomb.len() < 32 * 1024,
        "fixture is not a bomb: {}",
        bomb.len()
    );

    let mut out = Vec::new();
    let baseline = rebaseline();
    let error = ZReader::new(&bomb[..])
        .with_max_output(64 * 1024)
        .read_to_end(&mut out)
        .expect_err("the cap must fire");
    let growth = peak_growth(baseline);

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(out.len() <= 64 * 1024, "produced {} bytes", out.len());
    assert!(
        growth < READER_CEILING,
        "rejecting a 32 MiB bomb under a 64 KiB cap allocated {growth} bytes"
    );
}

#[test]
fn decompress_with_limit_never_materialises_a_bomb() {
    let _guard = exclusive();
    let payload = vec![0u8; 32 * 1024 * 1024];
    let bomb = compress(&payload, 16).expect("compress the bomb");

    let baseline = rebaseline();
    let result = decompress_with_limit(&bomb, 64 * 1024);
    let growth = peak_growth(baseline);

    assert!(result.is_err(), "a 32 MiB expansion must not fit in 64 KiB");
    assert!(
        growth < READER_CEILING,
        "rejecting a 32 MiB bomb under a 64 KiB cap allocated {growth} bytes"
    );
}

#[test]
fn the_sixteen_bit_tables_cost_what_the_docs_say() {
    let _guard = exclusive();
    // Pins the figures quoted in `oxiarc_lzw::z`'s module docs, so a change
    // to either table's representation has to update the prose too.
    let tiny = b"a handful of bytes".repeat(4);

    let baseline = rebaseline();
    let stream = compress(&tiny, 16).expect("compress");
    let encoder_growth = peak_growth(baseline);

    let baseline = rebaseline();
    let decoded = decompress_with_limit(&stream, 1 << 20).expect("decompress");
    let decoder_growth = peak_growth(baseline);

    assert_eq!(decoded, tiny);
    // Decoder: prefix (65 536 x u16) + suffix (65 536 x u8) = 192 KiB, plus a
    // 1 KiB stack and the output.
    assert!(
        (150 * 1024..300 * 1024).contains(&decoder_growth),
        "decoder table measured {decoder_growth} bytes; the docs say ~193 KiB"
    );
    // Encoder: 131 072 index slots of u32 + u16 = 768 KiB.
    assert!(
        (600 * 1024..1024 * 1024).contains(&encoder_growth),
        "encoder table measured {encoder_growth} bytes; the docs say ~768 KiB"
    );
}

#[test]
fn the_writer_staging_buffer_does_not_scale_with_one_write_call() {
    let _guard = exclusive();
    // `ZWriter::write` used to run the whole slice through the encoder
    // before checking whether to hand anything to the inner writer, so a
    // single `write_all` of an incompressible megabyte staged the entire
    // compressed result. It now pushes in bounded pieces.
    let payload = noise(4 * 1024 * 1024, 0x1234_5678);

    let baseline = rebaseline();
    let mut writer = ZWriter::new(std::io::sink(), 16).expect("writer");
    writer.write_all(&payload).expect("write");
    writer.finish().expect("finish");
    let growth = peak_growth(baseline);

    // Encoder index (~768 KiB) + one staging batch + slack. Measured here:
    // 851 968 bytes; before the fix it was that plus the whole ~5.2 MB of
    // compressed output.
    assert!(
        growth < 2 * 1024 * 1024,
        "one 4 MiB write staged {growth} bytes; the batch is supposed to be ~32 KiB"
    );

    // ...and the bytes are still exactly the one-shot encoder's.
    let mut collected = ZWriter::new(Vec::new(), 16).expect("writer");
    collected.write_all(&payload).expect("write");
    assert_eq!(
        collected.finish().expect("finish"),
        compress(&payload, 16).expect("one shot"),
        "bounded staging must not change a byte"
    );
}
