//! Pure-Rust CCSDS-121.0-B-2 / libaec-compatible Adaptive Entropy Coding.
//!
//! This crate implements the AEC (Adaptive Entropy Coding) algorithm described
//! in CCSDS-121.0-B-2, which is the standard underlying the SZIP compression
//! format used in HDF5 datasets and many scientific data archives.
//!
//! # Primary entry points
//!
//! - [`decode`] — decompress an AEC/SZIP byte stream into raw sample bytes.
//! - [`encode`] — compress a sample array into an AEC byte stream (uses the
//!   no-compression option; primarily for round-trip testing).
//!
//! # Quick example
//!
//! ```
//! use oxiarc_szip::{SzipParams, decode, encode};
//!
//! let params = SzipParams {
//!     bits_per_pixel: 8,
//!     pixels_per_block: 8,
//!     samples: 16,
//!     reference_sample_interval: 8,
//!     msb: true,
//!     nn_preprocess: false,
//!     rsi_byte_align: false,
//! };
//!
//! let samples: Vec<u64> = (0..16u64).collect();
//! let compressed = encode(&samples, &params).unwrap();
//! let raw_bytes  = decode(&compressed, &params).unwrap();
//!
//! // Verify that the decoded bytes round-trip correctly.
//! let decoded: Vec<u64> = raw_bytes.iter().map(|&b| b as u64).collect();
//! assert_eq!(decoded, samples);
//! ```

pub mod error;
pub mod params;

pub(crate) mod bitreader;
pub(crate) mod decode;
pub(crate) mod encode;

pub use error::SzipError;
pub use params::SzipParams;

/// Decode an AEC/SZIP compressed byte slice into raw sample bytes.
pub fn decode(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    decode::decode(input, params)
}

/// Encode raw sample values into an AEC/SZIP bit stream.
///
/// This always uses the no-compression option ID, making it useful for
/// generating valid AEC streams in tests but not for production compression.
pub fn encode(samples: &[u64], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    encode::encode(samples, params)
}

/// Encode raw bytes as an AEC/SZIP bit stream.
///
/// Convenience wrapper: converts `input` to a `u64` sample array (using the
/// big-endian packing implied by `params.bits_per_pixel`) and then calls
/// [`encode`].
pub fn encode_bytes(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    encode::encode_bytes(input, params)
}
