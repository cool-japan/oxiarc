//! Huffman coding for Brotli compression and decompression.
//!
//! Brotli uses canonical prefix codes (Huffman codes) extensively.
//! This module implements both building and decoding of these codes.
//!
//! ## RFC 7932 Prefix Code Format
//!
//! Prefix codes in Brotli are represented by code lengths. The canonical
//! code assignment is: shorter codes get smaller values, and within the
//! same length, symbols are ordered by their natural ordering.
//!
//! Special cases:
//! - A single symbol uses code length 0 (no bits needed).
//! - "Simple" prefix codes encode 1-4 symbols with fixed patterns.
//! - "Complex" prefix codes use a two-level Huffman scheme.

use crate::bit_reader::BitReader;
use crate::bit_writer::BitWriter;
use crate::error::{BrotliError, BrotliResult};

/// Maximum code length for Brotli Huffman codes.
pub const MAX_HUFFMAN_CODE_LENGTH: u32 = 15;

/// Maximum number of symbols in any Brotli alphabet.
pub const MAX_HUFFMAN_SYMBOLS: usize = 704;

/// A Huffman tree for decoding, represented as a lookup table.
#[derive(Debug, Clone)]
pub struct HuffmanTree {
    /// For each symbol, its code length. 0 means the symbol is not in the alphabet.
    pub code_lengths: Vec<u8>,
    /// Number of symbols in the alphabet.
    pub alphabet_size: u32,
    /// Lookup table for fast decoding (indexed by peeked bits).
    /// Each entry: (symbol, code_length).
    lut: Vec<(u16, u8)>,
    /// Number of bits used for the LUT.
    lut_bits: u32,
}

/// LUT bits for fast decoding.
const LUT_BITS: u32 = 10;

impl HuffmanTree {
    /// Create a Huffman tree from code lengths.
    pub fn from_code_lengths(code_lengths: &[u8], alphabet_size: u32) -> BrotliResult<Self> {
        let mut tree = HuffmanTree {
            code_lengths: code_lengths.to_vec(),
            alphabet_size,
            lut: Vec::new(),
            lut_bits: LUT_BITS,
        };
        tree.build_lut()?;
        Ok(tree)
    }

    /// Create a trivial single-symbol tree.
    pub fn single_symbol(symbol: u16, alphabet_size: u32) -> BrotliResult<Self> {
        let mut code_lengths = vec![0u8; alphabet_size as usize];
        if (symbol as u32) < alphabet_size {
            code_lengths[symbol as usize] = 0; // single symbol needs 0 bits
        }
        Ok(HuffmanTree {
            code_lengths,
            alphabet_size,
            lut: vec![(symbol, 0); 1 << LUT_BITS],
            lut_bits: LUT_BITS,
        })
    }

    /// Build the lookup table for fast decoding.
    fn build_lut(&mut self) -> BrotliResult<()> {
        let lut_size = 1usize << self.lut_bits;
        self.lut = vec![(0, 0); lut_size];

        // Count symbols per code length.
        let mut bl_count = vec![0u32; (MAX_HUFFMAN_CODE_LENGTH + 1) as usize];
        let mut max_len = 0u32;
        let mut num_codes = 0u32;

        for &cl in &self.code_lengths {
            if cl > 0 {
                bl_count[cl as usize] += 1;
                if cl as u32 > max_len {
                    max_len = cl as u32;
                }
                num_codes += 1;
            }
        }

        // Handle special cases.
        if num_codes == 0 {
            // No symbols - this shouldn't happen in valid data, but handle gracefully.
            return Ok(());
        }
        if num_codes == 1 {
            // Single symbol tree - find the symbol.
            for (sym, &cl) in self.code_lengths.iter().enumerate() {
                if cl > 0 {
                    for entry in &mut self.lut {
                        *entry = (sym as u16, cl);
                    }
                    return Ok(());
                }
            }
        }

        // Compute first code for each length (canonical Huffman).
        let mut next_code = vec![0u32; (max_len + 1) as usize];
        let mut code = 0u32;
        for bits in 1..=max_len {
            code = (code + bl_count[bits as usize - 1]) << 1;
            next_code[bits as usize] = code;
        }

        // Assign codes to symbols and fill LUT.
        let mut symbol_codes = Vec::with_capacity(self.code_lengths.len());
        for (sym, &cl) in self.code_lengths.iter().enumerate() {
            if cl > 0 {
                let c = next_code[cl as usize];
                next_code[cl as usize] += 1;
                symbol_codes.push((sym as u16, c, cl));
            } else {
                symbol_codes.push((sym as u16, 0, 0));
            }
        }

        // Fill LUT: for codes that fit in lut_bits, fill all matching entries.
        for &(sym, code_val, cl) in &symbol_codes {
            if cl == 0 {
                continue;
            }
            let len = cl as u32;
            if len > self.lut_bits {
                // For longer codes, we need a secondary lookup. For simplicity,
                // we use a brute-force approach since LUT_BITS=10 covers most codes.
                continue;
            }
            // Reverse bits for little-endian bit reader.
            let reversed = reverse_bits(code_val, len);
            let fill_count = 1u32 << (self.lut_bits - len);
            for i in 0..fill_count {
                let idx = (reversed | (i << len)) as usize;
                if idx < self.lut.len() {
                    self.lut[idx] = (sym, cl);
                }
            }
        }

        // For codes longer than LUT_BITS, store them with a special marker.
        // We'll handle them with a slow path in decode.
        // Store the long codes in a separate structure within the LUT.
        // Actually, for Brotli, max code length is 15, so with LUT_BITS=10
        // we need to handle 11-15 bit codes specially.
        // We use a fallback linear search for these rare cases.

        Ok(())
    }

