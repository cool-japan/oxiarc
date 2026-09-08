//! Memory gates, measured with a counting global allocator.
//!
//! Two numeric claims this crate makes are only meaningful if they are
//! measured:
//!
//! 1. **Peak memory is bounded by the staging buffers plus the codec
//!    window**, not by the body size. A decoder that quietly buffered to EOF
//!    would pass every correctness test in this suite and fail here.
//! 2. **`feed_into` does not allocate in the steady state.** Every buffer it
//!    needs is allocated once and reused.
//!
//! 3. **A `.Z` (`compress`) body is metered too.** `oxiarc_lzw::z::ZReader`
//!    decodes a whole pull of compressed input into a `Vec` of its own
//!    before serving the first byte of it, so peak memory would otherwise
//!    track the *body*; `decode/compress.rs`'s pull meter is what stops it,
//!    and nothing but a measurement can prove that.
//!
//! Everything lives in ONE `#[test]` on purpose: a global allocator is
//! process-wide, so a second concurrently-running test would pollute the
//! counters. (`cargo nextest` gives each test its own process and would be
//! safe either way; `cargo test` runs them as threads and would not.)

#![cfg(feature = "gzip")]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxiarc_http::{ContentCoding, DecodeLimits, DecodedBody, Decoder};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// A pass-through allocator that records live bytes, a running peak, and the
/// number of allocation calls.
///
/// `unsafe` is unavoidable for a `GlobalAlloc` implementation and is confined
/// to this test binary; the library itself is `#![forbid(unsafe_code)]`.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            if new_size >= layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size
                    - layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn reset_peak() {
    PEAK.store(live(), Ordering::Relaxed);
}

fn peak_since_reset(baseline: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}

