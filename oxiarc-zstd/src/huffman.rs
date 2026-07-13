//! Huffman coding for Zstandard literals.
//!
//! Zstandard uses canonical Huffman coding for literal compression.
//! The decoding table layout, weight validation and the implied-last-weight
//! deduction mirror the reference `HUF_readStats` / `HUF_readDTableX1`
//! (RFC 8878 §4.2.1).

use crate::fse::{FseBitReader, FseDecoder, read_fse_table_description};
use oxiarc_core::error::{OxiArcError, Result};

/// Maximum Huffman code length the encoder may use (RFC 8878).
pub const MAX_CODE_LENGTH: u8 = 11;

/// Maximum table log accepted when decoding (reference `HUF_TABLELOG_MAX`).
const MAX_TABLE_LOG: u8 = 12;

/// Maximum number of symbols (byte values).
pub const MAX_SYMBOLS: usize = 256;

/// Maximum accuracy log for the FSE table compressing Huffman weights.
const WEIGHTS_MAX_ACCURACY_LOG: u8 = 6;

/// Huffman decoding table entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct HuffmanEntry {
    /// Decoded symbol.
    pub symbol: u8,
    /// Number of bits for this code.
    pub num_bits: u8,
}

/// Huffman decoding table.
///
/// Indexed directly by the next `table_log` bits of the (backward) literals
/// bitstream, exactly like the reference single-symbol `DTable`.
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    /// Decoding entries indexed by bit prefix.
    entries: Vec<HuffmanEntry>,
    /// Table log (all lookups peek this many bits).
    max_bits: u8,
}

impl HuffmanTable {
    /// Build a Huffman decoding table from *stored* weights.
    ///
    /// `stored_weights` holds the weights for symbols `0..n`; the weight for
    /// symbol `n` is *implied* and deduced here per RFC 8878 §4.2.1.1: the
    /// total weight is completed to the next power of two, and the remainder
    /// must itself be a power of two.
    pub fn from_stored_weights(stored_weights: &[u8]) -> Result<Self> {
        if stored_weights.is_empty() {
            return Err(OxiArcError::corrupted(0, "empty Huffman weights"));
        }
        if stored_weights.len() >= MAX_SYMBOLS {
            return Err(OxiArcError::corrupted(0, "too many Huffman weights"));
        }

        // Sum stored weights; every weight contributes 2^(w-1) when w > 0.
        let mut total_weight: u64 = 0;
        let mut rank_stats = [0u32; (MAX_TABLE_LOG + 1) as usize];
        for &w in stored_weights {
            if w > MAX_TABLE_LOG {
                return Err(OxiArcError::corrupted(
                    0,
                    format!("Huffman weight {} exceeds maximum {}", w, MAX_TABLE_LOG),
                ));
            }
            if w > 0 {
                total_weight += 1u64 << (w - 1);
                rank_stats[w as usize] += 1;
            }
        }
        if total_weight == 0 {
            return Err(OxiArcError::corrupted(0, "all Huffman weights are zero"));
        }

        // Deduce table log and the implied last weight.
        let table_log = (64 - total_weight.leading_zeros()) as u8; // highbit + 1
        if table_log > MAX_TABLE_LOG {
            return Err(OxiArcError::corrupted(
                0,
                format!("Huffman table log {} exceeds maximum", table_log),
            ));
        }
        let table_size = 1u64 << table_log;
        let rest = table_size - total_weight;
        if rest == 0 || !rest.is_power_of_two() {
            return Err(OxiArcError::corrupted(
                0,
                "Huffman weights do not complete to a power of two",
            ));
        }
        let last_weight = (63 - rest.leading_zeros()) as u8 + 1; // highbit + 1
        rank_stats[last_weight as usize] += 1;

        // Tree validity: the number of weight-1 (longest-code) symbols must be
        // at least 2 and even (reference `HUF_readStats` check).
        if rank_stats[1] < 2 || (rank_stats[1] & 1) != 0 {
            return Err(OxiArcError::corrupted(
                0,
                "invalid Huffman tree (weight-1 symbol count must be even and >= 2)",
            ));
        }

        // Complete weight list: stored weights plus the implied last symbol.
        let num_symbols = stored_weights.len() + 1;

        // Compute each weight rank's starting position in the table
        // (weight-1 symbols occupy the lowest indices).
        let mut rank_start = [0usize; (MAX_TABLE_LOG + 2) as usize];
        let mut next_start = 0usize;
        for w in 1..=(table_log as usize) {
            rank_start[w] = next_start;
            next_start += (rank_stats[w] as usize) << (w - 1);
        }
        if next_start != table_size as usize {
            return Err(OxiArcError::corrupted(
                0,
                "Huffman rank layout does not fill the table",
            ));
        }

        // Fill the decode table in natural symbol order.
        let mut entries = vec![HuffmanEntry::default(); table_size as usize];
        for symbol in 0..num_symbols {
            let w = if symbol < stored_weights.len() {
                stored_weights[symbol]
            } else {
                last_weight
            };
            if w == 0 {
                continue;
            }
            let length = 1usize << (w - 1);
            let start = rank_start[w as usize];
            let end = start + length;
            if end > entries.len() {
                return Err(OxiArcError::corrupted(
                    0,
                    "Huffman table fill exceeds table size",
                ));
            }
            let num_bits = table_log + 1 - w;
            for entry in &mut entries[start..end] {
                *entry = HuffmanEntry {
                    symbol: symbol as u8,
                    num_bits,
                };
            }
            rank_start[w as usize] = end;
        }

        Ok(Self {
            entries,
            max_bits: table_log,
        })
    }

