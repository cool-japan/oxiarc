//! Parallel Snappy framing-format compression.
//!
//! Requires the `parallel` feature, which pulls in `rayon`.

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::compress::compress;
use crate::crc32c::masked_crc32c;

// Snappy stream identifier: type 0xFF, body-length 6, then "sNaPpY"
const STREAM_IDENTIFIER: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

const CHUNK_TYPE_COMPRESSED: u8 = 0x00;
const CHUNK_TYPE_UNCOMPRESSED: u8 = 0x01;

/// Maximum uncompressed bytes per chunk (64 KiB), matching `FrameEncoder`.
pub const MAX_UNCOMPRESSED_CHUNK_SIZE: usize = 65536;

/// Compress `input` using the Snappy framing format with parallel block compression.
///
/// Output is byte-for-byte compatible with the serial `FrameEncoder` and can
/// be decoded by `FrameDecoder`.  Each 64 KiB chunk is compressed
/// independently, which is the property that makes parallelism safe.
#[cfg(feature = "parallel")]
pub fn compress_parallel(input: &[u8]) -> Vec<u8> {
    let num_chunks = input.len().div_ceil(MAX_UNCOMPRESSED_CHUNK_SIZE).max(1);
    let capacity = STREAM_IDENTIFIER.len() + num_chunks * (4 + 4 + MAX_UNCOMPRESSED_CHUNK_SIZE);

    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&STREAM_IDENTIFIER);

    if input.is_empty() {
        return output;
    }

    let chunks: Vec<&[u8]> = input.chunks(MAX_UNCOMPRESSED_CHUNK_SIZE).collect();

    // Parallel phase: compute CRC and compress every chunk independently.
    let results: Vec<(u32, Vec<u8>, usize)> = chunks
        .par_iter()
        .map(|chunk| {
            let crc = masked_crc32c(chunk);
            let compressed = compress(chunk);
            (crc, compressed, chunk.len())
        })
        .collect();

    // Serial assembly: emit chunks in order.
    for (idx, (crc, compressed, orig_len)) in results.into_iter().enumerate() {
        let chunk_data = chunks[idx];
        let crc_bytes = crc.to_le_bytes();

        if compressed.len() < orig_len {
            // Compressed chunk — payload = 4-byte CRC + compressed data
            let payload_len = 4 + compressed.len();
            let len = payload_len as u32;
            output.push(CHUNK_TYPE_COMPRESSED);
            output.push(len as u8);
            output.push((len >> 8) as u8);
            output.push((len >> 16) as u8);
            output.extend_from_slice(&crc_bytes);
            output.extend_from_slice(&compressed);
        } else {
            // Uncompressed chunk — payload = 4-byte CRC + original data
            let payload_len = 4 + orig_len;
            let len = payload_len as u32;
            output.push(CHUNK_TYPE_UNCOMPRESSED);
            output.push(len as u8);
            output.push((len >> 8) as u8);
            output.push((len >> 16) as u8);
            output.extend_from_slice(&crc_bytes);
            output.extend_from_slice(chunk_data);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "parallel")]
    use std::io::{Read, Write};

    #[cfg(feature = "parallel")]
    use super::*;
    #[cfg(feature = "parallel")]
    use crate::frame::{FrameDecoder, FrameEncoder};

    #[cfg(feature = "parallel")]
    fn decode(data: &[u8]) -> Vec<u8> {
        let mut decoder = FrameDecoder::new(data);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("decode failed");
        out
    }

    #[cfg(feature = "parallel")]
    fn encode_serial(data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = FrameEncoder::new(&mut buf);
            enc.write_all(data).expect("serial encode write failed");
            enc.finish().expect("serial encode finish failed");
        }
        buf
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        let input = vec![0x42u8; 1000];
        let compressed = compress_parallel(&input);
        assert_eq!(decode(&compressed), input);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        let input = vec![0x55u8; 500_000];
        let compressed = compress_parallel(&input);
        assert_eq!(decode(&compressed), input);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_repeated() {
        let pattern = b"hello world ";
        let input: Vec<u8> = pattern.iter().cloned().cycle().take(200_000).collect();
        let compressed = compress_parallel(&input);
        assert_eq!(decode(&compressed), input);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial_output_decompresses_identically() {
        let input: Vec<u8> = (0u32..50_000).map(|i| (i % 251) as u8).collect();
        let parallel_out = compress_parallel(&input);
        let serial_out = encode_serial(&input);

        // Both must decompress back to the original.
        assert_eq!(decode(&parallel_out), input, "parallel → original mismatch");
        assert_eq!(decode(&serial_out), input, "serial → original mismatch");
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        let compressed = compress_parallel(&[]);
        assert_eq!(decode(&compressed), &[] as &[u8]);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_single_chunk() {
        let input: Vec<u8> = (0u8..100).collect();
        let compressed = compress_parallel(&input);
        assert_eq!(decode(&compressed), input);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multi_chunk_boundary() {
        for size in [
            MAX_UNCOMPRESSED_CHUNK_SIZE - 1,
            MAX_UNCOMPRESSED_CHUNK_SIZE,
            MAX_UNCOMPRESSED_CHUNK_SIZE + 1,
        ] {
            let input: Vec<u8> = (0u32..size as u32).map(|i| (i % 199) as u8).collect();
            let compressed = compress_parallel(&input);
            assert_eq!(
                decode(&compressed),
                input,
                "roundtrip mismatch at size {size}"
            );
        }
    }
}
