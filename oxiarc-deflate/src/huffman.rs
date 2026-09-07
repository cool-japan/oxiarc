//! Huffman coding for DEFLATE compression.
//!
//! This module implements Huffman tree construction and decoding as specified
//! in RFC 1951. DEFLATE uses canonical Huffman codes, where codes of the same
//! length are assigned consecutive values in lexicographic order.
//!
//! # Alphabets
//!
//! DEFLATE uses three Huffman alphabets:
//! - **Literal/Length**: 0-285 (0-255 literals, 256 EOB, 257-285 lengths)
//! - **Distance**: 0-29 (back-reference distances)
//! - **Code Length**: 0-18 (for encoding dynamic Huffman trees)

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{BitCache, BitReader};
use std::io::Read;

/// Maximum code length in DEFLATE (15 bits).
pub const MAX_CODE_LENGTH: usize = 15;

/// Size of the literal/length alphabet (0-285).
pub const LITLEN_ALPHABET_SIZE: usize = 286;

/// Size of the distance alphabet (0-29).
pub const DISTANCE_ALPHABET_SIZE: usize = 30;

/// Size of the code length alphabet (0-18).
pub const CODELEN_ALPHABET_SIZE: usize = 19;

/// End of block symbol.
pub const END_OF_BLOCK: u16 = 256;

/// Marks a root-table entry as a pointer to a sub-table rather than a symbol.
const ENTRY_SUBTABLE: u32 = 1 << 31;

/// Mask for the code-length field (or sub-table index width) of an entry.
const ENTRY_LEN_MASK: u32 = 0xFF;

/// Bit position of the symbol field (or sub-table offset) of an entry.
const ENTRY_PAYLOAD_SHIFT: u32 = 8;

/// Mask (post-shift) for the symbol field / sub-table offset of an entry.
const ENTRY_PAYLOAD_MASK: u32 = 0xFFFF;

/// Largest table index representable in an entry's payload field.
const MAX_TABLE_INDEX: usize = ENTRY_PAYLOAD_MASK as usize;

/// A Huffman tree for decoding.
///
/// Decoding is table driven, in the two-level layout used by zlib's
/// `inflate_table` and by libdeflate: a *root* table indexed by the next
/// [`HuffmanTree::ROOT_BITS`] bits of the stream resolves every code that
/// short in a single load; longer codes select a *sub-table* which is
/// indexed by the remaining bits. The root table and all sub-tables live in
/// one contiguous `Vec<u32>` so a decode costs at most two dependent L1
/// loads and never walks the stream bit by bit.
///
/// Each entry is packed into a `u32`:
///
/// | bits    | symbol entry            | sub-table entry              |
/// |---------|-------------------------|------------------------------|
/// | 0..8    | code length (1..=15)    | sub-table index width        |
/// | 8..24   | symbol                  | sub-table offset in `table`  |
/// | 31      | 0                       | 1 (`ENTRY_SUBTABLE`)         |
///
/// An all-zero entry means "no code here": either an unused slot of a
/// legitimately incomplete code, or a hole in a sub-table. Those decode to
/// a length of 0, which callers must reject.
///
/// The canonical per-length tables (`symbols` / `base_codes` /
/// `symbol_offsets`) are retained for `HuffmanTree::decode_slow`, the
/// bit-at-a-time fallback used when fewer bits are buffered than the code
/// might need (end of stream, or an exact-mode [`BitReader`] that must not
/// read ahead).
#[derive(Debug, Clone)]
pub struct HuffmanTree {
    /// Root table (`1 << root_bits` entries) followed by all sub-tables.
    table: Vec<u32>,
    /// Number of bits indexing the root table.
    root_bits: u8,
    /// `(1 << root_bits) - 1`.
    root_mask: u32,
    /// Maximum code length in this tree.
    max_code_length: u8,
    /// Symbol lookup for the bit-at-a-time fallback.
    /// Indexed by (code - base_code) for each length.
    symbols: Vec<u16>,
    /// Base codes for each length.
    base_codes: [u32; MAX_CODE_LENGTH + 1],
    /// Symbol offsets for each length.
    symbol_offsets: [u16; MAX_CODE_LENGTH + 1],
}

impl HuffmanTree {
    /// Number of bits indexing the root lookup table.
    ///
    /// 10 bits (a 4 KiB root table) resolves essentially every literal of a
    /// real DEFLATE stream in one load while keeping the per-block table
    /// build cheap. Trees whose longest code is shorter use a
    /// correspondingly smaller root table.
    pub const ROOT_BITS: u8 = 10;

    /// Build a Huffman tree from code lengths.
    ///
    /// # Arguments
    ///
    /// * `code_lengths` - Array where `code_lengths[i]` is the bit length for symbol `i`.
    ///   A length of 0 means the symbol is not used.
    pub fn from_code_lengths(code_lengths: &[u8]) -> Result<Self> {
        Self::from_code_lengths_inner(code_lengths, false)
    }