fn allocations() -> usize {
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// A reader that hands out at most `step` bytes per call, so the adapter's
/// own staging — not the caller's buffer — is what bounds memory.
struct Trickle<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.step.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn memory_gates() {
    // ── Gate 1: peak memory does not scale with the body ──────────────────
    //
    // 16 MiB of decoded output, read four kibibytes at a time and thrown
    // away. The whole point of the design is that this costs a constant.
    let plain = common::json(16 * 1024 * 1024);
    let wire = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    drop(plain);

    let baseline = live();
    reset_peak();
    let mut body = DecodedBody::with_codings(
        Trickle {
            data: &wire,
            pos: 0,
            step: 8 * 1024,
        },
        &[ContentCoding::Gzip],
        &DecodeLimits::default(),
    )
    .expect("decoder");
    let mut sink = [0u8; 4096];
    let mut total = 0usize;
    loop {
        let n = body.read(&mut sink).expect("read");
        if n == 0 {
            break;
        }
        total += n;
    }
    let streaming_peak = peak_since_reset(baseline);
    assert_eq!(total, 16 * 1024 * 1024, "the whole body must be delivered");
    assert!(body.is_finished());
    drop(body);

    // The budget the track fixed: two 64 KiB staging buffers, another
    // 64 KiB of slack, and the 32 KiB DEFLATE window — plus the Huffman
    // tables and the 4 KiB caller buffer, which live inside the slack.
    let bound = 2 * 64 * 1024 + 64 * 1024 + 32 * 1024;
    eprintln!("streaming peak: {streaming_peak} bytes (bound {bound})");
    assert!(
        streaming_peak <= bound,
        "streaming a 16 MiB body peaked at {streaming_peak} bytes, over the {bound}-byte bound"
    );

    // ── Gate 2: `feed_into` allocates nothing in the steady state ─────────
    //
    // The decoder's own staging buffer is allocated on the first call and
    // reused; the caller's `Vec` is cleared (keeping its capacity) between
    // rounds, exactly as a body loop that hands bytes onward would.
    let mut decoder =
        Decoder::new(&[ContentCoding::Gzip], &DecodeLimits::default()).expect("decoder");
    let mut out = Vec::with_capacity(1024 * 1024);
    let mut chunks = wire.chunks(4096);

    // Warm-up: three real chunks are enough to allocate every buffer the
    // steady state uses.
    for _ in 0..3 {
        let chunk = chunks.next().expect("wire is far longer than 3 chunks");
        decoder.feed_into(chunk, &mut out).expect("feed");
        out.clear();
    }

    let before = allocations();
    let mut rounds = 0usize;
    for chunk in chunks.by_ref().take(64) {
        decoder.feed_into(chunk, &mut out).expect("feed");
        out.clear();
        rounds += 1;
    }
    let per_round = allocations() - before;
    assert_eq!(rounds, 64, "the fixture must supply 64 more chunks");
    assert_eq!(
        per_round, 0,
        "feed_into allocated {per_round} time(s) over {rounds} steady-state calls"
    );

    // ── Gate 3: a refused bomb never materialises its output ──────────────
    let bomb = common::gzip_wrap(&common::single_block_bomb(123 * 1024 * 1024), &[]);
    let cap = 1024 * 1024;
    let baseline = live();
    reset_peak();
    let mut decoder = Decoder::new(
        &[ContentCoding::Gzip],
        &DecodeLimits::default()
            .with_max_output(cap)
            .with_max_ratio(None),
    )
    .expect("decoder");
    let mut out = Vec::new();
    decoder
        .feed_into(&bomb, &mut out)
        .expect_err("the bomb must be refused");
    let bomb_peak = peak_since_reset(baseline);
    // The cap itself (1 MiB), the output `Vec` that reached it, and the
    // decoder's fixed buffers — never the 123 MiB the block would have
    // produced.
    let bomb_bound = 4 * 1024 * 1024;
    eprintln!("refused-bomb peak: {bomb_peak} bytes (bound {bomb_bound})");
    assert!(
        bomb_peak <= bomb_bound,
        "a refused 123 MiB single-block bomb peaked at {bomb_peak} bytes"
    );

    // ── Gate 4: the `compress` (`.Z`) stage is metered the same way ───────
    //
    // Unlike every other coding here, `.Z` is decoded through a *pull*
    // reader (`oxiarc_lzw::z::ZReader`) bridged onto this crate's push seam,
    // and that reader expands one pull of compressed input to completion,
    // into a `Vec` of its own, before serving the first byte of it. Left at
    // its own 16 KiB pull, a body that expands 5000:1 therefore materialises
    // ~80 MiB inside one `read` call and peak memory tracks the body — which
    // is exactly what Gate 1 exists to forbid. `decode/compress.rs` meters
    // the pull instead; this is the measurement that keeps it honest.
    //
    // `DecodeLimits::unlimited()` on purpose: with `max_output` in play the
    // budget would stop the bomb before the meter had to, and the gate would
    // pass for the wrong reason.
    #[cfg(feature = "compress")]
    {
        let bomb = oxiarc_lzw::z::compress(&vec![0u8; 128 * 1024 * 1024], 16).expect("compress");
        let expansion = (128 * 1024 * 1024) / bomb.len();
        assert!(
            expansion > 1000,
            "this gate needs a body that really expands; got {expansion}:1"
        );

        let baseline = live();
        reset_peak();
        let mut body = DecodedBody::with_codings(
            Trickle {
                data: &bomb,
                pos: 0,
                step: 8 * 1024,
            },
            &[ContentCoding::Compress],
            &DecodeLimits::unlimited(),
        )
        .expect("decoder");
        let mut sink = [0u8; 4096];
        let mut total = 0usize;
        loop {
            let n = body.read(&mut sink).expect("read");
            if n == 0 {
                break;
            }
            total += n;
        }
        let z_peak = peak_since_reset(baseline);
        assert_eq!(total, 128 * 1024 * 1024, "the whole body must be delivered");
        drop(body);

        // The `.Z` code table at 16 bits is ~192 KiB on its own, one metered
        // fill targets 64 KiB (which a `Vec` may hold as 128 KiB of
        // capacity), and the adapter's two 64 KiB staging buffers sit on
        // top. The bound is deliberately *not* trimmed to the measurement:
        // this fixture (an all-zeros body) measures 893,668 bytes, but the
        // meter's headroom depends on the shape of the expansion curve — a
        // benign prefix followed by a bomb measures 1,475,295 — and one
        // extra `Vec` doubling either way should not read as a regression.
        // 2 MiB is still 67x under the 134,605,718 bytes this same fixture
        // measured before `decode/compress.rs` grew its pull meter, so the
        // gate keeps all of its teeth without being a tripwire.
        let z_bound = 2 * 1024 * 1024;
        eprintln!(".Z streaming peak: {z_peak} bytes (bound {z_bound}) at {expansion}:1");
        assert!(
            z_peak <= z_bound,
            "streaming a {expansion}:1 `.Z` bomb peaked at {z_peak} bytes, over the \
             {z_bound}-byte bound — the pull meter in decode/compress.rs is not holding \
             (this fixture measured 893,668 with the meter, 134,605,718 without it)"
        );
    }
}
