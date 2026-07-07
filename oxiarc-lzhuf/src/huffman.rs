//! Canonical LZH/LHA Huffman decoding (MSB-first).
//!
//! This is a clean-room re-implementation of the "new-style" LHA Huffman
//! machinery (`-lh4-`/`-lh5-`/`-lh6-`/`-lh7-`), translated directly from the
//! reference `lhasa` decoder (`fragglet/lhasa`, `lib/tree_decode.c` and
//! `lib/lh_new_decoder.c`). It reads the **most-significant bit first** via
//! [`MsbBitReader`], the opposite of DEFLATE's LSB-first packing.
//!
//! Three Huffman tables appear in every block, in this exact order:
//!
//! 1. **Temp table** (a.k.a. PT-tree): up to `MAX_TEMP_CODES` symbols. Its own
//!    code lengths are sent raw — a 3-bit value per symbol, extended by a unary
//!    run when the value is 7 (`read_length_value`) — with one special case:
//!    after the length of symbol index 2, a 2-bit field says how many of the
//!    immediately-following symbols (3, 4, 5) are unused (length 0).
//! 2. **Code table** (C-tree): `NUM_CODES` symbols (0-255 literals, 256+ copy
//!    lengths). Its code lengths are themselves Huffman-encoded *using the temp
//!    table*: a decoded value `v <= 2` is a run of zero-length codes
//!    (`read_skip_count`), and `v >= 3` means "code length = `v - 2`".
//! 3. **Offset table** (P-tree): up to `MAX_OFFSET_CODES` symbols, code lengths
//!    sent raw exactly like the temp table but **without** the index-2 skip.
//!
//! Canonical Huffman assignment: symbols are ordered by `(length, symbol
//! index)` and codes assigned shortest-first; reading MSB-first walks the tree
//! from the root. A table announced with a count `n == 0` is the degenerate
//! "single code" case — every lookup yields that one symbol while consuming
//! **zero** bits ([`LzhHuffmanTree::single`]).
//!
//! See `encode.rs` for the full bitstream spec and the inverse (encoder) side.

use crate::methods::constants::NC;
use oxiarc_core::MsbBitReader;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum representable Huffman code length in canonical LHA.
///
/// Temp-table values encode code lengths as `value - 2`; the largest temp
/// symbol (18) therefore denotes length 16.
const MAX_CODE_LENGTH: u8 = 16;

/// Number of bits in the temp-table code-count field (lhasa `TEMP_CODE_BITS`).
const TEMP_CODE_BITS: u8 = 5;

/// Maximum number of temp-table codes (lhasa `MAX_TEMP_CODES`).
const MAX_TEMP_CODES: usize = (1 << TEMP_CODE_BITS) - 1; // 31

/// Leaf marker set in a tree node value (lhasa `TREE_NODE_LEAF`).
const TREE_NODE_LEAF: u32 = 1 << 31;

/// A canonical LHA Huffman decode tree stored as a flat binary-tree array,
/// exactly as `lhasa`'s `build_tree` produces it.
///
/// Each array slot is either a **leaf** (`TREE_NODE_LEAF | symbol`) or an
/// **internal node** holding the array index of its `0`-child (the `1`-child is
/// the next index). Decoding walks from the root reading one bit at a time.
#[derive(Debug, Clone)]
pub struct LzhHuffmanTree {
    /// Flat tree array; slot 0 is the root.
    nodes: Vec<u32>,
    /// If `Some`, this table decodes to a single symbol consuming zero bits.
    single: Option<u16>,
}

impl LzhHuffmanTree {
    /// Construct the degenerate single-code table (lhasa `set_tree_single`):
    /// every [`decode`](Self::decode) returns `symbol` and reads no bits.
    pub fn single(symbol: u16) -> Self {
        Self {
            nodes: vec![symbol as u32 | TREE_NODE_LEAF],
            single: Some(symbol),
        }
    }

