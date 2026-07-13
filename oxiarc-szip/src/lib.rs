//! Pure-Rust CCSDS-121.0-B-2 / libaec-compatible Adaptive Entropy Coding.
//!
//! This crate implements the AEC (Adaptive Entropy Coding) algorithm described
//! in CCSDS-121.0-B-2, which is the standard underlying the SZIP compression
//! format used in HDF5 datasets and many scientific data archives.
//!
//! The bit-stream framing is differentially validated against the libaec
//! reference implementation in both directions (see
//! `tests/libaec_interop.rs` for the embedded reference fixtures and the
//! `libaec-oracle` feature for a live gate): streams produced by libaec
//! decode byte-identically, and streams produced by [`encode`] are accepted
//! byte-identically by libaec's decoder. This applies to the standard
//! MSB-first bit ordering (`SzipParams::msb = true`); the LSB-first mode is
//! a crate-local extension that only round-trips with itself. The CCSDS
//! *restricted* option set and signed-sample preprocessing are not
//! implemented.
//!
//! # Primary entry points
//!
//! - [`decode`] — decompress an AEC/SZIP byte stream into raw sample bytes.
//!   Supports every standard coding option: zero-block runs (including ROS),
//!   the second-extension option, sample-split (Golomb-Rice) blocks, and
//!   no-compression blocks, with optional unit-delay predictor inversion.
//! - [`encode`] — compress a sample array into an AEC byte stream (uses the
//!   no-compression option for every block; primarily for round-trip and
//!   interoperability testing).
//!
//! # Quick example
//!
//! ```
//! use oxiarc_szip::{SzipParams, decode, encode};
//!
//! let params = SzipParams {
//!     samples: 16,
//!     ..SzipParams::default()
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

#![warn(missing_docs)]

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
/// generating valid AEC streams (accepted byte-identically by libaec's
/// decoder) in tests but not for production compression.
///
/// Requires `samples.len() >= params.samples` and every encoded value to fit
/// in `params.bits_per_pixel` bits; violations return
/// [`SzipError::InputTooShort`] / [`SzipError::SampleOutOfRange`] instead of
/// panicking or silently truncating.
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
