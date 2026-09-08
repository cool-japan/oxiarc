//! Packed decode tables for the resumable DEFLATE core's symbol loops.
//!
//! [`crate::huffman::HuffmanTree`] is the crate's general-purpose Huffman
//! structure: it is public, it carries the canonical per-length tables the
//! bit-at-a-time fallback needs, and the encoder shares it. This module is
//! the opposite: a **decode-only**, crate-private table whose entries have
//! everything one DEFLATE symbol needs folded into a single `u32`, in the
//! style of zlib-ng's `inflate_table` and libdeflate's
//! `deflate_decode_table_entry`.
//!
//! What that buys, per symbol, over decoding through `HuffmanTree`:
//!
//! * no `LENGTH_EXTRA_BITS[code - 257]` / `DISTANCE_EXTRA_BITS[code]`
//!   lookup (two slice indexings with their bounds checks),
//! * no `decode_length` / `decode_distance` base lookup (two more),
//! * no `code < 256` / `code == 256` / `code > 285` comparison chain — the
//!   kind of the symbol is a flag bit that the builder resolved once,
//! * literal bytes come straight out of the entry.
//!
//! The tables are built from the same code lengths as the equivalent
//! `HuffmanTree` and are proven to agree with it, bit pattern for bit
//! pattern, by [`tests::decode_table_agrees_with_huffman_tree`].
//!
//! # Entry layout
//!
//! | bits | meaning |
//! |---|---|
//! | 0..=4 | code length in bits (`1..=15`); `0` marks an unused slot |
//! | 5..=8 | extra-bit count (`0..=13`), or a sub-table's index width |
//! | 9 | [`LITERAL`] |
//! | 10 | [`END_OF_BLOCK`] |
//! | 11 | [`SUBTABLE`] |
//! | 12 | [`INVALID`] |
//! | 16..=31 | payload: literal byte, length/distance base, sub-table offset, or the symbol of an [`INVALID`] entry |
//!
//! An all-zero entry is "no code here" — either an unused slot of a
//! legitimately incomplete code or a hole in a sub-table — and decodes to a
//! length of `0`, which callers must reject.

use oxiarc_core::error::{OxiArcError, Result};

use crate::tables::{
    DISTANCE_BASE, DISTANCE_EXTRA_BITS, LENGTH_BASE, LENGTH_EXTRA_BITS, fixed_distance_lengths,
    fixed_litlen_lengths,
};

/// Longest DEFLATE Huffman code (RFC 1951 §3.2.7).
const MAX_CODE_LENGTH: usize = 15;

/// Mask for an entry's code-length field.
pub(crate) const LEN_MASK: u32 = 0x1F;

/// Shift for an entry's extra-bit-count field.
const EXTRA_SHIFT: u32 = 5;

/// Mask (post-shift) for an entry's extra-bit-count field.
const EXTRA_MASK: u32 = 0x0F;

/// Shift for an entry's payload field.
const PAYLOAD_SHIFT: u32 = 16;

/// The entry describes a literal byte; the payload is that byte.
pub(crate) const LITERAL: u32 = 1 << 9;

/// The entry is the end-of-block symbol (256).
pub(crate) const END_OF_BLOCK: u32 = 1 << 10;

/// The entry points at a sub-table: the extra field is the sub-table's
/// index width and the payload its offset in `table`.
pub(crate) const SUBTABLE: u32 = 1 << 11;

/// The symbol exists in the alphabet but may never appear in a stream
/// (literal/length 286-287, distance 30-31). The payload is the symbol, so
/// the error message can name it.
pub(crate) const INVALID: u32 = 1 << 12;

/// Every flag that means "this is not a length/distance entry".
///
/// Used by the differential test that pins this module against
/// [`crate::huffman::HuffmanTree`]; the decode loops test the individual
/// flags they care about.
#[cfg(test)]
pub(crate) const NOT_A_MATCH: u32 = LITERAL | END_OF_BLOCK | SUBTABLE | INVALID;

/// The entry stored in a slot no code reaches: an incomplete code's unused
/// root slot, or a gap in a sub-table.
///
/// It carries [`INVALID`] with a **zero code length**, which is what
/// separates it from symbol 286/287 or distance 30/31 (those have a real
/// length). Marking holes rather than zeroing them lets the decode loops
/// dispatch on the flag bits alone and drop the "did a code match at all"
/// test from the hot path — the length check survives, but only on the
/// already-cold invalid branch.
pub(crate) const HOLE: u32 = INVALID;

