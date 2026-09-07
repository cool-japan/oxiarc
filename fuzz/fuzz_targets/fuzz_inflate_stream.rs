//! Fuzz target for `oxiarc_deflate::stream::InflateStream`: fed at random
//! split points, it must agree byte-for-byte with the whole-buffer
//! `oxiarc_deflate::inflate()` reference (which is itself
//! `InflateStream::new().inflate_to_vec(data)` under one `FlushMode::Finish`
//! call — see that function's source). This is the resumability invariant
//! Phase 8 W1-A1 exists to guarantee: the "fast path (cached symbol loop)"
//! and "careful resumable per-symbol path" must be indistinguishable from
//! the outside, however the input arrives.
//!
//! Real bytes are fed under `FlushMode::None` in chunks sized from the fuzz
//! input itself (so libFuzzer's corpus can discover interesting split
//! granularities, including the all-important one-byte-at-a-time case), then
//! end of input is signalled with one empty-slice `FlushMode::Finish` call —
//! the documented "HTTP response body: `None` until transport EOF, then
//! `Finish`" row (`oxiarc-deflate/src/stream.rs` module docs).
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::stream::{InflateStatus, InflateStream};

/// Output staging buffer. Deliberately small (not sized to the input) so
/// `InflateStatus::NeedOutput` is exercised on anything but a tiny payload.
const SINK: usize = 173;

/// Upper bound on `inflate()` calls, so a state machine that stops making
/// progress fails loudly (a fuzz crash) instead of hanging the run.
const CALL_GUARD: u32 = 2_000_000;

/// Drive `data` through a fresh [`InflateStream`] in chunks of `chunk_size`
/// bytes, then signal true end of input. `Err` propagates to the caller via
/// `?`; a state machine that reports `NeedInput` without consuming anything
/// trips the guard and panics (a hang, which libFuzzer would otherwise only
/// notice via its own timeout).
fn inflate_chunked(data: &[u8], chunk_size: usize) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = InflateStream::new();
    let mut out = Vec::new();
    let mut sink = [0u8; SINK];
    let mut pos = 0usize;
    let mut calls = 0u32;

    while pos < data.len() {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress feeding real input bytes");
        let end = (pos + chunk_size).min(data.len());
        let progress = stream.inflate(&data[pos..end], &mut sink, FlushMode::None)?;
        out.extend_from_slice(&sink[..progress.produced]);
        pos += progress.consumed;
        if progress.status == InflateStatus::StreamEnd {
            // Trailing bytes in `data` past this point are simply never
            // fed, matching `inflate()`'s own "does not require the whole
            // slice to be consumed" behaviour.
            return Ok(out);
        }
        assert!(
            progress.consumed > 0 || progress.produced > 0,
            "no progress: NeedInput/NeedOutput with an unconsumed, non-empty chunk"
        );
    }

    // All real bytes are in. Signal true EOF and let a genuinely truncated
    // stream surface as the documented `UnexpectedEof`.
    loop {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress in the finish phase");
        let progress = stream.inflate(&[], &mut sink, FlushMode::Finish)?;
        out.extend_from_slice(&sink[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
        assert!(
            progress.produced > 0,
            "Finish on empty input returned {:?} without producing or erroring",
            progress.status
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(granularity_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };
    // A handful of small, deliberately awkward granularities, weighted
    // toward the byte-at-a-time case that matters most for the resumable
    // per-symbol path.
    const GRANULARITIES: [usize; 8] = [1, 1, 1, 2, 3, 5, 7, 13];
    let chunk_size = GRANULARITIES[(granularity_pick as usize) % GRANULARITIES.len()];

    let payload = unstructured.take_rest();

    let whole = oxiarc_deflate::inflate(payload);
    let chunked = inflate_chunked(payload, chunk_size);

    match (&whole, &chunked) {
        (Ok(expected), Ok(actual)) => {
            assert_eq!(
                expected, actual,
                "chunked InflateStream (granularity {chunk_size}) diverged from inflate()"
            );
        }
        (Ok(expected), Err(error)) => {
            panic!(
                "chunked path (granularity {chunk_size}) rejected a stream inflate() \
                 accepted ({} bytes): {error}",
                expected.len()
            );
        }
        // The reverse direction is intentionally not asserted: `inflate()`
        // makes exactly one `Finish` call over the whole slice, while the
        // chunked path separates "feed real bytes under `None`" from
        // "signal EOF with an empty `Finish` call" — two different call
        // sequences the core is not contractually required to treat
        // identically at every boundary. A regression here would still be
        // caught the moment it also causes byte-mismatch or a hang above.
        (Err(_), Ok(_)) | (Err(_), Err(_)) => {}
    }
});
