//! FSE table construction for the Zstandard *encoder* (RFC 8878 §4.1.1).
//!
//! A compressed block's Sequences Section describes each of its three symbol
//! streams (literal lengths, offsets, match lengths) with one of four modes.
//! `Predefined` and `RLE` need no table description at all, which is why they
//! are the easy modes to emit — and why an encoder that only emits those is
//! RFC-valid but leaves compression ratio on the table whenever the block's
//! real symbol distribution differs from the RFC's fixed one.
//!
//! This module supplies the two pieces needed for `FSE_Compressed_Mode`:
//!
//! 1. [`normalize_counts`] — turn raw symbol frequencies into a *normalized*
//!    distribution whose entries sum to exactly `1 << table_log`, which is what
//!    an FSE table is built from.
//! 2. [`write_ncount`] — serialize that distribution into the bit-packed
//!    "FSE Table Description" the decoder parses back.
//!
//! Both are faithful ports of the reference implementation
//! (`FSE_normalizeCount` / `FSE_writeNCount` in `zstd/lib/common/fse.h` and
//! `entropy_common.c`), including the `FSE_normalizeM2` fallback, because the
//! bit-level format has no slack: a distribution that sums to the wrong total,
//! or a header written with an off-by-one threshold, produces a stream that
//! `zstd -d` rejects outright.
//!
//! [`estimate_encoded_bits`] rounds this out with the cost model the encoder
//! uses to decide whether a custom table is actually *worth* its header bytes.

use oxiarc_core::error::{OxiArcError, Result};

/// Smallest accuracy log the format permits (`FSE_MIN_TABLELOG`).
pub(crate) const MIN_TABLE_LOG: u8 = 5;

/// Index of the most significant set bit (`BIT_highbit32`).
///
/// Returns 0 for an input of 0, matching the reference's behaviour on the
/// paths where it is called with a guaranteed-nonzero value.
fn highest_bit(value: u32) -> u32 {
    if value == 0 {
        0
    } else {
        31 - value.leading_zeros()
    }
}

/// Reference `FSE_minTableLog`: the smallest accuracy log that can still
/// represent every symbol value in the alphabet.
fn min_table_log(src_size: usize, max_symbol: u8) -> u8 {
    let min_bits_src = highest_bit(src_size as u32) + 1;
    let min_bits_symbols = highest_bit(u32::from(max_symbol)) + 2;
    min_bits_src.min(min_bits_symbols) as u8
}