    /// Decode a single symbol from the bit reader.
    pub fn decode_symbol(&self, reader: &mut BitReader<'_>) -> BrotliResult<u16> {
        // Check for single-symbol tree first (no bits needed).
        if self.is_single_symbol() {
            return Ok(self.lut[0].0);
        }

        // Fast path: peek LUT_BITS and lookup.
        let peeked = reader.peek_bits(self.lut_bits)?;
        let (sym, len) = self.lut[peeked as usize];
        if len > 0 {
            reader.drop_bits(len as u32);
            return Ok(sym);
        }

        // Slow path: codes longer than LUT_BITS.
        self.decode_symbol_slow(reader)
    }

    /// Check if this is a true single-symbol tree (code_length = 0, no bits consumed).
    ///
    /// Returns true only when the tree was created via `single_symbol()` (all code_lengths are 0,
    /// with the LUT pre-filled). A tree with one symbol at code_length=1 is NOT single-symbol
    /// here — it still requires reading 1 bit per symbol.
    fn is_single_symbol(&self) -> bool {
        // A true single-symbol tree has ALL code lengths = 0.
        // The LUT was pre-filled by single_symbol() with (sym, 0) entries.
        self.code_lengths.iter().all(|&cl| cl == 0) && !self.lut.is_empty() && self.lut[0].1 == 0
    }

    /// Slow path for decoding codes longer than LUT_BITS.
    fn decode_symbol_slow(&self, reader: &mut BitReader<'_>) -> BrotliResult<u16> {
        // Rebuild code table and decode bit by bit.
        let mut bl_count = vec![0u32; (MAX_HUFFMAN_CODE_LENGTH + 1) as usize];
        let mut max_len = 0u32;

        for &cl in &self.code_lengths {
            if cl > 0 {
                bl_count[cl as usize] += 1;
                if cl as u32 > max_len {
                    max_len = cl as u32;
                }
            }
        }

        let mut next_code = vec![0u32; (max_len + 1) as usize];
        let mut code = 0u32;
        for bits in 1..=max_len {
            code = (code + bl_count[bits as usize - 1]) << 1;
            next_code[bits as usize] = code;
        }

        // Build (symbol, code, length) tuples for long codes.
        let mut long_codes: Vec<(u16, u32, u8)> = Vec::new();
        let mut nc = next_code.clone();
        for (sym, &cl) in self.code_lengths.iter().enumerate() {
            if cl > 0 && cl as u32 > self.lut_bits {
                let c = nc[cl as usize];
                nc[cl as usize] += 1;
                long_codes.push((sym as u16, c, cl));
            } else if cl > 0 {
                nc[cl as usize] += 1;
            }
        }

        // Read bits and try to match.
        let max_bits = max_len.min(MAX_HUFFMAN_CODE_LENGTH);
        let bits_val = reader.peek_bits(max_bits)?;
        // The bits we read are in reversed order (LSB first), so reverse them.
        let reversed = reverse_bits(bits_val, max_bits);

        for &(sym, code_val, cl) in &long_codes {
            let len = cl as u32;
            let shift = max_bits - len;
            if (reversed >> shift) == code_val {
                reader.drop_bits(len);
                return Ok(sym);
            }
        }

        Err(BrotliError::InvalidHuffmanCode(
            "no matching code found".to_string(),
        ))
    }
}

/// Reverse the bottom `n` bits of `value`.
pub fn reverse_bits(value: u32, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut result = 0u32;
    let mut v = value;
    for _ in 0..n {
        result = (result << 1) | (v & 1);
        v >>= 1;
    }
    result
}

/// Read a Brotli prefix code from the bitstream.
///
/// Per RFC 7932 Section 3.5:
/// - HSKIP (2 bits) determines the type:
///   - 1: "simple" prefix code
///   - Otherwise: "complex" prefix code with code-length codes
pub fn read_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    let hskip = reader.read_bits(2)?;

    if hskip == 1 {
        // Simple prefix code.
        read_simple_prefix_code(reader, alphabet_size)
    } else {
        // Complex prefix code.
        read_complex_prefix_code(reader, alphabet_size, hskip)
    }
}

/// Read a simple prefix code (1-4 symbols).
fn read_simple_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    let nsym_minus_1 = reader.read_bits(2)?; // NSYM - 1
    let nsym = nsym_minus_1 + 1;

    let symbol_bits = alphabet_bits(alphabet_size);

    let mut symbols = Vec::with_capacity(nsym as usize);
    for _ in 0..nsym {
        let sym = reader.read_bits(symbol_bits)?;
        if sym >= alphabet_size {
            return Err(BrotliError::InvalidPrefixCode(format!(
                "symbol {sym} exceeds alphabet size {alphabet_size}"
            )));
        }
        symbols.push(sym as u16);
    }

    let mut code_lengths = vec![0u8; alphabet_size as usize];

    match nsym {
        1 => {
            // Single symbol: code length 0 (implicit).
            return HuffmanTree::single_symbol(symbols[0], alphabet_size);
        }
        2 => {
            // Two symbols: each gets 1 bit.
            code_lengths[symbols[0] as usize] = 1;
            code_lengths[symbols[1] as usize] = 1;
        }
        3 => {
            // Three symbols: first gets 1 bit, other two get 2 bits.
            code_lengths[symbols[0] as usize] = 1;
            code_lengths[symbols[1] as usize] = 2;
            code_lengths[symbols[2] as usize] = 2;
        }
        4 => {
            // Four symbols with a tree-select bit.
            let tree_select = reader.read_bit()?;
            if tree_select {
                // All four get 2 bits.
                for &s in &symbols {
                    code_lengths[s as usize] = 2;
                }
            } else {
                // First symbol gets 1 bit, second gets 2 bits,
                // third and fourth get 3 bits.
                code_lengths[symbols[0] as usize] = 1;
                code_lengths[symbols[1] as usize] = 2;
                code_lengths[symbols[2] as usize] = 3;
                code_lengths[symbols[3] as usize] = 3;
            }
        }
        _ => {
            return Err(BrotliError::InvalidPrefixCode(format!(
                "invalid NSYM: {nsym}"
            )));
        }
    }

    HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
}

