//! Parallel GZIP/DEFLATE compression using a rayon thread pool.
//!
//! Implements pigz-style multi-member GZIP compression: the input is split
//! into fixed-size chunks, each chunk is compressed independently as a
//! complete GZIP member, then the members are concatenated.  The output is a
//! valid GZIP file — RFC 1952 §2.2 states that decoders MUST decompress all
//! concatenated members in sequence, so every standard tool (gunzip, zlib,
//! etc.) can decode the result transparently.
//!
//! # Example
//!
//! ```rust
//! # #[cfg(feature = "parallel")]
//! # {
//! use oxiarc_deflate::parallel::compress_gzip_parallel;
//! use oxiarc_deflate::gzip::gzip_decompress;
//!
//! let data: Vec<u8> = b"Hello, pigz world!".to_vec();
//! let compressed = compress_gzip_parallel(&data, 6).unwrap();
//! let decompressed = gzip_decompress(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//! # }
//! ```
//!
//! # Parallel Encoder Builder
//!
//! ```rust
//! # #[cfg(feature = "parallel")]
//! # {
//! use oxiarc_deflate::parallel::ParallelGzipEncoder;
//! use oxiarc_deflate::gzip::gzip_decompress;
//!
//! let data: Vec<u8> = (0u8..=255).cycle().take(2_000_000).collect();
//! let compressed = ParallelGzipEncoder::new()
//!     .level(6)
//!     .chunk_size(1024 * 1024)
//!     .encode(&data)
//!     .unwrap();
//! // Multi-member GZIP; decompress with a multi-member-aware reader.
//! # }
//! ```

use crate::gzip::gzip_compress;
use oxiarc_core::error::Result;
use rayon::prelude::*;

/// Default chunk size per parallel GZIP member: 512 KiB.
pub const DEFAULT_CHUNK_SIZE: usize = 512 * 1024;

/// Default chunk size for the builder API: 1 MiB.
///
/// Used by [`gzip_compress_parallel`] and [`ParallelGzipEncoder`] when no
/// explicit `chunk_size` is supplied.
pub const DEFAULT_PARALLEL_CHUNK_SIZE: usize = 1024 * 1024;

/// Minimum chunk size enforced by the parallel API.
const MIN_CHUNK_SIZE: usize = 65536;

/// Compress `input` as a multi-member GZIP stream using one thread per chunk.
///
/// The output is a valid GZIP file — a simple concatenation of independent
/// GZIP members, one per `DEFAULT_CHUNK_SIZE`-byte input chunk.  All standard
/// GZIP decompressors will reconstruct the original data from the result.
///
/// # Arguments
///
/// * `input` - Raw bytes to compress.  Empty input produces a valid single-member GZIP stream.
/// * `level` - Compression level (0–9).
///
/// # Errors
///
/// Returns an error if any chunk fails to compress.
pub fn compress_gzip_parallel(input: &[u8], level: u8) -> Result<Vec<u8>> {
    if input.is_empty() {
        // Produce a valid single-member GZIP stream for empty input.
        return gzip_compress(input, level);
    }

    // Split into chunks and compress each as an independent GZIP member.
    let chunks: Vec<&[u8]> = input.chunks(DEFAULT_CHUNK_SIZE).collect();

    // Parallel map: each chunk -> Result<Vec<u8>>
    let members: Vec<Vec<u8>> = chunks
        .par_iter()
        .map(|chunk| gzip_compress(chunk, level))
        .collect::<Result<Vec<_>>>()?;

    // Concatenate all members into the final output buffer.
    let total_len: usize = members.iter().map(|m| m.len()).sum();
    let mut output = Vec::with_capacity(total_len);
    members
        .into_iter()
        .for_each(|m| output.extend_from_slice(&m));

    Ok(output)
}

