//! Fuzz target for `oxiarc_zstd::ZstdStream`, the bounded resumable push
//! decoder: fed at random split points, it must never panic or hang, and
//! whenever it agrees with the whole-buffer reference
//! `oxiarc_zstd::decompress_multi_frame()` that an input decodes, the two
//! must produce byte-identical output.
//!
//! **Reference-function choice, and why it is not the single-frame
//! `decompress()`:** `ZstdStream::new()` sets `multi_frame: true`
//! (`stream.rs:277`) — after the first frame's `StreamEnd`, feeding it more
//! bytes transparently starts decoding a second concatenated frame, exactly
//! like `decompress_multi_frame` (concatenating each frame's output), *not*
//! like the single-frame `decompress()` (which decodes only the first frame
//! and silently ignores everything after it, undocumented in its own
//! signature). An earlier version of this target compared against
//! `decompress()` and immediately found a 2-concatenated-frame input where
//! `decompress()` returned one frame's output while `ZstdStream` correctly
//! returned both frames' — not a crate bug, a wrong-reference-function bug
//! in this target, fixed by switching to `decompress_multi_frame`.
//!
//! **`decompress_multi_frame()` accepting where `ZstdStream` refuses is
//! deliberately *not* asserted as a bug** (see the `Ok`+`Err` match arm).
//! This was not the starting assumption — it is the conclusion of six
//! separate fuzzing iterations against this exact pair, each initially
//! treated as "add one more named carve-out", until the pattern itself
//! became the finding. In order, all discovered empirically, unseeded,
//! inside roughly ten cumulative minutes of fuzzing a pair nothing had ever
//! compared against each other before this session:
//!
//! 1. A frame whose `Dictionary_ID` names a dictionary the caller did not
//!    supply (`stream.rs`'s `begin_frame`; error text
//!    `"requires dictionary ID"` — pinned contract: `oxiarc-zstd/src/
//!    read.rs`'s own `dictionary_id_frame_is_refused_without_a_dictionary`
//!    test asserts on this exact substring). RFC 8878-correct; the legacy
//!    per-frame decoder never checked it.
//! 2. A block whose regenerated size exceeds `min(Window_Size, 128 KiB)`
//!    further bounded by any declared `Frame_Content_Size` (`stream.rs`'s
//!    `block_rfc_max`, doc-commented there as "exceeding it is always a
//!    format error"). RFC 8878-correct; the legacy path never checked it.
//! 3. A frame declaring a `Window_Size` over `ZstdStream::new()`'s default
//!    `with_max_window` ceiling (`"memory budget exceeded"`) — the legacy
//!    path has no window ceiling at all (exactly why `decompress_with_
//!    limit`/`decompress_multi_frame_with_limit` exist as *separate*,
//!    purpose-built bounded entry points: the bare `decompress_multi_
//!    frame()` used as this target's reference was never meant to be
//!    resource-safe on untrusted input by itself).
//! 4. Three *different* symptoms of one root cause — `decompress_multi_
//!    frame`'s outer loop breaking the instant it meets fewer than 4 bytes,
//!    an unrecognized 4-byte magic, or (inside a skippable frame) a
//!    truncated size field, at *any* position including the very first,
//!    and returning whatever was accumulated (`Ok(vec![])` before anything
//!    decoded — its own doc comment: "trailing garbage is tolerated",
//!    which turns out to apply to *leading* garbage too). Observed
//!    `ZstdStream` error texts for the identical condition:
//!    `"truncated Zstandard frame magic"`, `"Invalid magic number: ..."`,
//!    `"truncated skippable frame size"` — a fourth phrasing is plausible.
//!    `ZstdStream`'s own EOF-handling comment (`stream.rs:585-591`) says it
//!    means to match `decompress_multi_frame` exactly here, and its coded
//!    condition just doesn't cover this one boundary case — the most
//!    likely of the four to be a fixable `ZstdStream` bug rather than an
//!    intentional split, but still not this track's crate to fix.
//!
//! No two of these four share a root cause, and the fourth **had already
//! replaced two prior string-matched carve-outs with a structural one**
//! before finding #3 (window budget) — a fresh, fifth, again-distinct
//! category — arrived. Chasing a sixth or seventh would not change the
//! conclusion: `ZstdStream` is a Wave-1 ground-up rewrite deliberately
//! hardened against exactly these classes of input (spec conformance,
//! resource bounds, EOF classification); the legacy `ZstdDecoder::
//! decode_frame` core `decompress_multi_frame` still uses was never
//! revisited to match. That is a real, useful thing for a differential
//! target to know about this pair, but it means "the legacy path accepted,
//! `ZstdStream` refused" is not evidence of anything by itself — only a
//! **byte mismatch when both sides accept**, or a panic/hang, is. Flagging
//! findings 1-3 (and the shape of 4) for whoever next owns `oxiarc-zstd`:
//! either harden the legacy path to match, or document the split
//! explicitly (the same shape as gzip's `FHCRC`, Phase 8 owner decision 1).
//!
//! `ZstdStream` accepting where `decompress_multi_frame()` refuses (the
//! reverse direction) is not asserted either, for the same reason.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream};

/// Deliberately small relative to typical fuzz inputs, so `NeedOutput` is
/// exercised on anything but a tiny payload.
const SINK: usize = 181;

/// Bound on `decode()` calls so a stalled state machine panics instead of
/// hanging the fuzzer.
const CALL_GUARD: u32 = 2_000_000;

fn decode_chunked(data: &[u8], chunk_size: usize) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = ZstdStream::new();
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
            ZstdStatus::StreamEnd => ended = true,
            // `ZstdStatus` is `#[non_exhaustive]`; a wildcard also covers
            // `NeedInput` and `NeedOutput` today.
            _ => assert!(
                progress.consumed > 0 || progress.produced > 0,
                "no progress: {:?} with an unconsumed, non-empty chunk",
                progress.status
            ),
        }
    }

    // Drain anything still buffered with empty final calls, then assert the
    // stream really ended (`finish()` is what turns "still wants input"
    // into a truncation error for a genuinely short stream).
    while !ended {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress draining after real input");
        let progress = stream.decode(&[], &mut sink, FlushMode::None)?;
        out.extend_from_slice(&sink[..progress.produced]);
        match progress.status {
            ZstdStatus::StreamEnd => ended = true,
            ZstdStatus::NeedOutput => {}
            // `ZstdStatus` is `#[non_exhaustive]`; a wildcard also covers
            // `NeedInput`, which here means draining is done and
            // `stream.finish()` below is the real arbiter of whether the
            // stream is actually complete. A future status is treated the
            // same conservative way rather than looping forever on it.
            _ => break,
        }
    }
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

    let whole = oxiarc_zstd::decompress_multi_frame(payload);
    let chunked = decode_chunked(payload, chunk_size);

    match (&whole, &chunked) {
        (Ok(expected), Ok(actual)) => {
            assert_eq!(
                expected, actual,
                "chunked ZstdStream (granularity {chunk_size}) diverged from \
                 decompress_multi_frame()"
            );
        }
        // `ZstdStream` is a deliberately hardened rewrite of a legacy core
        // `decompress_multi_frame` still uses unmodified (see the module
        // doc comment for the four distinct, empirically-confirmed reasons
        // this direction alone proves nothing): only agreement when *both*
        // sides accept, and never panicking or hanging, are asserted.
        (Ok(_), Err(_)) | (Err(_), Ok(_)) | (Err(_), Err(_)) => {}
    }
});
