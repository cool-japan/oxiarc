//! Fuzz target for `oxiarc_http::Decoder` over random `Content-Encoding`
//! coding lists and random `DecodeLimits`, fed at random split points
//! (simulating arbitrary network reads): must never panic or hang on
//! arbitrary bytes, whatever chain of codings and whatever limits are in
//! force, and must never produce more decoded bytes than
//! `DecodeLimits::max_output` allows — the load-bearing bomb guard
//! (`oxiarc-http/src/lib.rs` "Decompression bombs" section).
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::traits::FlushMode;
use oxiarc_http::{ContentCoding, DecodeLimits, DecodeStatus, Decoder};

/// Bound on `decode()` calls so a stalled decoder panics instead of hanging
/// the fuzzer.
const CALL_GUARD: u32 = 500_000;

/// Every coding this build can plausibly decode (`Compress`/`Dcb` are
/// permanently unsupported per `ContentCoding::is_decodable`'s doc comment,
/// so are omitted — `Decoder::new` would just refuse them every time,
/// adding no coverage).
const CANDIDATES: [ContentCoding; 5] = [
    ContentCoding::Identity,
    ContentCoding::Deflate,
    ContentCoding::Gzip,
    ContentCoding::Brotli,
    ContentCoding::Zstd,
];

fn random_codings(u: &mut Unstructured<'_>) -> Vec<ContentCoding> {
    let count = u.arbitrary::<u8>().unwrap_or(0) % 5; // 0..=4 stages
    let mut codings = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let Ok(pick) = u.arbitrary::<u8>() else {
            break;
        };
        codings.push(CANDIDATES[(pick as usize) % CANDIDATES.len()].clone());
    }
    codings
}

fn random_limits(u: &mut Unstructured<'_>) -> DecodeLimits {
    // Capped well below `DecodeLimits::default()` so a fuzz iteration cannot
    // spend its whole time budget materialising a legitimately huge body;
    // the property under test (never exceed the declared cap) does not need
    // a realistic cap to hold.
    let max_output = u.arbitrary::<u32>().unwrap_or(0) as u64 % (1 << 20);
    let max_codings = u.arbitrary::<u8>().unwrap_or(4) as usize % 8;
    let ratio_on = u.arbitrary::<bool>().unwrap_or(true);
    let max_ratio = if ratio_on {
        Some(1.0 + (u.arbitrary::<u16>().unwrap_or(1000) as f64))
    } else {
        None
    };
    DecodeLimits::default()
        .with_max_output(max_output)
        .with_max_codings(max_codings)
        .with_max_ratio(max_ratio)
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let codings = random_codings(&mut unstructured);
    let limits = random_limits(&mut unstructured);
    let Ok(granularity_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };
    const GRANULARITIES: [usize; 6] = [1, 1, 2, 5, 64, 4096];
    let chunk_size = GRANULARITIES[(granularity_pick as usize) % GRANULARITIES.len()];

    let payload = unstructured.take_rest();

    let Ok(mut decoder) = Decoder::new(&codings, &limits) else {
        // An unsupported coding, or the chain itself is too long for
        // `max_codings` — both clean, expected rejections.
        return;
    };

    let mut out = Vec::new();
    let mut sink = [0u8; 4096];
    let mut pos = 0usize;
    let mut calls = 0u32;
    let mut done = false;

    while !done && pos < payload.len() {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress feeding real input bytes");
        let end = (pos + chunk_size).min(payload.len());
        let outcome = decoder.decode(&payload[pos..end], &mut sink, FlushMode::None);
        match outcome {
            Ok(progress) => {
                out.extend_from_slice(&sink[..progress.produced]);
                pos += progress.consumed;
                assert!(
                    out.len() as u64 <= limits.max_output,
                    "produced {} bytes, over the {}-byte cap",
                    out.len(),
                    limits.max_output
                );
                if progress.status == DecodeStatus::StreamEnd {
                    done = true;
                } else {
                    assert!(
                        progress.consumed > 0 || progress.produced > 0,
                        "no progress: {:?} with an unconsumed, non-empty chunk",
                        progress.status
                    );
                }
            }
            Err(_) => return,
        }
    }

    if !done {
        // Signal true end of input; a genuinely truncated/corrupt chain is
        // an expected `Err` here, not a bug.
        let mut calls_finish = 0u32;
        loop {
            calls_finish += 1;
            assert!(calls_finish < CALL_GUARD, "no progress in the finish phase");
            match decoder.decode(&[], &mut sink, FlushMode::Finish) {
                Ok(progress) => {
                    out.extend_from_slice(&sink[..progress.produced]);
                    assert!(
                        out.len() as u64 <= limits.max_output,
                        "produced {} bytes, over the {}-byte cap",
                        out.len(),
                        limits.max_output
                    );
                    if progress.status == DecodeStatus::StreamEnd {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
    }
    // `close()` verifies trailers/checksums; a failure here is an expected
    // outcome for hostile input, not a panic.
    let _ = decoder.close();
});
