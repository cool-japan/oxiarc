//! LZH/LHA archive format support.
//!
//! This module provides reading, writing, and extraction of LZH archives with support for
//! header levels 0, 1, 2, and 3.

pub mod extensions;
pub use extensions::LzhExtensionMetadata;

#[cfg(test)]
mod extensions_tests;

pub mod header;
pub use header::LzhHeader;

pub mod reader;
pub use reader::LzhReader;
#[cfg(feature = "mmap")]
pub use reader::open_lzh_mmap;

pub mod writer;
pub use writer::{LzhCompressionLevel, LzhWriter};

pub mod stream;
pub use stream::{LzhStreamEntry, LzhStreamReader};

#[cfg(test)]
mod tests;

#[cfg(test)]
#[cfg(feature = "mmap")]
mod mmap_tests;