/// Largest sub-table offset an entry's payload can address.
const MAX_TABLE_INDEX: usize = 0xFFFF;

/// Which alphabet a table decodes, i.e. how symbols become payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableKind {
    /// RFC 1951 literal/length alphabet (0..=287).
    LitLen,
    /// RFC 1951 distance alphabet (0..=31).
    Distance,
}

impl TableKind {
    /// Root index width for this alphabet.
    ///
    /// The literal/length root resolves a whole literal in one load for
    /// essentially every real stream; the distance alphabet has 30 symbols
    /// and a correspondingly shallower code, so a narrower root costs
    /// nothing and keeps the per-block build (a zeroing pass over the root
    /// table) small — the shape zlib-ng and libdeflate also settled on.
    const fn root_bits(self) -> u8 {
        match self {
            TableKind::LitLen => 10,
            TableKind::Distance => 9,
        }
    }

    /// Pack one symbol of this alphabet into an entry body (everything but
    /// the code-length field).
    fn entry_body(self, symbol: u16) -> u32 {
        match self {
            TableKind::LitLen => match symbol {
                0..=255 => LITERAL | (u32::from(symbol) << PAYLOAD_SHIFT),
                256 => END_OF_BLOCK,
                257..=285 => {
                    let index = usize::from(symbol) - 257;
                    let base = LENGTH_BASE.get(index).copied().unwrap_or(3);
                    let extra = LENGTH_EXTRA_BITS.get(index).copied().unwrap_or(0);
                    (u32::from(extra) << EXTRA_SHIFT) | (u32::from(base) << PAYLOAD_SHIFT)
                }
                _ => INVALID | (u32::from(symbol) << PAYLOAD_SHIFT),
            },
            TableKind::Distance => match symbol {
                0..=29 => {
                    let index = usize::from(symbol);
                    let base = DISTANCE_BASE.get(index).copied().unwrap_or(1);
                    let extra = DISTANCE_EXTRA_BITS.get(index).copied().unwrap_or(0);
                    (u32::from(extra) << EXTRA_SHIFT) | (u32::from(base) << PAYLOAD_SHIFT)
                }
                _ => INVALID | (u32::from(symbol) << PAYLOAD_SHIFT),
            },
        }
    }
}

/// A two-level packed decode table.
///
/// The root table (`1 << root_bits` entries) is followed by every sub-table
/// in one contiguous `Vec<u32>`, so a decode costs at most two dependent L1
/// loads.
#[derive(Debug)]
pub(crate) struct DecodeTable {
    /// Root table followed by all sub-tables.
    table: Vec<u32>,
    /// Symbols in canonical order (by code length, then symbol), the build's
    /// only scratch buffer. At most one alphabet's worth of `u16`, reused
    /// across builds.
    sorted: Vec<u16>,
    /// Bits indexing the root table.
    root_bits: u8,
    /// `(1 << root_bits) - 1`.
    root_mask: u32,
    /// Longest code in this table (`0` when the table decodes nothing).
    max_len: u8,
    /// Which alphabet this table decodes.
    kind: TableKind,
}

impl DecodeTable {
    /// An empty table for `kind` that decodes nothing.
    pub(crate) fn new(kind: TableKind) -> Self {
        Self {
            table: Vec::new(),
            sorted: Vec::new(),
            root_bits: 0,
            root_mask: 0,
            max_len: 0,
            kind,
        }
    }

    /// Build a table for `kind` from `code_lengths`.
    pub(crate) fn from_code_lengths(kind: TableKind, code_lengths: &[u8]) -> Result<Self> {
        let mut table = Self::new(kind);
        table.rebuild(code_lengths)?;
        Ok(table)
    }

    /// Longest code this table can produce; `0` when it decodes nothing.
    #[inline(always)]
    pub(crate) fn max_len(&self) -> u8 {
        self.max_len
    }

    /// Packed entry for the next bits of the stream, LSB-first in `bits`.
    ///
    /// `bits` must hold at least [`DecodeTable::max_len`] stream bits in its
    /// low bits (zero padded past the end of the stream is fine — the
    /// caller validates the returned code length against the number of bits
    /// it really has).
    #[inline(always)]
    pub(crate) fn entry(&self, bits: u32) -> u32 {
        let entry = self
            .table
            .get((bits & self.root_mask) as usize)
            .copied()
            .unwrap_or(HOLE);
        if entry & SUBTABLE == 0 {
            return entry;
        }
        self.sub_entry(bits, entry)
    }

