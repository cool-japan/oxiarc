//! Fuzz target for `oxiarc_brotli::BrotliStream`, the bounded resumable push
//! decoder: fed at random split points, it must never panic or hang, and
//! whenever the whole-buffer reference `oxiarc_brotli::decompress()` accepts
//! the input, the streaming path must produce byte-identical output.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_brotli::{BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

/// Deliberately small relative to typical fuzz inputs, so `NeedOutput` is
/// exercised on anything but a tiny payload.
const SINK: usize = 197;

/// Bound on `decode()` calls so a stalled state machine panics instead of
/// hanging the fuzzer.
const CALL_GUARD: u32 = 2_000_000;

fn decode_chunked(data: &[u8], chunk_size: usize) -> oxiarc_brotli::BrotliResult<Vec<u8>> {
    let mut stream = BrotliStream::new();
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

    let whole = oxiarc_brotli::decompress(payload);
    let chunked = decode_chunked(payload, chunk_size);

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
