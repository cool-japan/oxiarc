//! Fuzz target for `oxiarc_zstd::ZstdStream`, the bounded resumable push
//! decoder: fed at random split points and compared against the
//! whole-buffer reference `oxiarc_zstd::decompress_multi_frame()` (the
//! crate's own one-shot counterpart to `ZstdStream::new()`'s
//! `multi_frame: true` default — see "Reference-function choice" in
//! History below).
//!
//! # Contract
//!
//! 1. **Never panic or hang.** A `CALL_GUARD` bounds `decode()` calls, so a
//!    stalled state machine fails as a crash rather than a libFuzzer
//!    timeout.
//! 2. **`Ok` + `Ok` ⇒ byte-identical.** Whenever both sides accept the
//!    input, their output must match exactly.
//! 3. **`Err(reference)` + `Ok(stream)` is always a bug.** As of the ZSTD4
//!    hardening pass (History, below), the legacy one-shot core
//!    `decompress_multi_frame()` still uses refuses everything `ZstdStream`
//!    refuses; there is no remaining, legitimate way for the streaming path
//!    to accept something the reference does not.
//! 4. **`Ok(reference)` + `Err(stream)` is a bug _unless_ the stream's error
//!    is `OxiArcError::MemoryBudgetExceeded { budget: MAX_WINDOW_SIZE, .. }`
//!    exactly** — the one surviving, deliberate split (finding 3 below),
//!    and the only exception this target carves out.
//!
//! Rule 4's carve-out is pinned to the exact `budget` value, not just the
//! error variant, so it cannot silently widen to swallow an unrelated
//! defect even if the crate's internals change later: `decompress_multi_
//! frame()` keeps no window ring of its own — its output `Vec` *is* the
//! window, so a declared `Window_Size` costs it nothing and any declaration
//! is accepted (`decompress`'s "Resource policy" doc section in
//! `frame.rs`) — while `ZstdStream::new()` defaults to an 8 MiB
//! `with_max_window` ceiling (`MAX_WINDOW_SIZE`, never changed by this
//! target, which never calls `with_max_window`) and refuses a larger
//! declared window in `begin_frame`, before a single block is decoded,
//! reporting `budget: self.max_window` — always exactly `MAX_WINDOW_SIZE`
//! here. Under *this* target's configuration (`ZstdStream::new()`, no
//! `with_max_output` call) that window check is the *only* place
//! `MemoryBudgetExceeded` can come from at all: with `max_output: None`,
//! `check_budget` is a no-op, and the other `MemoryBudgetExceeded` site
//! (`charge`'s own output-budget branch, inside block execution) is
//! provably unreachable — `budget_max` degenerates to `rfc_max` when
//! unbounded, which `charge_block` has already enforced (as a
//! `CorruptedData` error, not `MemoryBudgetExceeded`) by the time `charge`
//! re-checks it — and `ZstdWindow` (`window.rs`) is self-contained, with no
//! `MemoryBudgetExceeded` site of its own (it does not wrap `oxiarc_core`'s
//! `RingBuffer`, which does have one). So today, matching the variant alone
//! would already be exact, not merely a loose net — but matching the exact
//! `budget` value is strictly stronger for no extra cost: a future
//! `MemoryBudgetExceeded` from anywhere else in the crate, reported with
//! any other `budget`, still fails this guard and surfaces as a crash
//! rather than being absorbed by a carve-out whose original justification
//! no longer covers it.
//!
//! # History
//!
//! **Reference-function choice, and why it is not the single-frame
//! `decompress()`:** an earlier version of this target compared against
//! `decompress()`, which decodes only the first frame and silently drops
//! everything after it — wrong, because `ZstdStream::new()` sets
//! `multi_frame: true` and, after one frame's `StreamEnd`, transparently
//! starts decoding a second concatenated frame. Fixed by switching to
//! `decompress_multi_frame()`, the crate's actual one-shot counterpart.
//!
//! **Four distinct, independently-rooted `Ok(reference)` + `Err(stream)`
//! divergences**, found unseeded across several fuzzing passes once the
//! reference-function bug above was fixed (original analysis: track
//! FUZZCLI; hardening: track ZSTD4; further verification: track
//! ZSTD4-verify). Three are now fixed in code shared by both paths; the
//! fourth remains a deliberate, permanent split and is the sole exception
//! contract rule 4 (above) carves out:
//!
//! 1. **`Dictionary_ID` never validated by the legacy path — FIXED,
//!    shared.** A frame naming a dictionary the caller did not supply
//!    decoded to silently wrong bytes via `decompress_multi_frame()` while
//!    `ZstdStream` correctly refused it. `frame::require_dictionary` is now
//!    the single implementation both paths call, producing the identical
//!    `OxiArcError::InvalidHeader` ("requires dictionary ID ...").
//! 2. **A block's regenerated size was never checked against
//!    `min(Window_Size, 128 KiB)` (further bounded by any declared
//!    `Frame_Content_Size`) by the legacy path — FIXED, shared.**
//!    `frame::charge_block` is now called from both `ZstdStream`'s
//!    per-sequence executor and the legacy `execute_sequences`, so a block
//!    over the RFC ceiling is refused identically on both paths, with the
//!    same "block regenerated size N exceeds the frame maximum M" text.
//! 3. **`ZstdStream::new()`'s default `with_max_window` ceiling has no
//!    equivalent in the legacy path — DELIBERATE, DOCUMENTED, PERMANENT.**
//!    A frame declaring an ~11 MB window decoded fine via
//!    `decompress_multi_frame()` (its output `Vec` is its window) but was
//!    refused by `ZstdStream` with `MemoryBudgetExceeded`. Investigated and
//!    confirmed structurally unfixable-as-a-shared-rule by track ZSTD4: a
//!    `Window_Size` is not an output size, and an ordinary piped
//!    `zstd -3`/`--long` frame declares a window with nothing to do with
//!    its payload length — a rule phrased purely in terms of the
//!    declaration cannot refuse the malicious shape and accept the benign
//!    one, since the two are structurally identical. This is why the
//!    bounded one-shot helpers (`decompress_with_limit`,
//!    `decompress_multi_frame_with_limit`) exist as *separate*,
//!    purpose-built entry points rather than a tightened
//!    `decompress_multi_frame()` — and why this target does not compare
//!    against them instead: their window ceiling is
//!    `max(max_output, 128 MiB)` (`zstd -d`'s own default), which can
//!    never be pushed down to `ZstdStream::new()`'s 8 MiB by any choice of
//!    `max_output`, so no such reference reproduces this exact split (and
//!    a budgeted reference would also newly diverge in rule 3's direction,
//!    refusing on output size alone for any input past its budget that
//!    `ZstdStream::new()`, having no `with_max_output` of its own, still
//!    accepts). This is the one exception contract rule 4 encodes.
//! 4. **EOF/frame-boundary classification — FIXED, shared.** Originally
//!    reported as three different symptoms of one root cause (an
//!    unrecognized leading magic, a magic cut short by EOF, or a truncated
//!    skippable-frame size field, each returning `Ok(vec![])` from
//!    `decompress_multi_frame()` instead of an error, at any position
//!    including the very first). Track ZSTD4 traced this to the *legacy*
//!    path being the lenient one, not a `ZstdStream` bug: `ZstdStream`'s
//!    own EOF condition already matched its stated intent. Both
//!    `decompress_multi_frame` and `decompress_multi_frame_with_dict` now
//!    share one `multi_frame_scan` that mirrors `ZstdStream`'s state
//!    machine exactly (see `decompress_multi_frame`'s own "Where the
//!    stream ends" doc section). Track ZSTD4-verify's follow-up pass found
//!    and fixed one more edge in the same family: a *skippable* frame in
//!    front of the real frame was walked past by `ZstdStream`,
//!    `decompress_into` and `decompress_with_limit`, but rejected by
//!    `decompress`/`decompress_multi_frame`/`ZstdDecoder::decode_frame` —
//!    also now shared (`frame::skippable_prefix_len`).
//!
//! **Track FUZZTIGHTEN** (this track) restored the strong contract above
//! now that findings 1, 2 and 4 are closed: three of the four original
//! causes for "reference accepts, stream refuses" are gone, and the fourth
//! has a precise, structural test (rule 4) rather than a blanket
//! same-direction carve-out. The reverse direction (`Err(reference)` +
//! `Ok(stream)`, rule 3) was re-verified with no exception at all — see
//! ZSTD4's "Agreement sweep" (3,411 single-byte mutations over 7 frames, 0
//! divergences) and ZSTD4-verify's `adversarial_truncation_and_mutation_
//! agree_across_paths` test (5,600+ inputs) — and this track's own
//! 120-second corpus-seeded run found none either; see the FUZZTIGHTEN
//! handoff for the exact iteration count.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{MAX_WINDOW_SIZE, ZstdStatus, ZstdStream};

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
        (Err(reference_err), Ok(_)) => {
            panic!(
                "decompress_multi_frame() refused input that chunked ZstdStream \
                 (granularity {chunk_size}) accepted -- every known cause of this \
                 direction was closed by the ZSTD4 hardening pass (see the module \
                 doc's History section, finding 1/2/4): {reference_err}"
            );
        }
        (Ok(_), Err(stream_err)) => {
            // Pinned to the exact budget value `begin_frame` reports for
            // *this* stream's window ceiling (`ZstdStream::new()` never
            // calls `with_max_window`, so `self.max_window` is always
            // `MAX_WINDOW_SIZE` here), not just the error variant. A
            // `MemoryBudgetExceeded` with any other budget -- from a future
            // call site this module doc's enumeration did not anticipate,
            // not merely from a source it already ruled out -- fails this
            // guard and is correctly still treated as a bug, so this carve-
            // out cannot silently widen if the crate's internals change.
            assert!(
                matches!(
                    stream_err,
                    OxiArcError::MemoryBudgetExceeded { budget, .. } if *budget == MAX_WINDOW_SIZE
                ),
                "chunked ZstdStream (granularity {chunk_size}) refused input that \
                 decompress_multi_frame() accepted, and the refusal was not the one \
                 documented exception (a declared-window ceiling of exactly \
                 MAX_WINDOW_SIZE -- see the module doc's Contract rule 4 / History \
                 finding 3): {stream_err}"
            );
        }
        // Independent detection points inside each core; neither side
        // refusing for a reason the other also refuses for is asserted as
        // a bug.
        (Err(_), Err(_)) => {}
    }
});
