//! Length-limited **complete** canonical Huffman code construction.
//!
//! `-lh3-` (and the LHa `make_table` routine generally) rejects any code whose
//! lengths do not satisfy the Kraft equality exactly — an *incomplete* table is
//! a hard error there, not merely a wasteful one. The encoder in
//! [`crate::encode`] length-limits by clamping and then re-inflating lengths,
//! which keeps Kraft `<= 1` but can leave it strictly below `1`; that is fine
//! for the lh4-lh7 wire format this crate validated against real archives, and
//! wrong for `-lh3-`.
//!
//! This module therefore builds lengths a different way: an exact Huffman merge
//! (which is always a full binary tree, hence always Kraft-complete), repeated
//! on progressively flattened frequencies until the deepest code fits the
//! format's field width. Flattening halves every non-zero count, so the worst
//! case degenerates to all-ones, whose Huffman tree is balanced at
//! `ceil(log2(n))` — 9 levels for 286 symbols, 7 for 128 — and the loop is
//! guaranteed to terminate well inside any real field width.

/// Huffman code lengths for `freqs`, all `<= max_len`, forming a complete code.
///
/// Symbols with zero frequency get length `0`. Returns `None` when fewer than
/// two symbols are used, because a one-symbol code cannot be complete — callers
/// must emit the format's degenerate single-symbol form instead.
pub(crate) fn complete_code_lengths(freqs: &[u32], max_len: u8) -> Option<Vec<u8>> {
    let used = freqs.iter().filter(|&&f| f > 0).count();
    if used < 2 {
        return None;
    }

    let mut work: Vec<u32> = freqs.to_vec();
    loop {
        let lengths = huffman_lengths(&work);
        if lengths.iter().all(|&len| len <= max_len) {
            return Some(lengths);
        }
        for freq in work.iter_mut() {
            if *freq > 1 {
                *freq = freq.div_ceil(2);
            }
        }
    }
}

/// Exact greedy Huffman merge; the resulting tree is full, so the lengths are
/// Kraft-complete by construction.
fn huffman_lengths(freqs: &[u32]) -> Vec<u8> {
    let mut lengths = vec![0u8; freqs.len()];
    // (frequency, member symbols) sorted ascending; ties broken by insertion
    // order, which keeps the result deterministic across runs.
    let mut nodes: Vec<(u64, Vec<usize>)> = freqs
        .iter()
        .enumerate()
        .filter(|&(_, &freq)| freq > 0)
        .map(|(symbol, &freq)| (u64::from(freq), vec![symbol]))
        .collect();
    if nodes.len() < 2 {
        if let Some((_, symbols)) = nodes.first() {
            for &symbol in symbols {
                lengths[symbol] = 1;
            }
        }
        return lengths;
    }
    nodes.sort_by_key(|&(freq, _)| freq);

    while nodes.len() > 1 {
        let (freq_a, symbols_a) = nodes.remove(0);
        let (freq_b, symbols_b) = nodes.remove(0);
        let merged_freq = freq_a + freq_b;
        let mut merged: Vec<usize> = symbols_a;
        merged.extend(symbols_b);
        for &symbol in &merged {
            lengths[symbol] += 1;
        }
        let position = nodes
            .iter()
            .position(|&(freq, _)| freq > merged_freq)
            .unwrap_or(nodes.len());
        nodes.insert(position, (merged_freq, merged));
    }
    lengths
}

/// Canonical codes for `lengths`, assigned shortest-first and in ascending
/// symbol order within a length — the assignment LHa's `make_table` and this
/// crate's [`crate::huffman::LzhHuffmanTree`] both assume.
pub(crate) fn canonical_codes(lengths: &[u8]) -> Vec<u32> {
    let mut codes = vec![0u32; lengths.len()];
    let max_len = lengths.iter().copied().max().unwrap_or(0);
    if max_len == 0 {
        return codes;
    }
    let mut counts = vec![0u32; usize::from(max_len) + 1];
    for &len in lengths {
        if len > 0 {
            counts[usize::from(len)] += 1;
        }
    }
    let mut next = vec![0u32; usize::from(max_len) + 1];
    let mut code = 0u32;
    for len in 1..=usize::from(max_len) {
        code = (code + counts[len - 1]) << 1;
        next[len] = code;
    }
    for (symbol, &len) in lengths.iter().enumerate() {
        if len > 0 {
            codes[symbol] = next[usize::from(len)];
            next[usize::from(len)] += 1;
        }
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kraft sum scaled by `2^scale_bits`; a complete code sums to exactly
    /// `1 << scale_bits`.
    fn kraft(lengths: &[u8], scale_bits: u32) -> u64 {
        lengths
            .iter()
            .filter(|&&len| len > 0)
            .map(|&len| 1u64 << (scale_bits - u32::from(len)))
            .sum()
    }

    #[test]
    fn produces_a_complete_code_for_a_flat_distribution() {
        let freqs = vec![1u32; 286];
        let lengths = complete_code_lengths(&freqs, 16).expect("two or more symbols");
        assert_eq!(kraft(&lengths, 16), 1 << 16);
        assert!(lengths.iter().all(|&len| (8..=9).contains(&len)));
    }

    #[test]
    fn produces_a_complete_code_for_a_skewed_distribution() {
        // Fibonacci-like counts drive an unlimited Huffman code past 16 bits.
        let mut freqs = vec![0u32; 286];
        let (mut a, mut b) = (1u32, 1u32);
        for slot in freqs.iter_mut().take(40) {
            *slot = a;
            let next = a.saturating_add(b);
            a = b;
            b = next;
        }
        let lengths = complete_code_lengths(&freqs, 16).expect("many symbols");
        assert!(lengths.iter().all(|&len| len <= 16));
        assert_eq!(kraft(&lengths, 16), 1 << 16);
    }

    #[test]
    fn single_symbol_has_no_complete_code() {
        let mut freqs = vec![0u32; 128];
        freqs[7] = 99;
        assert!(complete_code_lengths(&freqs, 15).is_none());
    }

    #[test]
    fn two_symbols_get_one_bit_each() {
        let mut freqs = vec![0u32; 128];
        freqs[3] = 10;
        freqs[9] = 1;
        let lengths = complete_code_lengths(&freqs, 15).expect("two symbols");
        assert_eq!(lengths[3], 1);
        assert_eq!(lengths[9], 1);
        assert_eq!(kraft(&lengths, 15), 1 << 15);
    }

    #[test]
    fn canonical_codes_are_prefix_free_and_ordered() {
        let lengths = vec![2u8, 2, 3, 3, 3, 3];
        let codes = canonical_codes(&lengths);
        assert_eq!(codes, vec![0, 1, 4, 5, 6, 7]);
    }

    #[test]
    fn canonical_codes_handle_unused_symbols() {
        let lengths = vec![0u8, 1, 0, 1];
        let codes = canonical_codes(&lengths);
        assert_eq!(codes[1], 0);
        assert_eq!(codes[3], 1);
    }
}
