//! Parallel LZMA2 compression using rayon.
//!
//! This module splits input data into independent chunks, compresses each chunk
//! with a separate [`crate::Lzma2Encoder`] in parallel (via rayon), and then
//! concatenates the resulting LZMA2 sub-streams into a single valid LZMA2 stream
//! that the existing [`crate::Lzma2Decoder`] can decode without modification.
//!
//! ## Correctness invariant
//!
//! An LZMA2 stream is a sequence of LZMA2 chunks terminated by a single `0x00`
//! end-of-stream byte.  When we compress chunk *i* independently with
//! `Lzma2Encoder`, we get a complete LZMA2 sub-stream:
//!
//! ```text
//! [control byte ≥ 0x80] ... [0x00]
//! ```
//!
//! To concatenate N such sub-streams into one valid LZMA2 stream we strip the
//! trailing `0x00` from every sub-stream except the last:
//!
//! ```text
//! chunk0[..len-1]  chunk1[..len-1]  ...  chunkN-1  (already ends with 0x00)
//! ```
//!
//! Each independent sub-stream starts with a control byte that carries the
//! "reset dict + reset state + new properties" flag (`0xE0`), which is exactly
//! what `Lzma2Encoder` emits — so the decoder resets its state at every chunk
//! boundary, making concatenation safe.
//!
//! ## Compression ratio note
//!
//! Because each chunk is compressed independently (no cross-chunk dictionary
//! continuation) the compression ratio is lower than the serial
//! [`crate::Lzma2ChunkedEncoder`] for data with long-range repetitions.
//! Increase `chunk_size` to improve the ratio at the cost of reduced parallelism.
//! The default is 1 MiB, which balances the two concerns for typical workloads.

#![cfg(feature = "parallel")]

use crate::{Lzma2Encoder, LzmaLevel};
use oxiarc_core::error::{OxiArcError, Result};
use rayon::prelude::*;

/// Default chunk size for parallel LZMA2 compression (1 MiB).
///
/// Larger chunks improve the compression ratio (more context for the encoder)
/// but reduce the degree of parallelism.
pub const PARALLEL_DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Minimum chunk size for parallel LZMA2 compression (64 KiB).
///
/// Chunks smaller than this are silently clamped up to this value.
pub const PARALLEL_MIN_CHUNK_SIZE: usize = 65536;