/// Code length code order per RFC 7932 Section 3.5.
const CODE_LENGTH_CODE_ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Read a complex prefix code using code-length Huffman.
fn read_complex_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
    hskip: u32,
) -> BrotliResult<HuffmanTree> {
    // Read code lengths for the code-length alphabet (0-17).
    let num_code_length_codes = 18;
    let mut cl_code_lengths = vec![0u8; num_code_length_codes];

    // hskip tells us how many initial code length codes to skip (set to 0).
    let start = hskip as usize;

    // Read code lengths for code-length codes (each is 2-5 bits using a special scheme).
    // Per the spec, we read at most (num_code_length_codes - hskip) code lengths.
    let mut space = 32i32; // Kraft inequality tracker
    let mut num_non_zero = 0;

    for &idx in &CODE_LENGTH_CODE_ORDER[start..num_code_length_codes] {
        if space <= 0 {
            break;
        }
        // Read code length code (up to 5 bits, variable length).
        let v = read_code_length_value(reader)?;
        cl_code_lengths[idx] = v;
        if v != 0 {
            space -= 32 >> v;
            num_non_zero += 1;
        }
    }

    // Special case: if only one non-zero, the whole alphabet has that code length.
    if num_non_zero == 0 {
        return Err(BrotliError::InvalidPrefixCode(
            "all code length codes are zero".to_string(),
        ));
    }

    // Build Huffman tree for code lengths.
    let cl_tree = HuffmanTree::from_code_lengths(&cl_code_lengths, num_code_length_codes as u32)?;

    // Now decode the actual code lengths for the alphabet.
    let mut code_lengths = vec![0u8; alphabet_size as usize];
    let mut i = 0u32;
    let mut prev_code_len = 8u8;
    let mut repeat_count;

    while i < alphabet_size {
        let sym = cl_tree.decode_symbol(reader)?;

        match sym {
            0 => {
                // Literal zero.
                code_lengths[i as usize] = 0;
                i += 1;
            }
            1..=15 => {
                // Literal code length.
                code_lengths[i as usize] = sym as u8;
                prev_code_len = sym as u8;
                i += 1;
            }
            16 => {
                // Repeat previous code length 3-6 times.
                let extra = reader.read_bits(2)?;
                repeat_count = 3 + extra;
                for _ in 0..repeat_count {
                    if i >= alphabet_size {
                        break;
                    }
                    code_lengths[i as usize] = prev_code_len;
                    i += 1;
                }
            }
            17 => {
                // Repeat zero 3-10 times (code length 0).
                let extra = reader.read_bits(3)?;
                repeat_count = 3 + extra;
                for _ in 0..repeat_count {
                    if i >= alphabet_size {
                        break;
                    }
                    code_lengths[i as usize] = 0;
                    i += 1;
                }
            }
            _ => {
                return Err(BrotliError::InvalidPrefixCode(format!(
                    "invalid code length symbol: {sym}"
                )));
            }
        }
    }

    HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
}

/// Read a variable-length code length value (special encoding for code-length codes).
///
/// The encoding is:
/// - 0: value 0 (no bits read)
/// - 10: value stored in next 1 bit + offset
/// - Actually, Brotli uses a fixed 2-5 bit scheme:
///   Symbol value is read as: read some bits, small lookup.
fn read_code_length_value(reader: &mut BitReader<'_>) -> BrotliResult<u8> {
    // Per RFC 7932 Section 3.5, each code length code is encoded as:
    // 0, 1, 2, 3, 4, 5 => read with up to 5 bits
    // The encoding uses a special variable-length scheme:
    //
    // Value: Bit pattern
    // 0: 00
    // 1: 0100
    // 2: 0110
    // 3: 1000
    // 4: 1010
    // 5: 1100
    //
    // Simplified: read pairs of bits.
    let v0 = reader.read_bits(2)?;
    match v0 {
        0 => Ok(0), // 00 => 0
        1 => {
            // 01 + 1 more bit
            let v1 = reader.read_bits(1)?;
            Ok(if v1 == 0 { 4 } else { 3 })
        }
        2 => {
            // 10 + 0 or 1 => either 3-bit
            Ok(2)
        }
        3 => {
            // 11
            let v1 = reader.read_bits(1)?;
            Ok(if v1 == 0 { 1 } else { 5 })
        }
        _ => Ok(0), // unreachable but satisfies exhaustive match
    }
}