    /// Second-level lookup for a code longer than the root index.
    #[inline]
    fn sub_entry(&self, bits: u32, entry: u32) -> u32 {
        let sub_bits = (entry >> EXTRA_SHIFT) & EXTRA_MASK;
        let base = (entry >> PAYLOAD_SHIFT) as usize;
        let index = ((bits >> self.root_bits) & ((1u32 << sub_bits) - 1)) as usize;
        self.table.get(base + index).copied().unwrap_or(HOLE)
    }

    /// Rebuild in place from `code_lengths`, reusing every allocation.
    ///
    /// This is zlib's `inflate_table` algorithm: symbols are counting-sorted
    /// into canonical order and the table is filled in **one** pass, with
    /// the reversed code maintained by a backwards increment (add one at the
    /// top bit, carry downwards). The obvious two-pass build instead
    /// reverses every symbol's code with a per-bit loop — up to 15
    /// iterations per symbol, twice — which for a stream that starts a new
    /// dynamic block every few thousand symbols costs more than decoding
    /// them.
    ///
    /// Sub-tables are created on demand and sized from the code space still
    /// unaccounted for, so the table is no larger than it must be.
    ///
    /// Steady-state allocation-free: the root table is `clear` + `resize`d
    /// into the existing buffer, the canonical-order scratch is resized at
    /// most once per alphabet, and the sub-table space is reserved once from
    /// a bound no later block of the same shape can grow (see the comment on
    /// the reservation).
    pub(crate) fn rebuild(&mut self, code_lengths: &[u8]) -> Result<()> {
        // Any early return must leave a table that decodes nothing.
        self.max_len = 0;
        self.root_bits = 0;
        self.root_mask = 0;
        self.table.clear();
        self.table.push(HOLE);

        if code_lengths.is_empty() {
            return Err(OxiArcError::invalid_header("Empty code lengths"));
        }

        // ── how many codes of each length ───────────────────────────────
        let mut count = [0u16; MAX_CODE_LENGTH + 1];
        for &len in code_lengths {
            if usize::from(len) > MAX_CODE_LENGTH {
                return Err(OxiArcError::invalid_header(format!(
                    "Code length {} exceeds maximum {}",
                    len, MAX_CODE_LENGTH
                )));
            }
            if len != 0 {
                count[usize::from(len)] += 1;
            }
        }
        let mut max_length = 0usize;
        for len in (1..=MAX_CODE_LENGTH).rev() {
            if count[len] != 0 {
                max_length = len;
                break;
            }
        }
        if max_length == 0 {
            // No symbols at all: `self` already decodes nothing.
            return Ok(());
        }
        let mut min_length = max_length;
        for (len, &at_len) in count.iter().enumerate().take(max_length + 1).skip(1) {
            if at_len != 0 {
                min_length = len;
                break;
            }
        }

        // ── code space (Kraft): over-subscription is always invalid ──────
        //
        // Incomplete codes are tolerated exactly as
        // `HuffmanTree::from_code_lengths` tolerates them: unused slots stay
        // zero and decode to `InvalidHuffmanCode` if a stream selects one.
        let mut left = 1i32;
        for &at_len in count.iter().take(MAX_CODE_LENGTH + 1).skip(1) {
            left <<= 1;
            left -= i32::from(at_len);
            if left < 0 {
                return Err(OxiArcError::invalid_header("Over-subscribed Huffman tree"));
            }
        }

        // ── symbols in canonical order (counting sort) ──────────────────
        let mut offs = [0u16; MAX_CODE_LENGTH + 2];
        for len in 1..=MAX_CODE_LENGTH {
            offs[len + 1] = offs[len] + count[len];
        }
        let total = usize::from(offs[MAX_CODE_LENGTH + 1]);
        if self.sorted.len() < total {
            // Rounded up to the alphabet size so a wandering `HLIT` cannot
            // grow this more than once.
            let want = code_lengths.len().max(total);
            self.sorted.resize(want, 0u16);
        }
        for (symbol, &len) in code_lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let slot = usize::from(offs[usize::from(len)]);
            if let Some(cell) = self.sorted.get_mut(slot) {
                *cell = symbol as u16;
            }
            offs[usize::from(len)] += 1;
        }