/// Compress `input` as a parallel LZMA2 stream.
///
/// The input is split into `chunk_size` (clamped to [`PARALLEL_MIN_CHUNK_SIZE`])
/// slices.  Each slice is compressed independently by a rayon worker using
/// [`Lzma2Encoder`].  The resulting sub-streams are then concatenated (with the
/// trailing `0x00` end-of-stream marker stripped from every sub-stream except
/// the last) to form a single valid LZMA2 stream.
///
/// ## Parameters
///
/// * `input`       — data to compress (may be empty)
/// * `level`       — compression level 0–9 (clamped to the valid range internally)
/// * `chunk_size`  — uncompressed bytes per parallel chunk; clamped to
///   [`PARALLEL_MIN_CHUNK_SIZE`]
/// * `_num_threads` — ignored; to control rayon's thread count build a custom
///   [`rayon::ThreadPool`] and call this function from within it
///
/// ## Returns
///
/// A byte vector that is a valid LZMA2 stream decodable by [`crate::Lzma2Decoder`].
pub fn lzma2_compress_parallel(
    input: &[u8],
    level: u8,
    chunk_size: usize,
    _num_threads: Option<usize>,
) -> Result<Vec<u8>> {
    let chunk_size = chunk_size.max(PARALLEL_MIN_CHUNK_SIZE);
    let lzma_level = LzmaLevel::new(level);

    if input.is_empty() {
        // A valid LZMA2 stream for empty data is just the end-of-stream marker.
        return Ok(vec![0x00]);
    }

    // Single-chunk shortcut: no stripping required.
    if input.len() <= chunk_size {
        return Lzma2Encoder::new(lzma_level).encode(input);
    }

    // Collect chunk slices.
    let chunks: Vec<&[u8]> = input.chunks(chunk_size).collect();
    let n = chunks.len();

    // Parallel compression — each element is a Result<Vec<u8>>.
    let compressed: Vec<Result<Vec<u8>>> = chunks
        .into_par_iter()
        .map(|chunk| Lzma2Encoder::new(lzma_level).encode(chunk))
        .collect();

    // Propagate the first error, if any.
    let mut parts: Vec<Vec<u8>> = compressed.into_iter().collect::<Result<Vec<Vec<u8>>>>()?;

    // Validate the LZMA2 sub-stream invariants before assembly.
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(OxiArcError::corrupted(
                i as u64,
                "parallel chunk produced empty output",
            ));
        }

        // Every LZMA2 sub-stream must end with the end-of-stream marker 0x00.
        let last_byte = part[part.len() - 1];
        if last_byte != 0x00 {
            return Err(OxiArcError::corrupted(
                i as u64,
                format!(
                    "parallel chunk {}: last byte is 0x{:02X}, expected 0x00 (EOS marker)",
                    i, last_byte
                ),
            ));
        }

        // The first byte should be a valid LZMA2 control byte.  A sub-stream
        // produced by Lzma2Encoder starts with either an LZMA chunk (bit 7 set,
        // so ≥ 0x80) or an uncompressed chunk (0x01 or 0x02).
        let first_byte = part[0];
        let valid_first = first_byte >= 0x80 || first_byte == 0x01 || first_byte == 0x02;
        if !valid_first {
            return Err(OxiArcError::corrupted(
                i as u64,
                format!(
                    "parallel chunk {}: invalid LZMA2 control byte 0x{:02X}",
                    i, first_byte
                ),
            ));
        }
    }

    // Assemble: strip the trailing 0x00 from every sub-stream except the last.
    let total_len: usize = parts
        .iter()
        .enumerate()
        .map(|(i, p)| if i + 1 < n { p.len() - 1 } else { p.len() })
        .sum();

    let mut output = Vec::with_capacity(total_len);

    for (i, part) in parts.iter_mut().enumerate() {
        if i + 1 < n {
            // All but the last: strip the trailing 0x00.
            output.extend_from_slice(&part[..part.len() - 1]);
        } else {
            // Last sub-stream: keep intact (already ends with 0x00).
            output.extend_from_slice(part);
        }
    }

    Ok(output)
}

/// Builder-style encoder for parallel LZMA2 compression.
///
/// Each call to [`ParallelLzma2Encoder::encode`] is stateless — the struct
/// only holds configuration.
///
/// ## Example
///
/// ```ignore
/// use oxiarc_lzma::ParallelLzma2Encoder;
///
/// let data: Vec<u8> = (0..4 * 1024 * 1024).map(|i| i as u8).collect();
/// let compressed = ParallelLzma2Encoder::new()
///     .level(6)
///     .chunk_size(512 * 1024)
///     .encode(&data)?;
/// ```
#[derive(Debug, Clone)]
pub struct ParallelLzma2Encoder {
    /// Compression level (0–9).
    pub level: u8,
    /// Uncompressed bytes per parallel chunk.
    pub chunk_size: usize,
    /// Stored for documentation; rayon's global pool is used during encoding.
    pub num_threads: Option<usize>,
}

impl ParallelLzma2Encoder {
    /// Create a new encoder with default settings (level 6, 1 MiB chunks).
    pub fn new() -> Self {
        Self {
            level: 6,
            chunk_size: PARALLEL_DEFAULT_CHUNK_SIZE,
            num_threads: None,
        }
    }

    /// Set the compression level (0 = fastest, 9 = best ratio).
    #[must_use]
    pub fn level(mut self, level: u8) -> Self {
        self.level = level;
        self
    }

