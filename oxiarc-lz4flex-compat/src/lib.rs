//! # oxiarc-lz4flex-compat
//!
//! A Pure Rust implementation of the [`lz4_flex`](https://docs.rs/lz4_flex)
//! 0.14 public API over [`oxiarc_lz4`]: the [`block`] module
//! (`compress_into`, `decompress_into`, `get_maximum_output_size`,
//! size-prepended helpers, dictionary variants) and the [`frame`] module
//! (`FrameEncoder: Write` with `finish`, `AutoFinishEncoder`,
//! `FrameDecoder: Read + BufRead`, `FrameInfo`, `BlockSize`, `BlockMode`,
//! `Error`).
//!
//! ```
//! use std::io::{Read, Write};
//! use oxiarc_lz4flex_compat::{block, frame};
//!
//! let input = b"lz4 lz4 lz4 lz4 lz4 lz4";
//! let mut out = vec![0; block::get_maximum_output_size(input.len())];
//! let n = block::compress_into(input, &mut out)?;
//! let mut back = vec![0; input.len()];
//! assert_eq!(block::decompress_into(&out[..n], &mut back)?, input.len());
//! assert_eq!(&back, input);
//!
//! let mut enc = frame::FrameEncoder::new(Vec::new());
//! enc.write_all(input)?;
//! let framed = enc.finish()?;
//! let mut text = Vec::new();
//! frame::FrameDecoder::new(&framed[..]).read_to_end(&mut text)?;
//! assert_eq!(&text, input);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Compressed bytes are valid LZ4 but not byte-identical to lz4_flex's.
//! The legacy frame format (`FrameInfo::legacy_frame`) is not produced;
//! a standard frame is written instead.

#![warn(missing_docs)]

pub mod block;
pub mod frame;

#[deprecated(
    since = "0.11.0",
    note = "This re-export is deprecated as it can be confused with the frame API and is not suitable for very large data, use block:: instead"
)]
pub use block::compress_into;
#[deprecated(
    since = "0.11.0",
    note = "This re-export is deprecated as it can be confused with the frame API and is not suitable for very large data, use block:: instead"
)]
pub use block::decompress_into;
#[deprecated(
    since = "0.11.0",
    note = "This re-export is deprecated as it can be confused with the frame API and is not suitable for very large data, use block:: instead"
)]
pub use block::{compress, compress_prepend_size};
#[deprecated(
    since = "0.11.0",
    note = "This re-export is deprecated as it can be confused with the frame API and is not suitable for very large data, use block:: instead"
)]
pub use block::{decompress, decompress_size_prepended};
