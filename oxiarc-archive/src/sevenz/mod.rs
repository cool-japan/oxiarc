//! 7z archive format support.
//!
//! This module provides read support for 7z archives using LZMA/LZMA2 compression.

mod header;

pub use header::{SevenZEntry, SevenZReader};
