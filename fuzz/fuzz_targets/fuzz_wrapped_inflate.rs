//! Fuzz target for `oxiarc_deflate::wrapper::WrappedInflate` across all four
//! wrappers (`Raw`, `Zlib`, `Gzip`, `Auto`): must never panic or hang on
//! arbitrary input, and must be resumable — decoding the same bytes at two
//! different split granularities through fresh, identically configured
//! decoders must agree whenever either succeeds. This is the layer PNG's
//! IDAT chain, TIFF's per-strip streams and HTTP's gzip/deflate/zlib bodies
//! all sit on top of (`oxiarc-png`, `oxiarc-tiff`, `oxiarc-http`), so a
//! header-framing bug here (e.g. a gzip `FNAME` field split across a chunk
//! boundary, or `Auto`'s two-byte sniff fed one byte at a time) would affect
//! all three.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::stream::InflateStatus;
use oxiarc_deflate::wrapper::{InflateWrapper, WrappedInflate};

/// Deliberately small relative to typical fuzz inputs, so `NeedOutput` is
/// exercised on anything but a tiny payload.
const SINK: usize = 149;

/// Bound on `inflate()` calls so a stalled state machine panics instead of
/// hanging the fuzzer.
const CALL_GUARD: u32 = 2_000_000;

/// Feed `data` through a fresh [`WrappedInflate`] for `wrapper`, in chunks of
/// `chunk_size` bytes under `FlushMode::None`, then signal true end of input
/// with one empty `FlushMode::Finish` call — mirroring exactly how
/// `oxiarc-png`'s IDAT chain and `oxiarc-http`'s response body driver use
/// this same type.
fn drive(
    wrapper: InflateWrapper,
    data: &[u8],
    chunk_size: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut decoder = WrappedInflate::new(wrapper).multi_member(true);
    let mut out = Vec::new();
    let mut sink = [0u8; SINK];
    let mut pos = 0usize;
    let mut calls = 0u32;
    let mut ended = false;

    while !ended && pos < data.len() {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress feeding real input bytes");
        let end = (pos + chunk_size).min(data.len());
        let progress = decoder.inflate(&data[pos..end], &mut sink, FlushMode::None)?;
        out.extend_from_slice(&sink[..progress.produced]);
        pos += progress.consumed;
        match progress.status {
            InflateStatus::StreamEnd => ended = true,
            _ => assert!(
                progress.consumed > 0 || progress.produced > 0,
                "no progress: {:?} with an unconsumed, non-empty chunk",
                progress.status
            ),
        }
    }

    while !ended {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress in the finish phase");
        let progress = decoder.inflate(&[], &mut sink, FlushMode::Finish)?;
        out.extend_from_slice(&sink[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            ended = true;
        } else {
            assert!(
                progress.produced > 0,
                "Finish on empty input returned {:?} without producing or erroring",
                progress.status
            );
        }
    }
    Ok(out)
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(wrapper_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(granularity_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };

    let wrapper = match wrapper_pick % 4 {
        0 => InflateWrapper::Raw,
        1 => InflateWrapper::Zlib,
        2 => InflateWrapper::Gzip,
        _ => InflateWrapper::Auto,
    };
    // Small (adversarial, byte-at-a-time-ish) vs. large (near-whole-buffer)
    // granularity, from the same fuzz input so the fuzzer can still steer
    // both — a resumability bug that only shows up between these two shapes
    // is exactly what this target exists to catch.
    const SMALL: [usize; 4] = [1, 1, 2, 3];
    const LARGE: [usize; 4] = [64, 128, 512, 4096];
    let small = SMALL[(granularity_pick as usize) % SMALL.len()];
    let large = LARGE[(granularity_pick as usize) % LARGE.len()];

    let payload = unstructured.take_rest();

    let fine = drive(wrapper, payload, small);
    let coarse = drive(wrapper, payload, large);

    match (&fine, &coarse) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a, b,
                "WrappedInflate({wrapper:?}) diverged between granularities {small} and {large}"
            );
        }
        (Ok(a), Err(e)) => panic!(
            "WrappedInflate({wrapper:?}) at granularity {large} rejected what granularity \
             {small} accepted ({} bytes): {e}",
            a.len()
        ),
        (Err(e), Ok(b)) => panic!(
            "WrappedInflate({wrapper:?}) at granularity {small} rejected what granularity \
             {large} accepted ({} bytes): {e}",
            b.len()
        ),
        (Err(_), Err(_)) => {}
    }
});
