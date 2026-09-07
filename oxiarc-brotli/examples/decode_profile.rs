//! Interleaved A/B timing harness for the incremental decoder.
//!
//! Run with `cargo run --release --example decode_profile`.
//!
//! Three things make the numbers trustworthy, and all three were added because
//! a simpler harness gave answers that did not reproduce:
//!
//! * **Interleaved.** One-shot and push decodes alternate inside a single
//!   process, so thermal drift and core migration hit both columns equally. A
//!   run-A-then-run-B harness reported a 43 % difference on the same payload
//!   purely from ordering.
//! * **Min-of-N.** The reported figure is the fastest repetition. With a warm
//!   cache and nothing else in the loop, the minimum is the least noisy
//!   estimator of the code's cost.
//! * **The gate row is the one printed.** `lgwin 22`, 64 KiB input chunks,
//!   64 KiB output buffer, which is the shape an HTTP body reader has, and the
//!   configuration the throughput target is stated against.
//!
//! Set `PROFILE_ONLY=<name>` to restrict to one payload, `PROFILE_LGWIN=<n>`
//! to one window size, and `PROFILE_REPS=<n>` to change the repetition count.
//! `PROFILE_PUSH_ONLY=1` skips every one-shot decode and `PROFILE_ONE_SHOT_ONLY=1`
//! skips every push decode, so a sampling profiler attributes all of its
//! samples to one decoder and the two profiles can be compared function by
//! function (the ratios printed in either mode are meaningless by
//! construction).

use std::time::{Duration, Instant};

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

/// Drive the push decoder into a fixed, reused output buffer — the API's own
/// shape, and what an HTTP body reader or a proxy does.
fn push_decode(compressed: &[u8], in_chunk: usize, out_size: usize) -> usize {
    let mut stream = BrotliStream::new();
    let mut out = vec![0u8; out_size];
    let mut produced = 0usize;
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
        produced += progress.produced;
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    produced
}

/// The same drive, accumulating into a growing `Vec` — the *identical*
/// output-side work the one-shot [`decompress`] does, so the two columns differ
/// only in the decoder.
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

/// One column of an A/B run: every repetition, so both the minimum and the
/// median can be reported.
#[derive(Clone, Default)]
struct Column {
    reps: Vec<Duration>,
}

impl Column {
    fn new() -> Self {
        Column::default()
    }

    fn add(&mut self, dt: Duration) {
        self.reps.push(dt);
    }

    fn best(&self) -> Duration {
        self.reps.iter().copied().min().unwrap_or(Duration::ZERO)
    }
}

/// Print one A/B result line.
///
/// Two ratios are reported and they answer different questions:
///
/// * **min** is the estimator to trust on an idle machine — the fastest
///   repetition is the one least disturbed by anything else;
/// * **paired** is the estimator to trust on a *busy* one: the two calls of one
///   repetition run microseconds apart, so whatever the rest of the machine is
///   doing hits both and divides out. Taking the median of the per-repetition
///   ratios therefore stays put where the minimum wanders — the same binary
///   reported one-shot times between 16 ms and 43 ms for the same payload at
///   load average 110, and an unpaired min ratio for that row ranged 0.50-0.98
///   while the paired median stayed inside 0.71-0.75.
///
/// A run whose two figures disagree by much is a run to repeat on a quiet
/// machine.
fn report(label: &str, base: &Column, other: &Column) {
    println!(
        "  {label:<12} {:>10.3?} vs one-shot {:>10.3?}   {:.2}x min   {:.2}x paired",
        other.best(),
        base.best(),
        base.best().as_secs_f64() / other.best().as_secs_f64(),
        paired_median(base, other),
    );
}

/// Median of the per-repetition ratios `base[i] / other[i]`.
fn paired_median(base: &Column, other: &Column) -> f64 {
    let mut ratios: Vec<f64> = base
        .reps
        .iter()
        .zip(other.reps.iter())
        .filter(|(_, b)| b.as_secs_f64() > 0.0)
        .map(|(a, b)| a.as_secs_f64() / b.as_secs_f64())
        .collect();
    if ratios.is_empty() {
        return f64::NAN;
    }
    ratios.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    ratios[ratios.len() / 2]
}

/// Time `a` and `b` alternately, `reps` times each. Interleaving is what makes
/// the ratio meaningful.
fn ab(reps: usize, mut a: impl FnMut(), mut b: impl FnMut()) -> (Column, Column) {
    let (mut ca, mut cb) = (Column::new(), Column::new());
    for _ in 0..reps {
        let t = Instant::now();
        a();
        ca.add(t.elapsed());
        let t = Instant::now();
        b();
        cb.add(t.elapsed());
    }
    (ca, cb)
}

