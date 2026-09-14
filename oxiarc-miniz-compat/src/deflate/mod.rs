//! Compression: one-shot helpers, the `core` push interface and the
//! zlib-style `stream` interface.

pub mod core;
pub mod stream;

use self::core::{
    CompressorOxide, TDEFLFlush, TDEFLStatus, compress, create_comp_flags_from_zip_params,
};

/// How much processing the compressor should do to compress the data.
/// `NoCompression` and `BestSpeed` have special meanings, the other levels
/// determine the number of checks for matches in the hash chains.
#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CompressionLevel {
    /// Don't do any compression, only output uncompressed blocks.
    NoCompression = 0,
    /// Fast compression.
    BestSpeed = 1,
    /// Slow/high compression.
    BestCompression = 9,
    /// Even more compression (encoded like 9 here).
    UberCompression = 10,
    /// Default level.
    DefaultLevel = 6,
    /// Default, but also a negative value, as in zlib.
    DefaultCompression = -1,
}

/// Compress the input data to a vector, using the specified compression
/// level (0-10).
pub fn compress_to_vec(input: &[u8], level: u8) -> Vec<u8> {
    compress_to_vec_inner(input, level, 0, 0)
}

/// Compress the input data to a vector, using the specified compression
/// level (0-10), and with a zlib wrapper.
pub fn compress_to_vec_zlib(input: &[u8], level: u8) -> Vec<u8> {
    compress_to_vec_inner(input, level, 1, 0)
}

fn compress_to_vec_inner(mut input: &[u8], level: u8, window_bits: i32, strategy: i32) -> Vec<u8> {
    let flags = create_comp_flags_from_zip_params(level.into(), window_bits, strategy);
    let mut compressor = CompressorOxide::new(flags);
    let mut output = vec![0; (input.len() / 2).max(64)];
    let mut out_pos = 0;
    loop {
        let (status, bytes_in, bytes_out) = compress(
            &mut compressor,
            input,
            &mut output[out_pos..],
            TDEFLFlush::Finish,
        );
        out_pos += bytes_out;
        input = &input[bytes_in..];
        match status {
            TDEFLStatus::Done => {
                output.truncate(out_pos);
                break;
            }
            TDEFLStatus::Okay if output.len().saturating_sub(out_pos) < 30 => {
                let new_len = output.len() * 2;
                output.resize(new_len, 0);
            }
            TDEFLStatus::Okay => {
                if bytes_out == 0 && bytes_in == 0 {
                    let new_len = output.len() * 2;
                    output.resize(new_len, 0);
                }
            }
            // Unreachable with the in-memory encoder; return what we have
            // rather than loop forever.
            _ => {
                output.truncate(out_pos);
                break;
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn musicxml_level_10_roundtrip() {
        let xml = b"<?xml version=\"1.0\"?><score-partwise>".repeat(500);
        let packed = compress_to_vec(&xml, 10);
        assert!(packed.len() < xml.len() / 10);
        assert_eq!(crate::inflate::decompress_to_vec(&packed).ok(), Some(xml));
    }

    #[test]
    fn empty_and_stored() {
        assert_eq!(
            crate::inflate::decompress_to_vec(&compress_to_vec(b"", 6)).ok(),
            Some(Vec::new())
        );
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 31) as u8).collect();
        let stored = compress_to_vec_zlib(&data, 0);
        assert!(stored.len() > data.len());
        assert_eq!(
            crate::inflate::decompress_to_vec_zlib(&stored).ok(),
            Some(data)
        );
    }
}