        // ── the root table ──────────────────────────────────────────────
        let root_bits = self.kind.root_bits().min(max_length as u8);
        let root_size = 1usize << root_bits;
        let root_mask = (root_size - 1) as u32;
        self.table.clear();
        self.table.resize(root_size, HOLE);

        // Reserve the sub-table space once, rounded up to a power of two, so
        // a stream whose block shapes wander reallocates at most `log2`
        // times over the decoder's whole life instead of once per block.
        //
        // A prefix group whose longest code is `root_bits + j` gets a
        // sub-table of at most `1 << j` entries and contains at least one
        // code of exactly that length, so at most `count[root_bits + j]`
        // groups can be that deep: the sum below is an upper bound for every
        // *complete* code, which is what a real stream carries. (A
        // deliberately incomplete code can make the sizing loop below pick a
        // wider sub-table than its group needs; that grows the buffer once
        // and then stays, rather than allocating per block.) Reserving the
        // alphabet's absolute worst case instead would cost tens of KiB of
        // resident memory per table, which the streaming decoder's
        // allocation budget cannot afford.
        let mut sub_worst = 0usize;
        for j in 1..=(max_length - usize::from(root_bits)) {
            let deep = count.get(usize::from(root_bits) + j).copied().unwrap_or(0);
            sub_worst = sub_worst.saturating_add(usize::from(deep) << j);
        }
        let worst = root_size
            .saturating_add(sub_worst)
            .next_power_of_two()
            .min(MAX_TABLE_INDEX + 1);
        if self.table.capacity() < worst {
            self.table
                .reserve_exact(worst - self.table.len().min(worst));
        }

        // ── one pass over the canonical order ───────────────────────────
        let Self {
            table,
            sorted,
            kind,
            ..
        } = self;
        let mut remaining = count;
        // The code being assigned, bit-reversed (the accumulator holds
        // stream bits LSB-first, canonical codes are MSB-first).
        let mut huff = 0u32;
        let mut sym = 0usize;
        let mut len = min_length;
        // Base of the table currently being filled, its index width, and how
        // many of the code's low bits index the root table.
        let mut next_base = 0usize;
        let mut curr = usize::from(root_bits);
        let mut drop_bits = 0usize;
        // Root prefix of the sub-table being filled; `u32::MAX` can never
        // equal a real prefix, so the first long code starts a sub-table.
        let mut low = u32::MAX;
        // Size of the table the *last* fill went into, which is where the
        // next sub-table starts. Seeded with the root table's size, because
        // the first code may already need a sub-table: when every code in
        // the alphabet is longer than the root index (a legal, if
        // degenerate, incomplete code — a single 15-bit distance code, say)
        // there is no root slot to replicate into at all.
        let mut table_span = root_size;

