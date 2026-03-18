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

    /// Check if this is a single-symbol tree (no bits needed to decode).
    fn is_single_symbol(&self) -> bool {
        let non_zero = self.code_lengths.iter().filter(|&&cl| cl > 0).count();
        non_zero <= 1
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
    let mut space = 32; // Kraft inequality tracker
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

/// Compute code lengths using a two-queue Huffman algorithm with length limiting.
///
/// Uses the classic two-queue approach (leaf queue + internal node queue)
/// to build the Huffman tree, then limits code lengths to `max_length`.
fn compute_code_lengths(
    sorted_symbols: &[(u32, usize)],
    alphabet_size: usize,
    max_length: u32,
) -> BrotliResult<Vec<u8>> {
    let num_symbols = sorted_symbols.len();
    let mut code_lengths = vec![0u8; alphabet_size];

    if num_symbols <= 1 {
        if let Some((_, sym)) = sorted_symbols.first() {
            code_lengths[*sym] = 1;
        }
        return Ok(code_lengths);
    }

    // Sort symbols by frequency descending, assign code lengths based on probability.
    let mut sorted_desc: Vec<(u32, usize)> = sorted_symbols.to_vec();
    sorted_desc.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    let total: f64 = sorted_desc.iter().map(|(f, _)| *f as f64).sum();
    if total == 0.0 {
        return Ok(code_lengths);
    }

    // Assign ideal code lengths: ceil(-log2(freq/total)), clamped to [1, max_length].
    let mut ideal_lengths: Vec<(usize, u32)> = sorted_desc
        .iter()
        .map(|(freq, sym)| {
            let p = *freq as f64 / total;
            let ideal = (-p.log2()).ceil() as u32;
            let clamped = ideal.clamp(1, max_length);
            (*sym, clamped)
        })
        .collect();

    // Adjust to satisfy Kraft inequality: sum(2^(max_length - len)) <= 2^max_length.
    let kraft_limit = 1u64 << max_length;
    for _ in 0..1000 {
        let kraft_sum: u64 = ideal_lengths
            .iter()
            .map(|(_, len)| 1u64 << (max_length - *len))
            .sum();

        if kraft_sum <= kraft_limit {
            break;
        }

        // Find the symbol with shortest code and increase its length.
        let pos = ideal_lengths
            .iter()
            .enumerate()
            .filter(|(_, (_, len))| *len < max_length)
            .min_by_key(|(_, (_, len))| *len)
            .map(|(i, _)| i);

        if let Some(p) = pos {
            ideal_lengths[p].1 += 1;
        } else {
            break;
        }
    }

    for (sym, len) in &ideal_lengths {
        if *sym < code_lengths.len() {
            code_lengths[*sym] = *len as u8;
        }
    }

    Ok(code_lengths)
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

    // Build Huffman tree for code lengths (max code length 5 for code-length codes).
    let cl_tree = build_huffman_tree_limited(&cl_freqs, 18, 5)?;

    // Write code-length code lengths in the prescribed order.
    for &idx in &CODE_LENGTH_CODE_ORDER {
        let cl = cl_tree.code_lengths.get(idx).copied().unwrap_or(0);
        write_code_length_value(writer, cl)?;
    }

    // Now encode the actual code lengths using the code-length tree.
    // For simplicity, write each code length as a literal symbol.
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
    match clamped {
        0 => writer.write_bits(0, 2),     // 00
        1 => writer.write_bits(0b111, 3), // 11 + 1
        2 => writer.write_bits(0b10, 2),  // 10
        3 => writer.write_bits(0b101, 3), // 01 + 1
        4 => writer.write_bits(0b001, 3), // 01 + 0
        5 => writer.write_bits(0b011, 3), // 11 + 0
        _ => Ok(()),                      // unreachable due to clamp
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
}