/// Compress `input` as a sequence of independent raw DEFLATE streams, one per chunk.
///
/// Each chunk is compressed with raw DEFLATE (no gzip or zlib framing).
/// The output is the concatenation of those raw DEFLATE streams.
///
/// Note: The concatenated raw DEFLATE streams are **not** a standard format —
/// decompressors cannot decompress this without knowing the chunk boundaries.
/// Use [`compress_gzip_parallel`] for interoperable output.
///
/// # Arguments
///
/// * `input` - Raw bytes to compress.
/// * `level` - Compression level (0–9).
///
/// # Errors
///
/// Returns an error if any chunk fails to compress.
pub fn compress_deflate_parallel(input: &[u8], level: u8) -> Result<Vec<u8>> {
    use crate::deflate::deflate;

    if input.is_empty() {
        return deflate(input, level);
    }

    let chunks: Vec<&[u8]> = input.chunks(DEFAULT_CHUNK_SIZE).collect();

    let streams: Vec<Vec<u8>> = chunks
        .par_iter()
        .map(|chunk| deflate(chunk, level))
        .collect::<Result<Vec<_>>>()?;

    let total_len: usize = streams.iter().map(|s| s.len()).sum();
    let mut output = Vec::with_capacity(total_len);
    streams
        .into_iter()
        .for_each(|s| output.extend_from_slice(&s));

    Ok(output)
}

// ─── New builder-style parallel GZIP API ─────────────────────────────────────

/// Compress `input` as a multi-member GZIP stream with a configurable chunk size.
///
/// The output is a sequence of independent GZIP members (one per `chunk_size`
/// bytes of input) concatenated together.  Any RFC 1952-conforming decoder —
/// including [`crate::gzip::GzipDecoder`] — will transparently reconstruct the
/// original data by decompressing each member in sequence.
///
/// This mirrors the behaviour of [pigz](https://zlib.net/pigz/).
///
/// # Arguments
///
/// * `input` – Raw bytes to compress.  Empty input produces one valid
///   zero-length GZIP member.
/// * `level` – Compression level 0–9 (clamped; values > 9 become 9).
/// * `chunk_size` – Uncompressed bytes per GZIP member.  Values below 65 536
///   are silently raised to 65 536 to keep per-member overhead reasonable.
///
/// # Errors
///
/// Returns an error if any chunk fails to compress.
pub fn gzip_compress_parallel(input: &[u8], level: u32, chunk_size: usize) -> Result<Vec<u8>> {
    let chunk_size = chunk_size.max(MIN_CHUNK_SIZE);
    let level_u8 = (level.min(9)) as u8;

    if input.is_empty() {
        return gzip_compress(input, level_u8);
    }

    let chunks: Vec<&[u8]> = input.chunks(chunk_size).collect();

    // Parallel map: each chunk → compressed GZIP member bytes.
    let members: Vec<Vec<u8>> = chunks
        .par_iter()
        .map(|chunk| gzip_compress(chunk, level_u8))
        .collect::<Result<Vec<_>>>()?;

    // Serial assembly: concatenate members in original order.
    let total_len: usize = members.iter().map(|m| m.len()).sum();
    let mut output = Vec::with_capacity(total_len);
    members
        .into_iter()
        .for_each(|m| output.extend_from_slice(&m));
    Ok(output)
}

/// Builder for parallel GZIP compression.
///
/// Produces a pigz-compatible multi-member GZIP stream by compressing the
/// input in parallel chunks.
///
/// # Example
///
/// ```rust
/// # #[cfg(feature = "parallel")]
/// # {
/// use oxiarc_deflate::parallel::ParallelGzipEncoder;
///
/// let data: Vec<u8> = (0u8..=255).cycle().take(3_000_000).collect();
/// let compressed = ParallelGzipEncoder::new()
///     .level(6)
///     .chunk_size(1024 * 1024)
///     .encode(&data)
///     .unwrap();
/// assert!(compressed.starts_with(&[0x1f, 0x8b]));
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ParallelGzipEncoder {
    /// Compression level (0–9).
    pub level: u32,
    /// Uncompressed bytes per GZIP member.
    pub chunk_size: usize,
    /// Optional thread-count hint.  Stored for the caller to build a custom
    /// `rayon::ThreadPool`; the free function [`gzip_compress_parallel`] and
    /// `encode` both respect the *ambient* Rayon pool and ignore this field.
    pub num_threads: Option<usize>,
}

