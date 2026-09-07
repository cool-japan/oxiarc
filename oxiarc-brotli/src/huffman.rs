//! Huffman (prefix) coding for Brotli compression and decompression.
//!
//! Brotli uses canonical prefix codes (RFC 7932 Section 3). This module
//! implements:
//!
//! - a two-level lookup-table decoder (`O(1)` per symbol, an 8-bit root
//!   table plus sub-tables for longer codes),
//! - reading of "simple" (Section 3.4) and "complex" (Section 3.5) prefix
//!   code descriptors from the bitstream,
//! - writing of RFC-exact simple/complex descriptors for the encoder,
//! - optimal length-limited code construction via package-merge.
//!
//! ## RFC 7932 Prefix Code Format
//!
//! Prefix codes are canonical: within one code length, codes are assigned
//! in symbol order, and shorter codes lexicographically precede longer
//! codes. A code descriptor in the stream is either *simple* (1-4 symbols
//! listed explicitly) or *complex* (code lengths, themselves compressed
//! with a prefix code over the 18-symbol code-length alphabet).

use crate::bit_reader::BitReader;
use crate::bit_writer::BitWriter;
use crate::error::{BrotliError, BrotliResult};
use crate::tables::{
    CODE_LENGTH_CODE_ORDER, CODE_LENGTH_PREFIX_LENGTH, CODE_LENGTH_PREFIX_VALUE,
    CODE_LENGTH_VALUE_WRITE,
};

/// Maximum code length for Brotli Huffman codes.
pub const MAX_HUFFMAN_CODE_LENGTH: u32 = 15;

/// Maximum number of symbols in any Brotli alphabet.
pub const MAX_HUFFMAN_SYMBOLS: usize = 704;

/// Number of bits resolved by the root decode table. Codes longer than this
/// go through one sub-table indirection.
const ROOT_BITS: u32 = 8;

/// One decode-table entry: `bits` is the total code length for direct
/// entries (`<= ROOT_BITS` in the root, or the full length in a sub-table);
/// root entries with `bits > ROOT_BITS` point to the sub-table starting at
/// index `value`, sized `1 << (bits - ROOT_BITS)`.
#[derive(Debug, Clone, Copy)]
struct Entry {
    bits: u8,
    value: u16,
}

/// Marker for table slots not covered by any code. Complete codes cover
/// every slot; this only survives construction for defensive checking.
const INVALID_BITS: u8 = 0xFE;

/// A Huffman tree usable for both decoding (two-level table) and encoding
/// (precomputed canonical codes).
#[derive(Debug, Clone)]
pub struct HuffmanTree {
    /// For each symbol, its code length. 0 means the symbol is absent.
    pub code_lengths: Vec<u8>,
    /// Number of symbols in the alphabet.
    pub alphabet_size: u32,
    /// Flattened root + sub-tables for decoding. Empty for degenerate trees.
    entries: Vec<Entry>,
    /// `Some(symbol)` for a zero-bit single-symbol tree: decoding consumes
    /// no bits and always yields this symbol.
    degenerate: Option<u16>,
    /// Precomputed canonical codes, bit-reversed and ready to be written
    /// LSB-first. Indexed by symbol; length given by `code_lengths`.
    codes: Vec<u16>,
}

impl HuffmanTree {
    /// Create a Huffman tree from code lengths.
    ///
    /// The lengths must describe a *complete* canonical prefix code (Kraft
    /// sum exactly `2^15` at the 15-bit scale), or contain exactly one
    /// non-zero entry (a degenerate zero-bit code), or be entirely zero
    /// (an empty tree that fails on any decode; useful as a placeholder).
    pub fn from_code_lengths(code_lengths: &[u8], alphabet_size: u32) -> BrotliResult<Self> {
        if code_lengths.len() != alphabet_size as usize {
            return Err(BrotliError::InvalidParameter(format!(
                "code length table size {} != alphabet size {alphabet_size}",
                code_lengths.len()
            )));
        }
        let mut tree = HuffmanTree {
            code_lengths: code_lengths.to_vec(),
            alphabet_size,
            entries: Vec::new(),
            degenerate: None,
            codes: vec![0; alphabet_size as usize],
        };
        tree.build()?;
        Ok(tree)
    }

    /// Create a trivial single-symbol tree (zero-bit code).
    pub fn single_symbol(symbol: u16, alphabet_size: u32) -> BrotliResult<Self> {
        if symbol as u32 >= alphabet_size {
            return Err(BrotliError::InvalidParameter(format!(
                "symbol {symbol} outside alphabet of size {alphabet_size}"
            )));
        }
        Ok(HuffmanTree {
            code_lengths: vec![0u8; alphabet_size as usize],
            alphabet_size,
            entries: Vec::new(),
            degenerate: Some(symbol),
            codes: vec![0; alphabet_size as usize],
        })
    }