    /// Build a canonical decode tree from per-symbol code lengths.
    ///
    /// `tree_capacity` bounds the flat array (lhasa sizes it `num_symbols * 2`).
    /// Length-0 symbols are unused. This mirrors `lhasa`'s `build_tree`:
    /// symbols are placed shortest-length-first, and within a length in
    /// ascending symbol order, yielding standard canonical codes.
    pub fn from_code_lengths(code_lengths: &[u8], tree_capacity: usize) -> Result<Self> {
        let num = code_lengths.len();
        // All slots start as leaves (symbol 0), matching lhasa `init_tree`, so a
        // malformed/incomplete table never dereferences an unwritten pointer.
        let mut nodes = vec![TREE_NODE_LEAF; tree_capacity.max(1)];

        let mut next_entry: usize = 0;
        let mut tree_allocated: usize = 1;
        let mut code_len: u8 = 0;

        loop {
            // expand_queue: give every queued node two children, pushing the
            // queue one level deeper (codes one bit longer).
            let new_nodes = (tree_allocated - next_entry) * 2;
            if tree_allocated + new_nodes <= nodes.len() {
                let end_offset = tree_allocated;
                while next_entry < end_offset {
                    nodes[next_entry] = tree_allocated as u32;
                    tree_allocated += 2;
                    next_entry += 1;
                }
            }

            code_len += 1;

            // add_codes_with_length: attach every symbol whose length matches
            // the current depth to the next free queue slot.
            let mut codes_remaining = false;
            for (symbol, &len) in code_lengths.iter().enumerate().take(num) {
                if len == code_len {
                    if next_entry < tree_allocated {
                        let node = next_entry;
                        next_entry += 1;
                        nodes[node] = symbol as u32 | TREE_NODE_LEAF;
                    }
                } else if len > code_len {
                    codes_remaining = true;
                }
            }

            if !codes_remaining || code_len >= MAX_CODE_LENGTH {
                break;
            }
        }

        Ok(Self {
            nodes,
            single: None,
        })
    }

    /// Decode one symbol (lhasa `read_from_tree`): walk from the root taking
    /// the MSB-first bit at each internal node until a leaf is reached.
    pub fn decode<R: Read>(&self, reader: &mut MsbBitReader<R>) -> Result<u16> {
        if let Some(sym) = self.single {
            return Ok(sym);
        }

        let mut code = self.nodes[0];
        let mut steps = 0u32;
        while code & TREE_NODE_LEAF == 0 {
            let bit = usize::from(reader.get_bit()?);
            let idx = code as usize + bit;
            if idx >= self.nodes.len() {
                return Err(OxiArcError::invalid_huffman(reader.bits_read()));
            }
            code = self.nodes[idx];
            steps += 1;
            if steps > u32::from(MAX_CODE_LENGTH) {
                // Guards against a malformed (cyclic) table.
                return Err(OxiArcError::invalid_huffman(reader.bits_read()));
            }
        }
        Ok((code & !TREE_NODE_LEAF) as u16)
    }
}

/// Read a length value: 3 bits, extended by a unary run of 1-bits terminated by
/// a 0-bit when the base value is 7 (lhasa `read_length_value`).
fn read_length_value<R: Read>(reader: &mut MsbBitReader<R>) -> Result<u8> {
    let mut len = reader.get_bits(3)? as u8;
    if len == 7 {
        while reader.get_bit()? {
            len += 1;
            if len >= 32 {
                // A conformant stream never approaches this; bail defensively
                // rather than loop on zero-padded EOF.
                break;
            }
        }
    }
    Ok(len)
}

/// Read the skip/zero-run count encoded by temp-table value `v` in the C-tree
/// length list (lhasa `read_skip_count`): `0 -> 1`, `1 -> 4-bit + 3`,
/// `2 -> 9-bit + 20`.
fn read_skip_count<R: Read>(reader: &mut MsbBitReader<R>, v: u16) -> Result<usize> {
    Ok(match v {
        0 => 1,
        1 => reader.get_bits(4)? as usize + 3,
        _ => reader.get_bits(9)? as usize + 20,
    })
}

/// Read the temp table (PT-tree) that in turn encodes the C-tree code lengths
/// (lhasa `read_temp_table`).
pub fn read_temp_tree<R: Read>(reader: &mut MsbBitReader<R>) -> Result<LzhHuffmanTree> {
    let n = reader.get_bits(TEMP_CODE_BITS)? as usize;

    if n == 0 {
        // Single code, value in a 5-bit field.
        let code = reader.get_bits(5)? as u16;
        return Ok(LzhHuffmanTree::single(code));
    }

    let n = n.min(MAX_TEMP_CODES);
    let mut lengths = [0u8; MAX_TEMP_CODES];

    let mut i = 0usize;
    while i < n {
        lengths[i] = read_length_value(reader)?;

        // After the length of symbol index 2, a 2-bit field says how many of
        // the next symbols (3, 4, 5) are skipped (length 0).
        if i == 2 {
            let skip = reader.get_bits(2)? as usize;
            for _ in 0..skip {
                i += 1;
                if i < lengths.len() {
                    lengths[i] = 0;
                }
            }
        }

        i += 1;
    }

    LzhHuffmanTree::from_code_lengths(&lengths[..n], MAX_TEMP_CODES * 2)
}