impl ParallelGzipEncoder {
    /// Create a new encoder with defaults: level 6, 1 MiB chunks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            level: 6,
            chunk_size: DEFAULT_PARALLEL_CHUNK_SIZE,
            num_threads: None,
        }
    }

    /// Set the compression level (0–9; values above 9 are clamped to 9).
    #[must_use]
    pub fn level(mut self, level: u32) -> Self {
        self.level = level;
        self
    }

    /// Set the chunk size in bytes.  Values below 65 536 are raised to 65 536.
    #[must_use]
    pub fn chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Store a thread-count hint.
    ///
    /// This value is **not** used internally — `encode` always runs on the
    /// ambient Rayon pool.  It is provided so callers that build a dedicated
    /// `rayon::ThreadPoolBuilder` can query the desired parallelism from this
    /// encoder configuration.
    #[must_use]
    pub fn num_threads(mut self, n: usize) -> Self {
        self.num_threads = Some(n);
        self
    }

    /// Compress `input` and return the multi-member GZIP stream.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk fails to compress.
    pub fn encode(&self, input: &[u8]) -> Result<Vec<u8>> {
        gzip_compress_parallel(input, self.level, self.chunk_size)
    }
}

impl Default for ParallelGzipEncoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gzip::gzip_decompress;

    /// Decompress a multi-member GZIP stream by repeatedly consuming members.
    fn decompress_multi_member(data: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut pos = 0usize;

        while pos < data.len() {
            // Scan forward from pos to find the end of this GZIP member.
            // Strategy: try increasing suffix lengths until decompression succeeds.
            // For our tests the member boundaries are known, so we scan the raw
            // bytes for the next GZIP magic (0x1f 0x8b) after the minimum member size.
            let member_start = pos;
            let mut member_end = data.len(); // default: last member consumes rest

            // Find next GZIP magic after at least GZIP_MIN_SIZE bytes.
            const GZIP_MIN_SIZE: usize = 18;
            if pos + GZIP_MIN_SIZE < data.len() {
                let search_from = pos + GZIP_MIN_SIZE;
                for i in search_from..data.len().saturating_sub(1) {
                    if data[i] == 0x1f && data[i + 1] == 0x8b {
                        member_end = i;
                        break;
                    }
                }
            }

            let member_bytes = &data[member_start..member_end];
            let chunk = gzip_decompress(member_bytes)
                .unwrap_or_else(|e| panic!("decompress member at {} failed: {}", pos, e));
            result.extend_from_slice(&chunk);
            pos = member_end;
        }

        result
    }

    #[test]
    fn test_parallel_gzip_roundtrip_basic() {
        let original: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let compressed = compress_gzip_parallel(&original, 6).expect("compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(decompressed, original, "basic roundtrip failed");
    }

    #[test]
    fn test_parallel_gzip_roundtrip_large() {
        // 5 MB — exercises multiple chunks
        let original: Vec<u8> = (0u8..=255).cycle().take(5_000_000).collect();
        let compressed = compress_gzip_parallel(&original, 6).expect("compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(
            decompressed.len(),
            original.len(),
            "large roundtrip length mismatch"
        );
        assert_eq!(decompressed, original, "large roundtrip data mismatch");
    }

    #[test]
    fn test_parallel_gzip_roundtrip_pattern() {
        // 1 MB of "abcabc…" — highly compressible repeating pattern
        let pattern = b"abcdef";
        let original: Vec<u8> = pattern.iter().copied().cycle().take(1_000_000).collect();
        let compressed = compress_gzip_parallel(&original, 6).expect("compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(decompressed, original, "pattern roundtrip failed");
    }

    #[test]
    fn test_parallel_gzip_vs_serial_decompresses_identically() {
        use crate::gzip::gzip_compress;

        let original: Vec<u8> = (0u8..=127).cycle().take(200_000).collect();

        let serial = gzip_compress(&original, 6).expect("serial compress failed");
        let parallel = compress_gzip_parallel(&original, 6).expect("parallel compress failed");

        let serial_dec = gzip_decompress(&serial).expect("serial decompress failed");
        let parallel_dec = decompress_multi_member(&parallel);

        assert_eq!(
            serial_dec, original,
            "serial decompressed data does not match original"
        );
        assert_eq!(
            parallel_dec, original,
            "parallel decompressed data does not match original"
        );
    }

    #[test]
    fn test_parallel_gzip_empty() {
        let compressed = compress_gzip_parallel(&[], 6).expect("empty compress failed");
        // Must start with GZIP magic.
        assert!(
            compressed.len() >= 2,
            "empty output too short: {} bytes",
            compressed.len()
        );
        assert_eq!(compressed[0], 0x1f, "missing GZIP ID1");
        assert_eq!(compressed[1], 0x8b, "missing GZIP ID2");
        let decompressed = gzip_decompress(&compressed).expect("empty decompress failed");
        assert!(
            decompressed.is_empty(),
            "empty input should decompress to empty"
        );
    }

    #[test]
    fn test_parallel_gzip_single_chunk() {
        // 100 bytes — well under DEFAULT_CHUNK_SIZE, single member
        let original: Vec<u8> = (0u8..100).collect();
        let compressed =
            compress_gzip_parallel(&original, 6).expect("single chunk compress failed");
        let decompressed = gzip_decompress(&compressed).expect("single chunk decompress failed");
        assert_eq!(decompressed, original, "single chunk roundtrip failed");
    }

    #[test]
    fn test_parallel_gzip_multi_chunk_boundary() {
        // Test sizes at chunk boundaries: CHUNK-1, CHUNK, CHUNK+1.
        for size in [
            DEFAULT_CHUNK_SIZE - 1,
            DEFAULT_CHUNK_SIZE,
            DEFAULT_CHUNK_SIZE + 1,
        ] {
            let original: Vec<u8> = (0u8..=255).cycle().take(size).collect();
            let compressed =
                compress_gzip_parallel(&original, 6).expect("boundary compress failed");
            let decompressed = decompress_multi_member(&compressed);
            assert_eq!(
                decompressed, original,
                "boundary roundtrip failed for size={}",
                size
            );
        }
    }

    #[test]
    fn test_parallel_gzip_all_levels() {
        let original: Vec<u8> = (0u8..=255).cycle().take(10_000).collect();
        for level in [1u8, 5, 9] {
            let compressed =
                compress_gzip_parallel(&original, level).expect("all-levels compress failed");
            let decompressed = decompress_multi_member(&compressed);
            assert_eq!(
                decompressed, original,
                "all-levels roundtrip failed at level={}",
                level
            );
        }
    }

    // ── Tests for the new gzip_compress_parallel / ParallelGzipEncoder API ───

    #[test]
    fn test_gzip_compress_parallel_roundtrip_levels() {
        // Test 1: roundtrip via serial GzipDecoder at levels 1, 5, 9
        // (data is ~100 KiB — one chunk — so GzipDecoder can decode directly)
        let original: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();
        for level in [1u32, 5, 9] {
            let compressed = gzip_compress_parallel(&original, level, DEFAULT_PARALLEL_CHUNK_SIZE)
                .unwrap_or_else(|e| panic!("compress failed at level {}: {}", level, e));
            let decompressed = gzip_decompress(&compressed)
                .unwrap_or_else(|e| panic!("decompress failed at level {}: {}", level, e));
            assert_eq!(
                decompressed, original,
                "roundtrip failed at level {}",
                level
            );
        }
    }

    #[test]
    fn test_gzip_compress_parallel_empty_input() {
        // Test 2: empty input → valid (minimal) GZIP stream
        let compressed = gzip_compress_parallel(&[], 6, DEFAULT_PARALLEL_CHUNK_SIZE)
            .expect("empty compress failed");
        assert!(
            compressed.len() >= 2,
            "empty output too short: {} bytes",
            compressed.len()
        );
        assert_eq!(compressed[0], 0x1f, "missing GZIP ID1");
        assert_eq!(compressed[1], 0x8b, "missing GZIP ID2");
        let decompressed = gzip_decompress(&compressed).expect("empty decompress failed");
        assert!(
            decompressed.is_empty(),
            "empty input should decompress to empty output"
        );
    }

    #[test]
    fn test_gzip_compress_parallel_sub_chunk() {
        // Test 3: sub-chunk input (< chunk_size) roundtrips
        let original: Vec<u8> = b"small sub-chunk data".to_vec();
        let compressed = gzip_compress_parallel(&original, 6, DEFAULT_PARALLEL_CHUNK_SIZE)
            .expect("sub-chunk compress failed");
        let decompressed = gzip_decompress(&compressed).expect("sub-chunk decompress failed");
        assert_eq!(decompressed, original, "sub-chunk roundtrip failed");
    }

    #[test]
    fn test_gzip_compress_parallel_multi_chunk() {
        // Test 4: multi-chunk input (> chunk_size) roundtrips using multi-member decoder
        let chunk_size = DEFAULT_PARALLEL_CHUNK_SIZE;
        let original: Vec<u8> = (0u8..=255).cycle().take(chunk_size * 3 + 12345).collect();
        let compressed =
            gzip_compress_parallel(&original, 6, chunk_size).expect("multi-chunk compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(decompressed, original, "multi-chunk roundtrip failed");
    }

    #[test]
    fn test_gzip_compress_parallel_determinism() {
        // Test 5: determinism — same input → same output on two calls
        let original: Vec<u8> = (0u8..=127).cycle().take(500_000).collect();
        let first = gzip_compress_parallel(&original, 6, DEFAULT_PARALLEL_CHUNK_SIZE)
            .expect("first compress failed");
        let second = gzip_compress_parallel(&original, 6, DEFAULT_PARALLEL_CHUNK_SIZE)
            .expect("second compress failed");
        assert_eq!(first, second, "parallel compression is not deterministic");
    }

    #[test]
    fn test_gzip_compress_parallel_one_byte() {
        // Test 6: 1-byte input roundtrip
        let original = vec![0xABu8];
        let compressed = gzip_compress_parallel(&original, 6, DEFAULT_PARALLEL_CHUNK_SIZE)
            .expect("1-byte compress failed");
        let decompressed = gzip_decompress(&compressed).expect("1-byte decompress failed");
        assert_eq!(decompressed, original, "1-byte roundtrip failed");
    }

    #[test]
    fn test_parallel_gzip_encoder_builder_roundtrip() {
        // ParallelGzipEncoder builder: default config roundtrip
        let original: Vec<u8> = (0u8..=255).cycle().take(200_000).collect();
        let encoder = ParallelGzipEncoder::new();
        let compressed = encoder.encode(&original).expect("encoder compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(decompressed, original, "builder default roundtrip failed");
    }

    #[test]
    fn test_parallel_gzip_encoder_builder_custom_level_and_chunk() {
        // ParallelGzipEncoder: custom level and chunk_size
        let original: Vec<u8> = (0u8..=255).cycle().take(300_000).collect();
        let encoder = ParallelGzipEncoder::new().level(9).chunk_size(100_000);
        let compressed = encoder.encode(&original).expect("encoder compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(
            decompressed, original,
            "builder custom config roundtrip failed"
        );
    }

    #[test]
    fn test_parallel_gzip_encoder_builder_default_impl() {
        // ParallelGzipEncoder::default() is equivalent to ::new()
        let encoder_a = ParallelGzipEncoder::new();
        let encoder_b = ParallelGzipEncoder::default();
        assert_eq!(encoder_a.level, encoder_b.level);
        assert_eq!(encoder_a.chunk_size, encoder_b.chunk_size);
        assert!(encoder_b.num_threads.is_none());
    }

    #[test]
    fn test_parallel_gzip_encoder_num_threads_stored() {
        // num_threads is stored for caller inspection
        let encoder = ParallelGzipEncoder::new().num_threads(4);
        assert_eq!(encoder.num_threads, Some(4));
    }

    #[test]
    fn test_gzip_compress_parallel_chunk_size_clamped() {
        // chunk_size < MIN_CHUNK_SIZE is clamped to MIN_CHUNK_SIZE
        let original: Vec<u8> = (0u8..=255).cycle().take(200_000).collect();
        // Use tiny chunk_size — should be clamped and still produce valid output
        let compressed =
            gzip_compress_parallel(&original, 6, 1).expect("clamped chunk_size compress failed");
        let decompressed = decompress_multi_member(&compressed);
        assert_eq!(
            decompressed, original,
            "clamped chunk_size roundtrip failed"
        );
    }
}