/// Compute the number of bits needed to represent symbols in alphabet of given size.
pub fn alphabet_bits(alphabet_size: u32) -> u32 {
    if alphabet_size <= 1 {
        return 0;
    }
    32 - (alphabet_size - 1).leading_zeros()
}

/// Build a Huffman tree for encoding from frequency counts.
///
/// Uses the classic bottom-up algorithm to assign code lengths,
/// then generates canonical codes.
pub fn build_huffman_tree(frequencies: &[u32], alphabet_size: u32) -> BrotliResult<HuffmanTree> {
    build_huffman_tree_limited(frequencies, alphabet_size, MAX_HUFFMAN_CODE_LENGTH)
}

/// Build a Huffman tree with a custom maximum code length.
pub fn build_huffman_tree_limited(
    frequencies: &[u32],
    alphabet_size: u32,
    max_length: u32,
) -> BrotliResult<HuffmanTree> {
    let n = alphabet_size as usize;
    if n == 0 {
        return HuffmanTree::from_code_lengths(&[], 0);
    }

    // Count non-zero frequencies.
    let mut non_zero: Vec<(u32, usize)> = frequencies
        .iter()
        .enumerate()
        .take(n)
        .filter(|(_, f)| **f > 0)
        .map(|(i, f)| (*f, i))
        .collect();

    if non_zero.is_empty() {
        return HuffmanTree::from_code_lengths(&vec![0u8; n], alphabet_size);
    }

    if non_zero.len() == 1 {
        return HuffmanTree::single_symbol(non_zero[0].1 as u16, alphabet_size);
    }

    // Sort by frequency (ascending), then by symbol.
    non_zero.sort();

    let code_lengths = compute_code_lengths(&non_zero, n, max_length)?;
    HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
}

/// Compute length-limited canonical Huffman code lengths.
///
/// Returns a `code_lengths` vector (indexed by symbol, `0` = absent) that
/// always describes a **complete** prefix code limited to `max_length` bits,
/// i.e. one whose Kraft sum is exactly `2^max_length`:
///
/// ```text
///   Σ_{i : len_i > 0} 2^(max_length − len_i) = 2^max_length
/// ```
///
/// Completeness is the property the Brotli decoder relies on: the canonical
/// code it reconstructs from these lengths must cover *every* bit pattern of
/// the maximum length, with no gaps. An incomplete code (Kraft sum strictly
/// below the limit) leaves bit patterns that decode to no symbol, which is the
/// "no matching code found" failure that previously struck near-uniform,
/// all-symbols-present (high-entropy) literal distributions.
///
/// The lengths are also length-*optimal* for the limit because they are
/// produced by the package-merge algorithm (Larmore–Hirschberg), which yields
/// a minimum-redundancy prefix code subject to the `max_length` constraint.
fn compute_code_lengths(
    sorted_symbols: &[(u32, usize)],
    alphabet_size: usize,
    max_length: u32,
) -> BrotliResult<Vec<u8>> {
    let num_symbols = sorted_symbols.len();
    let mut code_lengths = vec![0u8; alphabet_size];

    // Zero or one symbol: callers (build_huffman_tree_limited) handle the
    // single-symbol case before reaching here, but guard anyway. A lone symbol
    // is assigned length 1 (a one-symbol *complete* code uses a single bit; the
    // true zero-bit single-symbol case is handled by `single_symbol`).
    if num_symbols <= 1 {
        if let Some((_, sym)) = sorted_symbols.first() {
            if *sym < code_lengths.len() {
                code_lengths[*sym] = 1;
            }
        }
        return Ok(code_lengths);
    }

    // A complete code limited to `max_length` bits exists only if the number of
    // leaves fits the code space: num_symbols ≤ 2^max_length. Every Brotli
    // alphabet satisfies this (256/704/64 symbols with a 15-bit limit; 18
    // code-length symbols with a 5-bit limit), but assert it defensively so the
    // package-merge invariants below hold.
    if (num_symbols as u64) > (1u64 << max_length) {
        return Err(BrotliError::InvalidParameter(format!(
            "{num_symbols} symbols cannot fit in a {max_length}-bit prefix code"
        )));
    }

    let lengths = package_merge_lengths(sorted_symbols, max_length);

    for (len, &(_, sym)) in lengths.iter().zip(sorted_symbols.iter()) {
        if sym < code_lengths.len() {
            code_lengths[sym] = *len;
        }
    }

    debug_assert!(
        is_complete_code(&code_lengths, max_length),
        "package-merge produced an incomplete code"
    );

    Ok(code_lengths)
}

/// Check that `code_lengths` describe a complete prefix code under `max_length`
/// (Kraft sum equals exactly `2^max_length`). Used by the `debug_assert!` in
/// `compute_code_lengths`; cheap enough to always compile.
fn is_complete_code(code_lengths: &[u8], max_length: u32) -> bool {
    let mut kraft: u64 = 0;
    for &cl in code_lengths {
        if cl > 0 {
            if cl as u32 > max_length {
                return false;
            }
            kraft += 1u64 << (max_length - cl as u32);
        }
    }
    kraft == (1u64 << max_length)
}