    /// Build a Huffman tree from code lengths, additionally REQUIRING that the
    /// resulting code be *complete* (Kraft sum exactly 1.0).
    ///
    /// This is used for the DEFLATE code-length (19-symbol) alphabet, which
    /// RFC 1951 §3.2.7 requires to be a complete Huffman code and which
    /// spec-compliant decoders (zlib `inflate_table`) reject when incomplete
    /// ("invalid code lengths set"). Enforcing completeness here means an
    /// encoder that ever regresses to emitting an incomplete code-length code
    /// can no longer silently round-trip through our own inflate — the defect
    /// is caught by the self-test instead of being hidden by it.
    ///
    /// The literal/length and distance alphabets are intentionally *not* routed
    /// through this stricter path: DEFLATE permits legitimately incomplete codes
    /// there — most notably the fixed distance code (30 codes of 5 bits, Kraft
    /// 30/32) and the single-distance case (RFC 1951 §3.2.7) — and zlib accepts
    /// them, deferring any error to the moment an unused code is actually
    /// decoded (which this decoder also does).
    pub fn from_code_length_code(code_lengths: &[u8]) -> Result<Self> {
        Self::from_code_lengths_inner(code_lengths, true)
    }

    /// Rebuild this tree in place from a fresh set of code lengths.
    ///
    /// Behaviourally identical to [`HuffmanTree::from_code_lengths`], but the
    /// existing decode-table and symbol allocations are reused instead of a
    /// new pair being allocated for every block. A DEFLATE stream made of
    /// many small dynamic blocks builds three trees per block, so reusing the
    /// buffers removes three allocations per block from the decode path.
    ///
    /// On error the tree is left in the degenerate "no codes" state, so a
    /// caller that ignores the error still decodes nothing rather than
    /// decoding through a half-written table.
    ///
    /// # Errors
    ///
    /// Same conditions as [`HuffmanTree::from_code_lengths`]: an empty slice,
    /// a code longer than 15 bits, or an over-subscribed code.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::HuffmanTree;
    ///
    /// let mut tree = HuffmanTree::from_code_lengths(&[1, 1]).expect("build");
    /// tree.rebuild_from_code_lengths(&[2, 2, 2, 2])
    ///     .expect("rebuild");
    /// assert_eq!(tree.max_code_length(), 2);
    /// ```
    pub fn rebuild_from_code_lengths(&mut self, code_lengths: &[u8]) -> Result<()> {
        self.build_into(code_lengths, false)
    }

    /// Rebuild this tree in place, additionally requiring the code to be
    /// complete — the in-place form of
    /// [`HuffmanTree::from_code_length_code`].
    ///
    /// # Errors
    ///
    /// As [`HuffmanTree::from_code_length_code`], plus the conditions listed
    /// for [`HuffmanTree::rebuild_from_code_lengths`].
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::HuffmanTree;
    ///
    /// let mut tree = HuffmanTree::from_code_lengths(&[1, 1]).expect("build");
    /// // A complete 4-symbol code: Kraft sum is exactly 1.
    /// tree.rebuild_from_code_length_code(&[2, 2, 2, 2])
    ///     .expect("rebuild");
    /// // An incomplete one is rejected.
    /// assert!(tree.rebuild_from_code_length_code(&[2, 2, 2]).is_err());
    /// ```
    pub fn rebuild_from_code_length_code(&mut self, code_lengths: &[u8]) -> Result<()> {
        self.build_into(code_lengths, true)
    }

    /// A tree that decodes nothing: every lookup yields a zero-length
    /// entry, which callers must reject. Used as the starting point for
    /// [`HuffmanTree::build_into`] and as the initial value of a decoder's
    /// reusable tree slots.
    pub(crate) fn degenerate() -> Self {
        Self {
            table: vec![0u32; 1],
            root_bits: 0,
            root_mask: 0,
            max_code_length: 0,
            symbols: Vec::new(),
            base_codes: [0; MAX_CODE_LENGTH + 1],
            symbol_offsets: [0; MAX_CODE_LENGTH + 1],
        }
    }

    fn from_code_lengths_inner(code_lengths: &[u8], require_complete: bool) -> Result<Self> {
        let mut tree = Self::degenerate();
        tree.build_into(code_lengths, require_complete)?;
        Ok(tree)
    }

