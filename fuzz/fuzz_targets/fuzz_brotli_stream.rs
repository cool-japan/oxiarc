//! Fuzz target for `oxiarc_brotli::BrotliStream`, the bounded resumable push
//! decoder: fed at random split points, it must never panic or hang, and
//! whenever the whole-buffer reference `oxiarc_brotli::decompress_with_limit()`
//! accepts the input, the streaming path must produce byte-identical output.
//!
//! # Why the window is capped
//!
//! Brotli's `WBITS` field is attacker-controlled, and `WBITS = 24` asks for a
//! 16 MiB sliding window. Almost every hostile input the fuzzer generates sets
//! it, so without a cap this target spent its time in `malloc`/page-faults
//! rather than in the state machine: FINALGATE measured **433 exec/s at 398 MB
//! RSS**, roughly 100x slower than its 17 sibling targets (10k-110k exec/s).
//! [`MAX_WINDOW`] refuses an over-large window *before* allocating it, which
//! is exactly the bounded path a real caller decoding untrusted input takes.
//!
//! The other half of the cost is *output*: `decompress()`'s built-in guard is
//! 256 MB, so one crafted meta-block chain can grow a quarter-gigabyte
//! `Vec` per execution. Both paths therefore run under the same
//! [`MAX_OUTPUT`] budget, which keeps the differential property intact (both
//! sides see the identical ceiling) while bounding the per-execution cost.
//!
//! A refusal by either ceiling is a legitimate outcome, not a divergence, so
//! [`BrotliError::WindowTooLarge`] from the chunked path and a budget refusal
//! by either path are excluded from the agreement check below — nothing else
//! is. The chunked path runs **first**, so an input the ceilings reject never
//! costs the reference decode at all.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_brotli::{BrotliError, BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

/// Deliberately small relative to typical fuzz inputs, so `NeedOutput` is
/// exercised on anything but a tiny payload.
const SINK: usize = 197;

/// Largest sliding window this harness will let a fuzz input allocate.
///
/// 1 MiB (`WBITS = 20`) still covers every window a real encoder picks at
/// quality <= 9 with default settings, so ordinary streams in the corpus are
/// unaffected; only inputs that declare a bigger window are refused, and they
/// are refused before the allocation.
const MAX_WINDOW: usize = 1 << 20;

/// Largest output either path will produce before refusing.
///
/// 4 MiB is far past anything a fuzz input of a few kilobytes legitimately
/// expands to, so it costs no real coverage; it only stops a deliberate
/// decompression bomb from spending a quarter of a gigabyte per execution.
/// Bomb *rejection* itself is covered by `oxiarc-brotli`'s own
/// `tests/memory_limit.rs`, with a heap-tracking allocator.
const MAX_OUTPUT: usize = 4 << 20;

/// Bound on `decode()` calls so a stalled state machine panics instead of
/// hanging the fuzzer.
const CALL_GUARD: u32 = 2_000_000;

fn decode_chunked(data: &[u8], chunk_size: usize) -> oxiarc_brotli::BrotliResult<Vec<u8>> {
    let mut stream = BrotliStream::new()
        .with_max_window(MAX_WINDOW)
        .with_max_output(MAX_OUTPUT as u64);
    let mut out = Vec::new();
    let mut sink = [0u8; SINK];
    let mut pos = 0usize;
    let mut calls = 0u32;
    let mut ended = false;

    while !ended && pos < data.len() {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress feeding real input bytes");
        let end = (pos + chunk_size).min(data.len());
        let progress = stream.decode(&data[pos..end], &mut sink, FlushMode::None)?;
        out.extend_from_slice(&sink[..progress.produced]);
        pos += progress.consumed;
        match progress.status {
            BrotliStatus::StreamEnd => ended = true,
            BrotliStatus::NeedInput | BrotliStatus::NeedOutput => assert!(
                progress.consumed > 0 || progress.produced > 0,
                "no progress: {:?} with an unconsumed, non-empty chunk",
                progress.status
            ),
        }
    }

    while !ended {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress draining after real input");
        let progress = stream.decode(&[], &mut sink, FlushMode::None)?;
        out.extend_from_slice(&sink[..progress.produced]);
        match progress.status {
            BrotliStatus::StreamEnd => ended = true,
            BrotliStatus::NeedOutput => {}
            BrotliStatus::NeedInput => break,
        }
    }
    // `finish()` is what turns "still wants input" into
    // `BrotliError::UnexpectedEof` for a genuinely truncated stream.
    stream.finish()?;
    Ok(out)
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(granularity_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };
    const GRANULARITIES: [usize; 8] = [1, 1, 1, 2, 3, 5, 7, 13];
    let chunk_size = GRANULARITIES[(granularity_pick as usize) % GRANULARITIES.len()];

    let payload = unstructured.take_rest();

    // Chunked first: an input either ceiling rejects is dropped here, before
    // the reference decode pays for it.
    let chunked = decode_chunked(payload, chunk_size);
    if matches!(
        chunked,
        Err(BrotliError::WindowTooLarge { .. } | BrotliError::MemoryBudgetExceeded { .. })
    ) {
        return;
    }
    let whole = oxiarc_brotli::decompress_with_limit(payload, MAX_OUTPUT);

    match (&whole, &chunked) {
        (Ok(expected), Ok(actual)) => {
            assert_eq!(
                expected, actual,
                "chunked BrotliStream (granularity {chunk_size}) diverged from decompress()"
            );
        }
        (Ok(expected), Err(error)) => {
            panic!(
                "chunked path (granularity {chunk_size}) rejected a stream decompress() \
                 accepted ({} bytes): {error}",
                expected.len()
            );
        }
        // Independent implementations under the hood; only "agree whenever
        // the reference succeeds, never panic or hang" is asserted.
        (Err(_), Ok(_)) | (Err(_), Err(_)) => {}
    }
});