/// Reference `FSE_optimalTableLog`: pick an accuracy log for `src_size`
/// symbols drawn from an alphabet of `0..=max_symbol`.
///
/// A larger table models the distribution more precisely but costs more header
/// bytes and more bits per state transition, so the reference caps it by the
/// input size (`maxBitsSrc`) as well as by the per-table format maximum.
pub(crate) fn optimal_table_log(max_table_log: u8, src_size: usize, max_symbol: u8) -> u8 {
    debug_assert!(src_size > 1, "RLE mode handles single-symbol inputs");
    let max_bits_src = highest_bit(src_size.saturating_sub(1) as u32).saturating_sub(2) as u8;
    let mut table_log = max_table_log;
    let min_bits = min_table_log(src_size, max_symbol);
    if max_bits_src < table_log {
        table_log = max_bits_src;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(MIN_TABLE_LOG, max_table_log)
}

/// Rounding thresholds from the reference `FSE_normalizeCount` (`rtbTable`).
///
/// For small probabilities the plain truncated scaling loses too much mass;
/// these are the fractional cut-offs above which a probability is rounded up
/// instead of down.
const RTB_TABLE: [u64; 8] = [
    0, 473_195, 504_333, 520_860, 550_000, 700_000, 750_000, 830_000,
];

/// Normalize raw `frequencies` into a distribution summing to `1 << table_log`.
///
/// `low_prob_count` is the value assigned to symbols too rare for a full slot:
/// `1` for a plain single slot, or `-1` for the format's "less than one"
/// marker (which the decoder places at the top of the state table). The
/// reference picks `-1` only for blocks with many sequences, where the extra
/// precision pays for itself.
///
/// # Errors
///
/// Returns [`OxiArcError::EncodingError`] when the frequencies cannot be
/// normalized at the requested accuracy — for example when the alphabet has
/// more distinct symbols than the table has slots. Callers treat that as
/// "custom table not usable" and fall back to a predefined table rather than
/// emitting an invalid description.
pub(crate) fn normalize_counts(
    frequencies: &[u32],
    total: u32,
    table_log: u8,
    low_prob_count: i16,
) -> Result<Vec<i16>> {
    if total == 0 {
        return Err(OxiArcError::encoding_error(
            "cannot normalize an empty symbol distribution",
        ));
    }
    if !(MIN_TABLE_LOG..=crate::fse::MAX_ACCURACY_LOG).contains(&table_log) {
        return Err(OxiArcError::encoding_error(format!(
            "FSE accuracy log {table_log} outside the representable range"
        )));
    }
    let table_size = 1i32 << table_log;
    if (frequencies.iter().filter(|&&f| f > 0).count() as i32) > table_size {
        return Err(OxiArcError::encoding_error(
            "more distinct symbols than FSE table slots",
        ));
    }

    let mut norm = vec![0i16; frequencies.len()];
    let total64 = u64::from(total);
    let scale = 62 - u32::from(table_log);
    let step = (1u64 << 62) / total64;
    let v_step = 1u64 << (scale - 20);
    let low_threshold = total >> table_log;

    let mut still_to_distribute = table_size;
    let mut largest = 0usize;
    let mut largest_p = 0i16;

    for (symbol, &count) in frequencies.iter().enumerate() {
        if count == total {
            return Err(OxiArcError::encoding_error(
                "single-symbol distribution must use RLE mode",
            ));
        }
        if count == 0 {
            continue;
        }
        if count <= low_threshold {
            norm[symbol] = low_prob_count;
            still_to_distribute -= 1;
            continue;
        }
        let scaled = u64::from(count) * step;
        let mut proba = (scaled >> scale) as i16;
        if proba < 8 {
            // Round up when the discarded fraction exceeds the reference's
            // per-probability threshold. `proba` is in 0..8 here, so the index
            // is always inside `RTB_TABLE`.
            let rest_to_beat = v_step * RTB_TABLE[proba as usize];
            if scaled - ((proba as u64) << scale) > rest_to_beat {
                proba += 1;
            }
        }
        if proba > largest_p {
            largest_p = proba;
            largest = symbol;
        }
        norm[symbol] = proba;
        still_to_distribute -= i32::from(proba);
    }

    if -still_to_distribute >= i32::from(norm[largest] >> 1) {
        // Handing the whole deficit to the most probable symbol would halve it
        // (or worse); the reference switches to a second, slower method that
        // re-derives every weight from the remaining mass.
        normalize_m2(&mut norm, frequencies, total, table_log, low_prob_count)?;
    } else {
        norm[largest] += still_to_distribute as i16;
    }

    validate_normalized(&norm, frequencies, table_size)?;
    Ok(norm)
}

/// Reference `FSE_normalizeM2`: the fallback used when proportional scaling
/// leaves too large a deficit to absorb into the most probable symbol.
fn normalize_m2(
    norm: &mut [i16],
    frequencies: &[u32],
    total: u32,
    table_log: u8,
    low_prob_count: i16,
) -> Result<()> {
    /// Sentinel for "weight not decided yet" (the reference uses -2, which can
    /// never be a legal normalized count).
    const NOT_YET_ASSIGNED: i16 = -2;

    let mut remaining_total = u64::from(total);
    let mut distributed = 0i32;
    let low_threshold = total >> table_log;
    let mut low_one = ((u64::from(total) * 3) >> (u32::from(table_log) + 1)) as u32;

    for (symbol, &count) in frequencies.iter().enumerate() {
        if count == 0 {
            norm[symbol] = 0;
            continue;
        }
        if count <= low_threshold {
            norm[symbol] = low_prob_count;
            distributed += 1;
            remaining_total -= u64::from(count);
            continue;
        }
        if count <= low_one {
            norm[symbol] = 1;
            distributed += 1;
            remaining_total -= u64::from(count);
            continue;
        }
        norm[symbol] = NOT_YET_ASSIGNED;
    }

    let table_size = 1i32 << table_log;
    let mut to_distribute = table_size - distributed;
    if to_distribute == 0 {
        return validate_normalized(norm, frequencies, table_size);
    }

    if to_distribute > 0 && remaining_total / (to_distribute as u64) > u64::from(low_one) {
        // Risk of scaling a symbol down to zero: re-classify more symbols as
        // single-slot before the proportional pass.
        low_one = ((remaining_total * 3) / (to_distribute as u64 * 2)) as u32;
        for (symbol, &count) in frequencies.iter().enumerate() {
            if norm[symbol] == NOT_YET_ASSIGNED && count <= low_one {
                norm[symbol] = 1;
                distributed += 1;
                remaining_total -= u64::from(count);
            }
        }
        to_distribute = table_size - distributed;
    }

    if distributed as usize == frequencies.len() {
        // Every symbol was rare: give the leftover slots to the most frequent
        // one (the reference's "probably incompressible" branch).
        let mut max_symbol = 0usize;
        let mut max_count = 0u32;
        for (symbol, &count) in frequencies.iter().enumerate() {
            if count > max_count {
                max_count = count;
                max_symbol = symbol;
            }
        }
        norm[max_symbol] += to_distribute as i16;
        return validate_normalized(norm, frequencies, table_size);
    }

    if remaining_total == 0 {
        // All mass was consumed by the low-probability classes; hand the
        // remaining slots out round-robin to symbols that already have one.
        let mut symbol = 0usize;
        while to_distribute > 0 {
            if norm[symbol] > 0 {
                norm[symbol] += 1;
                to_distribute -= 1;
            }
            symbol = (symbol + 1) % frequencies.len();
        }
        return validate_normalized(norm, frequencies, table_size);
    }

    let v_step_log = 62 - u32::from(table_log);
    let mid = (1u64 << (v_step_log - 1)) - 1;
    let r_step = ((1u64 << v_step_log) * to_distribute as u64 + mid) / remaining_total;
    let mut running = mid;
    for (symbol, &count) in frequencies.iter().enumerate() {
        if norm[symbol] != NOT_YET_ASSIGNED {
            continue;
        }
        let end = running + u64::from(count) * r_step;
        let weight = (end >> v_step_log) - (running >> v_step_log);
        if weight < 1 {
            return Err(OxiArcError::encoding_error(
                "FSE normalization assigned a zero weight to a used symbol",
            ));
        }
        norm[symbol] = weight as i16;
        running = end;
    }

    validate_normalized(norm, frequencies, table_size)
}

/// Check the two invariants an FSE table builder relies on: the counts sum to
/// the table size, and no symbol that actually occurs was given zero slots.
fn validate_normalized(norm: &[i16], frequencies: &[u32], table_size: i32) -> Result<()> {
    let mut sum = 0i32;
    for (symbol, &probability) in norm.iter().enumerate() {
        if probability < -1 {
            return Err(OxiArcError::encoding_error(
                "FSE normalization produced an out-of-range count",
            ));
        }
        if frequencies[symbol] > 0 && probability == 0 {
            return Err(OxiArcError::encoding_error(
                "FSE normalization dropped a symbol that occurs in the block",
            ));
        }
        if frequencies[symbol] == 0 && probability != 0 {
            return Err(OxiArcError::encoding_error(
                "FSE normalization allocated slots to an unused symbol",
            ));
        }
        sum += if probability == -1 {
            1
        } else {
            i32::from(probability)
        };
    }
    if sum != table_size {
        return Err(OxiArcError::encoding_error(format!(
            "FSE normalized counts sum to {sum}, expected {table_size}"
        )));
    }
    Ok(())
}

/// Serialize a normalized distribution as an RFC 8878 §4.1.1 FSE Table
/// Description.
///
/// This is a port of `FSE_writeNCount_generic`. The encoding is a forward,
/// little-endian bit stream: 4 bits of `Accuracy_Log - 5`, then one
/// variable-width field per symbol whose width shrinks as the remaining
/// probability mass shrinks, with runs of zero-probability symbols coded as
/// 2-bit repeat flags.
///
/// # Errors
///
/// Returns [`OxiArcError::EncodingError`] if `table_log` is out of range or the
/// distribution does not sum to `1 << table_log` (which would make the header
/// undecodable).
pub(crate) fn write_ncount(norm: &[i16], table_log: u8) -> Result<Vec<u8>> {
    if !(MIN_TABLE_LOG..=crate::fse::MAX_ACCURACY_LOG).contains(&table_log) {
        return Err(OxiArcError::encoding_error(format!(
            "FSE accuracy log {table_log} outside the representable range"
        )));
    }
    let table_size = 1i32 << table_log;

    let mut out: Vec<u8> = Vec::new();
    let mut bit_stream: u32 = 0;
    let mut bit_count: u32 = 0;

    // Header: Accuracy_Log - 5, four bits.
    bit_stream |= u32::from(table_log - MIN_TABLE_LOG) << bit_count;
    bit_count += 4;

    // `remaining` carries one extra unit of accuracy, exactly as the reference
    // does, so that the "value + 1" encoding below stays in range.
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nb_bits = u32::from(table_log) + 1;
    let mut symbol = 0usize;
    let mut previous_is_zero = false;
    let alphabet = norm.len();

    while symbol < alphabet && remaining > 1 {
        if previous_is_zero {
            // Skip a run of zero-probability symbols, coding its length in
            // base-3 chunks of two bits (3 = "another chunk follows"), with a
            // 24-symbol fast path that emits eight consecutive 3s at once.
            let mut start = symbol;
            while symbol < alphabet && norm[symbol] == 0 {
                symbol += 1;
            }
            if symbol == alphabet {
                break;
            }
            while symbol >= start + 24 {
                start += 24;
                bit_stream |= 0xFFFFu32 << bit_count;
                out.push(bit_stream as u8);
                out.push((bit_stream >> 8) as u8);
                bit_stream >>= 16;
            }
            while symbol >= start + 3 {
                start += 3;
                bit_stream |= 3u32 << bit_count;
                bit_count += 2;
            }
            bit_stream |= ((symbol - start) as u32) << bit_count;
            bit_count += 2;
            if bit_count > 16 {
                out.push(bit_stream as u8);
                out.push((bit_stream >> 8) as u8);
                bit_stream >>= 16;
                bit_count -= 16;
            }
            // The reference clears `previousIs0` here; this port does not need
            // to, because the symbol block below assigns it unconditionally
            // before the next iteration reads it.
        }

        let probability = norm[symbol];
        symbol += 1;
        let max = (2 * threshold - 1) - remaining;
        remaining -= i32::from(probability.abs());
        // "+1 for extra accuracy": the wire value is probability + 1, so the
        // "less than one" marker (-1) encodes as 0 and probability 0 as 1.
        let mut value = i32::from(probability) + 1;
        if value >= threshold {
            value += max;
        }
        bit_stream |= (value as u32) << bit_count;
        bit_count += nb_bits;
        if value < max {
            bit_count -= 1;
        }
        previous_is_zero = value == 1;
        if remaining < 1 {
            return Err(OxiArcError::encoding_error(
                "FSE table description overran the table size",
            ));
        }
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }

        if bit_count > 16 {
            out.push(bit_stream as u8);
            out.push((bit_stream >> 8) as u8);
            bit_stream >>= 16;
            bit_count -= 16;
        }
    }

    if remaining != 1 {
        return Err(OxiArcError::encoding_error(format!(
            "FSE table description ended with {remaining} units undistributed"
        )));
    }

    // Flush: write two bytes, but only advance by the bits actually used.
    out.push(bit_stream as u8);
    out.push((bit_stream >> 8) as u8);
    out.truncate(out.len() - 2 + (bit_count as usize).div_ceil(8));

    Ok(out)
}

