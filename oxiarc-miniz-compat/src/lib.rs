//! # oxiarc-miniz-compat
//!
//! A Pure Rust implementation of the [`miniz_oxide`](https://docs.rs/miniz_oxide)
//! 0.8 public API over oxiarc's DEFLATE codec, so crates that call
//! `miniz_oxide` directly (e.g. `backtrace`'s compressed-debug-section
//! reader, `musicxml`'s `.mxl` support) run without it.
//!
//! Covered surface:
//!
//! * [`inflate::decompress_to_vec`] (+ `_zlib`, `_with_limit` variants),
//!   [`inflate::decompress_slice_iter_to_slice`], [`inflate::TINFLStatus`],
//!   [`inflate::DecompressError`];
//! * [`inflate::core::decompress`] with [`inflate::core::DecompressorOxide`]
//!   and every [`inflate::core::inflate_flags`] constant;
//! * [`inflate::stream::inflate`] with [`inflate::stream::InflateState`];
//! * [`deflate::compress_to_vec`] / [`deflate::compress_to_vec_zlib`],
//!   [`deflate::CompressionLevel`], [`deflate::core::CompressorOxide`] with
//!   [`deflate::core::compress`], and [`deflate::stream::deflate`];
//! * the crate-root [`MZFlush`], [`MZStatus`], [`MZError`], [`DataFormat`],
//!   [`StreamResult`] and [`mz_adler32_oxide`].
//!
//! ```
//! use oxiarc_miniz_compat::deflate::compress_to_vec;
//! use oxiarc_miniz_compat::inflate::decompress_to_vec;
//!
//! let packed = compress_to_vec(b"miniz-shaped, oxiarc-backed", 6);
//! assert_eq!(decompress_to_vec(&packed).ok(), Some(b"miniz-shaped, oxiarc-backed".to_vec()));
//! ```
//!
//! ## Differences from miniz_oxide
//!
//! * The inflater keeps its own 32 KiB history, so a "wrapping" output
//!   buffer is filled linearly up to its end and the caller continues at
//!   `out_pos = 0`, exactly as miniz callers already do; decoded bytes are
//!   identical.
//! * Compressed output is valid DEFLATE but not byte-identical to miniz's.
//! * `TINFL_FLAG_COMPUTE_ADLER32` without a zlib header (raw streams) does
//!   not populate [`inflate::core::DecompressorOxide::adler32`].

#![warn(missing_docs)]

pub mod deflate;
pub mod inflate;

/// Initial value of an Adler-32 checksum.
pub const MZ_ADLER32_INIT: u32 = 1;

/// Default (maximum) window bits, as in zlib.
pub const MZ_DEFAULT_WINDOW_BITS: i32 = 15;

/// Update a running Adler-32 checksum with `data`.
pub fn mz_adler32_oxide(adler: u32, data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    const NMAX: usize = 5552;
    let mut a = adler & 0xffff;
    let mut b = adler >> 16;
    for chunk in data.chunks(NMAX) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// A list of flush types.
#[repr(i32)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MZFlush {
    /// Compress/decompress as much data as possible.
    None = 0,
    /// Currently treated as [`MZFlush::Sync`].
    Partial = 1,
    /// Flush all output and align to a byte boundary.
    Sync = 2,
    /// Like sync, and reset the dictionary (compression only).
    Full = 3,
    /// Finish the stream.
    Finish = 4,
    /// Not supported.
    Block = 5,
}

impl MZFlush {
    /// Create an `MZFlush` value from an integer value.
    ///
    /// # Errors
    ///
    /// [`MZError::Param`] for an unknown value.
    pub fn new(flush: i32) -> Result<Self, MZError> {
        match flush {
            0 => Ok(MZFlush::None),
            1 | 2 => Ok(MZFlush::Sync),
            3 => Ok(MZFlush::Full),
            4 => Ok(MZFlush::Finish),
            _ => Err(MZError::Param),
        }
    }
}

/// A list of miniz successful status codes.
#[repr(i32)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MZStatus {
    /// Operation succeeded; more input or output space may be needed.
    Ok = 0,
    /// The end of the stream was reached.
    StreamEnd = 1,
    /// A preset dictionary is needed (unused).
    NeedDict = 2,
}

/// A list of miniz failed status codes.
#[repr(i32)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MZError {
    /// Unused.
    ErrNo = -1,
    /// General stream error (e.g. an invalid flush or a call after finish).
    Stream = -2,
    /// Corrupt input.
    Data = -3,
    /// Unused.
    Mem = -4,
    /// No progress was possible.
    Buf = -5,
    /// Unused.
    Version = -6,
    /// Bad parameters.
    Param = -10_000,
}

/// How compressed data is wrapped.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum DataFormat {
    /// zlib header and Adler-32 trailer.
    Zlib,
    /// zlib header, trailer consumed but not verified.
    ZLibIgnoreChecksum,
    /// Raw DEFLATE.
    Raw,
}

impl DataFormat {
    /// Positive window bits mean zlib, negative raw.
    pub fn from_window_bits(window_bits: i32) -> DataFormat {
        if window_bits > 0 {
            DataFormat::Zlib
        } else {
            DataFormat::Raw
        }
    }

    /// The window bits value corresponding to this format.
    pub fn to_window_bits(self) -> i32 {
        match self {
            DataFormat::Zlib | DataFormat::ZLibIgnoreChecksum => MZ_DEFAULT_WINDOW_BITS,
            DataFormat::Raw => -MZ_DEFAULT_WINDOW_BITS,
        }
    }
}

/// `Result` alias for all miniz status codes both successful and failed.
pub type MZResult = Result<MZStatus, MZError>;

/// A structure containing the result of a call to the inflate or deflate
/// streaming functions.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct StreamResult {
    /// The number of bytes consumed from the input slice.
    pub bytes_consumed: usize,
    /// The number of bytes written to the output slice.
    pub bytes_written: usize,
    /// The return status of the call.
    pub status: MZResult,
}

impl StreamResult {
    /// A result with no progress and the given error.
    #[inline]
    pub const fn error(error: MZError) -> StreamResult {
        StreamResult {
            bytes_consumed: 0,
            bytes_written: 0,
            status: Err(error),
        }
    }
}

impl From<StreamResult> for MZResult {
    fn from(res: StreamResult) -> Self {
        res.status
    }
}

impl From<&StreamResult> for MZResult {
    fn from(res: &StreamResult) -> Self {
        res.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler_known_vector() {
        assert_eq!(mz_adler32_oxide(MZ_ADLER32_INIT, b"Wikipedia"), 0x11E6_0398);
        let big = vec![0xffu8; 100_000];
        let split = mz_adler32_oxide(mz_adler32_oxide(1, &big[..33_333]), &big[33_333..]);
        assert_eq!(split, mz_adler32_oxide(1, &big));
    }
}
