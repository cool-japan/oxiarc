//! Huffman tables on the encode side: the code/size lookup derived from a
//! `DHT`, and libjpeg's optimal table generator.
//!
//! # Tie-breaking is part of the format
//!
//! Annex K.2 specifies the code-length assignment but leaves two things to
//! the implementation, and both are visible in the output bytes:
//!
//! * libjpeg injects a **pseudo-symbol 256 with frequency 1** before running
//!   Huffman's algorithm, then removes one code from the longest length
//!   afterwards. That is what guarantees no real symbol receives the all-ones
//!   code, which a decoder is allowed to treat as a marker prefix;
//! * when two symbols tie on frequency, libjpeg takes the **larger** symbol
//!   index, because its search scans upwards with `freq[i] <= v`.
//!
//! Reproducing both is what makes `cjpeg -optimize` and every progressive
//! frame byte-comparable, since progressive coding always generates tables.

use crate::error::{JpegError, Result};
use crate::huffman::HuffmanTable;

/// Symbols a `DHT` can name, plus libjpeg's pseudo-symbol.
pub(crate) const SYMBOL_SLOTS: usize = 257;

/// Per-symbol code and length, indexed by symbol value.
#[derive(Debug, Clone)]
pub(crate) struct DerivedTable {
    code: [u16; 256],
    size: [u8; 256],
}

impl DerivedTable {
    /// Build the encoding tables from a `DHT`'s `BITS`/`HUFFVAL`
    /// (`jchuff.c`'s `jpeg_make_c_derived_tbl`, figures C.1 to C.3).
    pub(crate) fn new(table: &HuffmanTable) -> Self {
        let mut code = [0u16; 256];
        let mut size = [0u8; 256];
        let mut next = 0u32;
        let mut index = 0usize;
        let values = table.values();
        for (length, &count) in table.bits().iter().enumerate() {
            for _ in 0..count {
                if let Some(&symbol) = values.get(index) {
                    code[usize::from(symbol)] = next as u16;
                    size[usize::from(symbol)] = (length + 1) as u8;
                }
                index += 1;
                next += 1;
            }
            next <<= 1;
        }
        Self { code, size }
    }

    /// The code and its length for `symbol`, or an error when the table does
    /// not define it.
    #[inline]
    pub(crate) fn lookup(&self, symbol: u8) -> Result<(u32, u32)> {
        let index = usize::from(symbol);
        let size = self.size[index];
        if size == 0 {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "huffman_tables",
                reason: "the image needs a symbol the chosen Huffman table does not define",
            });
        }
        Ok((u32::from(self.code[index]), u32::from(size)))
    }
}

/// Symbol frequencies gathered over one table's worth of a scan.
///
/// Slot 256 is libjpeg's pseudo-symbol; callers never touch it.
#[derive(Debug, Clone)]
pub(crate) struct Histogram {
    counts: Box<[u64; SYMBOL_SLOTS]>,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: Box::new([0u64; SYMBOL_SLOTS]),
        }
    }
}

impl Histogram {
    /// Count one occurrence of `symbol`.
    #[inline]
    pub(crate) fn count(&mut self, symbol: u8) {
        self.counts[usize::from(symbol)] += 1;
    }

    /// `true` when nothing was counted.
    pub(crate) fn is_empty(&self) -> bool {
        self.counts.iter().all(|&c| c == 0)
    }

    /// Generate the optimal table for these frequencies.
    pub(crate) fn optimal_table(&self) -> Result<HuffmanTable> {
        generate_optimal_table(&self.counts)
    }
}

/// The longest code length the tree-building step may produce before the
/// length-limiting pass folds it back to sixteen.
///
/// libjpeg uses 32 and raises `JERR_HUFF_CLEN_OVERFLOW` above it. We size the
/// arrays for the true worst case (a 257-leaf tree cannot be deeper than 256)
/// so no input can be rejected. For every histogram libjpeg accepts, the
/// result is identical: the extra slots are all zero and the limiting loop
/// skips them.
const MAX_CODE_LENGTH: usize = 256;