    /// The shared table builder, writing into `self`'s existing allocations.
    fn build_into(&mut self, code_lengths: &[u8], require_complete: bool) -> Result<()> {
        // Any early return below must leave a tree that decodes nothing.
        self.max_code_length = 0;
        self.root_bits = 0;
        self.root_mask = 0;
        self.symbols.clear();
        self.table.clear();
        self.table.push(0);
        self.base_codes = [0; MAX_CODE_LENGTH + 1];
        self.symbol_offsets = [0; MAX_CODE_LENGTH + 1];

        if code_lengths.is_empty() {
            return Err(OxiArcError::invalid_header("Empty code lengths"));
        }

        // Count codes of each length
        let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
        let mut max_length = 0u8;

        for &len in code_lengths {
            if len > 0 {
                if len as usize > MAX_CODE_LENGTH {
                    return Err(OxiArcError::invalid_header(format!(
                        "Code length {} exceeds maximum {}",
                        len, MAX_CODE_LENGTH
                    )));
                }
                bl_count[len as usize] += 1;
                max_length = max_length.max(len);
            }
        }

        // Check for valid code (at least one symbol)
        if max_length == 0 {
            // Special case: no symbols (all zeros). `self` is already in the
            // degenerate state set up above, which decodes nothing.
            return Ok(());
        }

        // Compute first code for each length (RFC 1951 algorithm)
        let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
        let mut code = 0u32;
        for bits in 1..=max_length as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Validate the code space. A canonical Huffman code fills the code
        // space exactly (Kraft sum == 1) when *complete*; `filled` is the number
        // of leaves the assigned lengths occupy at the deepest level. `> ` means
        // over-subscribed (always invalid); `< ` means incomplete.
        let total_codes: u32 = bl_count[1..=max_length as usize].iter().sum();
        if total_codes > 0 {
            let max_codes = 1u32 << max_length;
            let filled = code + bl_count[max_length as usize];
            if filled > max_codes {
                return Err(OxiArcError::invalid_header("Over-subscribed Huffman tree"));
            }
            // Incomplete (under-subscribed) codes are rejected only where the
            // spec unconditionally forbids them (the code-length alphabet).
            // Elsewhere we stay lenient — see `from_code_length_code`.
            if require_complete && filled < max_codes {
                return Err(OxiArcError::invalid_header(
                    "Incomplete code-length Huffman code",
                ));
            }
        }

        // Build symbol table, reusing the existing allocation.
        self.symbols.resize(total_codes as usize, 0u16);
        self.symbols.fill(0u16);
        let symbols = &mut self.symbols;
        let mut symbol_offsets = [0u16; MAX_CODE_LENGTH + 1];
        let mut base_codes = [0u32; MAX_CODE_LENGTH + 1];

        // Calculate offsets
        let mut offset = 0u16;
        for bits in 1..=max_length as usize {
            symbol_offsets[bits] = offset;
            base_codes[bits] = next_code[bits];
            offset += bl_count[bits] as u16;
        }
        // Set the final offset for bounds checking
        if max_length < MAX_CODE_LENGTH as u8 {
            symbol_offsets[max_length as usize + 1] = offset;
        }

        // Assign symbols to codes
        let mut current_code = next_code;
        for (symbol, &len) in code_lengths.iter().enumerate() {
            if len > 0 {
                let len = len as usize;
                let idx =
                    symbol_offsets[len] as usize + (current_code[len] - base_codes[len]) as usize;
                if idx < symbols.len() {
                    symbols[idx] = symbol as u16;
                }
                current_code[len] += 1;
            }
        }

        // ── Build the two-level decode table ────────────────────────────────
        let root_bits = Self::ROOT_BITS.min(max_length);
        let root_size = 1usize << root_bits;
        let root_mask = (root_size - 1) as u32;
        // Reuse the existing table allocation: `clear` then `resize` gives a
        // zeroed root table without asking the allocator for a new one.
        self.table.clear();
        self.table.resize(root_size, 0u32);
        let table = &mut self.table;

        // Pass 1: size the sub-tables. For every code longer than
        // `root_bits`, the first `root_bits` bits (in stream order, i.e. the
        // low bits of the reversed canonical code) select a root slot; that
        // slot needs a sub-table wide enough for the longest code sharing the
        // prefix.
        let mut sub_max_len = vec![0u8; root_size];
        let mut assign = next_code;
        for (symbol, &len) in code_lengths.iter().enumerate() {
            let _ = symbol;
            if len == 0 {
                continue;
            }
            let len_us = len as usize;
            let reversed = Self::reverse_bits(assign[len_us] as u16, len);
            assign[len_us] += 1;
            if len > root_bits {
                let slot = (reversed as u32 & root_mask) as usize;
                // Deliberately nested rather than an `if let ... && ...`
                // chain: let-chains need Rust 1.88 and this workspace's MSRV
                // is 1.85.
                if let Some(cur) = sub_max_len.get_mut(slot) {
                    if *cur < len {
                        *cur = len;
                    }
                }
            }
        }

        // Allocate the sub-tables contiguously after the root table and
        // install the pointer entries.
        let mut sub_offset = vec![0u32; root_size];
        for slot in 0..root_size {
            let longest = sub_max_len.get(slot).copied().unwrap_or(0);
            if longest == 0 {
                continue;
            }
            let sub_bits = longest - root_bits;
            let offset = table.len();
            if offset > MAX_TABLE_INDEX {
                return Err(OxiArcError::invalid_header(
                    "Huffman decode table too large",
                ));
            }
            table.resize(offset + (1usize << sub_bits), 0u32);
            if let Some(slot_entry) = table.get_mut(slot) {
                *slot_entry =
                    ENTRY_SUBTABLE | ((offset as u32) << ENTRY_PAYLOAD_SHIFT) | (sub_bits as u32);
            }
            if let Some(off) = sub_offset.get_mut(slot) {
                *off = offset as u32;
            }
        }

        // Pass 2: fill in the symbol entries, replaying the same canonical
        // code assignment.
        let mut assign = next_code;
        for (symbol, &len) in code_lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let len_us = len as usize;
            let reversed = Self::reverse_bits(assign[len_us] as u16, len) as u32;
            assign[len_us] += 1;
            let entry = ((symbol as u32) << ENTRY_PAYLOAD_SHIFT) | (len as u32);

            if len <= root_bits {
                // Replicate over every root index sharing this prefix.
                let step = 1usize << len;
                let mut index = reversed as usize;
                while index < root_size {
                    if let Some(slot) = table.get_mut(index) {
                        *slot = entry;
                    }
                    index += step;
                }
            } else {
                let slot = (reversed & root_mask) as usize;
                let longest = sub_max_len.get(slot).copied().unwrap_or(0);
                let base = sub_offset.get(slot).copied().unwrap_or(0) as usize;
                let sub_size = 1usize << (longest - root_bits);
                let step = 1usize << (len - root_bits);
                let mut index = (reversed >> root_bits) as usize;
                while index < sub_size {
                    if let Some(slot_entry) = table.get_mut(base + index) {
                        *slot_entry = entry;
                    }
                    index += step;
                }
            }
        }