/// Estimate, in bits, what it costs to encode `frequencies` with the FSE table
/// described by `norm` at `table_log`.
///
/// An FSE state that holds `p` of the table's `1 << table_log` slots spends
/// about `log2(table_size / p)` bits per occurrence, so the total is
/// `sum(freq[s] * log2(table_size / norm[s]))`. This is the same cost model the
/// reference uses in `ZSTD_fseBitCost`, and it is what lets the encoder decide
/// honestly whether a custom table earns back its header bytes.
///
/// Returns `None` when `norm` cannot encode the given frequencies at all (a
/// symbol occurs but has no slots), so the caller must not use that table.
pub(crate) fn estimate_encoded_bits(
    frequencies: &[u32],
    norm: &[i16],
    table_log: u8,
) -> Option<f64> {
    let table_size = f64::from(1u32 << table_log);
    let mut bits = 0.0f64;
    for (symbol, &count) in frequencies.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let probability = *norm.get(symbol)?;
        let slots = if probability == -1 {
            // A "less than one" symbol behaves like a single slot but costs the
            // full accuracy log, since its state always spans the whole table.
            1.0
        } else if probability > 0 {
            f64::from(probability)
        } else {
            return None;
        };
        bits += f64::from(count) * (table_size / slots).log2();
    }
    Some(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fse::read_ncount;

    /// Round the description through the crate's own reference-verified parser.
    ///
    /// The parser in `fse.rs` is the code path that decodes real `zstd` output
    /// (64/64 reference frames), so agreeing with it is a meaningful check —
    /// unlike agreeing with a second writer we also wrote.
    fn parse_back(bytes: &[u8], max_symbol: u8, max_log: u8) -> (Vec<i16>, u8, usize) {
        read_ncount(bytes, max_symbol, max_log).expect("written FSE table description must parse")
    }

    /// The RFC 8878 predefined literal-length distribution must survive a
    /// write/parse round trip — including its four trailing `-1` entries.
    #[test]
    fn writes_predefined_ll_distribution() {
        let norm: Vec<i16> = vec![
            4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
            1, 1, 1, -1, -1, -1, -1,
        ];
        let bytes = write_ncount(&norm, 6).expect("predefined LL distribution is writable");
        let (parsed, log, consumed) = parse_back(&bytes, 35, 9);
        assert_eq!(log, 6);
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, norm);
    }

    /// The predefined offset distribution uses accuracy log 5, the format
    /// minimum, which exercises the `Accuracy_Log - 5 == 0` header value.
    #[test]
    fn writes_predefined_offset_distribution() {
        let norm: Vec<i16> = vec![
            1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1,
            -1,
        ];
        let bytes = write_ncount(&norm, 5).expect("predefined OF distribution is writable");
        let (parsed, log, _) = parse_back(&bytes, 31, 8);
        assert_eq!(log, 5);
        assert_eq!(parsed, norm);
    }

    /// A distribution with long interior runs of zeros exercises both the
    /// 3-symbol and the 24-symbol repeat-flag paths of the writer.
    #[test]
    fn writes_long_zero_runs() {
        let mut norm = vec![0i16; 53];
        norm[0] = 30;
        norm[1] = 20;
        norm[40] = 8;
        norm[52] = 6;
        let bytes = write_ncount(&norm, 6).expect("sparse distribution is writable");
        let (mut parsed, log, _) = parse_back(&bytes, 52, 9);
        assert_eq!(log, 6);
        parsed.resize(norm.len(), 0);
        assert_eq!(parsed, norm);
    }

    /// A leading zero-probability symbol is the trickiest repeat-flag case:
    /// the run starts before any symbol has been written.
    #[test]
    fn writes_leading_zero_run() {
        let mut norm = vec![0i16; 36];
        norm[5] = 40;
        norm[6] = 16;
        norm[35] = 8;
        let bytes = write_ncount(&norm, 6).expect("leading-zero distribution is writable");
        let (mut parsed, _, _) = parse_back(&bytes, 35, 9);
        parsed.resize(norm.len(), 0);
        assert_eq!(parsed, norm);
    }

    /// Every accuracy log the format allows must round-trip.
    #[test]
    fn writes_every_accuracy_log() {
        for table_log in MIN_TABLE_LOG..=crate::fse::MAX_ACCURACY_LOG {
            let table_size = 1i16 << table_log;
            let norm = vec![table_size - 3, 1, 1, 1];
            let bytes = write_ncount(&norm, table_log)
                .unwrap_or_else(|e| panic!("log {table_log} must be writable: {e}"));
            let (mut parsed, log, _) = parse_back(&bytes, 3, crate::fse::MAX_ACCURACY_LOG);
            assert_eq!(log, table_log);
            parsed.resize(norm.len(), 0);
            assert_eq!(parsed, norm, "mismatch at accuracy log {table_log}");
        }
    }

    /// Normalization must always produce something the table builder accepts:
    /// the counts sum to the table size and every observed symbol keeps a slot.
    #[test]
    fn normalizes_skewed_distribution() {
        let frequencies = [1000u32, 500, 1, 1, 1, 0, 0, 3];
        let total: u32 = frequencies.iter().sum();
        for low_prob in [1i16, -1] {
            let norm = normalize_counts(&frequencies, total, 6, low_prob)
                .expect("skewed distribution normalizes");
            let sum: i32 = norm
                .iter()
                .map(|&p| if p == -1 { 1 } else { i32::from(p) })
                .sum();
            assert_eq!(sum, 64, "low_prob={low_prob}");
            for (symbol, &count) in frequencies.iter().enumerate() {
                if count > 0 {
                    assert_ne!(norm[symbol], 0, "symbol {symbol} lost its slot");
                } else {
                    assert_eq!(norm[symbol], 0, "unused symbol {symbol} got slots");
                }
            }
            let bytes = write_ncount(&norm, 6).expect("normalized table is writable");
            let (mut parsed, _, _) = parse_back(&bytes, 7, 9);
            parsed.resize(norm.len(), 0);
            assert_eq!(parsed, norm);
        }
    }

    /// A near-uniform distribution over many symbols forces the deficit into
    /// the `FSE_normalizeM2` fallback rather than the fast path.
    #[test]
    fn normalizes_uniform_wide_alphabet() {
        let frequencies: Vec<u32> = (0..52).map(|i| 7 + (i % 3) as u32).collect();
        let total: u32 = frequencies.iter().sum();
        let norm =
            normalize_counts(&frequencies, total, 6, 1).expect("uniform distribution normalizes");
        let sum: i32 = norm
            .iter()
            .map(|&p| if p == -1 { 1 } else { i32::from(p) })
            .sum();
        assert_eq!(sum, 64);
        let bytes = write_ncount(&norm, 6).expect("normalized table is writable");
        let (mut parsed, _, _) = parse_back(&bytes, 51, 9);
        parsed.resize(norm.len(), 0);
        assert_eq!(parsed, norm);
    }

    /// A single-symbol distribution has no FSE representation; the caller must
    /// be told to use RLE mode instead of getting a silently broken table.
    #[test]
    fn rejects_single_symbol_distribution() {
        let frequencies = [0u32, 0, 42, 0];
        let err = normalize_counts(&frequencies, 42, 6, 1)
            .expect_err("single-symbol input must be rejected");
        assert!(
            format!("{err}").contains("RLE"),
            "error should point at RLE mode, got: {err}"
        );
    }

    /// More distinct symbols than table slots cannot be represented.
    #[test]
    fn rejects_alphabet_larger_than_table() {
        let frequencies: Vec<u32> = (0..40).map(|_| 1u32).collect();
        let total: u32 = frequencies.iter().sum();
        assert!(normalize_counts(&frequencies, total, 5, 1).is_err());
    }

    /// The accuracy-log heuristic must stay inside the format's bounds and
    /// grow with the amount of data being modelled.
    #[test]
    fn optimal_table_log_is_bounded_and_monotonic() {
        let small = optimal_table_log(9, 4, 35);
        let large = optimal_table_log(9, 100_000, 35);
        assert!((MIN_TABLE_LOG..=9).contains(&small), "small log {small}");
        assert!((MIN_TABLE_LOG..=9).contains(&large), "large log {large}");
        assert!(small <= large);
        assert!(optimal_table_log(8, 100_000, 28) <= 8);
    }

    /// The cost model must prefer a table that matches the data over one that
    /// does not, and must refuse a table with no slots for an observed symbol.
    #[test]
    fn cost_model_prefers_matching_table() {
        let frequencies = [90u32, 5, 5, 0];
        let matching = [58i16, 3, 3, 0];
        let uniform = [16i16, 16, 16, 16];
        let matching_bits =
            estimate_encoded_bits(&frequencies, &matching, 6).expect("matching table is usable");
        let uniform_bits =
            estimate_encoded_bits(&frequencies, &uniform, 6).expect("uniform table is usable");
        assert!(
            matching_bits < uniform_bits,
            "matching {matching_bits} should beat uniform {uniform_bits}"
        );

        let missing = [64i16, 0, 0, 0];
        assert!(estimate_encoded_bits(&frequencies, &missing, 6).is_none());
    }
}