    /// Set the chunk size in bytes (clamped to [`PARALLEL_MIN_CHUNK_SIZE`]).
    #[must_use]
    pub fn chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size;
        self
    }

    /// Set the desired thread count.
    ///
    /// **Note:** this value is stored but currently has no effect — rayon's
    /// global thread pool is used.  To control parallelism, build a custom
    /// [`rayon::ThreadPool`] and call [`lzma2_compress_parallel`] from within it.
    #[must_use]
    pub fn num_threads(mut self, n: usize) -> Self {
        self.num_threads = Some(n);
        self
    }

    /// Compress `input` and return a valid LZMA2 stream.
    pub fn encode(&self, input: &[u8]) -> Result<Vec<u8>> {
        lzma2_compress_parallel(input, self.level, self.chunk_size, self.num_threads)
    }
}

impl Default for ParallelLzma2Encoder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lzma2Decoder;
    use std::io::Cursor;

    /// Helper: round-trip `data` through parallel encode + serial decode.
    fn roundtrip(data: &[u8], level: u8, chunk_size: usize) -> Vec<u8> {
        let compressed =
            lzma2_compress_parallel(data, level, chunk_size, None).expect("compress failed");
        let dict_size = LzmaLevel::new(level).dict_size();
        let mut cursor = Cursor::new(&compressed);
        Lzma2Decoder::new(dict_size)
            .decode(&mut cursor)
            .expect("decompress failed")
    }

    // ── Test 1: roundtrip at multiple levels over 256 KiB ────────────────────

    #[test]
    fn roundtrip_256k_level1() {
        let data: Vec<u8> = (0..256 * 1024).map(|i| (i * 7) as u8).collect();
        assert_eq!(roundtrip(&data, 1, PARALLEL_DEFAULT_CHUNK_SIZE), data);
    }

    #[test]
    fn roundtrip_256k_level5() {
        let data: Vec<u8> = (0..256 * 1024).map(|i| (i * 13) as u8).collect();
        assert_eq!(roundtrip(&data, 5, PARALLEL_DEFAULT_CHUNK_SIZE), data);
    }

    #[test]
    fn roundtrip_256k_level9() {
        let data: Vec<u8> = (0..256 * 1024).map(|i| (i * 17) as u8).collect();
        assert_eq!(roundtrip(&data, 9, PARALLEL_DEFAULT_CHUNK_SIZE), data);
    }

    // ── Test 2: roundtrip over larger inputs ─────────────────────────────────

    #[test]
    fn roundtrip_1m() {
        let data: Vec<u8> = (0..1024 * 1024).map(|i| (i & 0xFF) as u8).collect();
        assert_eq!(roundtrip(&data, 6, PARALLEL_DEFAULT_CHUNK_SIZE), data);
    }

    #[test]
    fn roundtrip_4m() {
        let data: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        // Use smaller chunk size to exercise multi-chunk path faster.
        assert_eq!(roundtrip(&data, 1, 512 * 1024), data);
    }

    // ── Test 3: empty input ───────────────────────────────────────────────────

    #[test]
    fn empty_input() {
        let compressed = lzma2_compress_parallel(&[], 6, PARALLEL_DEFAULT_CHUNK_SIZE, None)
            .expect("compress failed");
        assert_eq!(compressed, vec![0x00], "empty input must produce [0x00]");

        let mut cursor = Cursor::new(&compressed);
        let decompressed = Lzma2Decoder::new(1 << 16)
            .decode(&mut cursor)
            .expect("decompress failed");
        assert!(
            decompressed.is_empty(),
            "decoding empty stream must produce empty output"
        );
    }

    // ── Test 4: input < chunk_size → single-chunk path ───────────────────────

    #[test]
    fn single_chunk_path() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 200) as u8).collect();
        // chunk_size larger than data → single-chunk path
        let result = roundtrip(&data, 6, 1024 * 1024);
        assert_eq!(result, data);
    }

    // ── Test 5: input exactly 2x chunk_size → two-chunk path ─────────────────

    #[test]
    fn two_chunk_path() {
        let chunk_size = 128 * 1024;
        let data: Vec<u8> = (0..2 * chunk_size).map(|i| (i % 199) as u8).collect();
        assert_eq!(roundtrip(&data, 6, chunk_size), data);
    }

    // ── Test 6: determinism ───────────────────────────────────────────────────

    #[test]
    fn determinism() {
        let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 127) as u8).collect();
        let run1 = lzma2_compress_parallel(&data, 6, PARALLEL_DEFAULT_CHUNK_SIZE, None)
            .expect("run 1 failed");
        let run2 = lzma2_compress_parallel(&data, 6, PARALLEL_DEFAULT_CHUNK_SIZE, None)
            .expect("run 2 failed");
        assert_eq!(run1, run2, "compression must be deterministic");
    }

    // ── Test 7: verify_concatenation_invariant ────────────────────────────────
    //
    // For a 3-chunk input (chunk_size = 128 KiB, data = 3 * 128 KiB) verify:
    //   a) exactly one trailing 0x00
    //   b) assembled length == sum(chunk_lens) - (N-1)  where N = number of chunks
    //   c) each sub-stream starts with a valid LZMA2 control byte (≥ 0x80, 0x01, or 0x02)

    #[test]
    fn verify_concatenation_invariant() {
        let chunk_size = 128 * 1024;
        let n_chunks = 3usize;
        let data: Vec<u8> = (0..n_chunks * chunk_size)
            .map(|i| (i % 211) as u8)
            .collect();

        // Compress each chunk individually so we know their lengths.
        let lzma_level = LzmaLevel::new(6);
        let chunk_outputs: Vec<Vec<u8>> = data
            .chunks(chunk_size)
            .map(|chunk| {
                Lzma2Encoder::new(lzma_level)
                    .encode(chunk)
                    .expect("chunk compress")
            })
            .collect();

        let expected_total: usize = chunk_outputs
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i + 1 < n_chunks {
                    c.len() - 1
                } else {
                    c.len()
                }
            })
            .sum();

        let assembled =
            lzma2_compress_parallel(&data, 6, chunk_size, None).expect("compress failed");

        // (a) Exactly one trailing 0x00.
        assert_eq!(
            *assembled.last().expect("non-empty"),
            0x00,
            "assembled stream must end with 0x00"
        );
        // All but the last byte should not end with stray 0x00 markers at chunk
        // boundaries — we check this indirectly via the decoder below.

        // (b) Length matches sum - (N-1).
        assert_eq!(assembled.len(), expected_total, "assembled length mismatch");

        // (c) Each sub-stream starts with a valid control byte.
        for (i, chunk_out) in chunk_outputs.iter().enumerate() {
            let first = chunk_out[0];
            let valid = first >= 0x80 || first == 0x01 || first == 0x02;
            assert!(
                valid,
                "chunk {} starts with invalid control byte 0x{:02X}",
                i, first
            );
        }

        // Final sanity: full roundtrip decodes correctly.
        let mut cursor = Cursor::new(&assembled);
        let decoded = Lzma2Decoder::new(lzma_level.dict_size())
            .decode(&mut cursor)
            .expect("decompress failed");
        assert_eq!(decoded, data);
    }

    // ── Test 8: builder API ───────────────────────────────────────────────────

    #[test]
    fn builder_api() {
        let data: Vec<u8> = (0..200_000).map(|i| (i % 97) as u8).collect();
        let compressed = ParallelLzma2Encoder::new()
            .level(5)
            .chunk_size(64 * 1024)
            .num_threads(2)
            .encode(&data)
            .expect("encode failed");

        let dict_size = LzmaLevel::new(5).dict_size();
        let mut cursor = Cursor::new(&compressed);
        let decoded = Lzma2Decoder::new(dict_size)
            .decode(&mut cursor)
            .expect("decode failed");
        assert_eq!(decoded, data);
    }

    // ── Test 9: single zero byte ──────────────────────────────────────────────

    #[test]
    fn single_zero_byte() {
        let data = &[0u8];
        let result = roundtrip(data, 6, PARALLEL_DEFAULT_CHUNK_SIZE);
        assert_eq!(result, data);
    }
}
