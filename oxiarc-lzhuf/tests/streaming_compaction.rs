//! Byte-exactness of [`StreamingLzhDecoder`] under the `carry` compaction that
//! bounds its memory.
//!
//! Compaction drops the fully-consumed prefix of the retained compressed
//! buffer and rebases the bit reader's cursor. If any surviving index into
//! that buffer were missed, decoding would silently produce *wrong bytes*
//! rather than an error — the one failure mode the rest of the suite's
//! truncation/corruption tests cannot catch. These tests therefore compare a
//! chunked streaming decode against the one-shot decoder byte for byte, over
//! a fixture large enough (and incompressible enough) that compaction fires
//! many times, at chunk sizes that are neither 1 nor powers of two, so a
//! compaction event cannot accidentally land on a symbol boundary every time.

use oxiarc_core::DecompressStatus;
use oxiarc_lzhuf::encode::LzhEncoder;
use oxiarc_lzhuf::streaming::StreamingLzhDecoder;
use oxiarc_lzhuf::{LzhMethod, decode_lzh};

/// SplitMix64 output: every byte decorrelated, so LZSS finds essentially no
/// matches and the compressed stream stays roughly as large as the input.
fn incompressible(size: usize) -> Vec<u8> {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(size + 8);
    while out.len() < size {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// Mixed content: long runs (matches) interleaved with high-entropy noise
/// (literals), so compaction lands inside both kinds of symbol.
fn mixed(size: usize) -> Vec<u8> {
    let noise = incompressible(size);
    let mut out = Vec::with_capacity(size + 64);
    let mut i = 0usize;
    let mut run = 3usize;
    while out.len() < size {
        let take = (run % 97) + 1;
        out.extend_from_slice(&noise[i..(i + take).min(noise.len())]);
        i = (i + take) % (noise.len() - 128);
        out.extend(std::iter::repeat_n(b'Z', (run % 61) + 5));
        run = run.wrapping_mul(31).wrapping_add(7);
    }
    out.truncate(size);
    out
}

/// Feed `compressed` to a fresh decoder in `chunk` byte slices, draining into
/// an `out_buf`-sized sink, and return the decoded bytes.
///
/// Bounded by a hard call ceiling so a regression that stops making progress
/// fails the assertion instead of hanging the suite.
fn stream_decode(
    compressed: &[u8],
    original_len: usize,
    chunk: usize,
    out_buf: usize,
    method: LzhMethod,
) -> Vec<u8> {
    let mut decoder = StreamingLzhDecoder::new(method, original_len as u64);
    let mut scratch = vec![0u8; out_buf];
    let mut output = Vec::with_capacity(original_len);

    let max_calls = compressed.len().div_ceil(chunk) + original_len.div_ceil(out_buf) + 4096;
    let mut calls = 0usize;

    for slice in compressed.chunks(chunk) {
        let mut remaining = slice;
        loop {
            calls += 1;
            assert!(
                calls < max_calls,
                "decoder made no progress after {calls} calls \
                 (chunk {chunk}, out_buf {out_buf})"
            );
            let (consumed, produced, status) = decoder
                .decompress(remaining, &mut scratch)
                .expect("streaming decode failed");
            output.extend_from_slice(&scratch[..produced]);
            remaining = &remaining[consumed..];
            match status {
                DecompressStatus::NeedsOutput => continue,
                _ => break,
            }
        }
    }

    // Drain anything still pending once the input is exhausted.
    loop {
        calls += 1;
        assert!(
            calls < max_calls,
            "drain made no progress after {calls} calls"
        );
        let (_, produced, status) = decoder
            .decompress(&[], &mut scratch)
            .expect("streaming drain failed");
        output.extend_from_slice(&scratch[..produced]);
        if produced == 0 || matches!(status, DecompressStatus::Done) {
            break;
        }
    }

    output
}

#[test]
fn compaction_is_byte_exact_at_prime_chunk_sizes() {
    // 400 KiB of incompressible data compresses to > 400 KiB with lh5, i.e.
    // more than six times the 64 KiB compaction threshold.
    let original = incompressible(400 * 1024);
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let compressed = encoder.compress_to_vec(&original).expect("encode failed");
    assert!(
        compressed.len() > 6 * 64 * 1024,
        "fixture must be large enough to compact repeatedly; got {} bytes",
        compressed.len()
    );

    let one_shot = decode_lzh(&compressed, LzhMethod::Lh5, original.len() as u64)
        .expect("one-shot decode failed");
    assert_eq!(one_shot, original, "one-shot decode is the reference");

    // Neither 1 nor a power of two, so compaction cannot align to the same
    // symbol boundary on every event.
    for chunk in [7usize, 97, 1021, 4093, 65_537] {
        for out_buf in [3usize, 1000, 64 * 1024] {
            let streamed =
                stream_decode(&compressed, original.len(), chunk, out_buf, LzhMethod::Lh5);
            assert_eq!(
                streamed.len(),
                original.len(),
                "chunk {chunk}, out_buf {out_buf}: length mismatch"
            );
            assert_eq!(
                streamed, original,
                "chunk {chunk}, out_buf {out_buf}: streaming decode differs from one-shot \
                 (carry compaction corrupted the bit stream?)"
            );
        }
    }
}

#[test]
fn compaction_is_byte_exact_for_mixed_content_and_wide_windows() {
    let original = mixed(300 * 1024);

    for method in [
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
    ] {
        let mut encoder = LzhEncoder::new(method);
        let compressed = encoder.compress_to_vec(&original).expect("encode failed");
        let one_shot =
            decode_lzh(&compressed, method, original.len() as u64).expect("one-shot decode failed");
        assert_eq!(
            one_shot, original,
            "{method:?}: one-shot reference mismatch"
        );

        for chunk in [1usize, 97, 1021] {
            let streamed = stream_decode(&compressed, original.len(), chunk, 777, method);
            assert_eq!(
                streamed, original,
                "{method:?}, chunk {chunk}: streaming decode differs from one-shot"
            );
        }
    }
}

/// A caller that hands the whole compressed stream over in one call must still
/// decode byte-exactly: compaction runs at the end of that call, after the
/// bit reader has moved far past the threshold in a single pass.
#[test]
fn compaction_is_byte_exact_for_a_single_whole_stream_call() {
    let original = incompressible(300 * 1024);
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let compressed = encoder.compress_to_vec(&original).expect("encode failed");

    let streamed = stream_decode(
        &compressed,
        original.len(),
        compressed.len(),
        4096,
        LzhMethod::Lh5,
    );
    assert_eq!(streamed, original, "whole-stream call decoded incorrectly");
}
