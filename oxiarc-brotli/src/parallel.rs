//! Parallel Brotli compression using a custom framing format.
//!
//! Requires the `parallel` feature, which pulls in `rayon`.
//!
//! ## Custom Frame Format
//!
//! Because the standard Brotli decompressor stops at the first
//! "last meta-block" marker, raw Brotli stream concatenation is
//! not transparently decodable by the existing `decompress` function.
//! Instead, this module defines a thin custom framing:
//!
//! ```text
//! +------------------+-------------------+------------------------------+-----------+
//! | magic   (4 B LE) | num_chunks (4 B LE)| chunk_sizes[n] (n * 4 B LE) | chunks... |
//! +------------------+-------------------+------------------------------+-----------+
//! ```
//!
//! - **magic**: `0x425F4C50` (bytes `50 4C 5F 42`, i.e. `"PL_B"` little-endian)
//! - **num_chunks**: number of independently compressed Brotli chunks
//! - **chunk_sizes**: compressed byte-length of each chunk (u32 LE)
//! - **chunks**: the raw Brotli-compressed data, concatenated in order
//!
//! Each chunk is a complete, self-contained Brotli stream that decompresses
//! to a slice of the original input of at most [`DEFAULT_CHUNK_SIZE`](crate::parallel::DEFAULT_CHUNK_SIZE) bytes.
//! `decompress_frame_parallel` reads the header, decompresses each chunk
//! independently (also in parallel), and concatenates the results.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::compress::{BrotliParams, compress_with_params};
use crate::decompress::decompress;
use crate::error::{BrotliError, BrotliResult};

/// Magic number written at the start of every parallel-framed stream.
///
/// `0x425F4C50` ↔ bytes `[0x50, 0x4C, 0x5F, 0x42]` stored little-endian,
/// which reads as the ASCII string `"PL_B"` (Parallel brotLi Block).
const FRAME_MAGIC: u32 = 0x425F_4C50;

/// Default chunk size for parallel compression: 256 KiB.
pub const DEFAULT_CHUNK_SIZE: usize = 256 * 1024;

/// Byte length of the fixed-size frame header (magic + num_chunks).
const HEADER_FIXED_LEN: usize = 8;

// ─── Compression ────────────────────────────────────────────────────────────

/// Compress `input` using multiple threads with the given quality level.
///
/// The input is split into [`DEFAULT_CHUNK_SIZE`]-byte chunks; each chunk is
/// compressed independently using the full Brotli algorithm at the requested
/// quality.  The result is a self-describing framed stream decodable by
/// [`decompress_frame_parallel`].
///
/// # Errors
///
/// Returns [`BrotliError`] if any individual chunk fails to compress (e.g.
/// due to an invalid quality value).
#[cfg(feature = "parallel")]
pub fn compress_parallel(input: &[u8], quality: u32) -> BrotliResult<Vec<u8>> {
    let params = BrotliParams {
        quality,
        ..BrotliParams::default()
    };
    compress_parallel_with_params(input, params)
}

/// Compress `input` using multiple threads with full parameter control.
///
/// Like [`compress_parallel`] but accepts a [`BrotliParams`] struct for
/// fine-grained control over quality, window size, and block size.
///
/// # Errors
///
/// Returns [`BrotliError`] if parameter validation fails or any chunk
/// compression fails.
#[cfg(feature = "parallel")]
pub fn compress_parallel_with_params(input: &[u8], params: BrotliParams) -> BrotliResult<Vec<u8>> {
    // Validate parameters once up-front before spawning any threads.
    params.validate()?;

    // Collect chunk slices so we can index them by position later.
    let chunks: Vec<&[u8]> = if input.is_empty() {
        // Always emit at least one chunk so the frame is non-trivial.
        vec![input]
    } else {
        input.chunks(DEFAULT_CHUNK_SIZE).collect()
    };

    let num_chunks = chunks.len();

    // Parallel phase: compress every chunk independently.
    let compressed_chunks: Vec<BrotliResult<Vec<u8>>> = chunks
        .par_iter()
        .map(|chunk| compress_with_params(chunk, &params))
        .collect();

    // Propagate the first error (if any) before allocating the output buffer.
    let compressed_chunks: Vec<Vec<u8>> = compressed_chunks
        .into_iter()
        .collect::<BrotliResult<Vec<Vec<u8>>>>()?;

    // Compute total output size to avoid reallocations.
    let total_payload: usize = compressed_chunks.iter().map(|c| c.len()).sum();
    let header_len = HEADER_FIXED_LEN + num_chunks * 4;
    let mut output = Vec::with_capacity(header_len + total_payload);

    // Write magic (u32 LE).
    output.extend_from_slice(&FRAME_MAGIC.to_le_bytes());

    // Write num_chunks (u32 LE).
    output.extend_from_slice(&(num_chunks as u32).to_le_bytes());

    // Write chunk_sizes table (u32 LE each).
    for chunk in &compressed_chunks {
        output.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    }

    // Append compressed chunk data in order.
    for chunk in compressed_chunks {
        output.extend_from_slice(&chunk);
    }

    Ok(output)
}

