//! Property-based tests for the incremental decoder.
//!
//! The one-shot [`oxiarc_brotli::decompress`] is the oracle throughout: for any
//! payload, any quality, and any random split schedule, the push decoder must
//! agree with it exactly — same bytes when it succeeds, same verdict when it
//! fails. Arbitrary (almost certainly invalid) bytes must never panic and must
//! never hang.

use oxiarc_brotli::{
    BrotliProgress, BrotliStatus, BrotliStream, compress, decompress, decompress_reporting_shapes,
};
use oxiarc_core::traits::FlushMode;
use proptest::prelude::*;

/// Drive the push decoder over `data` using an explicit schedule of input
/// chunk sizes and output slice sizes, cycling both.
///
/// Returns `Ok(bytes)` or the decoder's error; never loops forever (the call
/// budget is asserted).
fn drive_scheduled(
    data: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    stream: &mut BrotliStream,
) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    let mut pos = 0usize;
    let mut in_idx = 0usize;
    let mut out_idx = 0usize;
    let budget = (data.len() as u64 + 1) * 64 + 2_000_000;
    let mut calls = 0u64;
    let mut buf = vec![0u8; 1 + out_sizes.iter().copied().max().unwrap_or(1)];

    loop {
        calls += 1;
        if calls > budget {
            return Err("decoder did not terminate within the call budget".to_string());
        }
        let in_chunk = in_sizes[in_idx % in_sizes.len()].max(1);
        in_idx += 1;
        let out_chunk = out_sizes[out_idx % out_sizes.len()].max(1);
        out_idx += 1;

        let end = (pos + in_chunk).min(data.len());
        let flush = if end == data.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress: BrotliProgress =
            match stream.decode(&data[pos..end], &mut buf[..out_chunk], flush) {
                Ok(p) => p,
                Err(e) => return Err(e.to_string()),
            };
        pos += progress.consumed;
        decoded.extend_from_slice(&buf[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd && pos == data.len() {
            break;
        }
        if progress.consumed == 0 && progress.produced == 0 && end == data.len() {
            return Err("decoder stalled with all input offered".to_string());
        }
    }
    match stream.finish() {
        Ok(()) => Ok(decoded),
        Err(e) => Err(e.to_string()),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Random split schedules must reproduce the one-shot output exactly.
    #[test]
    fn random_splits_match_one_shot(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        quality in 0u32..=11,
        in_sizes in prop::collection::vec(1usize..8192, 1..8),
        out_sizes in prop::collection::vec(1usize..65536, 1..8),
    ) {
        let compressed = compress(&data, quality).expect("compress");
        let expected = decompress(&compressed).expect("one-shot decode");
        let mut stream = BrotliStream::new();
        let got = drive_scheduled(&compressed, &in_sizes, &out_sizes, &mut stream)
            .map_err(|e| TestCaseError::fail(format!("incremental decode failed: {e}")))?;
        prop_assert_eq!(got, expected);
    }

    /// The recorded meta-block shape sequence must match the one-shot decoder
    /// under any split schedule.
    #[test]
    fn random_splits_preserve_meta_block_shapes(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        quality in 0u32..=11,
        in_sizes in prop::collection::vec(1usize..512, 1..6),
    ) {
        let compressed = compress(&data, quality).expect("compress");
        let (expected_bytes, expected_shapes) =
            decompress_reporting_shapes(&compressed).expect("one-shot with shapes");
        let mut stream = BrotliStream::new().with_shape_recording(true);
        let got = drive_scheduled(&compressed, &in_sizes, &[1, 7, 4096], &mut stream)
            .map_err(|e| TestCaseError::fail(format!("incremental decode failed: {e}")))?;
        prop_assert_eq!(got, expected_bytes);
        prop_assert_eq!(stream.recorded_shapes().to_vec(), expected_shapes);
    }

    /// Arbitrary bytes: the push decoder and the one-shot decoder must reach
    /// the same verdict, with no panic and no stall.
    #[test]
    fn arbitrary_bytes_agree_with_one_shot(
        data in prop::collection::vec(any::<u8>(), 0..2048),
        in_sizes in prop::collection::vec(1usize..64, 1..5),
    ) {
        let one_shot = decompress(&data);
        let mut stream = BrotliStream::new();
        let incremental = drive_scheduled(&data, &in_sizes, &[1, 33, 1024], &mut stream);
        match (one_shot, incremental) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
            (Err(_), Err(_)) => {}
            (Ok(a), Err(e)) => prop_assert!(
                false,
                "one-shot decoded {} bytes, incremental failed: {}", a.len(), e
            ),
            (Err(e), Ok(b)) => prop_assert!(
                false,
                "one-shot failed ({}), incremental decoded {} bytes", e, b.len()
            ),
        }
    }

    /// Any prefix of a valid stream must be rejected, with a bounded number of
    /// calls and no panic.
    #[test]
    fn arbitrary_truncations_are_rejected(
        data in prop::collection::vec(any::<u8>(), 1..1024),
        quality in 0u32..=11,
        cut_fraction in 0u32..1000,
    ) {
        let compressed = compress(&data, quality).expect("compress");
        let cut = (compressed.len() * cut_fraction as usize) / 1000;
        if cut < compressed.len() {
            let mut stream = BrotliStream::new();
            let result = drive_scheduled(&compressed[..cut], &[1, 5, 97], &[1, 64], &mut stream);
            prop_assert!(
                result.is_err(),
                "a {cut}-byte prefix of a {}-byte stream decoded successfully",
                compressed.len()
            );
        }
    }
}