        self.root_bits = root_bits;
        self.root_mask = root_mask;
        self.max_code_length = max_length;
        self.base_codes = base_codes;
        self.symbol_offsets = symbol_offsets;
        Ok(())
    }

    /// Reverse bits in a code.
    fn reverse_bits(mut code: u16, length: u8) -> u16 {
        let mut reversed = 0u16;
        for _ in 0..length {
            reversed = (reversed << 1) | (code & 1);
            code >>= 1;
        }
        reversed
    }

    /// Longest code this tree can produce.
    #[inline(always)]
    pub fn max_code_length(&self) -> u8 {
        self.max_code_length
    }

    /// Look up the packed table entry for the bits currently buffered in
    /// `reader`, **without** refilling or consuming anything.
    ///
    /// The returned entry's length field is `0` when no code matches (either
    /// the slot is unused or too few bits were buffered). Callers must reject
    /// a length of `0` and any length exceeding
    /// [`BitReader::available_bits`] — only then are all the consumed bits
    /// known to be real stream bits rather than the zero padding
    /// [`BitReader::peek_bits_prefilled`] supplies past the end.
    ///
    /// This returns a plain `u32` rather than a `Result` on purpose: the
    /// crate's error type is 48 bytes, so a `Result` return in the innermost
    /// decode loop costs an indirect store per symbol.
    #[inline(always)]
    pub fn lookup<R: Read>(&self, reader: &BitReader<R>) -> u32 {
        let entry = self.root_entry(reader.peek_bits_prefilled(self.root_bits));
        if entry & ENTRY_SUBTABLE == 0 {
            return entry;
        }
        self.lookup_subtable(reader.peek_bits_prefilled(self.max_code_length), entry)
    }

    /// Same as [`HuffmanTree::lookup`] but against a detached
    /// [`BitCache`] — the form used by the decoder's inner loop.
    ///
    /// The common case costs a single mask and a single load; the wider peek
    /// needed to index a sub-table is done only on the rare long-code path.
    #[inline(always)]
    pub fn lookup_cached(&self, cache: &BitCache) -> u32 {
        let entry = self.root_entry(cache.peek_mask(self.root_mask));
        if entry & ENTRY_SUBTABLE == 0 {
            return entry;
        }
        self.lookup_subtable(cache.peek_bits(self.max_code_length), entry)
    }

    /// Root-table entry for `bits` (zero padded past the end of the stream).
    #[inline(always)]
    fn root_entry(&self, bits: u32) -> u32 {
        self.table
            .get((bits & self.root_mask) as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Second-level lookup for codes longer than [`HuffmanTree::ROOT_BITS`].
    #[inline]
    fn lookup_subtable(&self, bits: u32, entry: u32) -> u32 {
        let sub_bits = (entry & ENTRY_LEN_MASK) as u8;
        let base = ((entry >> ENTRY_PAYLOAD_SHIFT) & ENTRY_PAYLOAD_MASK) as usize;
        let index = ((bits >> self.root_bits) & ((1u32 << sub_bits) - 1)) as usize;
        self.table.get(base + index).copied().unwrap_or(0)
    }

    /// Code length encoded in a table entry (`0` when the entry is unused).
    #[inline(always)]
    pub fn entry_length(entry: u32) -> u8 {
        (entry & ENTRY_LEN_MASK) as u8
    }

    /// Symbol encoded in a table entry.
    #[inline(always)]
    pub fn entry_symbol(entry: u32) -> u16 {
        ((entry >> ENTRY_PAYLOAD_SHIFT) & ENTRY_PAYLOAD_MASK) as u16
    }

    /// Decode a symbol from the bit stream.
    /// This is a hot path - inline for better performance.
    #[inline]
    pub fn decode<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        if self.max_code_length == 0 {
            return Err(OxiArcError::invalid_huffman(reader.bit_position()));
        }

        // Top up the accumulator so a whole code is present. `try_fill`
        // reports end of stream as `Ok(false)` rather than an error; a short
        // tail is handled by the length check below.
        if reader.available_bits() < self.max_code_length {
            reader.try_fill(self.max_code_length)?;
        }

        let entry = self.lookup(reader);
        let len = Self::entry_length(entry);
        if len != 0 && len <= reader.available_bits() {
            reader.consume_bits(len);
            return Ok(Self::entry_symbol(entry));
        }

        // Too few bits buffered for the code the table selected, or no code
        // at all: fall back to the bit-at-a-time canonical decoder, which
        // reads exactly what it needs and reports EOF precisely.
        self.decode_slow(reader)
    }

    /// Slow decoding path for codes longer than fast_bits.
    fn decode_slow<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        let mut code = 0u32;

        for len in 1..=self.max_code_length as usize {
            let bit = reader.read_bits(1)?;
            code = (code << 1) | bit;

            let count = if len < MAX_CODE_LENGTH {
                self.symbol_offsets[len + 1] - self.symbol_offsets[len]
            } else {
                self.symbols.len() as u16 - self.symbol_offsets[len]
            };

            if count > 0 && code >= self.base_codes[len] {
                let idx = code - self.base_codes[len];
                if idx < count as u32 {
                    let symbol_idx = self.symbol_offsets[len] as usize + idx as usize;
                    if symbol_idx < self.symbols.len() {
                        return Ok(self.symbols[symbol_idx]);
                    }
                }
            }
        }

        Err(OxiArcError::invalid_huffman(reader.bit_position()))
    }
}