/// Compute optimal length-limited code lengths via the package-merge algorithm.
///
/// `sorted_symbols` must contain ≥ 2 entries and be sorted ascending by
/// frequency (the caller guarantees both). The returned vector is parallel to
/// `sorted_symbols`: `result[k]` is the bit length assigned to
/// `sorted_symbols[k]`. The resulting code is always complete (Kraft sum =
/// `2^max_length`) and minimises `Σ freq · length` subject to every length
/// being ≤ `max_length`.
///
/// ## Algorithm
///
/// Package-merge views the problem as the "binary coin collector" problem. For
/// each level `l` in `1..=max_length` we conceptually have one coin per symbol
/// of denomination `2^(−l)` and numismatic value equal to the symbol weight; we
/// must collect coins of total denomination `num_symbols − 1` while minimising
/// total value. The dynamic program builds, level by level, the cheapest list
/// of items: starting from the leaves, each pass *packages* adjacent pairs of
/// the previous list and *merges* them with that level's leaves (both kept
/// sorted by weight). Selecting the `2·num_symbols − 2` cheapest items from the
/// final list and counting, for each symbol, how many selected items contain it
/// gives that symbol's code length.
///
/// Items are tracked by an index into a flat arena of nodes; each node stores
/// its weight, and either a leaf symbol-slot or two child node indices. Symbol
/// membership counts are recovered by walking the selected items' subtrees.
/// With Brotli's small alphabets (≤ 704 symbols) and `max_length ≤ 15`, the
/// arena stays small and the whole computation is inexpensive.
fn package_merge_lengths(sorted_symbols: &[(u32, usize)], max_length: u32) -> Vec<u8> {
    let n = sorted_symbols.len();

    // Arena of package-merge nodes. A node is either a leaf (referencing the
    // index `k` of a symbol within `sorted_symbols`) or an internal package
    // (referencing two child node indices). We only need the weight plus enough
    // structure to count, per symbol, how many final items cover it.
    enum Node {
        /// Leaf for `sorted_symbols[k]`.
        Leaf { k: usize },
        /// Package of two previously created nodes.
        Pair { left: usize, right: usize },
    }

    let mut arena: Vec<Node> = Vec::new();
    let mut weight: Vec<u64> = Vec::new();

    // Helper to push a leaf node and return its arena index.
    let push_leaf = |arena: &mut Vec<Node>, weight: &mut Vec<u64>, k: usize| -> usize {
        let id = arena.len();
        arena.push(Node::Leaf { k });
        weight.push(sorted_symbols[k].0 as u64);
        id
    };

    // The list of leaf node indices for one level, in ascending weight order
    // (identical for every level, so build it once as a template of (weight, k)).
    // We rebuild concrete leaf nodes per level to keep node identities distinct,
    // which matters when counting subtree membership.

    // `prev` holds the previous level's list as arena indices, ascending by weight.
    // Level 1 (the deepest, l = max_length) starts as just the leaves.
    let mut prev: Vec<usize> = Vec::with_capacity(n);
    for k in 0..n {
        let id = push_leaf(&mut arena, &mut weight, k);
        prev.push(id);
    }
    // `prev` is already ascending because `sorted_symbols` is ascending by weight.

    // Perform `max_length - 1` package+merge passes. After the loop, `prev`
    // is the list for the top level from which we select 2n-2 items.
    for _ in 1..max_length {
        // Package adjacent pairs of `prev` (drop a trailing odd item).
        let mut packaged: Vec<usize> = Vec::with_capacity(prev.len() / 2 + n);
        let mut j = 0;
        while j + 1 < prev.len() {
            let left = prev[j];
            let right = prev[j + 1];
            let w = weight[left] + weight[right];
            let id = arena.len();
            arena.push(Node::Pair { left, right });
            weight.push(w);
            packaged.push(id);
            j += 2;
        }

        // Fresh leaves for this level.
        let mut leaves: Vec<usize> = Vec::with_capacity(n);
        for k in 0..n {
            let id = push_leaf(&mut arena, &mut weight, k);
            leaves.push(id);
        }

        // Merge `leaves` and `packaged`, both ascending by weight, into `prev`.
        // Stable on ties: leaves before packages, preserving leaf order, which
        // keeps the selection deterministic and the result canonical.
        let mut merged: Vec<usize> = Vec::with_capacity(leaves.len() + packaged.len());
        let (mut a, mut b) = (0usize, 0usize);
        while a < leaves.len() && b < packaged.len() {
            if weight[leaves[a]] <= weight[packaged[b]] {
                merged.push(leaves[a]);
                a += 1;
            } else {
                merged.push(packaged[b]);
                b += 1;
            }
        }
        merged.extend_from_slice(&leaves[a..]);
        merged.extend_from_slice(&packaged[b..]);
        prev = merged;
    }

    // Select the cheapest `2n - 2` items from the final list and count, for each
    // symbol, how many selected items cover it. That count is the symbol length.
    let select = 2 * n - 2;
    let mut lengths = vec![0u8; n];
    let mut stack: Vec<usize> = Vec::new();
    for &item in prev.iter().take(select) {
        // Walk the item's subtree, incrementing the length of every leaf symbol.
        stack.clear();
        stack.push(item);
        while let Some(id) = stack.pop() {
            match &arena[id] {
                Node::Leaf { k } => {
                    lengths[*k] = lengths[*k].saturating_add(1);
                }
                Node::Pair { left, right } => {
                    stack.push(*left);
                    stack.push(*right);
                }
            }
        }
    }

    lengths
}