    /// Build the two-level decode table and the canonical encode codes.
    fn build(&mut self) -> BrotliResult<()> {
        // Histogram of code lengths.
        let mut count = [0u32; (MAX_HUFFMAN_CODE_LENGTH + 1) as usize];
        let mut num_codes = 0u32;
        let mut last_symbol = 0u16;
        for (sym, &len) in self.code_lengths.iter().enumerate() {
            if len > 0 {
                if len as u32 > MAX_HUFFMAN_CODE_LENGTH {
                    return Err(BrotliError::InvalidPrefixCode(format!(
                        "code length {len} exceeds maximum 15"
                    )));
                }
                count[len as usize] += 1;
                num_codes += 1;
                last_symbol = sym as u16;
            }
        }

        if num_codes == 0 {
            // Empty placeholder tree; decoding will fail.
            return Ok(());
        }
        if num_codes == 1 {
            self.degenerate = Some(last_symbol);
            return Ok(());
        }

        // Kraft completeness check at the 15-bit scale.
        let mut kraft = 0u64;
        for (len, &n) in count.iter().enumerate().skip(1) {
            kraft += (n as u64) << (MAX_HUFFMAN_CODE_LENGTH as usize - len);
        }
        if kraft != 1u64 << MAX_HUFFMAN_CODE_LENGTH {
            return Err(BrotliError::InvalidPrefixCode(
                "code lengths do not describe a complete prefix code".to_string(),
            ));
        }

        // Canonical first-code per length (MSB form).
        let mut next_code = [0u32; (MAX_HUFFMAN_CODE_LENGTH + 2) as usize];
        let mut code = 0u32;
        for bits in 1..=MAX_HUFFMAN_CODE_LENGTH as usize {
            code = (code + count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Assign codes; fill root table for short codes, collect long codes.
        self.entries = vec![
            Entry {
                bits: INVALID_BITS,
                value: 0
            };
            1 << ROOT_BITS
        ];
        // (symbol, reversed code, length) for codes longer than ROOT_BITS.
        let mut long_codes: Vec<(u16, u32, u8)> = Vec::new();
        // Maximum code length per root slot among long codes.
        let mut sub_max_len = [0u8; 1 << ROOT_BITS];

        for (sym, &len) in self.code_lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let len_u = len as u32;
            let c = next_code[len as usize];
            next_code[len as usize] += 1;
            let rev = reverse_bits(c, len_u);
            self.codes[sym] = rev as u16;
            if len_u <= ROOT_BITS {
                let step = 1u32 << len_u;
                let mut idx = rev;
                while idx < (1 << ROOT_BITS) {
                    self.entries[idx as usize] = Entry {
                        bits: len,
                        value: sym as u16,
                    };
                    idx += step;
                }
            } else {
                let root = (rev & ((1 << ROOT_BITS) - 1)) as usize;
                sub_max_len[root] = sub_max_len[root].max(len);
                long_codes.push((sym as u16, rev, len));
            }
        }

        // Allocate sub-tables and point root entries at them.
        for (root, &max_len) in sub_max_len.iter().enumerate() {
            if max_len == 0 {
                continue;
            }
            let sub_bits = max_len as u32 - ROOT_BITS;
            let offset = self.entries.len();
            if offset + (1usize << sub_bits) > u16::MAX as usize + 1 {
                return Err(BrotliError::InvalidPrefixCode(
                    "decode table overflow".to_string(),
                ));
            }
            self.entries[root] = Entry {
                bits: (ROOT_BITS + sub_bits) as u8,
                value: offset as u16,
            };
            self.entries.resize(
                offset + (1usize << sub_bits),
                Entry {
                    bits: INVALID_BITS,
                    value: 0,
                },
            );
        }

        // Fill sub-tables.
        for &(sym, rev, len) in &long_codes {
            let root = (rev & ((1 << ROOT_BITS) - 1)) as usize;
            let root_entry = self.entries[root];
            let sub_bits = root_entry.bits as u32 - ROOT_BITS;
            let base = root_entry.value as usize;
            let code_sub_bits = len as u32 - ROOT_BITS;
            let idx_in_sub = rev >> ROOT_BITS;
            let step = 1u32 << code_sub_bits;
            let mut idx = idx_in_sub;
            while idx < (1 << sub_bits) {
                self.entries[base + idx as usize] = Entry {
                    bits: len,
                    value: sym,
                };
                idx += step;
            }
        }

        Ok(())
    }

    /// Decode a single symbol from the bit reader in `O(1)`.
    ///
    /// # Partial input
    ///
    /// [`BitReader::peek_bits`] zero-pads past the end of its buffer, so two
    /// decision points can be reached by *phantom* bits: a root slot that
    /// matches no code, and the sub-table index of a code longer than
    /// the 8-bit root table. When the reader was built with
    /// [`BitReader::resume_partial`] — i.e. more bytes of the stream may still
    /// arrive — both report [`BrotliError::UnexpectedEof`] instead of a
    /// corruption error, so an incremental decoder can rewind and retry.
    /// Over a complete buffer the behaviour is unchanged.
    #[inline]
    pub fn decode_symbol(&self, reader: &mut BitReader<'_>) -> BrotliResult<u16> {
        if let Some(sym) = self.degenerate {
            return Ok(sym);
        }
        if self.entries.is_empty() {
            return Err(BrotliError::InvalidHuffmanCode(
                "decode with empty prefix code".to_string(),
            ));
        }
        let peeked = reader.peek_bits(ROOT_BITS)?;
        let entry = self.entries[peeked as usize];
        if entry.bits as u32 <= ROOT_BITS {
            // `drop_bits` refuses to consume bits that do not exist, so a code
            // resolved entirely inside the root table is already EOF-safe.
            reader.drop_bits(entry.bits as u32)?;
            return Ok(entry.value);
        }
        if entry.bits == INVALID_BITS {
            if reader.is_partial_input() && reader.buffered_bits() < ROOT_BITS {
                // The zero padding, not the stream, chose this empty slot.
                return Err(BrotliError::UnexpectedEof);
            }
            return Err(BrotliError::InvalidHuffmanCode(
                "bit pattern matches no code".to_string(),
            ));
        }
        let total_bits = entry.bits as u32;
        if reader.is_partial_input() && reader.buffered_bits() < total_bits {
            // The sub-table index would be selected by bits that have not
            // arrived yet; any symbol read here would be a guess.
            return Err(BrotliError::UnexpectedEof);
        }
        let peeked2 = reader.peek_bits(total_bits)?;
        let sub_index = (peeked2 >> ROOT_BITS) as usize;
        let entry2 = self.entries[entry.value as usize + sub_index];
        if entry2.bits == INVALID_BITS {
            return Err(BrotliError::InvalidHuffmanCode(
                "bit pattern matches no code".to_string(),
            ));
        }
        reader.drop_bits(entry2.bits as u32)?;
        Ok(entry2.value)
    }

    /// Encode a single symbol to the bit writer using the precomputed
    /// canonical code. Degenerate (single-symbol) trees emit no bits.
    pub fn encode_symbol(&self, writer: &mut BitWriter, symbol: u16) -> BrotliResult<()> {
        if self.degenerate == Some(symbol) {
            return Ok(());
        }
        let sym = symbol as usize;
        let len = *self.code_lengths.get(sym).unwrap_or(&0);
        if len == 0 {
            return Err(BrotliError::InvalidParameter(format!(
                "symbol {sym} has no code in this tree"
            )));
        }
        writer.write_bits(self.codes[sym] as u32, len as u32)
    }
}

/// Reverse the bottom `n` bits of `value`.
pub fn reverse_bits(value: u32, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    value.reverse_bits() >> (32 - n)
}

/// Compute the number of bits needed to represent all symbols in an
/// alphabet of the given size (`ALPHABET_BITS` in RFC 7932 Section 3.4).
pub fn alphabet_bits(alphabet_size: u32) -> u32 {
    if alphabet_size <= 1 {
        return 0;
    }
    32 - (alphabet_size - 1).leading_zeros()
}

// ─────────────────────────────────────────────────────────────────────────────
// Reading prefix code descriptors (decoder side)
// ─────────────────────────────────────────────────────────────────────────────

/// Read a Brotli prefix code descriptor from the bitstream
/// (RFC 7932 Sections 3.4 and 3.5).
pub fn read_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    if alphabet_size == 0 || alphabet_size as usize > MAX_HUFFMAN_SYMBOLS {
        return Err(BrotliError::InvalidParameter(format!(
            "invalid alphabet size {alphabet_size}"
        )));
    }
    let hskip = reader.read_bits(2)?;
    if hskip == 1 {
        read_simple_prefix_code(reader, alphabet_size)
    } else {
        read_complex_prefix_code(reader, alphabet_size, hskip)
    }
}