/// Build a bit-cost table from code lengths.
///
/// Returns a Vec where `result[symbol] = lengths[symbol] as u32`.
/// Symbols with length 0 are unreachable and get cost `u32::MAX`.
pub(crate) fn cost_table_from_lengths(lengths: &[u8]) -> Vec<u32> {
    lengths
        .iter()
        .map(|&l| if l == 0 { u32::MAX } else { l as u32 })
        .collect()
}

/// Compute the total bit cost for encoding a (length, distance) match.
///
/// Returns `u32::MAX` if any required symbol is unreachable (cost == `u32::MAX`)
/// or if integer overflow would occur.
pub(crate) fn cost_of_match(
    length: u16,
    distance: u16,
    litlen_costs: &[u32],
    dist_costs: &[u32],
) -> u32 {
    use crate::tables::{DISTANCE_EXTRA_BITS, LENGTH_EXTRA_BITS, distance_to_code, length_to_code};

    let (len_code, len_extra_bits, _) = length_to_code(length);
    let len_sym_cost = litlen_costs
        .get(len_code as usize)
        .copied()
        .unwrap_or(u32::MAX);
    if len_sym_cost == u32::MAX {
        return u32::MAX;
    }

    let (dist_code, dist_extra_bits, _) = distance_to_code(distance);
    let dist_sym_cost = dist_costs
        .get(dist_code as usize)
        .copied()
        .unwrap_or(u32::MAX);
    if dist_sym_cost == u32::MAX {
        return u32::MAX;
    }

    // Extra bits come from the tables; sanity-check the indices.
    let len_eb = LENGTH_EXTRA_BITS
        .get((len_code as usize).saturating_sub(257))
        .copied()
        .unwrap_or(len_extra_bits) as u32;
    let dist_eb = DISTANCE_EXTRA_BITS
        .get(dist_code as usize)
        .copied()
        .unwrap_or(dist_extra_bits) as u32;

    len_sym_cost
        .saturating_add(len_eb)
        .saturating_add(dist_sym_cost)
        .saturating_add(dist_eb)
}

/// Builder for creating Huffman code lengths from frequencies.
#[derive(Debug)]
pub struct HuffmanBuilder {
    frequencies: Vec<u32>,
    max_length: u8,
}

impl HuffmanBuilder {
    /// Create a new Huffman builder.
    pub fn new(alphabet_size: usize, max_length: u8) -> Self {
        Self {
            frequencies: vec![0; alphabet_size],
            max_length,
        }
    }

    /// Add a symbol occurrence.
    pub fn add(&mut self, symbol: u16) {
        if (symbol as usize) < self.frequencies.len() {
            self.frequencies[symbol as usize] += 1;
        }
    }

    /// Add multiple occurrences of a symbol.
    pub fn add_count(&mut self, symbol: u16, count: u32) {
        if (symbol as usize) < self.frequencies.len() {
            self.frequencies[symbol as usize] += count;
        }
    }