/// Write a prefix code for the given set of non-zero symbols and return the Huffman tree
/// that matches what was written to the stream (so the encoder can use it for symbol encoding).
/// Uses simple prefix code for 1-4 symbols, complex prefix code otherwise.
pub fn write_prefix_code_and_build_tree(
    writer: &mut BitWriter,
    non_zero_symbols: &[u16],
    full_tree: &HuffmanTree,
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    if non_zero_symbols.len() <= 4 {
        // Write simple prefix code and build the corresponding tree.
        write_simple_prefix_code(writer, non_zero_symbols, alphabet_size)?;

        // Build the tree that matches the simple prefix code assignment.
        let mut code_lengths = vec![0u8; alphabet_size as usize];
        let nsym = non_zero_symbols.len();
        match nsym {
            1 => {
                return HuffmanTree::single_symbol(non_zero_symbols[0], alphabet_size);
            }
            2 => {
                code_lengths[non_zero_symbols[0] as usize] = 1;
                code_lengths[non_zero_symbols[1] as usize] = 1;
            }
            3 => {
                code_lengths[non_zero_symbols[0] as usize] = 1;
                code_lengths[non_zero_symbols[1] as usize] = 2;
                code_lengths[non_zero_symbols[2] as usize] = 2;
            }
            4 => {
                // write_simple_prefix_code always writes tree_select=true for 4 symbols,
                // which means all 4 get 2-bit codes.
                for &s in non_zero_symbols {
                    code_lengths[s as usize] = 2;
                }
            }
            _ => {}
        }
        HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
    } else {
        // Write complex prefix code (uses the full tree's code lengths).
        write_complex_prefix_code(writer, full_tree)?;
        Ok(full_tree.clone())
    }
}

/// Encode a Huffman tree as a simple prefix code to the bit writer.
pub fn write_simple_prefix_code(
    writer: &mut BitWriter,
    symbols: &[u16],
    alphabet_size: u32,
) -> BrotliResult<()> {
    let nsym = symbols.len();
    if nsym == 0 || nsym > 4 {
        return Err(BrotliError::InvalidParameter(
            "simple prefix code supports 1-4 symbols".to_string(),
        ));
    }

    // Write HSKIP = 1 (simple prefix code).
    writer.write_bits(1, 2)?;

    // Write NSYM - 1.
    writer.write_bits((nsym - 1) as u32, 2)?;

    // Write symbols.
    let sym_bits = alphabet_bits(alphabet_size);
    for &s in symbols {
        writer.write_bits(s as u32, sym_bits)?;
    }

    // For 4 symbols, write tree-select bit.
    if nsym == 4 {
        writer.write_bit(true)?; // All 2-bit codes.
    }

    Ok(())
}

/// Write a complex prefix code to the bit writer.
pub fn write_complex_prefix_code(writer: &mut BitWriter, tree: &HuffmanTree) -> BrotliResult<()> {
    // Count non-zero code lengths.
    let non_zero_symbols: Vec<(usize, u8)> = tree
        .code_lengths
        .iter()
        .enumerate()
        .filter(|(_, cl)| **cl > 0)
        .map(|(i, cl)| (i, *cl))
        .collect();

    // If 1-4 symbols, use simple prefix code.
    if non_zero_symbols.len() <= 4 {
        let symbols: Vec<u16> = non_zero_symbols.iter().map(|&(s, _)| s as u16).collect();
        return write_simple_prefix_code(writer, &symbols, tree.alphabet_size);
    }

    // Write HSKIP = 0 (complex prefix code, no skip).
    writer.write_bits(0, 2)?;

    // Build code-length frequencies.
    let mut cl_freqs = [0u32; 18];
    for &cl in &tree.code_lengths {
        cl_freqs[cl as usize] += 1;
    }

    // Count distinct non-zero code-length values.
    let distinct_cl_values: Vec<usize> = cl_freqs
        .iter()
        .enumerate()
        .filter(|&(_, &f)| f > 0)
        .map(|(v, _)| v)
        .collect();

    // Build code-length-of-code-length (cl_code_lengths) array:
    // These are written into the bitstream as the Huffman tree for decoding code lengths.
    // Each entry is the number of bits used to encode that code-length symbol.
    // With max code length 5 for these meta-codes.
    let cl_tree = if distinct_cl_values.len() == 1 {
        // Special case: only one distinct code-length value.
        // We cannot use a true single-symbol Huffman tree (code_length=0) because the
        // decoder requires at least one non-zero entry. Use code_length=1 for that symbol,
        // matching the Brotli spec for single-symbol code-length alphabets.
        let sym = distinct_cl_values[0];
        let mut cl_code_lengths = vec![0u8; 18];
        cl_code_lengths[sym] = 1;
        HuffmanTree::from_code_lengths(&cl_code_lengths, 18)?
    } else {
        // Build Huffman tree for code lengths (max code length 5 for code-length codes).
        build_huffman_tree_limited(&cl_freqs, 18, 5)?
    };

    // Write code-length code lengths in the prescribed order.
    // Mirror the decoder's early-stop logic: stop as soon as space reaches 0
    // (Kraft inequality is satisfied), so encoder and decoder stay in sync.
    let mut space = 32i32;
    for &idx in &CODE_LENGTH_CODE_ORDER {
        if space <= 0 {
            break;
        }
        let cl = cl_tree.code_lengths.get(idx).copied().unwrap_or(0);
        write_code_length_value(writer, cl)?;
        if cl != 0 {
            space -= 32 >> cl;
        }
    }

    // Now encode the actual code lengths using the code-length tree.
    for &cl in &tree.code_lengths {
        encode_symbol(writer, &cl_tree, cl as u16)?;
    }

    Ok(())
}