/// The physical cost of being a *bounded* push decoder on stored
/// (uncompressed) data, measured rather than argued.
///
/// A one-shot decoder decompresses into a growing `Vec` and uses that same
/// `Vec` as its LZ77 window, so a stored byte is copied exactly once. A push
/// decoder hands the byte to the caller's buffer *and* has to keep the part of
/// it a later distance could still reach, so those bytes are copied twice.
/// This routine does exactly that much work and nothing else, with the same
/// per-call allocations `push_decode` makes (a fresh output buffer and a fresh
/// ring per run), so the `incompressible` rows can be read against a floor
/// instead of against 1.0.
///
/// Two details make it a floor rather than a strawman:
///
/// * only the last `window` bytes of the whole run are mirrored — a byte with
///   more than `window` bytes behind it is out of reach of every legal distance
///   the moment the run ends, and the real decoder skips it too
///   (`BrotliWindow::push_slice_tail`). A model that mirrored every byte would
///   be beaten by the shipped decoder, which is not what a floor means;
/// * it is timed against the *same* one-shot decode the decoder rows are timed
///   against, so the two ratios share a denominator and can be compared
///   directly. (Timing it against a `Vec`-copy model of the one-shot decoder
///   instead — an earlier version of this harness — silently changed the
///   denominator by ~9 % and made the decoder look faster than the floor.)
fn two_copy_model(src: &[u8], window: usize, out_size: usize) {
    let mut out = vec![0u8; out_size];
    let mut ring = vec![0u8; window];
    let mut pos = 0usize;
    let mut done = 0usize;
    for chunk in src.chunks(out_size) {
        // Copy 1: into the caller's fixed buffer.
        out[..chunk.len()].copy_from_slice(chunk);
        done += chunk.len();
        // Copy 2: into the ring — but only the bytes still reachable once the
        // rest of the run has been produced.
        let future = src.len() - done;
        let keep = window.saturating_sub(future).min(chunk.len());
        if keep > 0 {
            let tail = &out[chunk.len() - keep..chunk.len()];
            let first = (ring.len() - pos).min(keep);
            ring[pos..pos + first].copy_from_slice(&tail[..first]);
            if first < keep {
                ring[..keep - first].copy_from_slice(&tail[first..]);
            }
            pos = (pos + keep) % ring.len();
        }
    }
    std::hint::black_box(&ring);
    std::hint::black_box(&out);
}

fn main() {
    let reps: usize = std::env::var("PROFILE_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let only = std::env::var("PROFILE_ONLY").ok();

    let mut hex_dump = Vec::new();
    for (i, chunk) in pseudo_random(1 << 20, 7).chunks(16).enumerate() {
        hex_dump.extend_from_slice(format!("line {i}: ").as_bytes());
        for b in chunk {
            hex_dump.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        hex_dump.push(b'\n');
    }
    let payloads: Vec<(&str, Vec<u8>)> = vec![
        (
            "repetitive",
            b"The quick brown fox jumps over the lazy dog. ".repeat(24_000),
        ),
        // `literal_dense` below is a misnomer at large windows and the name is
        // kept only because the baseline tables use it: the hex dump is
        // literal-dominated at `lgwin 10` (95,605 copy commands, 73 % of the
        // output is literals) but copy-dense at `lgwin 22`, where the encoder
        // finds a 5-byte match nearly everywhere and 97 % of the output arrives
        // as half a million short copies at a mean distance of 116,525. The two
        // rows therefore measure two different decoder paths; see TODO.md.
        ("single_byte", vec![0x5Au8; 1 << 20]),
        ("literal_dense", hex_dump),
        ("incompressible", pseudo_random(1 << 20, 11)),
    ];

    let push_only = std::env::var_os("PROFILE_PUSH_ONLY").is_some();
    let one_shot_only = std::env::var_os("PROFILE_ONE_SHOT_ONLY").is_some();
    println!(
        "interleaved A/B, best of {reps}, one-shot vs BrotliStream \
         (64 KiB in, 64 KiB out unless noted)\n"
    );
    for (name, data) in &payloads {
        if only.as_deref().is_some_and(|want| want != *name) {
            continue;
        }
        let only_lgwin: Option<u32> = std::env::var("PROFILE_LGWIN")
            .ok()
            .and_then(|s| s.parse().ok());
        for lgwin in [10u32, 22] {
            if only_lgwin.is_some_and(|want| want != lgwin) {
                continue;
            }
            let params = oxiarc_brotli::BrotliParams {
                quality: 5,
                lgwin,
                lgblock: 0,
            };
            let compressed = oxiarc_brotli::compress_with_params(data, &params).expect("compress");
            println!(
                "{name} lgwin {lgwin}: {} plain -> {} compressed",
                data.len(),
                compressed.len()
            );

            let one_shot_decode = |compressed: &[u8]| {
                if push_only {
                    return;
                }
                let got = decompress(compressed).expect("one-shot");
                assert_eq!(got.len(), data.len());
            };

            // The gate: 64 KiB in, 64 KiB out.
            let (one_shot, push) = ab(
                reps,
                || one_shot_decode(&compressed),
                || {
                    if one_shot_only {
                        return;
                    }
                    let n = push_decode(&compressed, 64 * 1024, 64 * 1024);
                    assert_eq!(n, data.len());
                },
            );
            report("64k/64k", &one_shot, &push);

            // Same output-side work as `decompress`: a growing `Vec`.
            let (one_shot_v, vec_sink) = ab(
                reps,
                || one_shot_decode(&compressed),
                || {
                    if one_shot_only {
                        return;
                    }
                    let v = push_decode_to_vec(&compressed, 64 * 1024, 256 * 1024);
                    assert_eq!(v.len(), data.len());
                },
            );
            report("Vec sink", &one_shot_v, &vec_sink);

            for (label, in_chunk, out_size) in [
                ("whole/256k", compressed.len(), 256 * 1024),
                ("1k/256k", 1024, 256 * 1024),
            ] {
                let (base, dt) = ab(
                    reps,
                    || one_shot_decode(&compressed),
                    || {
                        if one_shot_only {
                            return;
                        }
                        let n = push_decode(&compressed, in_chunk, out_size);
                        assert_eq!(n, data.len());
                    },
                );
                report(label, &base, &dt);
            }
            if *name == "incompressible" {
                // The same denominator as every row above: the real one-shot
                // decode, not a model of it.
                let ring = (data.len().next_power_of_two()).min(1usize << lgwin);
                let (base, model) = ab(
                    reps,
                    || one_shot_decode(&compressed),
                    || two_copy_model(data, ring, 64 * 1024),
                );
                report("copy floor", &base, &model);
            }
            println!();
        }
    }
}