    /// Build code lengths from frequencies.
    ///
    /// Returns an array where `result[i]` is the code length for symbol `i`.
    ///
    /// The result is guaranteed to be a **complete** length-limited prefix code
    /// (Kraft sum exactly 1.0 over the used symbols), as required by
    /// RFC 1951 §3.2.2 and by spec-compliant decoders (zlib `inflate_table`,
    /// which rejects incomplete code-length / literal-length tables with
    /// "invalid code lengths set"). The lengths are computed with the
    /// package-merge algorithm (optimal under the `max_length` constraint).
    pub fn build_lengths(&self) -> Vec<u8> {
        let n = self.frequencies.len();
        let mut lengths = vec![0u8; n];

        // Collect (frequency, symbol) for every used symbol.
        let mut symbols: Vec<(u32, usize)> = self
            .frequencies
            .iter()
            .enumerate()
            .filter(|&(_, f)| *f > 0)
            .map(|(i, f)| (*f, i))
            .collect();

        if symbols.is_empty() {
            return lengths;
        }

        if symbols.len() == 1 {
            // A code with a single symbol cannot be a *complete* prefix code
            // (a 1-bit code has Kraft sum 0.5, which spec decoders reject for
            // the literal/length and code-length alphabets). Following zlib's
            // deflate, we assign the lone symbol a 1-bit code and synthesise a
            // second 1-bit code for the lowest unused symbol so the resulting
            // table is complete. The phantom symbol has frequency 0 and is
            // therefore never emitted in the data stream.
            let only = symbols[0].1;
            lengths[only] = 1;
            let phantom = if only == 0 { 1.min(n - 1) } else { 0 };
            // `n >= 1` here; if the alphabet has at least two slots we can place
            // the phantom symbol, otherwise the single 1-bit code is the best we
            // can represent (degenerate 1-symbol alphabet).
            if phantom != only {
                lengths[phantom] = 1;
            }
            return lengths;
        }

        // Sort by (frequency, symbol) ascending for deterministic, canonical
        // tie-breaking.
        symbols.sort_by_key(|&(f, i)| (f, i));

        let code_lengths = Self::package_merge(&symbols, self.max_length as usize);

        for (i, (_, symbol)) in symbols.iter().enumerate() {
            lengths[*symbol] = code_lengths[i];
        }

        lengths
    }