/// Write a code length value in the special encoding.
/// Per RFC 7932, code-length code lengths are encoded with a max of 5 bits.
/// Values 0-5 are the only valid values for code-length code lengths.
/// We clamp higher values and use the nearest valid encoding.
fn write_code_length_value(writer: &mut BitWriter, value: u8) -> BrotliResult<()> {
    // Clamp to valid range 0-5.
    let clamped = value.min(5);
    // Encoding must match read_code_length_value (LSB-first bit ordering):
    //   0: bits(0,0)       => 0b00  (2 bits)
    //   1: bits(1,1,0)     => 0b011 (3 bits)  [v0=3,v1=0 → reader returns 1]
    //   2: bits(0,1)       => 0b10  (2 bits)  [v0=2 → reader returns 2]
    //   3: bits(1,0,1)     => 0b101 (3 bits)  [v0=1,v1=1 → reader returns 3]
    //   4: bits(1,0,0)     => 0b001 (3 bits)  [v0=1,v1=0 → reader returns 4]
    //   5: bits(1,1,1)     => 0b111 (3 bits)  [v0=3,v1=1 → reader returns 5]
    match clamped {
        0 => writer.write_bits(0, 2),
        1 => writer.write_bits(0b011, 3),
        2 => writer.write_bits(0b10, 2),
        3 => writer.write_bits(0b101, 3),
        4 => writer.write_bits(0b001, 3),
        5 => writer.write_bits(0b111, 3),
        _ => Ok(()),
    }
}

/// Encode a single symbol using a Huffman tree.
pub fn encode_symbol(writer: &mut BitWriter, tree: &HuffmanTree, symbol: u16) -> BrotliResult<()> {
    let sym = symbol as usize;
    if sym >= tree.code_lengths.len() {
        return Err(BrotliError::InvalidParameter(format!(
            "symbol {sym} exceeds alphabet"
        )));
    }
    let code_len = tree.code_lengths[sym];
    if code_len == 0 {
        // Check if this is a single-symbol tree.
        let non_zero_count = tree.code_lengths.iter().filter(|&&cl| cl > 0).count();
        if non_zero_count <= 1 {
            // Single symbol tree, no bits needed.
            return Ok(());
        }
        return Err(BrotliError::InvalidParameter(format!(
            "symbol {sym} has zero code length"
        )));
    }

    // Compute canonical code for this symbol.
    let code = canonical_code(&tree.code_lengths, sym)?;
    // Write in reversed bit order (LSB first for the bit writer).
    let reversed = reverse_bits(code, code_len as u32);
    writer.write_bits(reversed, code_len as u32)
}