// ─── Decompression ──────────────────────────────────────────────────────────

/// Decompress a stream produced by [`compress_parallel`] or
/// [`compress_parallel_with_params`].
///
/// The frame header is parsed first; then all chunks are decompressed in
/// parallel and their outputs are concatenated in the original order.
///
/// # Errors
///
/// Returns [`BrotliError::CorruptedData`] if the magic number is missing,
/// the header is truncated, a chunk boundary overflows the data buffer, or
/// any individual Brotli chunk fails to decompress.
#[cfg(feature = "parallel")]
pub fn decompress_frame_parallel(data: &[u8]) -> BrotliResult<Vec<u8>> {
    // ── Parse fixed header ────────────────────────────────────────────────
    if data.len() < HEADER_FIXED_LEN {
        return Err(BrotliError::CorruptedData(format!(
            "parallel frame too short: {} bytes (need at least {HEADER_FIXED_LEN})",
            data.len()
        )));
    }

    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic != FRAME_MAGIC {
        return Err(BrotliError::CorruptedData(format!(
            "invalid parallel frame magic: {magic:#010x} (expected {FRAME_MAGIC:#010x})"
        )));
    }

    let num_chunks = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;

    // ── Parse chunk-size table ────────────────────────────────────────────
    let sizes_start = HEADER_FIXED_LEN;
    let sizes_end = sizes_start.checked_add(num_chunks * 4).ok_or_else(|| {
        BrotliError::CorruptedData("chunk count overflows address space".to_string())
    })?;

    if data.len() < sizes_end {
        return Err(BrotliError::CorruptedData(format!(
            "parallel frame header truncated: need {sizes_end} bytes for {num_chunks} chunk sizes, \
             have {}",
            data.len()
        )));
    }

    let mut chunk_sizes: Vec<usize> = Vec::with_capacity(num_chunks);
    for i in 0..num_chunks {
        let off = sizes_start + i * 4;
        let sz = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        chunk_sizes.push(sz as usize);
    }

    // ── Build per-chunk byte-slice references ─────────────────────────────
    let payload = &data[sizes_end..];
    let mut chunk_slices: Vec<&[u8]> = Vec::with_capacity(num_chunks);
    let mut cursor = 0usize;
    for &sz in &chunk_sizes {
        let end = cursor.checked_add(sz).ok_or_else(|| {
            BrotliError::CorruptedData("chunk size overflows address space".to_string())
        })?;
        if end > payload.len() {
            return Err(BrotliError::CorruptedData(format!(
                "chunk boundary {end} exceeds payload length {}",
                payload.len()
            )));
        }
        chunk_slices.push(&payload[cursor..end]);
        cursor = end;
    }

    // ── Parallel decompression ────────────────────────────────────────────
    let results: Vec<BrotliResult<Vec<u8>>> = chunk_slices
        .par_iter()
        .map(|slice| decompress(slice))
        .collect();

    // ── Propagate errors then concatenate ─────────────────────────────────
    let decompressed_chunks: Vec<Vec<u8>> = results
        .into_iter()
        .collect::<BrotliResult<Vec<Vec<u8>>>>()?;

    let total_len: usize = decompressed_chunks.iter().map(|c| c.len()).sum();
    let mut output = Vec::with_capacity(total_len);
    for chunk in decompressed_chunks {
        output.extend_from_slice(&chunk);
    }

    Ok(output)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(feature = "parallel")]
    use super::*;
    #[cfg(feature = "parallel")]
    use crate::compress::compress_with_params;

    /// Generate a byte vec filled with the given byte value.
    #[cfg(feature = "parallel")]
    fn uniform(byte: u8, len: usize) -> Vec<u8> {
        vec![byte; len]
    }

    /// Generate a repeating-pattern byte vec of the given total length.
    #[cfg(feature = "parallel")]
    fn pattern(src: &[u8], len: usize) -> Vec<u8> {
        src.iter().cloned().cycle().take(len).collect()
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        let input = uniform(0x42, 1000);
        let compressed = compress_parallel(&input, 6).expect("compress");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress");
        assert_eq!(decompressed, input, "roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        let input = uniform(0x55, 2_000_000);
        let compressed = compress_parallel(&input, 6).expect("compress");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress");
        assert_eq!(decompressed, input, "large roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_pattern() {
        let input = pattern(b"hello world ", 500_000);
        let compressed = compress_parallel(&input, 6).expect("compress");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress");
        assert_eq!(decompressed, input, "pattern roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial_decompresses_identically() {
        let input: Vec<u8> = (0u32..100_000).map(|i| (i % 251) as u8).collect();
        let params = BrotliParams::default();

        // Parallel path.
        let parallel_compressed =
            compress_parallel(&input, params.quality).expect("parallel compress");
        let parallel_out =
            decompress_frame_parallel(&parallel_compressed).expect("parallel decomp");

        // Serial path: use compress_with_params on the full input.
        let serial_params = BrotliParams {
            quality: params.quality,
            ..BrotliParams::default()
        };
        let serial_compressed =
            compress_with_params(&input, &serial_params).expect("serial compress");
        let serial_out = crate::decompress::decompress(&serial_compressed).expect("serial decomp");

        assert_eq!(parallel_out, input, "parallel → original mismatch");
        assert_eq!(serial_out, input, "serial → original mismatch");
        assert_eq!(
            parallel_out, serial_out,
            "parallel and serial decompressed outputs differ"
        );
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        let input: &[u8] = &[];
        let compressed = compress_parallel(input, 6).expect("compress empty");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress empty");
        assert_eq!(decompressed, input, "empty roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_single_chunk() {
        // Well below DEFAULT_CHUNK_SIZE — everything fits in one chunk.
        let input = uniform(0xAB, 1000);
        let compressed = compress_parallel(&input, 6).expect("compress single chunk");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress single chunk");
        assert_eq!(decompressed, input, "single-chunk roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multi_chunk_boundary() {
        for size in [
            DEFAULT_CHUNK_SIZE - 1,
            DEFAULT_CHUNK_SIZE,
            DEFAULT_CHUNK_SIZE + 1,
        ] {
            let input: Vec<u8> = (0u32..size as u32).map(|i| (i % 199) as u8).collect();
            let compressed =
                compress_parallel(&input, 6).unwrap_or_else(|_| panic!("compress size={size}"));
            let decompressed = decompress_frame_parallel(&compressed)
                .unwrap_or_else(|_| panic!("decompress size={size}"));
            assert_eq!(decompressed, input, "roundtrip mismatch at size {size}");
        }
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_with_params() {
        let input: Vec<u8> = (0u16..10_000).map(|i| (i % 256) as u8).collect();
        let params = BrotliParams {
            quality: 4,
            ..BrotliParams::default()
        };
        let compressed =
            compress_parallel_with_params(&input, params).expect("compress with params");
        let decompressed = decompress_frame_parallel(&compressed).expect("decompress with params");
        assert_eq!(decompressed, input, "with-params roundtrip mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_decompress_invalid_magic() {
        let bad: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03, 0x01, 0x00, 0x00, 0x00];
        let result = decompress_frame_parallel(&bad);
        assert!(result.is_err(), "expected error on bad magic");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_decompress_truncated_header() {
        let short: Vec<u8> = vec![0x50, 0x4C, 0x5F]; // only 3 bytes
        let result = decompress_frame_parallel(&short);
        assert!(result.is_err(), "expected error on truncated header");
    }
}