/// Read a simple prefix code (1-4 symbols), RFC 7932 Section 3.4.
fn read_simple_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    let nsym = reader.read_bits(2)? + 1;
    let symbol_bits = alphabet_bits(alphabet_size);

    let mut symbols = [0u16; 4];
    for i in 0..nsym as usize {
        let sym = reader.read_bits(symbol_bits)?;
        if sym >= alphabet_size {
            return Err(BrotliError::InvalidPrefixCode(format!(
                "simple code symbol {sym} exceeds alphabet size {alphabet_size}"
            )));
        }
        // RFC: a symbol identical to a previous one makes the stream invalid.
        for &prev in &symbols[..i] {
            if prev == sym as u16 {
                return Err(BrotliError::InvalidPrefixCode(format!(
                    "duplicate symbol {sym} in simple prefix code"
                )));
            }
        }
        symbols[i] = sym as u16;
    }

    if nsym == 1 {
        return HuffmanTree::single_symbol(symbols[0], alphabet_size);
    }

    let mut code_lengths = vec![0u8; alphabet_size as usize];
    match nsym {
        2 => {
            code_lengths[symbols[0] as usize] = 1;
            code_lengths[symbols[1] as usize] = 1;
        }
        3 => {
            code_lengths[symbols[0] as usize] = 1;
            code_lengths[symbols[1] as usize] = 2;
            code_lengths[symbols[2] as usize] = 2;
        }
        _ => {
            let tree_select = reader.read_bit()?;
            if tree_select {
                code_lengths[symbols[0] as usize] = 1;
                code_lengths[symbols[1] as usize] = 2;
                code_lengths[symbols[2] as usize] = 3;
                code_lengths[symbols[3] as usize] = 3;
            } else {
                for &s in &symbols {
                    code_lengths[s as usize] = 2;
                }
            }
        }
    }

    HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
}