/// Compute the canonical Huffman code for a given symbol.
fn canonical_code(code_lengths: &[u8], symbol: usize) -> BrotliResult<u32> {
    let target_len = code_lengths[symbol];
    if target_len == 0 {
        return Err(BrotliError::InvalidParameter(
            "symbol has zero code length".to_string(),
        ));
    }

    let mut bl_count = vec![0u32; (MAX_HUFFMAN_CODE_LENGTH + 1) as usize];
    for &cl in code_lengths {
        if cl > 0 {
            bl_count[cl as usize] += 1;
        }
    }

    let mut next_code = vec![0u32; (MAX_HUFFMAN_CODE_LENGTH + 1) as usize];
    let mut code = 0u32;
    let max_len = code_lengths.iter().copied().max().unwrap_or(0);
    for bits in 1..=max_len {
        code = (code + bl_count[bits as usize - 1]) << 1;
        next_code[bits as usize] = code;
    }

    let mut result_code = next_code[target_len as usize];
    for (i, &cl) in code_lengths.iter().enumerate() {
        if cl == target_len {
            if i == symbol {
                return Ok(result_code);
            }
            result_code += 1;
        }
    }

    Err(BrotliError::InvalidParameter(format!(
        "could not compute code for symbol {symbol}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_bits() {
        assert_eq!(reverse_bits(0b110, 3), 0b011);
        assert_eq!(reverse_bits(0b1010, 4), 0b0101);
        assert_eq!(reverse_bits(0b1, 1), 0b1);
        assert_eq!(reverse_bits(0, 4), 0);
    }

    #[test]
    fn test_alphabet_bits() {
        assert_eq!(alphabet_bits(1), 0);
        assert_eq!(alphabet_bits(2), 1);
        assert_eq!(alphabet_bits(3), 2);
        assert_eq!(alphabet_bits(4), 2);
        assert_eq!(alphabet_bits(256), 8);
        assert_eq!(alphabet_bits(257), 9);
    }

    #[test]
    fn test_single_symbol_tree() {
        let tree = HuffmanTree::single_symbol(42, 256).expect("should create tree");
        let data = [0x00]; // doesn't matter, no bits consumed
        let mut reader = BitReader::new(&data);
        let sym = tree.decode_symbol(&mut reader).expect("should decode");
        assert_eq!(sym, 42);
    }

    #[test]
    fn test_two_symbol_tree() {
        // Two symbols with code length 1 each.
        let mut code_lengths = vec![0u8; 256];
        code_lengths[0] = 1; // symbol 0: code 0
        code_lengths[1] = 1; // symbol 1: code 1
        let tree = HuffmanTree::from_code_lengths(&code_lengths, 256).expect("should create tree");

        // Need at least 2 bytes for the LUT peek (10 bits).
        let data = [0b10, 0x00]; // bits: 0, 1, then padding zeros
        let mut reader = BitReader::new(&data);
        assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(0));
        assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(1));
    }

    #[test]
    fn test_build_huffman_tree() {
        let mut freqs = vec![0u32; 4];
        freqs[0] = 10;
        freqs[1] = 5;
        freqs[2] = 3;
        freqs[3] = 1;
        let tree = build_huffman_tree(&freqs, 4).expect("should build tree");
        // Most frequent symbol should have shortest code.
        assert!(tree.code_lengths[0] <= tree.code_lengths[3]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut freqs = vec![0u32; 8];
        freqs[0] = 100;
        freqs[1] = 50;
        freqs[2] = 25;
        freqs[3] = 10;
        let tree = build_huffman_tree(&freqs, 8).expect("should build");

        let mut writer = BitWriter::new();
        for sym in [0u16, 1, 2, 3, 0, 1, 0] {
            encode_symbol(&mut writer, &tree, sym).expect("should encode");
        }
        let mut data = writer.finish();
        // Add padding bytes so the bit reader can peek LUT_BITS ahead.
        data.extend_from_slice(&[0u8; 4]);

        let mut reader = BitReader::new(&data);
        for expected in [0u16, 1, 2, 3, 0, 1, 0] {
            let decoded = tree.decode_symbol(&mut reader).expect("should decode");
            assert_eq!(decoded, expected);
        }
    }

    /// Kraft sum of a code-length table under the given limit.
    fn kraft_sum(code_lengths: &[u8], max_length: u32) -> u64 {
        let mut sum = 0u64;
        for &cl in code_lengths {
            if cl > 0 {
                assert!(
                    cl as u32 <= max_length,
                    "code length {cl} exceeds {max_length}"
                );
                sum += 1u64 << (max_length - cl as u32);
            }
        }
        sum
    }

    /// The package-merge length assignment must always produce a **complete**
    /// code (Kraft sum == 2^max_length) for the near-uniform, all-symbols-present
    /// distribution that previously yielded an incomplete code and the
    /// "no matching code found" decode failure.
    #[test]
    fn test_compute_code_lengths_complete_for_near_uniform() {
        // 256 symbols with small, slightly-varying frequencies — the shape that
        // arises from high-entropy literal data.
        let mut freqs = vec![0u32; 256];
        for (i, f) in freqs.iter_mut().enumerate() {
            *f = 8 + ((i as u32).wrapping_mul(2654435761) % 24); // range [8, 31]
        }
        let tree = build_huffman_tree(&freqs, 256).expect("build");
        assert_eq!(
            kraft_sum(&tree.code_lengths, MAX_HUFFMAN_CODE_LENGTH),
            1u64 << MAX_HUFFMAN_CODE_LENGTH,
            "near-uniform 256-symbol code must be complete"
        );
        // Every present symbol must have a positive length.
        for (sym, &f) in freqs.iter().enumerate() {
            if f > 0 {
                assert!(tree.code_lengths[sym] > 0, "symbol {sym} lost its code");
            }
        }
    }

    /// Completeness must hold across many random distributions, alphabet sizes,
    /// and length limits (including the tight 18-symbol / 5-bit code-length
    /// alphabet and limits that force the length-limiting path).
    #[test]
    fn test_compute_code_lengths_complete_random_sweep() {
        let mut state = 0x1357_9BDFu64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };

        for &(alphabet, max_len) in &[(256u32, 15u32), (704, 15), (64, 15), (18, 5), (32, 6)] {
            for _ in 0..50 {
                let mut freqs = vec![0u32; alphabet as usize];
                let mut nonzero = 0;
                for f in freqs.iter_mut() {
                    if next() % 4 != 0 {
                        *f = next() % 1000 + 1;
                        nonzero += 1;
                    }
                }
                if nonzero < 2 {
                    continue;
                }
                let tree =
                    build_huffman_tree_limited(&freqs, alphabet, max_len).expect("build limited");
                assert_eq!(
                    kraft_sum(&tree.code_lengths, max_len),
                    1u64 << max_len,
                    "incomplete code: alphabet={alphabet} max_len={max_len}"
                );
            }
        }
    }

    /// Highly skewed weights (Fibonacci) would naturally need > 15-bit codes;
    /// the length-limiting path must clamp to ≤ max_length and stay complete.
    #[test]
    fn test_compute_code_lengths_complete_when_limiting() {
        let mut freqs = vec![0u32; 64];
        let (mut a, mut b) = (1u32, 1u32);
        for f in freqs.iter_mut() {
            *f = a;
            let c = a.saturating_add(b);
            a = b;
            b = c;
        }
        let tree = build_huffman_tree_limited(&freqs, 64, 15).expect("build");
        let max = tree.code_lengths.iter().copied().max().unwrap_or(0);
        assert!(max <= 15, "limiting failed: max code length {max} > 15");
        assert_eq!(
            kraft_sum(&tree.code_lengths, 15),
            1u64 << 15,
            "limited code must remain complete"
        );
    }

    /// Uniform full alphabet maps to all length-8 codes (Kraft sum exactly 2^15).
    #[test]
    fn test_compute_code_lengths_uniform_full_alphabet() {
        let freqs = vec![7u32; 256];
        let tree = build_huffman_tree(&freqs, 256).expect("build");
        for &cl in &tree.code_lengths {
            assert_eq!(cl, 8, "uniform 256 alphabet should be all length 8");
        }
        assert_eq!(kraft_sum(&tree.code_lengths, 15), 1u64 << 15);
    }
}