        loop {
            // Start a sub-table whenever this code is longer than the root
            // index and its root prefix is one no sub-table covers yet.
            //
            // zlib's `inflate_table` makes this test *after* filling each
            // symbol, which works there because zlib widens the root to the
            // shortest code (`if (min > root) root = min;`) and rejects
            // incomplete sets outright, so its first code always fits the
            // root. This table tolerates incomplete codes — `HuffmanTree`
            // does, and the two must agree — so the first code can be
            // longer than the root, and the test has to run before the
            // first fill: with `curr` still the root width and `drop_bits`
            // still zero, the replication stride `1 << (len - drop_bits)`
            // would exceed `1 << curr` and the fill loop's `fill -= incr`
            // would wrap (a panic in a debug build, a loop that never
            // reaches zero in a release one). Everything else about the
            // test is unchanged: at this point `huff`, `len` and
            // `remaining` hold exactly what they held at the end of the
            // previous iteration, which is where zlib evaluates it.
            if len > usize::from(root_bits) && (huff & root_mask) != low {
                if drop_bits == 0 {
                    drop_bits = usize::from(root_bits);
                }
                next_base += table_span;

                // Width of the new sub-table: enough for this prefix's
                // codes, grown while the code space below it is still
                // unaccounted for.
                curr = len - drop_bits;
                let mut space = 1i32 << curr;
                while curr + drop_bits < max_length {
                    let deeper = remaining.get(curr + drop_bits).copied().unwrap_or(0);
                    space -= i32::from(deeper);
                    if space <= 0 {
                        break;
                    }
                    curr += 1;
                    space <<= 1;
                }

                let need = next_base.saturating_add(1usize << curr);
                if need > MAX_TABLE_INDEX + 1 {
                    return Err(OxiArcError::invalid_header(
                        "Huffman decode table too large",
                    ));
                }
                if table.len() < need {
                    table.resize(need, HOLE);
                }
                low = huff & root_mask;
                if let Some(slot) = table.get_mut(low as usize) {
                    *slot = SUBTABLE
                        | ((next_base as u32) << PAYLOAD_SHIFT)
                        | ((curr as u32) << EXTRA_SHIFT);
                }
            }

            let symbol = sorted.get(sym).copied().unwrap_or(0);
            let entry = kind.entry_body(symbol) | (len as u32);

            // Replicate over every index of the current table whose low
            // bits are this code.
            let incr = 1usize << (len - drop_bits);
            let mut fill = 1usize << curr;
            table_span = fill;
            loop {
                fill -= incr;
                let index = next_base + ((huff >> drop_bits) as usize) + fill;
                if let Some(slot) = table.get_mut(index) {
                    *slot = entry;
                }
                if fill == 0 {
                    break;
                }
            }

            // Backwards increment of the reversed code.
            let mut bit = 1u32 << (len - 1);
            while huff & bit != 0 {
                bit >>= 1;
            }
            if bit != 0 {
                huff &= bit - 1;
                huff += bit;
            } else {
                huff = 0;
            }

            sym += 1;
            if let Some(slot) = remaining.get_mut(len) {
                *slot -= 1;
                if *slot == 0 {
                    if len == max_length {
                        break;
                    }
                    let next_symbol = sorted.get(sym).copied().unwrap_or(0);
                    len = code_lengths
                        .get(usize::from(next_symbol))
                        .map_or(max_length, |l| usize::from(*l));
                }
            }
        }

        self.root_bits = root_bits;
        self.root_mask = root_mask;
        self.max_len = max_length as u8;
        Ok(())
    }
}

/// Code length encoded in an entry (`0` when the entry is unused).
#[inline(always)]
pub(crate) fn entry_len(entry: u32) -> u8 {
    (entry & LEN_MASK) as u8
}

/// Extra-bit count encoded in an entry.
#[inline(always)]
pub(crate) fn entry_extra(entry: u32) -> u8 {
    ((entry >> EXTRA_SHIFT) & EXTRA_MASK) as u8
}

/// Payload encoded in an entry (literal byte, length/distance base, or the
/// symbol of an [`INVALID`] entry).
#[inline(always)]
pub(crate) fn entry_payload(entry: u32) -> u32 {
    entry >> PAYLOAD_SHIFT
}

/// The RFC 1951 §3.2.6 fixed literal/length table, built once.
pub(crate) fn fixed_litlen_table() -> &'static DecodeTable {
    static TABLE: std::sync::OnceLock<DecodeTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        DecodeTable::from_code_lengths(TableKind::LitLen, &fixed_litlen_lengths())
            .unwrap_or_else(|_| DecodeTable::new(TableKind::LitLen))
    })
}

