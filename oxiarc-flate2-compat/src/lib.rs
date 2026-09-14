//! # oxiarc-flate2-compat
//!
//! A Pure Rust implementation of the [`flate2`](https://docs.rs/flate2) 1.x
//! public API on top of [`oxiarc_deflate`], for COOLJAPAN projects whose
//! dependency graph needs `flate2` but must not pull in `miniz_oxide`,
//! `zlib-rs` or a C zlib.
//!
//! Use it either directly under the upstream name:
//!
//! ```toml
//! [dependencies]
//! flate2 = { package = "oxiarc-flate2-compat", version = "0.4" }
//! ```
//!
//! or through the same-name `flate2` shim in `oxiarc/shims/flate2` with
//! `[patch.crates-io]`, which re-exports this crate for third-party crates
//! that name `flate2` themselves.
//!
//! Every module path of flate2 is present: [`Compress`] / [`Decompress`]
//! (with `*_vec` and `*_uninit` variants, flush modes and exact
//! `total_in` / `total_out` accounting), [`Crc`], [`CrcReader`],
//! [`CrcWriter`], [`GzBuilder`], [`GzHeader`] and the [`read`], [`write`]
//! and [`bufread`] encoder/decoder families for gzip (single and
//! multi-member), zlib and raw DEFLATE.
//!
//! ```
//! use std::io::{Read, Write};
//! use oxiarc_flate2_compat::{Compression, read::GzDecoder, write::GzEncoder};
//!
//! let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
//! encoder.write_all(b"hello gzip")?;
//! let gz = encoder.finish()?;
//!
//! let mut text = String::new();
//! GzDecoder::new(&gz[..]).read_to_string(&mut text)?;
//! assert_eq!(text, "hello gzip");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ## Differences from flate2
//!
//! * `window_bits` arguments are accepted but the codec always uses the
//!   full 32 KiB window, and a compressor's zlib header always declares it.
//! * Output is valid, interoperable DEFLATE but not byte-identical to
//!   miniz_oxide's or zlib's output at the same level.

#![warn(missing_docs)]

mod bufreader;
mod crc;
mod flate_streams;
mod gz;
mod mem;
mod zio;

pub use crate::crc::{Crc, CrcReader, CrcWriter};
pub use crate::gz::{GzBuilder, GzHeader};
pub use crate::mem::{
    Compress, CompressError, Decompress, DecompressError, FlushCompress, FlushDecompress, Status,
};

/// Raw DEFLATE streams (RFC 1951).
mod deflate {
    crate::flate_streams::flate_streams!(false, DeflateEncoder, DeflateDecoder, "DEFLATE");
}

/// zlib streams (RFC 1950).
mod zlib {
    crate::flate_streams::flate_streams!(true, ZlibEncoder, ZlibDecoder, "ZLIB");
}

/// Types which operate over [`Read`](std::io::Read) streams, both
/// encoders and decoders for various formats.
pub mod read {
    pub use crate::deflate::read::DeflateDecoder;
    pub use crate::deflate::read::DeflateEncoder;
    pub use crate::gz::read::GzDecoder;
    pub use crate::gz::read::GzEncoder;
    pub use crate::gz::read::MultiGzDecoder;
    pub use crate::zlib::read::ZlibDecoder;
    pub use crate::zlib::read::ZlibEncoder;
}

/// Types which operate over [`Write`](std::io::Write) streams, both
/// encoders and decoders for various formats.
pub mod write {
    pub use crate::deflate::write::DeflateDecoder;
    pub use crate::deflate::write::DeflateEncoder;
    pub use crate::gz::write::GzDecoder;
    pub use crate::gz::write::GzEncoder;
    pub use crate::gz::write::MultiGzDecoder;
    pub use crate::zlib::write::ZlibDecoder;
    pub use crate::zlib::write::ZlibEncoder;
}

/// Types which operate over [`BufRead`](std::io::BufRead) streams, both
/// encoders and decoders for various formats.
pub mod bufread {
    pub use crate::deflate::bufread::DeflateDecoder;
    pub use crate::deflate::bufread::DeflateEncoder;
    pub use crate::gz::bufread::GzDecoder;
    pub use crate::gz::bufread::GzEncoder;
    pub use crate::gz::bufread::MultiGzDecoder;
    pub use crate::zlib::bufread::ZlibDecoder;
    pub use crate::zlib::bufread::ZlibEncoder;
}

/// When compressing data, the compression level can be specified by a value
/// in this struct.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Compression(u32);

impl Compression {
    /// Creates a new description of the compression level with an
    /// explicitly specified integer.
    ///
    /// The integer here is typically on a scale of 0-9 where 0 means "no
    /// compression" and 9 means "take as long as you'd like". Level 10 is
    /// accepted (as with flate2's miniz backend) and encodes like 9.
    pub const fn new(level: u32) -> Compression {
        Compression(level)
    }

    /// No compression is to be performed, this may actually inflate data
    /// slightly when encoding.
    pub const fn none() -> Compression {
        Compression(0)
    }

    /// Optimize for the best speed of encoding.
    pub const fn fast() -> Compression {
        Compression(1)
    }

    /// Optimize for the size of data being encoded.
    pub const fn best() -> Compression {
        Compression(9)
    }

    /// Returns an integer representing the compression level, typically on
    /// a scale of 0-9.
    pub fn level(&self) -> u32 {
        self.0
    }
}

impl Default for Compression {
    fn default() -> Compression {
        Compression(6)
    }
}

#[cfg(test)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<Compress>();
    assert::<Decompress>();
    assert::<read::GzEncoder<&[u8]>>();
    assert::<read::GzDecoder<&[u8]>>();
    assert::<read::MultiGzDecoder<&[u8]>>();
    assert::<write::GzEncoder<Vec<u8>>>();
    assert::<write::GzDecoder<Vec<u8>>>();
    assert::<write::ZlibEncoder<Vec<u8>>>();
    assert::<bufread::ZlibDecoder<&[u8]>>();
}