    /// Length-limited optimal Huffman code lengths via the package-merge
    /// (Larmore–Hirschberg) algorithm.
    ///
    /// `symbols` must be sorted by weight ascending and contain at least two
    /// entries. `max_len` is the maximum permitted code length (≤ 15 for
    /// DEFLATE literal/length and distance alphabets, ≤ 7 for the code-length
    /// alphabet). Returns a `Vec<u8>` of code lengths parallel to `symbols`.
    ///
    /// The produced code is always **complete** (Kraft sum exactly 1.0): the
    /// package-merge solution selects exactly `2*n - 2` coins, which is
    /// equivalent to the Kraft equality for a full binary tree over `n` leaves.
    fn package_merge(symbols: &[(u32, usize)], max_len: usize) -> Vec<u8> {
        let n = symbols.len();

        // The shortest length that can describe `n` symbols is ceil(log2(n)).
        // If `max_len` is below that the alphabet is unrepresentable; clamp the
        // effective limit up so we still emit a valid (complete) code. For the
        // DEFLATE alphabets this never triggers (15 bits covers 286 symbols,
        // 7 bits covers 19), but we stay defensive rather than panicking.
        let min_bits = {
            let mut b = 1usize;
            while (1usize << b) < n {
                b += 1;
            }
            b
        };
        let limit = max_len.max(min_bits);

        // Each "coin" references the index of an original symbol it covers.
        // A package-merge "list" at a given bit-width is a sorted sequence of
        // items; each item is either an original coin (one symbol) or a package
        // (the merge of two items from the previous, wider list).
        #[derive(Clone)]
        struct Item {
            weight: u64,
            // Symbol indices (into `symbols`) covered by this item.
            coverage: Vec<usize>,
        }

        // Base list: one coin per symbol, sorted ascending by weight (input is
        // already sorted by (weight, symbol)).
        let base: Vec<Item> = symbols
            .iter()
            .enumerate()
            .map(|(idx, &(w, _))| Item {
                weight: w as u64,
                coverage: vec![idx],
            })
            .collect();

        // Build successive lists from the widest bit position (`limit`) down to
        // bit position 1. At each step we package adjacent pairs of the previous
        // list, then merge those packages with the base coins.
        let mut prev: Vec<Item> = base.clone();
        for _ in 1..limit {
            // Package adjacent pairs of `prev`.
            let mut packages: Vec<Item> = Vec::with_capacity(prev.len() / 2);
            let mut i = 0;
            while i + 1 < prev.len() {
                let a = &prev[i];
                let b = &prev[i + 1];
                let mut coverage = Vec::with_capacity(a.coverage.len() + b.coverage.len());
                coverage.extend_from_slice(&a.coverage);
                coverage.extend_from_slice(&b.coverage);
                packages.push(Item {
                    weight: a.weight + b.weight,
                    coverage,
                });
                i += 2;
            }

            // Merge `base` coins with `packages`, keeping ascending weight order.
            let mut merged: Vec<Item> = Vec::with_capacity(base.len() + packages.len());
            let mut bi = 0;
            let mut pi = 0;
            while bi < base.len() || pi < packages.len() {
                let take_base = match (base.get(bi), packages.get(pi)) {
                    (Some(b), Some(p)) => b.weight <= p.weight,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if take_base {
                    merged.push(base[bi].clone());
                    bi += 1;
                } else {
                    merged.push(packages[pi].clone());
                    pi += 1;
                }
            }
            prev = merged;
        }

        // Select the first `2*n - 2` items of the final list. The code length of
        // a symbol equals the number of selected items that cover it.
        let select = 2 * n - 2;
        let mut lengths = vec![0u8; n];
        for item in prev.iter().take(select) {
            for &sym_idx in &item.coverage {
                lengths[sym_idx] = lengths[sym_idx].saturating_add(1);
            }
        }

        // Every symbol must receive a positive length and none may exceed the
        // limit (the algorithm guarantees both, but clamp defensively against
        // saturation on pathological inputs).
        for l in lengths.iter_mut() {
            if *l == 0 {
                *l = 1;
            }
            if *l as usize > limit {
                *l = limit as u8;
            }
        }

        lengths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_huffman_tree_simple() {
        // Simple tree: A=0, B=10, C=11
        // Code lengths: A=1, B=2, C=2
        // Canonical codes: A=0 (1 bit), B=10 (2 bits), C=11 (2 bits)
        // In LSB-first: A=0, B=01 (reversed from 10), C=11 (reversed from 11)
        let lengths = [1u8, 2, 2];
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build huffman tree");

        // Test decoding A B C A
        // Bits needed: 0 (A) + 01 (B) + 11 (C) + 0 (A) = 7 bits
        // Packed LSB-first into byte: bits 0-6 = 0 01 11 0 0 = 0b00011010 = 0x1A
        let data = vec![0b00011010u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).expect("decode symbol A"), 0); // A
        assert_eq!(tree.decode(&mut reader).expect("decode symbol B"), 1); // B
        assert_eq!(tree.decode(&mut reader).expect("decode symbol C"), 2); // C
        assert_eq!(tree.decode(&mut reader).expect("decode symbol A again"), 0); // A
    }

    #[test]
    fn test_huffman_builder() {
        let mut builder = HuffmanBuilder::new(4, 15);
        builder.add_count(0, 100); // High frequency
        builder.add_count(1, 50);
        builder.add_count(2, 25);
        builder.add_count(3, 25);

        let lengths = builder.build_lengths();

        // Higher frequency symbols should have shorter codes
        assert!(lengths[0] <= lengths[1]);
        assert!(lengths[1] <= lengths[2]);

        // All used symbols should have non-zero lengths
        assert!(lengths[0] > 0);
        assert!(lengths[1] > 0);
        assert!(lengths[2] > 0);
        assert!(lengths[3] > 0);
    }

    #[test]
    fn test_empty_tree() {
        let lengths: [u8; 4] = [0, 0, 0, 0];
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build empty huffman tree");
        assert_eq!(tree.max_code_length, 0);
    }

    #[test]
    fn test_single_symbol() {
        // Single symbol tree
        let lengths = [1u8, 0, 0, 0];
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build single symbol tree");

        let data = vec![0b00000000u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).expect("decode single symbol"), 0);
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(HuffmanTree::reverse_bits(0b101, 3), 0b101);
        assert_eq!(HuffmanTree::reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(HuffmanTree::reverse_bits(0b10101010, 8), 0b01010101);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Regression: `build_lengths` must ALWAYS yield a COMPLETE, length-limited
    // canonical Huffman code (Kraft sum exactly 1.0, no length > max_bits).
    //
    // The historical dynamic-Huffman corruption bug was an *incomplete*
    // code-length code (Kraft sum 0.75 < 1.0), which oxiarc's own lenient
    // inflate accepted but which standard `zlib`/`unzip` reject at bit 0 with
    // "invalid compressed data to inflate". This battery — degenerate
    // distributions, two-symbol alphabets, and skews that force length-limiting
    // — would have caught that class of defect directly, with no external
    // dependency. Kraft is evaluated in exact integer arithmetic.
    // ─────────────────────────────────────────────────────────────────────────

    /// Returns `Some(())` if `lengths` is a COMPLETE prefix code within
    /// `max_bits`, else `None`. Complete ⇔ Σ 2^(-len) over used symbols == 1.
    fn assert_complete_within(lengths: &[u8], max_bits: u8, label: &str) {
        let max_len = lengths.iter().copied().max().unwrap_or(0);
        if max_len == 0 {
            // No used symbols (empty alphabet) — vacuously fine.
            return;
        }
        assert!(
            max_len <= max_bits,
            "{label}: max code length {max_len} exceeds limit {max_bits}: {lengths:?}"
        );
        // Σ 2^(max_len - len) must equal 2^max_len for a complete code.
        let mut num: u128 = 0;
        for &l in lengths {
            if l > 0 {
                num += 1u128 << (max_len as u32 - l as u32);
            }
        }
        assert_eq!(
            num,
            1u128 << max_len as u32,
            "{label}: INCOMPLETE code (Kraft {num}/{} != 1): {lengths:?}",
            1u128 << max_len as u32
        );
    }

    fn build_from(freqs: &[(u16, u32)], alphabet: usize, max_bits: u8) -> Vec<u8> {
        let mut b = HuffmanBuilder::new(alphabet, max_bits);
        for &(s, c) in freqs {
            b.add_count(s, c);
        }
        b.build_lengths()
    }

    #[test]
    fn test_build_lengths_degenerate_is_complete() {
        // Code-length alphabet (19 symbols, max 7) and litlen/dist (max 15).
        for &(alpha, max_bits) in &[(19usize, 7u8), (30, 15), (286, 15)] {
            // Exactly one symbol carries all the frequency, at three positions.
            for &pos in &[0u16, (alpha as u16) / 2, alpha as u16 - 1] {
                let l = build_from(&[(pos, 9_999)], alpha, max_bits);
                assert_complete_within(&l, max_bits, &format!("one-sym a{alpha} pos{pos}"));
                // The lone symbol must actually receive a code.
                assert!(l[pos as usize] > 0, "one-sym: used symbol got length 0");
            }
            // Exactly two symbols, wildly skewed.
            for &(a, c) in &[(0u16, 1u16), (0, alpha as u16 - 1), (3, 9)] {
                let l = build_from(&[(a, 1_000_000), (c, 1)], alpha, max_bits);
                assert_complete_within(&l, max_bits, &format!("two-sym a{alpha} {a},{c}"));
            }
        }
    }

    #[test]
    fn test_build_lengths_length_limited_is_complete() {
        // Fibonacci and power-of-two frequency profiles drive the natural
        // Huffman depth far past the alphabet limit, forcing the length-limiting
        // machinery to rebalance while preserving completeness.
        for &(alpha, max_bits) in &[(19usize, 7u8), (30, 15), (286, 15)] {
            let mut fib = Vec::new();
            let (mut a, mut b) = (1u32, 1u32);
            for s in 0..alpha {
                fib.push((s as u16, a));
                let n = a.saturating_add(b);
                a = b;
                b = n;
            }
            let lf = build_from(&fib, alpha, max_bits);
            assert_complete_within(&lf, max_bits, &format!("fib a{alpha}"));

            let pow: Vec<(u16, u32)> = (0..alpha)
                .map(|s| (s as u16, 1u32 << (s.min(30) as u32)))
                .collect();
            let lp = build_from(&pow, alpha, max_bits);
            assert_complete_within(&lp, max_bits, &format!("pow2 a{alpha}"));
        }
    }

    #[test]
    fn test_build_lengths_fuzz_is_complete() {
        // Deterministic xorshift fuzz across all three alphabets and a
        // heavy-tailed frequency distribution.
        let mut rng: u64 = 0x0f1e_2d3c_4b5a_6978;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _ in 0..20_000 {
            let (alpha, max_bits) = match next() % 3 {
                0 => (19usize, 7u8),
                1 => (30, 15),
                _ => (286, 15),
            };
            let used = 1 + (next() as usize % alpha);
            let mut freqs = Vec::with_capacity(used);
            for _ in 0..used {
                let s = (next() as usize % alpha) as u16;
                let c = match next() % 5 {
                    0 => 1u32,
                    1 => (next() % 10) as u32 + 1,
                    2 => (next() % 1_000) as u32 + 1,
                    _ => (next() % 3_000_000) as u32 + 1,
                };
                freqs.push((s, c));
            }
            let l = build_from(&freqs, alpha, max_bits);
            assert_complete_within(&l, max_bits, "fuzz");
        }
    }

    #[test]
    fn test_from_code_length_code_rejects_incomplete() {
        // Three symbols each of length 2 → Kraft 3/4 (the historical corruption).
        let incomplete = [2u8, 2, 2];
        assert!(
            HuffmanTree::from_code_length_code(&incomplete).is_err(),
            "strict constructor must reject an incomplete code-length code"
        );
        // The lenient constructor still accepts it (used for litlen/dist, and to
        // preserve decoding of the legitimately-incomplete fixed distance code).
        assert!(HuffmanTree::from_code_lengths(&incomplete).is_ok());

        // A complete code-length code is accepted by both.
        let complete = [1u8, 2, 2];
        assert!(HuffmanTree::from_code_length_code(&complete).is_ok());
        assert!(HuffmanTree::from_code_lengths(&complete).is_ok());

        // The legitimately-incomplete fixed distance code (30 × 5 bits, Kraft
        // 30/32) MUST still build via the lenient path — tightening must not
        // break fixed-Huffman decompression.
        let fixed_dist = [5u8; 30];
        assert!(HuffmanTree::from_code_lengths(&fixed_dist).is_ok());
    }
}
