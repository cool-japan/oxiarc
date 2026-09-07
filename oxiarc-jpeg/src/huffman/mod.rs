//! Huffman entropy coding: canonical table construction and the bit reader.

mod decode;
mod table;

pub use table::HuffmanTable;

pub(crate) use decode::BitReader;
pub(crate) use table::{emit_dht, parse_dht};

/// Number of bits the fast lookup table resolves in one step.
pub(crate) const LOOKUP_BITS: u32 = 9;
/// Size of the fast lookup table.
pub(crate) const LOOKUP_SIZE: usize = 1 << LOOKUP_BITS;
