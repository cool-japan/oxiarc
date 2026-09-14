//! # oxiarc-snap-compat
//!
//! A Pure Rust implementation of the [`snap`](https://docs.rs/snap) 1.x
//! public API over [`oxiarc_snappy`]: the raw block format
//! ([`raw::Encoder`], [`raw::Decoder`], [`raw::decompress_len`],
//! [`raw::max_compress_len`]), the framing format as `Read`/`Write`
//! adapters ([`read::FrameDecoder`], [`read::FrameEncoder`],
//! [`write::FrameEncoder`]) and the [`Error`] enum with snap's variants.
//!
//! ```
//! use oxiarc_snap_compat::raw::{Decoder, Encoder, decompress_len, max_compress_len};
//!
//! let input = b"snappy snappy snappy snappy";
//! let mut compressed = vec![0; max_compress_len(input.len())];
//! let n = Encoder::new().compress(input, &mut compressed)?;
//! let mut out = vec![0; decompress_len(&compressed[..n])?];
//! Decoder::new().decompress(&compressed[..n], &mut out)?;
//! assert_eq!(out, input);
//! # Ok::<(), oxiarc_snap_compat::Error>(())
//! ```
//!
//! Compressed bytes are valid Snappy but not byte-identical to snap's.

#![warn(missing_docs)]

mod error;
pub mod raw;
pub mod read;
pub mod write;

pub use crate::error::{Error, Result};

/// The maximum block that Snappy processes at a time (the framing format's
/// uncompressed chunk limit).
const MAX_BLOCK_SIZE: usize = 1 << 16;

/// The maximum size of input the raw format accepts (`u32::MAX`).
const MAX_INPUT_SIZE: u64 = u32::MAX as u64;

/// The stream identifier chunk that starts every framed stream.
const STREAM_IDENTIFIER: &[u8] = b"\xFF\x06\x00\x00sNaPpY";