    /// Look up the entry for a `max_bits`-bit prefix, bounds-checked.
    #[inline]
    pub fn entry(&self, prefix: usize) -> Result<&HuffmanEntry> {
        self.entries.get(prefix).ok_or_else(|| {
            OxiArcError::corrupted(
                0,
                format!(
                    "Huffman prefix {} out of range (table size {})",
                    prefix,
                    self.entries.len()
                ),
            )
        })
    }

    /// Get the table log (bits peeked per lookup) for this table.
    pub fn max_bits(&self) -> u8 {
        self.max_bits
    }
}

/// Read a Huffman table description from compressed literals content.
///
/// Returns the decoding table and the number of bytes consumed.
pub fn read_huffman_table(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    if data.is_empty() {
        return Err(OxiArcError::corrupted(0, "empty Huffman table data"));
    }

    let header = data[0];

    if header < 128 {
        // FSE-compressed weights
        read_huffman_table_fse(data)
    } else {
        // Direct representation
        read_huffman_table_direct(data)
    }
}

/// Read Huffman table with direct 4-bit weights.
///
/// `header - 127` weights are stored (4 bits each, high nibble first); the
/// final symbol's weight is implied and reconstructed by
/// [`HuffmanTable::from_stored_weights`].
fn read_huffman_table_direct(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    let header = data[0];
    let num_weights = (header - 127) as usize;

    if num_weights == 0 || num_weights >= MAX_SYMBOLS {
        return Err(OxiArcError::corrupted(
            0,
            format!("invalid number of Huffman weights: {}", num_weights),
        ));
    }

    let bytes_needed = num_weights.div_ceil(2);
    if data.len() < 1 + bytes_needed {
        return Err(OxiArcError::corrupted(0, "truncated Huffman table"));
    }

    let mut weights = vec![0u8; num_weights];

    for (i, weight) in weights.iter_mut().enumerate() {
        let byte_idx = 1 + i / 2;
        let is_high = i % 2 == 0;

        *weight = if is_high {
            data[byte_idx] >> 4
        } else {
            data[byte_idx] & 0x0F
        };
    }

    let table = HuffmanTable::from_stored_weights(&weights)?;
    Ok((table, 1 + bytes_needed))
}

