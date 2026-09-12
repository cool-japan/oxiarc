//! Fuzz target for `oxiarc_png::StreamingDecoder`: proves the push parser's
//! event sequence is independent of how the input is chunked. One
//! `chunk_size`-parameterized helper drives *both* sides — a single large
//! chunk ("whole") and a small one ("piecewise") — so the two calls are
//! guaranteed symmetric (the crate's own `byte_at_a_time_matches_whole_buffer`
//! unit test is asymmetric: its whole-buffer side calls `notify_eof()` via
//! the shared `events()` helper and its piecewise side does not, which is
//! fine for a single well-formed fixture but would misattribute a genuine
//! `StreamingDecoder` divergence to "just needed `notify_eof()`" on fuzz
//! bytes). Must never panic or hang; whenever both sides succeed, the
//! event sequence must be identical.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_png::{Decoded, DecodingError, StreamingDecoder};

/// Bound on `update()` calls so a state machine that stops making progress
/// panics instead of hanging the fuzzer.
const CALL_GUARD: u32 = 200_000;

/// Drive `data` through a fresh [`StreamingDecoder`] in chunks of
/// `chunk_size` bytes, calling `notify_eof()` the moment input is
/// exhausted (exactly like a real incremental consumer that does not know
/// in advance how much data there is), and collect every non-`Nothing`
/// event. `image_data: None` throughout — pixel bytes are irrelevant to
/// this target; only the parser's framing/event decisions are.
fn run(data: &[u8], chunk_size: usize) -> Result<Vec<Decoded>, DecodingError> {
    let mut decoder = StreamingDecoder::new();
    let mut events = Vec::new();
    let mut pos = 0usize;
    let mut eof_sent = false;
    let mut calls = 0u32;

    loop {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress driving StreamingDecoder");

        let end = (pos + chunk_size).min(data.len());
        let (consumed, event) = decoder.update(&data[pos..end], None)?;
        pos += consumed;
        if event != Decoded::Nothing {
            events.push(event);
        }
        if decoder.is_finished() {
            return Ok(events);
        }
        // A single `Ok((0, Nothing))` after `notify_eof()` is a legitimate
        // internal state transition (e.g. draining into `FlushImageData`
        // with `image_data: None`), not a stall by itself, so it is *not*
        // asserted here. A decoder that is truly stuck making no progress
        // forever is still caught: it will keep looping with an empty
        // input slice and trip `CALL_GUARD` above.
        if pos >= data.len() && !eof_sent {
            decoder.notify_eof();
            eof_sent = true;
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // A small, deliberately awkward granularity vs. one chunk covering the
    // whole input (the largest possible "one call" shape without hardcoding
    // `usize::MAX` and risking an accidental overflow in `pos + chunk_size`
    // on a 32-bit target).
    let small = 1 + (data[0] as usize % 4); // 1..=4
    let large = data.len();

    let whole = run(data, large);
    let piecewise = run(data, small);

    match (&whole, &piecewise) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a, b,
                "StreamingDecoder event sequence diverged between granularities \
                 {large} and {small}"
            );
        }
        // Both sides use the identical helper and the identical
        // notify_eof() discipline, so either direction disagreeing on
        // Ok-vs-Err is exactly the class of bug this target exists to
        // catch — assert it loudly rather than silently tolerate it.
        (Ok(_), Err(e)) => {
            panic!("granularity {small} rejected what granularity {large} accepted: {e}")
        }
        (Err(e), Ok(_)) => {
            panic!("granularity {large} rejected what granularity {small} accepted: {e}")
        }
        (Err(_), Err(_)) => {}
    }
});
