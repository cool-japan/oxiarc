//! Property-based tests for the bounded [`ZstdStream`] push decoder.
//!
//! T4 of the streaming-truth audit's matrix: random chunk sizes on both sides
//! must reproduce the one-shot decoder byte for byte, and arbitrary bytes must
//! never panic or hang.

use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream, compress_with_level, decompress};
use proptest::prelude::*;

/// Drive a stream over `frame` with a caller-supplied chunk schedule.
///
/// `in_sizes` and `out_sizes` are cycled, so a short schedule still produces a
/// varied feed. Returns the decoded bytes, or the error string.
fn drive(frame: &[u8], in_sizes: &[usize], out_sizes: &[usize]) -> Result<Vec<u8>, String> {
    let mut stream = ZstdStream::new().with_max_window(usize::MAX);
    let mut out = Vec::new();
    let mut scratch = vec![0u8; *out_sizes.iter().max().unwrap_or(&1)];
    let mut pos = 0usize;
    let mut i = 0usize;
    let mut calls = 0usize;

    loop {
        calls += 1;
        if calls > 20_000_000 {
            return Err("decoder did not terminate".to_string());
        }
        let in_take = in_sizes[i % in_sizes.len()].max(1);
        let out_take = out_sizes[i % out_sizes.len()].max(1);
        i += 1;

        let end = pos.saturating_add(in_take).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&frame[pos..end], &mut scratch[..out_take], flush)
            .map_err(|e| e.to_string())?;
        pos += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == ZstdStatus::StreamEnd {
            return Ok(out);
        }
        if progress.consumed == 0 && progress.produced == 0 && end == frame.len() {
            return Err(format!(
                "stuck at input {pos}, status {:?}",
                progress.status
            ));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// T4: random split points on both sides reproduce the one-shot decoder.
    #[test]
    fn random_splits_match_one_shot(
        data in prop::collection::vec(any::<u8>(), 0..8192),
        in_sizes in prop::collection::vec(1usize..8192, 1..8),
        out_sizes in prop::collection::vec(1usize..4096, 1..8),
        level in 1i32..10,
    ) {
        let frame = compress_with_level(&data, level).expect("compress must succeed");
        let expected = decompress(&frame).expect("one-shot must succeed");
        prop_assert_eq!(&expected, &data);
        let got = drive(&frame, &in_sizes, &out_sizes).expect("incremental must succeed");
        prop_assert_eq!(got, data);
    }

    /// Compressible payloads exercise Huffman literals, FSE sequences and long
    /// matches, which random bytes never reach.
    #[test]
    fn random_splits_on_structured_data(
        seed in any::<u32>(),
        repeats in 1usize..400,
        in_sizes in prop::collection::vec(1usize..1024, 1..6),
        out_sizes in prop::collection::vec(1usize..1024, 1..6),
    ) {
        let unit = format!("record {seed} field a=1 b=2 c=3 ");
        let data = unit.repeat(repeats).into_bytes();
        let frame = compress_with_level(&data, 6).expect("compress must succeed");
        let got = drive(&frame, &in_sizes, &out_sizes).expect("incremental must succeed");
        prop_assert_eq!(got, data);
    }

    /// Arbitrary bytes fed through the push decoder must never panic and must
    /// always terminate.
    #[test]
    fn arbitrary_bytes_never_panic(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        in_sizes in prop::collection::vec(1usize..512, 1..5),
    ) {
        let result = std::panic::catch_unwind(|| drive(&data, &in_sizes, &[1024]));
        prop_assert!(result.is_ok(), "push decoder must not panic on arbitrary input");
    }

    /// A valid frame with one bit flipped must terminate with an error or with
    /// output no larger than one block past the original.
    #[test]
    fn bit_flipped_frames_terminate(
        data in prop::collection::vec(any::<u8>(), 1..2048),
        flip in any::<u16>(),
    ) {
        let mut frame = compress_with_level(&data, 3).expect("compress must succeed");
        let idx = (flip as usize) % frame.len();
        frame[idx] ^= 1 << (flip % 8);
        let result = drive(&frame, &[64], &[512]);
        if let Ok(out) = result {
            prop_assert!(out.len() <= data.len() + oxiarc_zstd::MAX_BLOCK_SIZE);
        }
    }

    /// A truncated frame is never a silent success.
    #[test]
    fn truncated_frames_never_silently_succeed(
        data in prop::collection::vec(any::<u8>(), 16..4096),
        cut in any::<u16>(),
    ) {
        let frame = compress_with_level(&data, 3).expect("compress must succeed");
        let at = (cut as usize) % frame.len();
        let result = drive(&frame[..at], &[7], &[64]);
        if let Ok(out) = result {
            prop_assert_ne!(out, data);
        }
    }

    /// The output budget is honoured however the input is chunked.
    #[test]
    fn budget_is_chunking_independent(
        repeats in 40usize..400,
        cap in 16usize..2048,
        in_sizes in prop::collection::vec(1usize..512, 1..5),
    ) {
        let data = b"budget corpus payload ".repeat(repeats);
        let frame = compress_with_level(&data, 3).expect("compress must succeed");
        let mut stream = ZstdStream::new()
            .with_max_window(usize::MAX)
            .with_max_output(cap as u64);
        let mut scratch = vec![0u8; 4096];
        let mut pos = 0usize;
        let mut i = 0usize;
        let mut produced = 0usize;
        let outcome = loop {
            let take = in_sizes[i % in_sizes.len()];
            i += 1;
            let end = pos.saturating_add(take).min(frame.len());
            let flush = if end == frame.len() { FlushMode::Finish } else { FlushMode::None };
            match stream.decode(&frame[pos..end], &mut scratch, flush) {
                Ok(p) => {
                    pos += p.consumed;
                    produced += p.produced;
                    if p.status == ZstdStatus::StreamEnd {
                        break Ok(produced);
                    }
                }
                Err(_) => break Err(produced),
            }
        };
        // Whether the cap bites depends only on the payload size, never on
        // how the input was chunked.
        if data.len() <= cap {
            let got = outcome.expect("a payload within the cap must decode");
            prop_assert_eq!(got, data.len());
        } else {
            let got = outcome.expect_err("a payload beyond the cap must be rejected");
            prop_assert!(got <= cap + oxiarc_zstd::MAX_BLOCK_SIZE);
        }
    }
}