/// The RFC 1951 §3.2.6 fixed distance table, built once.
pub(crate) fn fixed_distance_table() -> &'static DecodeTable {
    static TABLE: std::sync::OnceLock<DecodeTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        DecodeTable::from_code_lengths(TableKind::Distance, &fixed_distance_lengths())
            .unwrap_or_else(|_| DecodeTable::new(TableKind::Distance))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::huffman::HuffmanTree;
    use crate::tables::{decode_distance, decode_length};
    use oxiarc_core::BitCache;

    /// Load `bits` (LSB-first, `width` bits) into a fresh cache.
    fn cache_with(bits: u32, width: u8) -> BitCache {
        let bytes = bits.to_le_bytes();
        let mut cache = BitCache::default();
        cache.refill_bytes(&bytes, width.min(32));
        cache
    }

    /// What one [`assert_agrees`] call actually compared, so a sweep can
    /// prove it did not go vacuous.
    #[derive(Default)]
    struct Compared {
        /// Bit patterns checked through both structures.
        patterns: u64,
        /// `HuffmanTree` rejected the shape, so nothing was compared.
        rejected: bool,
    }

    /// Every bit pattern must decode to the same code length and the same
    /// symbol semantics through both structures.
    fn assert_agrees(lengths: &[u8], kind: TableKind) -> Compared {
        let tree = match HuffmanTree::from_code_lengths(lengths) {
            Ok(tree) => tree,
            Err(_) => {
                assert!(
                    DecodeTable::from_code_lengths(kind, lengths).is_err(),
                    "HuffmanTree rejected the lengths but DecodeTable accepted them"
                );
                return Compared {
                    patterns: 0,
                    rejected: true,
                };
            }
        };
        let table = DecodeTable::from_code_lengths(kind, lengths).expect("decode table");
        assert_eq!(table.max_len(), tree.max_code_length(), "max length");
        let width = tree.max_code_length();
        if width == 0 {
            return Compared::default();
        }
        for bits in 0..(1u32 << width) {
            let cache = cache_with(bits, width);
            let tree_entry = tree.lookup_cached(&cache);
            let entry = table.entry(bits);
            assert_eq!(
                entry_len(entry),
                HuffmanTree::entry_length(tree_entry),
                "code length for bits {bits:#x}"
            );
            if entry_len(entry) == 0 {
                continue;
            }
            let symbol = HuffmanTree::entry_symbol(tree_entry);
            match kind {
                TableKind::LitLen => match symbol {
                    0..=255 => {
                        assert_eq!(entry & LITERAL, LITERAL, "literal flag for {symbol}");
                        assert_eq!(entry_payload(entry), u32::from(symbol), "literal byte");
                    }
                    256 => assert_eq!(entry & END_OF_BLOCK, END_OF_BLOCK, "end-of-block flag"),
                    257..=285 => {
                        assert_eq!(entry & NOT_A_MATCH, 0, "length entry has no flag");
                        assert_eq!(
                            entry_extra(entry),
                            LENGTH_EXTRA_BITS[usize::from(symbol) - 257],
                            "length extra bits for {symbol}"
                        );
                        assert_eq!(
                            entry_payload(entry) as u16,
                            decode_length(symbol, 0),
                            "length base for {symbol}"
                        );
                    }
                    _ => {
                        assert_eq!(entry & INVALID, INVALID, "invalid flag for {symbol}");
                        assert_eq!(entry_payload(entry), u32::from(symbol), "invalid symbol");
                    }
                },
                TableKind::Distance => {
                    if symbol < 30 {
                        assert_eq!(entry & NOT_A_MATCH, 0, "distance entry has no flag");
                        assert_eq!(
                            entry_extra(entry),
                            DISTANCE_EXTRA_BITS[usize::from(symbol)],
                            "distance extra bits for {symbol}"
                        );
                        assert_eq!(
                            entry_payload(entry) as u16,
                            decode_distance(symbol, 0),
                            "distance base for {symbol}"
                        );
                    } else {
                        assert_eq!(entry & INVALID, INVALID, "invalid flag for {symbol}");
                        assert_eq!(entry_payload(entry), u32::from(symbol), "invalid symbol");
                    }
                }
            }
        }
        Compared {
            patterns: 1u64 << width,
            rejected: false,
        }
    }

    #[test]
    fn decode_table_agrees_with_huffman_tree() {
        assert_agrees(&fixed_litlen_lengths(), TableKind::LitLen);
        assert_agrees(&fixed_distance_lengths(), TableKind::Distance);

        // A deep literal/length code that forces sub-tables: a handful of
        // 15-bit codes plus a dense short-code population.
        let mut lengths = vec![0u8; 288];
        for (i, slot) in lengths.iter_mut().enumerate() {
            *slot = match i {
                0..=63 => 7,
                64..=127 => 9,
                256 => 7,
                257..=262 => 10,
                280..=287 => 15,
                _ => 0,
            };
        }
        assert_agrees(&lengths, TableKind::LitLen);

        // Distance code with codes on both sides of the root width.
        let dist = [
            4u8, 4, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15,
            15, 15, 15, 0, 0, 0, 0,
        ];
        assert_agrees(&dist, TableKind::Distance);

        // Degenerate single-symbol (incomplete) codes, which DEFLATE allows
        // for the distance alphabet.
        assert_agrees(&[1u8, 0, 0, 0], TableKind::Distance);
        let mut single = vec![0u8; 288];
        single[65] = 1;
        assert_agrees(&single, TableKind::LitLen);
    }

    #[test]
    fn pseudorandom_shapes_agree_with_huffman_tree() {
        // xorshift so the sweep is reproducible without a dependency.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..48 {
            let alphabet = if round % 2 == 0 { 288usize } else { 32 };
            let kind = if round % 2 == 0 {
                TableKind::LitLen
            } else {
                TableKind::Distance
            };
            // Build a *valid* canonical shape by handing out code space.
            let mut lengths = vec![0u8; alphabet];
            let mut remaining = 1u64 << 15;
            for slot in lengths.iter_mut() {
                if remaining == 0 {
                    break;
                }
                let len = 1 + (next() % 12) as u8;
                let cost = 1u64 << (15 - len);
                if cost > remaining || next() % 3 == 0 {
                    continue;
                }
                *slot = len;
                remaining -= cost;
            }
            if lengths.iter().all(|&l| l == 0) {
                continue;
            }
            assert_agrees(&lengths, kind);
        }
    }

    /// A far wider shape sweep than
    /// [`pseudorandom_shapes_agree_with_huffman_tree`], aimed at the places
    /// a one-pass table build goes wrong: codes that sit exactly on the
    /// root/sub-table boundary, populations of many maximum-length codes
    /// (which force several sub-tables), single-symbol alphabets at every
    /// length, and deliberately incomplete codes (legal here, since
    /// `HuffmanTree` accepts them and this table must not disagree with it).
    ///
    /// Every shape is compared against [`HuffmanTree`] for **every** bit
    /// pattern of the alphabet's maximum code length, so a sub-table that is
    /// one entry short, a replication stride that is off by a factor of two
    /// or a hole that should have been a code all show up as a mismatch
    /// rather than as a rare wrong byte in a decoded stream.
    #[test]
    fn a_wide_shape_sweep_agrees_with_huffman_tree() {
        let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut shapes = 0usize;
        let mut patterns = 0u64;
        let mut deep = 0usize;
        let mut note = |c: Compared| {
            if !c.rejected {
                shapes += 1;
                patterns += c.patterns;
            }
        };

        // Single-symbol alphabets at every code length, both alphabets.
        //
        // The lengths above each alphabet's root width (10 for
        // literal/length, 9 for distance) are the shapes whose *shortest*
        // code does not fit the root table — the case zlib avoids by
        // widening the root and rejecting incomplete sets, and the one that
        // used to panic in debug and loop forever in release. `deep` counts
        // them so the sweep cannot silently stop covering them.
        for len in 1..=MAX_CODE_LENGTH {
            let mut litlen = vec![0u8; 288];
            litlen[7] = len as u8;
            if len > usize::from(TableKind::LitLen.root_bits()) {
                deep += 1;
            }
            note(assert_agrees(&litlen, TableKind::LitLen));
            let mut dist = vec![0u8; 32];
            dist[3] = len as u8;
            if len > usize::from(TableKind::Distance.root_bits()) {
                deep += 1;
            }
            note(assert_agrees(&dist, TableKind::Distance));
        }

        // Whole populations pinned to one length, including the two lengths
        // either side of each alphabet's root width.
        for len in 1..=MAX_CODE_LENGTH {
            let population = 1usize << len;
            let mut litlen = vec![0u8; 288];
            for slot in litlen.iter_mut().take(population.min(288)) {
                *slot = len as u8;
            }
            note(assert_agrees(&litlen, TableKind::LitLen));
            let mut dist = vec![0u8; 32];
            for slot in dist.iter_mut().take(population.min(32)) {
                *slot = len as u8;
            }
            note(assert_agrees(&dist, TableKind::Distance));
        }

        // Randomised shapes, half of them then punched full of holes so the
        // incomplete-code path is swept as hard as the complete one.
        for round in 0..200 {
            let (alphabet, kind) = if round % 2 == 0 {
                (288usize, TableKind::LitLen)
            } else {
                (32usize, TableKind::Distance)
            };
            let mut lengths = vec![0u8; alphabet];
            let mut remaining = 1u64 << MAX_CODE_LENGTH;
            // Bias towards long codes on some rounds, so sub-tables are
            // built and refilled rather than always being one deep.
            let long_bias = round % 3 == 0;
            for slot in lengths.iter_mut() {
                if remaining == 0 {
                    break;
                }
                let len = if long_bias {
                    9 + (next() % 7) as u8
                } else {
                    1 + (next() % 15) as u8
                };
                let cost = 1u64 << (MAX_CODE_LENGTH - usize::from(len));
                if cost > remaining || next() % 4 == 0 {
                    continue;
                }
                *slot = len;
                remaining -= cost;
            }
            if lengths.iter().all(|&l| l == 0) {
                continue;
            }
            note(assert_agrees(&lengths, kind));

            // The same shape with a random third of its symbols removed:
            // still canonical, deliberately incomplete.
            let mut holed = lengths.clone();
            for slot in holed.iter_mut() {
                if next() % 3 == 0 {
                    *slot = 0;
                }
            }
            if holed.iter().any(|&l| l != 0) {
                note(assert_agrees(&holed, kind));
            }
        }

        // The sweep is only worth anything if it really compared a large
        // number of bit patterns *and* really covered the degenerate shapes
        // this test exists for. A refactor that made `assert_agrees` return
        // early would otherwise leave a green test proving nothing.
        assert!(
            shapes > 400 && patterns > 10_000_000 && deep == 11,
            "the shape sweep went vacuous: {shapes} shapes, {patterns} bit \
             patterns, {deep} shapes whose shortest code exceeds the root"
        );
    }

    /// Rebuilding in place must produce exactly the table a fresh build
    /// would, for a long sequence of unrelated shapes — the steady state a
    /// stream of dynamic blocks puts the decoder in.
    #[test]
    fn a_long_rebuild_sequence_never_diverges_from_a_fresh_build() {
        let mut state = 0x0BAD_F00D_1234_5678u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut reused = DecodeTable::new(TableKind::LitLen);
        for _ in 0..64 {
            let mut lengths = vec![0u8; 288];
            let mut remaining = 1u64 << MAX_CODE_LENGTH;
            for slot in lengths.iter_mut() {
                if remaining == 0 {
                    break;
                }
                let len = 1 + (next() % 15) as u8;
                let cost = 1u64 << (MAX_CODE_LENGTH - usize::from(len));
                if cost > remaining || next() % 3 == 0 {
                    continue;
                }
                *slot = len;
                remaining -= cost;
            }
            if lengths.iter().all(|&l| l == 0) {
                continue;
            }
            let fresh = match DecodeTable::from_code_lengths(TableKind::LitLen, &lengths) {
                Ok(fresh) => fresh,
                Err(_) => {
                    assert!(
                        reused.rebuild(&lengths).is_err(),
                        "rebuild accepted a shape a fresh build rejected"
                    );
                    continue;
                }
            };
            reused.rebuild(&lengths).expect("rebuild");
            assert_eq!(reused.max_len(), fresh.max_len());
            for bits in 0..(1u32 << fresh.max_len()) {
                assert_eq!(reused.entry(bits), fresh.entry(bits), "bits {bits:#x}");
            }
        }
    }

    #[test]
    fn rebuild_reuses_allocations_and_matches_a_fresh_build() {
        let mut table = DecodeTable::new(TableKind::LitLen);
        table.rebuild(&fixed_litlen_lengths()).expect("first build");
        let mut lengths = vec![0u8; 288];
        for (i, slot) in lengths.iter_mut().enumerate() {
            *slot = match i {
                0..=127 => 8,
                256 => 8,
                257..=260 => 12,
                _ => 0,
            };
        }
        table.rebuild(&lengths).expect("second build");
        let fresh = DecodeTable::from_code_lengths(TableKind::LitLen, &lengths).expect("fresh");
        assert_eq!(table.max_len(), fresh.max_len());
        for bits in 0..(1u32 << fresh.max_len()) {
            assert_eq!(table.entry(bits), fresh.entry(bits), "bits {bits:#x}");
        }
    }

    #[test]
    fn invalid_shapes_are_rejected() {
        assert!(DecodeTable::from_code_lengths(TableKind::LitLen, &[]).is_err());
        assert!(DecodeTable::from_code_lengths(TableKind::LitLen, &[16, 1]).is_err());
        // Over-subscribed: two 1-bit codes plus a third.
        assert!(DecodeTable::from_code_lengths(TableKind::LitLen, &[1, 1, 1]).is_err());
        // All zeros decodes nothing but is not an error.
        let table = DecodeTable::from_code_lengths(TableKind::Distance, &[0, 0]).expect("zeros");
        assert_eq!(table.max_len(), 0);
        assert_eq!(entry_len(table.entry(0)), 0);
        assert_eq!(table.entry(0) & INVALID, INVALID);
    }
}
