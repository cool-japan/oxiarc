//! Ad-hoc timing harness for the incremental decoder.
//!
//! Prints one-shot vs push-decoder timings for a few payload shapes and chunk
//! schedules. Run with `cargo run --release --example decode_profile`.

use std::time::Instant;

use oxiarc_brotli::{BrotliStatus, BrotliStream, decompress};
use oxiarc_core::traits::FlushMode;

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Drive the push decoder into a fixed, reused output buffer.
///
/// This is the API's own shape — a caller that owns its buffer and consumes
/// each chunk as it appears, which is what an HTTP body reader or a proxy does.
fn push_decode(compressed: &[u8], in_chunk: usize, out_size: usize) -> usize {
    let mut stream = BrotliStream::new();
    let mut out = vec![0u8; out_size];
    let mut produced = 0usize;
    let mut pos = 0usize;
    let mut calls = 0usize;
    loop {
        let end = (pos + in_chunk).min(compressed.len());
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&compressed[pos..end], &mut out, flush)
            .expect("decode");
        pos += progress.consumed;
        produced += progress.produced;
        calls += 1;
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    let _ = calls;
    produced
}

/// The same drive, but accumulating into a growing `Vec` — the *identical*
/// output-side work the one-shot [`decompress`] does.
///
/// The fixed-buffer variant above is the honest measure of the streaming API,
/// but it does strictly less work than `decompress`, which allocates and grows
/// a `Vec` for the whole body. Reporting both keeps the comparison auditable:
/// the gap between the two rows is the allocation the one-shot decoder pays and
/// the push decoder does not.
fn push_decode_to_vec(compressed: &[u8], in_chunk: usize, out_size: usize) -> Vec<u8> {
    let mut stream = BrotliStream::new();
    let mut out = vec![0u8; out_size];
    let mut collected = Vec::new();
    let mut pos = 0usize;
    loop {
        let end = (pos + in_chunk).min(compressed.len());
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&compressed[pos..end], &mut out, flush)
            .expect("decode");
        pos += progress.consumed;
        collected.extend_from_slice(&out[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    collected
}

/// Repetitions per measurement. The reported figure is the fastest run: with
/// a warm cache and no other work in the loop, the minimum is the least noisy
/// estimator of the code's cost.
const REPS: usize = 12;

/// Run `f` `reps` times and return the shortest elapsed time.
fn best_of(reps: usize, mut f: impl FnMut()) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed());
    }
    best
}

fn main() {
    let mut semi = Vec::new();
    for (i, chunk) in pseudo_random(1 << 20, 7).chunks(16).enumerate() {
        semi.extend_from_slice(format!("line {i}: ").as_bytes());
        for b in chunk {
            semi.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        semi.push(b'\n');
    }
    let payloads: Vec<(&str, Vec<u8>)> = vec![
        (
            "text_1m",
            b"The quick brown fox jumps over the lazy dog. ".repeat(24_000),
        ),
        ("matchy_1m", vec![0x5Au8; 1 << 20]),
        ("semi_random", semi),
        ("random_1m", pseudo_random(1 << 20, 11)),
    ];

    for (name, data) in &payloads {
        // `lgwin` is swept because the declared window is what the push
        // decoder must actually hold: a small window is cache-resident and
        // costs nothing, a 4 MiB one is where the bounded-memory trade shows
        // up against a one-shot decoder that uses its output `Vec` as the
        // window and so touches each byte once.
        for lgwin in [10u32, 22] {
            let params = oxiarc_brotli::BrotliParams {
                quality: 5,
                lgwin,
                lgblock: 0,
            };
            let compressed = oxiarc_brotli::compress_with_params(data, &params).expect("compress");
            println!("  --- lgwin {lgwin} ---");
            println!(
                "\n{name} lgwin {lgwin}: {} plain -> {} compressed",
                data.len(),
                compressed.len()
            );
            let one_shot = best_of(REPS, || {
                let got = decompress(&compressed).expect("one-shot");
                assert_eq!(got.len(), data.len());
            });
            println!("  one-shot            {one_shot:>12.3?}");

            // Apples to apples with `decompress`: same growing-`Vec` output.
            let dt = best_of(REPS, || {
                let v = push_decode_to_vec(&compressed, 64 * 1024, 256 * 1024);
                assert_eq!(v.len(), data.len());
            });
            println!(
                "  64k -> Vec sink     {dt:>12.3?}   {:.2}x one-shot (same output-side work)",
                one_shot.as_secs_f64() / dt.as_secs_f64()
            );

            for (label, in_chunk, out_size) in [
                ("whole/256k", compressed.len(), 256 * 1024),
                ("64k/256k", 64 * 1024, 256 * 1024),
                ("1k/256k", 1024, 256 * 1024),
                ("whole/64k", compressed.len(), 64 * 1024),
            ] {
                let dt = best_of(REPS, || {
                    let n = push_decode(&compressed, in_chunk, out_size);
                    assert_eq!(n, data.len());
                });
                let ratio = one_shot.as_secs_f64() / dt.as_secs_f64();
                println!("  {label:<18}  {dt:>12.3?}   {ratio:.2}x one-shot");
            }
        }
    }
}