/// Read a complex prefix code descriptor, RFC 7932 Section 3.5.
fn read_complex_prefix_code(
    reader: &mut BitReader<'_>,
    alphabet_size: u32,
    hskip: u32,
) -> BrotliResult<HuffmanTree> {
    // ── Level 1: code lengths of the code-length alphabet ────────────────
    let mut cl_lengths = [0u8; 18];
    let mut space = 32i32;
    let mut num_codes = 0u32;
    let mut single_code = 0usize;

    for &sym_idx in &CODE_LENGTH_CODE_ORDER[hskip as usize..] {
        if space <= 0 {
            break;
        }
        let peeked = reader.peek_bits(4)? as usize;
        let vlc_len = CODE_LENGTH_PREFIX_LENGTH[peeked] as u32;
        let value = CODE_LENGTH_PREFIX_VALUE[peeked];
        reader.drop_bits(vlc_len)?;
        cl_lengths[sym_idx] = value;
        if value != 0 {
            space -= 32 >> value;
            num_codes += 1;
            single_code = sym_idx;
        }
    }

    if num_codes == 0 {
        return Err(BrotliError::InvalidPrefixCode(
            "all code length codes are zero".to_string(),
        ));
    }
    if num_codes != 1 && space != 0 {
        return Err(BrotliError::InvalidPrefixCode(format!(
            "code length code is not complete (space {space})"
        )));
    }

    let cl_tree = if num_codes == 1 {
        HuffmanTree::single_symbol(single_code as u16, 18)?
    } else {
        HuffmanTree::from_cl_lengths(&cl_lengths)?
    };

    // ── Level 2: code lengths of the actual alphabet ─────────────────────
    let mut code_lengths = vec![0u8; alphabet_size as usize];
    let mut symbol = 0usize;
    let mut space = 32768i64;
    let mut prev_nonzero_len = 8u8;
    // Cumulative repeat count of the current 16-run or 17-run.
    let mut repeat = 0u32;
    // Length being repeated: prev non-zero for 16-runs, 0 for 17-runs.
    let mut repeat_code_len = 0u8;

    while symbol < alphabet_size as usize && space > 0 {
        let s = cl_tree.decode_symbol(reader)?;
        match s {
            0..=15 => {
                code_lengths[symbol] = s as u8;
                symbol += 1;
                repeat = 0;
                if s != 0 {
                    prev_nonzero_len = s as u8;
                    space -= 32768 >> s;
                }
            }
            16 | 17 => {
                let extra_bits = if s == 16 { 2u32 } else { 3 };
                let new_len = if s == 16 { prev_nonzero_len } else { 0 };
                if repeat_code_len != new_len {
                    repeat = 0;
                    repeat_code_len = new_len;
                }
                let old_repeat = repeat;
                if repeat > 0 {
                    repeat = (repeat - 2) << extra_bits;
                }
                repeat += reader.read_bits(extra_bits)? + 3;
                let delta = (repeat - old_repeat) as usize;
                if symbol + delta > alphabet_size as usize {
                    return Err(BrotliError::InvalidPrefixCode(
                        "code length repeat exceeds alphabet size".to_string(),
                    ));
                }
                for slot in &mut code_lengths[symbol..symbol + delta] {
                    *slot = new_len;
                }
                symbol += delta;
                if new_len != 0 {
                    space -= (delta as i64) << (15 - new_len as i64);
                }
            }
            _ => {
                return Err(BrotliError::InvalidPrefixCode(format!(
                    "invalid code length symbol {s}"
                )));
            }
        }
    }

    if space != 0 {
        return Err(BrotliError::InvalidPrefixCode(format!(
            "prefix code is not complete (space {space})"
        )));
    }

    HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
}