/// libjpeg's `jpeg_gen_optimal_table` (`jchuff.c`), Annex K.2.
fn generate_optimal_table(frequencies: &[u64; SYMBOL_SLOTS]) -> Result<HuffmanTable> {
    let mut freq = *frequencies;
    let mut codesize = [0usize; SYMBOL_SLOTS];
    let mut others = [usize::MAX; SYMBOL_SLOTS];

    // The pseudo-symbol guarantees that no real symbol ends up with the
    // all-ones code, because 256 is placed last in the longest length.
    freq[256] = 1;

    loop {
        // Smallest non-zero frequency; ties take the larger symbol index,
        // which is what scanning upwards with `<=` produces.
        let mut c1 = usize::MAX;
        let mut best = u64::MAX;
        for (symbol, &count) in freq.iter().enumerate() {
            if count != 0 && count <= best {
                best = count;
                c1 = symbol;
            }
        }
        let mut c2 = usize::MAX;
        let mut best = u64::MAX;
        for (symbol, &count) in freq.iter().enumerate() {
            if count != 0 && count <= best && symbol != c1 {
                best = count;
                c2 = symbol;
            }
        }
        if c2 == usize::MAX {
            break;
        }

        freq[c1] += freq[c2];
        freq[c2] = 0;

        let mut walk = c1;
        codesize[walk] += 1;
        while others[walk] != usize::MAX {
            walk = others[walk];
            codesize[walk] += 1;
        }
        others[walk] = c2;

        let mut walk = c2;
        codesize[walk] += 1;
        while others[walk] != usize::MAX {
            walk = others[walk];
            codesize[walk] += 1;
        }
    }

    let mut bits = [0u32; MAX_CODE_LENGTH + 1];
    for &length in codesize.iter() {
        if length != 0 {
            bits[length] += 1;
        }
    }

    // Annex K.2's length-limiting procedure: fold every code longer than
    // sixteen bits back by lengthening a shorter prefix.
    for length in (17..=MAX_CODE_LENGTH).rev() {
        while bits[length] > 0 {
            let mut shorter = length - 2;
            while bits[shorter] == 0 {
                shorter -= 1;
            }
            bits[length] -= 2;
            bits[length - 1] += 1;
            bits[shorter + 1] += 2;
            bits[shorter] -= 1;
        }
    }

    // Drop the pseudo-symbol's code from the longest length still in use.
    // The C loop leaves `i == 16`, so the search starts there and not at
    // `MAX_CODE_LENGTH`.
    let mut longest = 16usize;
    while longest > 0 && bits[longest] == 0 {
        longest -= 1;
    }
    if longest == 0 {
        // Nothing was ever counted; an empty table is still well formed.
        return HuffmanTable::new([0u8; 16], Vec::new());
    }
    bits[longest] -= 1;

    let mut out_bits = [0u8; 16];
    for (slot, target) in out_bits.iter_mut().enumerate() {
        *target = u8::try_from(bits[slot + 1]).map_err(|_| JpegError::InvalidEncodeParameter {
            parameter: "huffman_tables",
            reason: "more than 255 codes of one length",
        })?;
    }

    // The symbol list is sorted by the *pre-adjustment* code lengths. The
    // lengths changed above, but Annex K.2 keeps the ordering, and so does
    // libjpeg with an explicit note that it is not obvious why.
    let mut values = Vec::new();
    for length in 1..=MAX_CODE_LENGTH {
        for (symbol, &assigned) in codesize.iter().enumerate().take(256) {
            if assigned == length {
                values.push(symbol as u8);
            }
        }
    }

    HuffmanTable::new(out_bits, values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES};

    fn annex_k_dc() -> HuffmanTable {
        HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec()).expect("valid")
    }

    #[test]
    fn derived_codes_are_canonical() {
        let derived = DerivedTable::new(&annex_k_dc());
        // Annex K.3.1: one code of two bits (symbol 0), then five of three.
        assert_eq!(derived.lookup(0).expect("symbol 0"), (0b00, 2));
        assert_eq!(derived.lookup(1).expect("symbol 1"), (0b010, 3));
        assert_eq!(derived.lookup(5).expect("symbol 5"), (0b110, 3));
        assert_eq!(derived.lookup(6).expect("symbol 6"), (0b1110, 4));
        assert_eq!(derived.lookup(11).expect("symbol 11"), (0b1_1111_1110, 9));
    }

    #[test]
    fn an_undefined_symbol_is_an_error_not_a_panic() {
        let derived = DerivedTable::new(&annex_k_dc());
        assert!(derived.lookup(12).is_err());
        assert!(derived.lookup(255).is_err());
    }

    #[test]
    fn a_generated_table_round_trips_and_stays_prefix_free() {
        let mut histogram = Histogram::default();
        for (symbol, count) in [(0u8, 100u64), (1, 50), (2, 25), (3, 10), (4, 1)] {
            for _ in 0..count {
                histogram.count(symbol);
            }
        }
        let table = histogram.optimal_table().expect("generated");
        let total: usize = table.bits().iter().map(|&b| usize::from(b)).sum();
        assert_eq!(total, table.values().len());
        assert_eq!(total, 5, "five distinct symbols");
        // The most frequent symbol gets the shortest code.
        let derived = DerivedTable::new(&table);
        let (_, short) = derived.lookup(0).expect("symbol 0");
        let (_, long) = derived.lookup(4).expect("symbol 4");
        assert!(short < long);
    }

    /// The pseudo-symbol exists so that no real symbol gets the all-ones
    /// code. With two symbols the naive tree would give codes `0` and `1`;
    /// libjpeg's gives `0` and `10`.
    #[test]
    fn no_symbol_receives_the_all_ones_code() {
        let mut histogram = Histogram::default();
        for _ in 0..8 {
            histogram.count(0);
        }
        for _ in 0..8 {
            histogram.count(1);
        }
        let table = histogram.optimal_table().expect("generated");
        let derived = DerivedTable::new(&table);
        for symbol in [0u8, 1] {
            let (code, size) = derived.lookup(symbol).expect("defined");
            assert_ne!(code, (1u32 << size) - 1, "symbol {symbol} is all ones");
        }
    }

    /// Ties take the larger symbol index, so with `{0: 1, 1: 1, 2: 2}` the
    /// pseudo-symbol merges with symbol 1 and symbol 0 keeps the shorter
    /// code. Choosing the smaller index instead swaps them. The exact answer
    /// is confirmed against `cjpeg -optimize` in `tests/encode_oracle.rs`;
    /// this pins it without needing the tool.
    #[test]
    fn ties_prefer_the_larger_symbol() {
        let mut histogram = Histogram::default();
        histogram.count(0);
        histogram.count(1);
        histogram.count(2);
        histogram.count(2);
        let table = histogram.optimal_table().expect("generated");
        assert_eq!(table.values(), &[2, 0, 1]);
        assert_eq!(&table.bits()[..3], &[1, 1, 1]);
    }

    /// Equal frequencies give equal lengths, and the all-ones code stays
    /// unused because of the pseudo-symbol.
    #[test]
    fn three_equal_symbols_share_a_length() {
        let mut histogram = Histogram::default();
        for symbol in [0u8, 1, 2] {
            histogram.count(symbol);
        }
        let table = histogram.optimal_table().expect("generated");
        let derived = DerivedTable::new(&table);
        for symbol in 0..3u8 {
            let (code, size) = derived.lookup(symbol).expect("defined");
            assert_eq!(size, 2);
            assert_ne!(code, 0b11);
        }
    }

    #[test]
    fn an_empty_histogram_gives_an_empty_table() {
        let histogram = Histogram::default();
        assert!(histogram.is_empty());
        let table = histogram.optimal_table().expect("empty table");
        assert_eq!(table.values().len(), 0);
        assert_eq!(table.bits(), &[0u8; 16]);
    }

    /// A frequency distribution that would produce codes longer than sixteen
    /// bits must be folded back, and the result must still be a valid table.
    #[test]
    fn very_skewed_frequencies_are_length_limited() {
        let mut counts = Box::new([0u64; SYMBOL_SLOTS]);
        // Fibonacci frequencies force a maximally deep Huffman tree.
        let (mut a, mut b) = (1u64, 1u64);
        for slot in counts.iter_mut().take(40) {
            *slot = a;
            let next = a + b;
            a = b;
            b = next;
        }
        let histogram = Histogram { counts };
        let table = histogram.optimal_table().expect("length limited");
        assert!(table.bits().iter().map(|&b| usize::from(b)).sum::<usize>() == 40);
        let derived = DerivedTable::new(&table);
        for symbol in 0..40u8 {
            let (_, size) = derived.lookup(symbol).expect("defined");
            assert!((1..=16).contains(&size), "symbol {symbol} length {size}");
        }
    }
}
