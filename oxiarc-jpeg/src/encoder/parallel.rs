//! Parallel entropy encoding across restart intervals (`rayon` feature).
//!
//! Restart intervals make an encoder's job splittable for the same reason
//! they make a decoder's job splittable: T.81 E.1.4 resets the predictions
//! and the entropy coder at every marker, so the bytes an interval produces
//! depend on nothing before it. Split the MCU range at interval boundaries,
//! code each part into its own buffer, concatenate — and the result is
//! **byte-identical** to the serial encoder's, because each part begins
//! exactly where the serial coder would have begun a fresh interval and ends
//! with the same padding.
//!
//! Only a sequential scan is split. A progressive scan's bands are usually
//! small and its scans are already independent of each other, and a lossless
//! scan predicts across interval boundaries row by row.

use super::coefficients::CoefficientPlane;
use super::enctable::DerivedTable;
use super::plan::EncodePlan;
use super::sequential::ScanTables;
use crate::error::Result;
use crate::frame::EntropyCoding;
use rayon::prelude::*;

/// Smallest scan worth splitting.
const MINIMUM_MCUS: usize = 256;

/// The MCU ranges to code in parallel, or `None` when the scan is not worth
/// splitting (or cannot be).
fn plan_ranges(total: usize, restart_interval: usize) -> Option<Vec<std::ops::Range<usize>>> {
    if restart_interval == 0 || total < MINIMUM_MCUS {
        return None;
    }
    let intervals = total.div_ceil(restart_interval);
    if intervals < 2 {
        return None;
    }
    // One chunk per thread: the chunks are equal-sized in MCUs, so the work
    // is already balanced, and every extra chunk costs a buffer allocation
    // and a copy at the concatenation.
    let target = rayon::current_num_threads().max(2);
    let intervals_per_chunk = intervals.div_ceil(target).max(1);
    let chunk = intervals_per_chunk * restart_interval;
    let mut ranges = Vec::with_capacity(intervals.div_ceil(intervals_per_chunk));
    let mut start = 0usize;
    while start < total {
        let end = (start + chunk).min(total);
        ranges.push(start..end);
        start = end;
    }
    if ranges.len() < 2 {
        return None;
    }
    Some(ranges)
}

/// Entropy-code one sequential scan in parallel.
///
/// Returns `None` when the scan is not splittable, in which case the caller
/// codes it serially.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_sequential_scan_parallel(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    tables: &[ScanTables],
    restart_interval: usize,
    dc_tables: &[Option<DerivedTable>; 4],
    ac_tables: &[Option<DerivedTable>; 4],
) -> Option<Result<Vec<u8>>> {
    let total = plan.mcus_per_row_for(components) * plan.mcu_rows_for(components);
    let ranges = plan_ranges(total, restart_interval)?;

    let parts: Result<Vec<Vec<u8>>> = ranges
        .into_par_iter()
        .map(|range| match plan.entropy {
            #[cfg(feature = "arithmetic")]
            EntropyCoding::Arithmetic => super::arith::encode_sequential_range(
                plan,
                coefficients,
                components,
                tables,
                restart_interval,
                range,
            ),
            #[cfg(not(feature = "arithmetic"))]
            EntropyCoding::Arithmetic => Err(crate::error::JpegError::Unsupported(
                crate::error::UnsupportedFeature::ArithmeticCoding,
            )),
            EntropyCoding::Huffman => super::sequential::encode_scan_range(
                plan,
                coefficients,
                components,
                tables,
                restart_interval,
                range,
                dc_tables,
                ac_tables,
            ),
        })
        .collect();

    Some(parts.map(|parts| {
        let total_len: usize = parts.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total_len);
        for part in parts {
            out.extend_from_slice(&part);
        }
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_start_on_interval_boundaries_and_cover_everything() {
        let ranges = plan_ranges(1024, 8).expect("splittable");
        assert!(ranges.len() >= 2);
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[ranges.len() - 1].end, 1024);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
            assert_eq!(pair[0].start % 8, 0);
        }
    }

    #[test]
    fn small_or_unsplittable_scans_decline() {
        assert!(plan_ranges(64, 8).is_none(), "too small to be worth it");
        assert!(plan_ranges(1024, 0).is_none(), "no restart interval");
        assert!(plan_ranges(1024, 4096).is_none(), "one interval only");
    }
}