/// Read Huffman table with FSE-compressed weights.
///
/// Weights are decoded with **two interleaved FSE states** exactly like the
/// reference `FSE_decompress`: symbols alternate between the states, and
/// decoding stops when a state-transition read consumes past the start of the
/// backward bitstream, at which point the other state contributes the final
/// weight.
fn read_huffman_table_fse(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    let compressed_size = data[0] as usize;

    if compressed_size == 0 {
        return Err(OxiArcError::corrupted(0, "zero-length FSE Huffman table"));
    }

    if data.len() < 1 + compressed_size {
        return Err(OxiArcError::corrupted(0, "truncated FSE Huffman table"));
    }

    let fse_data = &data[1..1 + compressed_size];

    // Read FSE table for weight values (max symbol 12, accuracy log <= 6).
    let (fse_table, fse_bytes) =
        read_fse_table_description(fse_data, MAX_TABLE_LOG, WEIGHTS_MAX_ACCURACY_LOG)?;

    // Decode weights using two interleaved FSE states.
    let bitstream_data = &fse_data[fse_bytes..];
    let mut reader = FseBitReader::new(bitstream_data)?;
    let mut even = FseDecoder::new(&fse_table, &mut reader);
    let mut odd = FseDecoder::new(&fse_table, &mut reader);
    if reader.is_overflowed() {
        return Err(OxiArcError::corrupted(
            0,
            "FSE weight bitstream too short for initial states",
        ));
    }

    let mut weights: Vec<u8> = Vec::with_capacity(64);
    let max_weights = MAX_SYMBOLS - 1; // at most 255 stored weights
    loop {
        if weights.len() >= max_weights {
            return Err(OxiArcError::corrupted(0, "too many Huffman weights"));
        }
        weights.push(even.decode(&mut reader)?);
        if reader.is_overflowed() {
            weights.push(odd.peek()?);
            break;
        }

        if weights.len() >= max_weights {
            return Err(OxiArcError::corrupted(0, "too many Huffman weights"));
        }
        weights.push(odd.decode(&mut reader)?);
        if reader.is_overflowed() {
            weights.push(even.peek()?);
            break;
        }
    }
    if weights.len() > max_weights {
        return Err(OxiArcError::corrupted(0, "too many Huffman weights"));
    }

    let table = HuffmanTable::from_stored_weights(&weights)?;
    Ok((table, 1 + compressed_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_huffman_table_from_weights() {
        // One stored weight + implied last: two symbols, 1 bit each.
        let weights = [1u8];
        let table = HuffmanTable::from_stored_weights(&weights).expect("valid huffman table");
        assert_eq!(table.max_bits(), 1);
        // prefix 0 -> symbol 0, prefix 1 -> symbol 1 (implied).
        assert_eq!(table.entry(0).expect("entry").symbol, 0);
        assert_eq!(table.entry(1).expect("entry").symbol, 1);
    }

    #[test]
    fn test_huffman_table_varying_weights() {
        // Stored weights [3, 1, 1]: total = 4+1+1 = 6, table log = 3,
        // rest = 8-6 = 2 -> implied last weight 2. Rank-1 count = 2 (even).
        let weights = [3u8, 1, 1];
        let table = HuffmanTable::from_stored_weights(&weights).expect("valid huffman table");
        assert_eq!(table.max_bits(), 3);
        // Weight-1 symbols occupy the lowest indices, in natural order.
        assert_eq!(table.entry(0).expect("entry").symbol, 1);
        assert_eq!(table.entry(0).expect("entry").num_bits, 3);
        assert_eq!(table.entry(1).expect("entry").symbol, 2);
        // Implied symbol 3 (weight 2) follows, then symbol 0 (weight 3).
        assert_eq!(table.entry(2).expect("entry").symbol, 3);
        assert_eq!(table.entry(7).expect("entry").symbol, 0);
    }

    #[test]
    fn test_direct_huffman_table() {
        // 3 stored weights (header = 130): 3, 1, 1 -> implied 4th weight 2.
        let data = vec![127 + 3, 0x31, 0x10];
        let (table, consumed) = read_huffman_table(&data).expect("valid huffman table");

        assert_eq!(consumed, 3);
        assert_eq!(table.max_bits(), 3);
    }

    #[test]
    fn test_invalid_weights_rejected() {
        // Empty weights.
        assert!(HuffmanTable::from_stored_weights(&[]).is_err());
        // All zero.
        assert!(HuffmanTable::from_stored_weights(&[0, 0, 0]).is_err());
        // Weight exceeds the maximum table log.
        assert!(HuffmanTable::from_stored_weights(&[13, 1]).is_err());
        // Non-Kraft-completable set: total 4 (power of two) leaves rest 4? No:
        // total = 2^(3-1)=4, table log 3, rest = 4 -> power of two -> valid.
        // But total = 3 (weights [2,1]) -> table log 2, rest = 1 -> last weight 1,
        // rank1 = 2 -> valid. Use weights that leave rest = 0:
        // impossible by construction; instead check an odd weight-1 count:
        // weights [1,1,1] -> total 3, log 2, rest 1 -> last weight 1 ->
        // rank1 = 4 (even, ok). Try [2,2] -> total 4, log 3, rest 4 ->
        // last weight 3 -> rank1 = 0 -> invalid (must be >= 2).
        assert!(HuffmanTable::from_stored_weights(&[2, 2]).is_err());
    }

    #[test]
    fn test_direct_table_truncated() {
        // Header says 10 weights but only 1 nibble byte present.
        let data = vec![127 + 10, 0x21];
        assert!(read_huffman_table(&data).is_err());
    }
}