impl HuffmanTree {
    /// Build the code-length-alphabet tree (max length 5, always complete
    /// when reached with `space == 0`).
    fn from_cl_lengths(cl_lengths: &[u8; 18]) -> BrotliResult<Self> {
        // The 5-bit-limited code is complete at the 5-bit scale; scale is
        // irrelevant to construction, which checks at the 15-bit scale, so
        // completeness carries over automatically.
        HuffmanTree::from_code_lengths(cl_lengths, 18)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Writing prefix code descriptors (encoder side)
// ─────────────────────────────────────────────────────────────────────────────

/// Build a prefix code from symbol frequencies, write its RFC 7932
/// descriptor to `writer`, and return the tree to use for encoding the
/// symbols (which always matches what a conforming decoder reconstructs).
///
/// Alphabets with zero used symbols emit a valid 1-symbol simple code for
/// symbol 0 (the code is never used to encode anything, but the descriptor
/// must still be present and well-formed).
pub fn build_and_write_prefix_code(
    writer: &mut BitWriter,
    frequencies: &[u32],
    alphabet_size: u32,
) -> BrotliResult<HuffmanTree> {
    let mut nonzero: Vec<(u16, u32)> = frequencies
        .iter()
        .take(alphabet_size as usize)
        .enumerate()
        .filter(|&(_, &f)| f > 0)
        .map(|(s, &f)| (s as u16, f))
        .collect();

    match nonzero.len() {
        0 => {
            write_simple_descriptor(writer, &[0], alphabet_size, false)?;
            HuffmanTree::single_symbol(0, alphabet_size)
        }
        1 => {
            write_simple_descriptor(writer, &[nonzero[0].0], alphabet_size, false)?;
            HuffmanTree::single_symbol(nonzero[0].0, alphabet_size)
        }
        2..=4 => {
            // Most frequent first: the first listed symbol receives the
            // shortest code in the 3- and 4-symbol layouts.
            nonzero.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let symbols: Vec<u16> = nonzero.iter().map(|&(s, _)| s).collect();
            let mut code_lengths = vec![0u8; alphabet_size as usize];
            let tree_select = match symbols.len() {
                2 => {
                    code_lengths[symbols[0] as usize] = 1;
                    code_lengths[symbols[1] as usize] = 1;
                    false
                }
                3 => {
                    code_lengths[symbols[0] as usize] = 1;
                    code_lengths[symbols[1] as usize] = 2;
                    code_lengths[symbols[2] as usize] = 2;
                    false
                }
                _ => {
                    // Choose the cheaper of the two four-symbol layouts.
                    let f: Vec<u64> = nonzero.iter().map(|&(_, f)| f as u64).collect();
                    let flat = 2 * (f[0] + f[1] + f[2] + f[3]);
                    let skewed = f[0] + 2 * f[1] + 3 * (f[2] + f[3]);
                    let select = skewed < flat;
                    if select {
                        code_lengths[symbols[0] as usize] = 1;
                        code_lengths[symbols[1] as usize] = 2;
                        code_lengths[symbols[2] as usize] = 3;
                        code_lengths[symbols[3] as usize] = 3;
                    } else {
                        for &s in &symbols {
                            code_lengths[s as usize] = 2;
                        }
                    }
                    select
                }
            };
            write_simple_descriptor(writer, &symbols, alphabet_size, tree_select)?;
            HuffmanTree::from_code_lengths(&code_lengths, alphabet_size)
        }
        _ => {
            let tree = build_huffman_tree(frequencies, alphabet_size)?;
            write_complex_descriptor(writer, &tree.code_lengths)?;
            Ok(tree)
        }
    }
}

/// Write a simple prefix code descriptor (RFC 7932 Section 3.4).
fn write_simple_descriptor(
    writer: &mut BitWriter,
    symbols: &[u16],
    alphabet_size: u32,
    tree_select: bool,
) -> BrotliResult<()> {
    let nsym = symbols.len();
    if nsym == 0 || nsym > 4 {
        return Err(BrotliError::InvalidParameter(
            "simple prefix code supports 1-4 symbols".to_string(),
        ));
    }
    writer.write_bits(1, 2)?; // HSKIP = 1 marks a simple code.
    writer.write_bits((nsym - 1) as u32, 2)?;
    let sym_bits = alphabet_bits(alphabet_size);
    for &s in symbols {
        writer.write_bits(s as u32, sym_bits)?;
    }
    if nsym == 4 {
        writer.write_bit(tree_select)?;
    }
    Ok(())
}

/// One token of the RLE-compressed code length sequence.
#[derive(Debug, Clone, Copy)]
struct ClToken {
    /// Code length alphabet symbol (0..=17).
    symbol: u8,
    /// Number of extra bits (0, 2, or 3).
    extra_bits: u8,
    /// Extra bits value.
    extra: u32,
}

/// Write a complex prefix code descriptor (RFC 7932 Section 3.5) for the
/// given complete code length assignment.
fn write_complex_descriptor(writer: &mut BitWriter, code_lengths: &[u8]) -> BrotliResult<()> {
    writer.write_bits(0, 2)?; // HSKIP = 0: no skipped code length codes.

    let tokens = tokenize_code_lengths(code_lengths);

    // Histogram of code-length-alphabet symbols.
    let mut cl_freqs = [0u32; 18];
    for t in &tokens {
        cl_freqs[t.symbol as usize] += 1;
    }
    let distinct = cl_freqs.iter().filter(|&&f| f > 0).count();

    // Build the code-length code. A single distinct token symbol becomes
    // the RFC "one non-zero code length" degenerate case (the decoder then
    // reads that symbol with zero bits).
    let cl_tree = if distinct == 1 {
        let sym = cl_freqs.iter().position(|&f| f > 0).unwrap_or(0);
        let mut cl_lengths = [0u8; 18];
        cl_lengths[sym] = 1;
        // Build only for the code_lengths bookkeeping; symbol emission
        // below writes zero bits via the degenerate tree.
        let mut t = HuffmanTree::single_symbol(sym as u16, 18)?;
        t.code_lengths = cl_lengths.to_vec();
        t
    } else {
        build_huffman_tree_limited(&cl_freqs, 18, 5)?
    };

    // Write the code-length-code lengths in the prescribed order, mirroring
    // the decoder's stop condition exactly: stop before an entry would be
    // read with no code space remaining.
    let mut space = 32i32;
    for &idx in &CODE_LENGTH_CODE_ORDER {
        if space <= 0 {
            break;
        }
        let len = cl_tree.code_lengths[idx];
        let (pattern, nbits) = CODE_LENGTH_VALUE_WRITE[len as usize];
        writer.write_bits(pattern, nbits)?;
        if len != 0 {
            space -= 32 >> len;
        }
    }

    // Emit the token stream. For the degenerate code-length code, symbol
    // emission is zero bits; only the extra bits appear in the stream.
    for t in &tokens {
        if cl_tree.degenerate != Some(t.symbol as u16) {
            cl_tree.encode_symbol(writer, t.symbol as u16)?;
        }
        if t.extra_bits > 0 {
            writer.write_bits(t.extra, t.extra_bits as u32)?;
        }
    }

    Ok(())
}

/// Convert a code length array into the RFC 7932 Section 3.5 token stream:
/// literal lengths 0..=15, repeat-previous (16, 2 extra bits), and
/// repeat-zero (17, 3 extra bits), with the multi-token accumulation rule.
/// Trailing zeros are omitted.
fn tokenize_code_lengths(code_lengths: &[u8]) -> Vec<ClToken> {
    let end = code_lengths
        .iter()
        .rposition(|&l| l != 0)
        .map_or(0, |p| p + 1);
    let lengths = &code_lengths[..end];

    let mut tokens = Vec::new();
    // The decoder's "previous non-zero code length" starts at 8.
    let mut prev_nonzero = 8u8;
    let mut i = 0usize;
    while i < lengths.len() {
        let v = lengths[i];
        let mut run = 1usize;
        while i + run < lengths.len() && lengths[i + run] == v {
            run += 1;
        }
        i += run;

        if v == 0 {
            if run < 3 {
                for _ in 0..run {
                    tokens.push(ClToken {
                        symbol: 0,
                        extra_bits: 0,
                        extra: 0,
                    });
                }
            } else {
                push_repeat_tokens(&mut tokens, 17, 3, run as u32);
            }
        } else {
            let mut remaining = run;
            if v != prev_nonzero {
                // A 16 token repeats the previous non-zero length, so the
                // first occurrence of a new length must be literal.
                tokens.push(ClToken {
                    symbol: v,
                    extra_bits: 0,
                    extra: 0,
                });
                prev_nonzero = v;
                remaining -= 1;
            }
            if remaining > 0 {
                if remaining < 3 {
                    for _ in 0..remaining {
                        tokens.push(ClToken {
                            symbol: v,
                            extra_bits: 0,
                            extra: 0,
                        });
                    }
                } else {
                    push_repeat_tokens(&mut tokens, 16, 2, remaining as u32);
                }
            }
        }
    }
    tokens
}

/// Emit a sequence of repeat tokens (16 or 17) whose decoder-side
/// accumulation yields exactly `total` repetitions.
///
/// Decoder recurrence for consecutive same-symbol repeats:
/// `t1 = 3 + d1`, `t_{k+1} = ((t_k - 2) << extra_bits) + 3 + d_{k+1}`,
/// where each digit `d` is the extra-bits value. The final `t_k` is the
/// total count. Digits are derived by running the recurrence backwards.
fn push_repeat_tokens(tokens: &mut Vec<ClToken>, symbol: u8, extra_bits: u8, total: u32) {
    let base = 1u32 << extra_bits;
    let max_single = 3 + base - 1;
    let mut digits = Vec::new();
    let mut t = total;
    loop {
        if t <= max_single {
            digits.push(t.saturating_sub(3));
            break;
        }
        let d = (t - 3) % base;
        digits.push(d);
        t = (t - 3 - d) / base + 2;
    }
    digits.reverse();
    for d in digits {
        tokens.push(ClToken {
            symbol,
            extra_bits,
            extra: d,
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Code construction (encoder side)
// ─────────────────────────────────────────────────────────────────────────────

/// Build a Huffman tree for encoding from frequency counts, limited to the
/// Brotli maximum code length of 15 bits.
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
/// the maximum length, with no gaps.
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
    // single-symbol case before reaching here, but guard anyway.
    if num_symbols <= 1 {
        if let Some((_, sym)) = sorted_symbols.first() {
            if *sym < code_lengths.len() {
                code_lengths[*sym] = 1;
            }
        }
        return Ok(code_lengths);
    }

    // A complete code limited to `max_length` bits exists only if the number
    // of leaves fits the code space: num_symbols ≤ 2^max_length.
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

/// Check that `code_lengths` describe a complete prefix code under
/// `max_length` (Kraft sum equals exactly `2^max_length`).
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

/// Compute optimal length-limited code lengths via the package-merge
/// algorithm.
///
/// `sorted_symbols` must contain ≥ 2 entries and be sorted ascending by
/// frequency (the caller guarantees both). The returned vector is parallel to
/// `sorted_symbols`: `result[k]` is the bit length assigned to
/// `sorted_symbols[k]`. The resulting code is always complete (Kraft sum =
/// `2^max_length`) and minimises `Σ freq · length` subject to every length
/// being ≤ `max_length`.
fn package_merge_lengths(sorted_symbols: &[(u32, usize)], max_length: u32) -> Vec<u8> {
    let n = sorted_symbols.len();

    // Arena of package-merge nodes. A node is either a leaf (referencing the
    // index `k` of a symbol within `sorted_symbols`) or a package of two
    // previously created nodes.
    enum Node {
        /// Leaf for `sorted_symbols[k]`.
        Leaf { k: usize },
        /// Package of two previously created nodes.
        Pair { left: usize, right: usize },
    }

    let mut arena: Vec<Node> = Vec::new();
    let mut weight: Vec<u64> = Vec::new();

    let push_leaf = |arena: &mut Vec<Node>, weight: &mut Vec<u64>, k: usize| -> usize {
        let id = arena.len();
        arena.push(Node::Leaf { k });
        weight.push(sorted_symbols[k].0 as u64);
        id
    };

    // `prev` holds the previous level's list as arena indices, ascending by
    // weight. The deepest level starts as just the leaves.
    let mut prev: Vec<usize> = Vec::with_capacity(n);
    for k in 0..n {
        let id = push_leaf(&mut arena, &mut weight, k);
        prev.push(id);
    }

    // Perform `max_length - 1` package+merge passes.
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

        // Merge `leaves` and `packaged`, both ascending by weight.
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

    // Select the cheapest `2n - 2` items; each symbol's length is the number
    // of selected items whose subtree contains it.
    let select = 2 * n - 2;
    let mut lengths = vec![0u8; n];
    let mut stack: Vec<usize> = Vec::new();
    for &item in prev.iter().take(select) {
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
        assert_eq!(alphabet_bits(26), 5);
        assert_eq!(alphabet_bits(256), 8);
        assert_eq!(alphabet_bits(257), 9);
        assert_eq!(alphabet_bits(704), 10);
    }

    #[test]
    fn test_single_symbol_tree() {
        let tree = HuffmanTree::single_symbol(42, 256).expect("should create tree");
        let data = [0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(42));
        // No bits consumed.
        assert_eq!(reader.bits_consumed(), 0);
    }

    #[test]
    fn test_two_symbol_tree() {
        let mut code_lengths = vec![0u8; 256];
        code_lengths[0] = 1;
        code_lengths[1] = 1;
        let tree = HuffmanTree::from_code_lengths(&code_lengths, 256).expect("tree");
        let data = [0b10, 0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(0));
        assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(1));
    }

    #[test]
    fn test_incomplete_code_rejected() {
        // A lone 2-bit code (Kraft sum 1/4) is not complete and has more
        // than one... actually one symbol becomes degenerate; use two.
        let mut code_lengths = vec![0u8; 8];
        code_lengths[0] = 2;
        code_lengths[1] = 2;
        assert!(HuffmanTree::from_code_lengths(&code_lengths, 8).is_err());
        // Over-subscribed: three 1-bit codes.
        let mut code_lengths = vec![0u8; 8];
        code_lengths[0] = 1;
        code_lengths[1] = 1;
        code_lengths[2] = 1;
        assert!(HuffmanTree::from_code_lengths(&code_lengths, 8).is_err());
    }

    /// Long codes (> ROOT_BITS) must decode through sub-tables correctly.
    /// Uses a comb distribution that produces 15-bit codes, and checks
    /// encode→decode round-trip for every symbol.
    #[test]
    fn test_two_level_table_roundtrip_all_symbols() {
        // Fibonacci-ish weights force a wide range of code lengths.
        let mut freqs = vec![0u32; 64];
        let (mut a, mut b) = (1u32, 1u32);
        for f in freqs.iter_mut() {
            *f = a;
            let c = a.saturating_add(b);
            a = b;
            b = c;
        }
        let tree = build_huffman_tree(&freqs, 64).expect("build");
        let max_len = tree.code_lengths.iter().copied().max().unwrap_or(0);
        assert!(max_len as u32 > ROOT_BITS, "test must exercise sub-tables");

        let mut writer = BitWriter::new();
        for sym in 0..64u16 {
            tree.encode_symbol(&mut writer, sym).expect("encode");
        }
        let data = writer.finish();
        let mut reader = BitReader::new(&data);
        for sym in 0..64u16 {
            assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(sym), "sym {sym}");
        }
    }

    /// Randomized decode-table validation across many shapes: every
    /// encodable symbol must round-trip.
    #[test]
    fn test_random_trees_roundtrip() {
        let mut state = 0x2468_ACE0u64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for &alphabet in &[18u32, 26, 64, 256, 704] {
            for _ in 0..20 {
                let mut freqs = vec![0u32; alphabet as usize];
                let mut nonzero = 0;
                for f in freqs.iter_mut() {
                    if next() % 3 != 0 {
                        *f = next() % 5000 + 1;
                        nonzero += 1;
                    }
                }
                if nonzero < 2 {
                    continue;
                }
                let tree = build_huffman_tree(&freqs, alphabet).expect("build");
                let symbols: Vec<u16> = (0..alphabet as u16)
                    .filter(|&s| freqs[s as usize] > 0)
                    .collect();
                let mut writer = BitWriter::new();
                for &s in &symbols {
                    tree.encode_symbol(&mut writer, s).expect("encode");
                }
                let data = writer.finish();
                let mut reader = BitReader::new(&data);
                for &s in &symbols {
                    assert_eq!(tree.decode_symbol(&mut reader).ok(), Some(s));
                }
            }
        }
    }

    /// Writing a descriptor with `build_and_write_prefix_code` and reading
    /// it back with `read_prefix_code` must reproduce identical code
    /// lengths, for simple and complex codes alike.
    #[test]
    fn test_descriptor_write_read_roundtrip() {
        let mut state = 0x1357_9BDFu64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for &alphabet in &[26u32, 64, 256, 704] {
            for density in [2usize, 3, 4, 8, 40, alphabet as usize] {
                let mut freqs = vec![0u32; alphabet as usize];
                let mut placed = 0;
                while placed < density.min(alphabet as usize) {
                    let s = (next() % alphabet) as usize;
                    if freqs[s] == 0 {
                        freqs[s] = next() % 1000 + 1;
                        placed += 1;
                    }
                }
                let mut writer = BitWriter::new();
                let wrote =
                    build_and_write_prefix_code(&mut writer, &freqs, alphabet).expect("write");
                let mut data = writer.finish();
                data.extend_from_slice(&[0u8; 4]); // peek padding
                let mut reader = BitReader::new(&data);
                let read = read_prefix_code(&mut reader, alphabet).expect("read");
                assert_eq!(
                    wrote.code_lengths, read.code_lengths,
                    "alphabet {alphabet} density {density}"
                );
                assert_eq!(wrote.degenerate, read.degenerate);
            }
        }
    }

    /// The repeat-token digit decomposition must reproduce every total
    /// under the decoder's accumulation rule, for both 16- and 17-codes.
    #[test]
    fn test_repeat_token_accumulation_exhaustive() {
        for &(extra_bits, base) in &[(2u8, 4u32), (3u8, 8u32)] {
            for total in 3u32..3000 {
                let mut tokens = Vec::new();
                push_repeat_tokens(&mut tokens, 16, extra_bits, total);
                // Simulate the decoder accumulation.
                let mut repeat = 0u32;
                for t in &tokens {
                    let old = repeat;
                    if repeat > 0 {
                        repeat = (repeat - 2) * base;
                    }
                    repeat += t.extra + 3;
                    assert!(repeat > old, "monotone");
                }
                assert_eq!(repeat, total, "extra_bits {extra_bits} total {total}");
            }
        }
    }

    /// All-lengths-equal codes exercise the degenerate code-length-code path
    /// ("single symbol is 16, previous length taken to be 8").
    #[test]
    fn test_uniform_256_roundtrips_via_descriptor() {
        let freqs = vec![7u32; 256];
        let mut writer = BitWriter::new();
        let wrote = build_and_write_prefix_code(&mut writer, &freqs, 256).expect("write");
        assert!(wrote.code_lengths.iter().all(|&l| l == 8));
        let mut data = writer.finish();
        data.extend_from_slice(&[0u8; 4]);
        let mut reader = BitReader::new(&data);
        let read = read_prefix_code(&mut reader, 256).expect("read");
        assert_eq!(read.code_lengths, wrote.code_lengths);
    }

    /// Kraft sum of a code-length table under the given limit.
    fn kraft_sum(code_lengths: &[u8], max_length: u32) -> u64 {
        let mut sum = 0u64;
        for &cl in code_lengths {
            if cl > 0 {
                assert!(cl as u32 <= max_length);
                sum += 1u64 << (max_length - cl as u32);
            }
        }
        sum
    }

    #[test]
    fn test_compute_code_lengths_complete_for_near_uniform() {
        let mut freqs = vec![0u32; 256];
        for (i, f) in freqs.iter_mut().enumerate() {
            *f = 8 + ((i as u32).wrapping_mul(2654435761) % 24);
        }
        let tree = build_huffman_tree(&freqs, 256).expect("build");
        assert_eq!(
            kraft_sum(&tree.code_lengths, MAX_HUFFMAN_CODE_LENGTH),
            1u64 << MAX_HUFFMAN_CODE_LENGTH
        );
        for (sym, &f) in freqs.iter().enumerate() {
            if f > 0 {
                assert!(tree.code_lengths[sym] > 0, "symbol {sym} lost its code");
            }
        }
    }

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
        assert!(max <= 15);
        assert_eq!(kraft_sum(&tree.code_lengths, 15), 1u64 << 15);
    }

    #[test]
    fn test_compute_code_lengths_uniform_full_alphabet() {
        let freqs = vec![7u32; 256];
        let tree = build_huffman_tree(&freqs, 256).expect("build");
        for &cl in &tree.code_lengths {
            assert_eq!(cl, 8);
        }
        assert_eq!(kraft_sum(&tree.code_lengths, 15), 1u64 << 15);
    }
}
