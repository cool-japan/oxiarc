//! Allocation behaviour of the streamed ZIP entry path.
//!
//! Lives in its own test binary because it installs a counting global
//! allocator, which is a whole-binary decision. The count is taken over a
//! thread-local window so it observes only the allocations the archive write
//! itself makes, and so tests running in parallel on other threads do not
//! perturb it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Cursor, Write};

use oxiarc_archive::zip::{ZipReader, ZipWriter};

// -------------------------------------------------------------- allocation

thread_local! {
    /// Bytes requested by `alloc`/`realloc` while `COUNTING` is set.
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
    /// Whether the counters are live.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn count(size: usize) {
    if COUNTING.get() {
        ALLOCATED.set(ALLOCATED.get() + size as u64);
    }
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `body`, returning the bytes it allocated through the global allocator.
fn allocated_by<R>(body: impl FnOnce() -> R) -> (R, u64) {
    ALLOCATED.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let result = body();
    COUNTING.with(|c| c.set(false));
    (result, ALLOCATED.with(|c| c.get()))
}

/// A sink that discards, so the measurement is of the writer's own buffers
/// rather than of a growing output file.
struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::hint::black_box(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A small, easily compressible payload.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| b"recycled deflater "[i % 18]).collect()
}

// -------------------------------------------------------------------- tests

/// Streaming many entries must not build a deflate encoder per entry.
///
/// A level-6 deflate encoder allocates its window and hash tables up front —
/// around 270 KiB — so the encoder used to cost more to construct than a
/// typical entry costs to compress, and an archive of *N* small entries paid
/// that construction *N* times. The archive now keeps one encoder per level
/// and resets it between entries, which reproduces a fresh encoder's bytes
/// exactly (see the byte-identity test below).
#[test]
fn streamed_entries_reuse_one_deflate_encoder() {
    const ENTRIES: usize = 500;
    // Comfortably over the archive's fixed cost (the encoder itself, the
    // central directory, the name strings) and far under the ~130 MiB that
    // building one encoder per entry costs.
    const BUDGET: u64 = 16 << 20;

    let data = payload(64);
    let (_, allocated) = allocated_by(|| {
        let mut writer = ZipWriter::new(Sink);
        for i in 0..ENTRIES {
            {
                let mut entry = writer
                    .add_stream(&format!("entry-{i}.bin"))
                    .expect("add_stream");
                entry.write_all(&data).expect("stream write");
                entry.finish().expect("finish entry");
            }
        }
        writer.finish().expect("finish archive");
    });

    assert!(
        allocated < BUDGET,
        "{ENTRIES} streamed entries allocated {allocated} bytes ({:.1} MiB), over the \
         {BUDGET}-byte budget; a fresh deflate encoder (~270 KiB) is still being built per entry",
        allocated as f64 / (1024.0 * 1024.0),
    );
}

/// Reusing the encoder must not change what it produces: a reset encoder that
/// failed to clear its window or its Huffman state would still decode
/// correctly but would not reproduce the bytes a fresh encoder makes, and
/// that equivalence is the whole basis on which the archive recycles one.
#[test]
fn a_recycled_encoder_produces_the_same_bytes_as_a_fresh_one() {
    let payloads: Vec<Vec<u8>> = (0..4)
        .map(|i| {
            let base = payload(4_000 + i * 3_000);
            // Different content per entry, so a stale window cannot pass.
            base.iter()
                .enumerate()
                .map(|(j, b)| b ^ (j as u8 % 7))
                .collect()
        })
        .collect();

    let mut archive = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut archive);
        for (i, data) in payloads.iter().enumerate() {
            {
                let mut entry = writer.add_stream(&format!("p{i}.bin")).expect("add_stream");
                // Ragged write sizes, as a real caller produces.
                for chunk in data.chunks(701 + i * 13) {
                    entry.write_all(chunk).expect("stream write");
                }
                entry.finish().expect("finish entry");
            }
        }
        writer.finish().expect("finish archive");
    }

    let mut reader = ZipReader::new(Cursor::new(&archive)).expect("ZipReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), payloads.len());
    for (entry, data) in entries.iter().zip(payloads.iter()) {
        let raw = reader.extract_raw(entry).expect("extract_raw");
        let fresh = oxiarc_deflate::deflate(data, 6).expect("deflate");
        assert_eq!(
            raw, fresh,
            "entry {}'s deflate bytes differ from a one-shot deflate of the same payload",
            entry.name
        );
        assert_eq!(reader.extract(entry).expect("extract"), *data);
    }
}