/// Read the C-tree (character/length codes), whose lengths are Huffman-encoded
/// using `temp_tree` (lhasa `read_code_table`).
pub fn read_code_tree<R: Read>(
    reader: &mut MsbBitReader<R>,
    temp_tree: &LzhHuffmanTree,
) -> Result<LzhHuffmanTree> {
    let n = reader.get_bits(9)? as usize;

    if n == 0 {
        let code = reader.get_bits(9)? as u16;
        return Ok(LzhHuffmanTree::single(code));
    }

    let n = n.min(NC);
    let mut lengths = vec![0u8; NC];

    let mut i = 0usize;
    while i < n {
        let v = temp_tree.decode(reader)?;
        if v <= 2 {
            let skip = read_skip_count(reader, v)?;
            for _ in 0..skip {
                if i >= n {
                    break;
                }
                lengths[i] = 0;
                i += 1;
            }
        } else {
            // Temp value v (>= 3) denotes C-tree code length v - 2.
            lengths[i] = (v - 2) as u8;
            i += 1;
        }
    }

    LzhHuffmanTree::from_code_lengths(&lengths[..n], NC * 2)
}

/// Read the offset/position tree (lhasa `read_offset_table`).
///
/// `offset_bits` is the method's count-field width and `max_codes` is
/// `(1 << offset_bits) - 1`.
pub fn read_offset_tree<R: Read>(
    reader: &mut MsbBitReader<R>,
    offset_bits: u8,
    max_codes: usize,
) -> Result<LzhHuffmanTree> {
    let n = reader.get_bits(offset_bits)? as usize;

    if n == 0 {
        let code = reader.get_bits(offset_bits)? as u16;
        return Ok(LzhHuffmanTree::single(code));
    }

    let n = n.min(max_codes);
    let mut lengths = vec![0u8; max_codes.max(1)];
    for length in lengths.iter_mut().take(n) {
        *length = read_length_value(reader)?;
    }

    LzhHuffmanTree::from_code_lengths(&lengths[..n], max_codes.max(1) * 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxiarc_core::MsbBitWriter;
    use std::io::Cursor;

    /// Emit a canonical Huffman code (MSB-first) matching the decoder.
    fn canonical_codes(lengths: &[u8]) -> Vec<u32> {
        let max_len = *lengths.iter().max().unwrap_or(&0) as usize;
        let mut bl_count = vec![0u32; max_len + 1];
        for &l in lengths {
            if l > 0 {
                bl_count[l as usize] += 1;
            }
        }
        let mut next_code = vec![0u32; max_len + 2];
        let mut code = 0u32;
        for bits in 1..=max_len {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }
        let mut codes = vec![0u32; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                codes[sym] = next_code[l as usize];
                next_code[l as usize] += 1;
            }
        }
        codes
    }

    #[test]
    fn single_code_consumes_no_bits() {
        let tree = LzhHuffmanTree::single(42);
        let data = vec![0u8; 4];
        let mut reader = MsbBitReader::new(Cursor::new(data));
        assert_eq!(tree.decode(&mut reader).expect("decode"), 42);
        assert_eq!(reader.bits_read(), 0, "single code must read zero bits");
    }

    #[test]
    fn canonical_tree_roundtrips_msb_first() {
        // Symbols 0..=3 with lengths [2,1,3,3]: canonical codes are
        // 1 -> 0, 0 -> 10, 2 -> 110, 3 -> 111.
        let lengths = [2u8, 1, 3, 3];
        let codes = canonical_codes(&lengths);
        let tree = LzhHuffmanTree::from_code_lengths(&lengths, lengths.len() * 2).expect("tree");

        let mut buf = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut buf);
            for sym in [1u16, 0, 2, 3, 1, 1, 3] {
                w.put_bits(lengths[sym as usize], codes[sym as usize])
                    .expect("put");
            }
            w.flush().expect("flush");
        }
        let mut reader = MsbBitReader::new(Cursor::new(buf));
        for sym in [1u16, 0, 2, 3, 1, 1, 3] {
            assert_eq!(tree.decode(&mut reader).expect("decode"), sym);
        }
    }

    #[test]
    fn empty_lengths_build_ok() {
        let tree = LzhHuffmanTree::from_code_lengths(&[], 2).expect("empty tree builds");
        // A degenerate empty tree decodes to the leaf-initialised root (symbol 0)
        // without panicking; real streams never invoke this.
        let mut reader = MsbBitReader::new(Cursor::new(vec![0u8; 2]));
        assert_eq!(tree.decode(&mut reader).expect("decode"), 0);
    }
}
