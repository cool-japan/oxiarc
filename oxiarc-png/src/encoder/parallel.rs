//! Parallel per-row filtering for the encoder, behind the `parallel` Cargo
//! feature.
//!
//! **DEFLATE stays serial, unconditionally — this is architectural, not a
//! shortcut.** Filter *selection* only ever reads raw (unfiltered) samples
//! -- this row's and the row above's, both always available from the
//! caller's original image buffer regardless of processing order -- never
//! another row's *filtered* output, so filtering every row independently
//! and in any order is correct. Compression is not: a PNG `IDAT`/`fdAT`
//! payload is one continuous zlib stream (one header, one Adler-32
//! trailer, one final block), and this crate drives
//! `oxiarc_deflate::Deflater::deflate` directly across the whole frame so
//! LZ77 back-references and the entropy coder's state carry across every
//! row (see `encoder::zlib`'s module doc). Splitting that into
//! independently-compressed pieces is a *different, incompatible* format —
//! exactly what `oxiarc_deflate::parallel::compress_deflate_parallel`
//! produces: a concatenation of independent raw DEFLATE streams, each with
//! its own final-block terminator, valid only for a decoder that knows the
//! chunk boundaries and decodes each piece separately (as
//! `compress_gzip_parallel`'s multi-member GZIP output is designed to be
//! read). A single PNG zlib stream has no such per-piece framing: a real
//! decoder inflates it as one stream and errors (or stops) at the first
//! piece's own final-block marker. So `compress_deflate_parallel` is not a
//! valid substitute for the serial `Deflater::deflate` calls here, at any
//! chunk size, and this module never routes IDAT/fdAT bytes through it.
//!
//! Only the row list this module hands back is new; the caller (`write_image_data`)
//! still feeds each row's bytes to the encoder's `FrameEncoder` one at a
//! time, in transmission order, exactly as the non-parallel path does.

use rayon::prelude::*;

use crate::filter::{AdaptiveScratch, Filter, select_filter};
use crate::header::BytesPerPixel;

/// Below this many rows, per-row `rayon` dispatch overhead is likely to
/// cost more than the parallelism saves, so the caller should use the
/// serial path instead.
pub(crate) const MIN_ROWS_FOR_PARALLEL_FILTER: usize = 64;

/// How many rows [`filter_rows_parallel`]'s caller gathers before filtering
/// and draining them.
///
/// The caller does **not** gather the whole frame: doing so would hold each
/// raw row twice (as the row, and as the next row's `previous`) plus every
/// filtered row, roughly three times the image in memory before the first
/// compressed byte — strictly worse than the serial path's two rows, and
/// growing without bound with the caller's image size. A window keeps the
/// extra memory at `PARALLEL_ROW_WINDOW` rows whatever the image is, while
/// still handing `rayon` enough work per dispatch (several times
/// [`MIN_ROWS_FOR_PARALLEL_FILTER`]) to amortise it. Output bytes are
/// unaffected: rows are filtered independently and pushed to `Deflater` in
/// transmission order either way.
pub(crate) const PARALLEL_ROW_WINDOW: usize = 256;

// Compile-time invariants for the two constants above. A window smaller than
// the dispatch threshold would hand `rayon` less work per batch than the
// threshold itself says is worth dispatching; a window that grew to
// image-scale would reintroduce the whole-frame buffering it exists to
// avoid. Checked here rather than in a `#[test]` so a bad edit cannot even
// build.
const _: () = {
    assert!(PARALLEL_ROW_WINDOW >= MIN_ROWS_FOR_PARALLEL_FILTER);
    assert!(PARALLEL_ROW_WINDOW <= 4096);
};

/// Whether [`filter_rows_parallel`] is worth using for a frame of
/// `row_count` transmission-order rows (for an interlaced image, the sum
/// over all seven Adam7 passes' row counts, not just the image height).
#[must_use]
pub(crate) fn should_parallelize(row_count: usize) -> bool {
    row_count >= MIN_ROWS_FOR_PARALLEL_FILTER
}

/// One row filtered and ready for the (still-serial) `Deflater`: the
/// filter-type byte followed by the filtered samples — the same shape
/// [`crate::filter::select_filter`]'s callers already build by hand.
pub(crate) type FilteredRow = Vec<u8>;

/// Filter every row in `rows` independently and in parallel.
///
/// `rows[i]` is `(raw_row, previous_raw_row)`; `previous_raw_row` is empty
/// for a pass's or frame's first row, exactly like the serial path's `prev`
/// reset at those same boundaries. The result is in the same order as
/// `rows` — parallel *computation*, still-sequential *order* — so the
/// caller can feed it to `Deflater` exactly as it would the serial path's
/// output.
pub(crate) fn filter_rows_parallel(
    filter_choice: Filter,
    bpp: BytesPerPixel,
    rows: &[(Vec<u8>, Vec<u8>)],
) -> Vec<FilteredRow> {
    rows.par_iter()
        .map(|(raw, previous)| {
            // A fresh scratch buffer per task: `AdaptiveScratch` is a
            // reusable-allocation optimisation for the serial loop's single
            // thread, not something safe (or useful) to share across a
            // parallel iterator's worker threads.
            let mut scratch = AdaptiveScratch::new();
            let filter = select_filter(filter_choice, bpp, previous, raw, &mut scratch);
            let filtered = scratch.filtered();
            let mut with_type = Vec::with_capacity(1 + filtered.len());
            with_type.push(filter.into_u8());
            with_type.extend_from_slice(filtered);
            with_type
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::unfilter;

    #[test]
    fn parallel_filtering_matches_serial_filtering_row_by_row() {
        let bpp = BytesPerPixel::Three;
        let rows: Vec<Vec<u8>> = (0..80u8)
            .map(|y| {
                (0..30u8)
                    .map(|x| x.wrapping_mul(7).wrapping_add(y.wrapping_mul(11)))
                    .collect()
            })
            .collect();

        let mut pairs = Vec::new();
        let mut prev = Vec::new();
        for row in &rows {
            pairs.push((row.clone(), prev.clone()));
            prev = row.clone();
        }

        let parallel_out = filter_rows_parallel(Filter::Adaptive, bpp, &pairs);

        let mut scratch = AdaptiveScratch::new();
        let mut serial_out = Vec::new();
        let mut prev = Vec::new();
        for row in &rows {
            let filter = select_filter(Filter::Adaptive, bpp, &prev, row, &mut scratch);
            let mut with_type = vec![filter.into_u8()];
            with_type.extend_from_slice(scratch.filtered());
            serial_out.push(with_type);
            prev = row.clone();
        }

        assert_eq!(parallel_out, serial_out);
    }

    #[test]
    fn every_parallel_filtered_row_unfilters_back_to_the_original() {
        let bpp = BytesPerPixel::Four;
        let rows: Vec<Vec<u8>> = (0..MIN_ROWS_FOR_PARALLEL_FILTER as u8 + 5)
            .map(|y| {
                (0..40u8)
                    .map(|x| x.wrapping_mul(13).wrapping_sub(y))
                    .collect()
            })
            .collect();
        assert!(should_parallelize(rows.len()));

        let mut pairs = Vec::new();
        let mut prev = Vec::new();
        for row in &rows {
            pairs.push((row.clone(), prev.clone()));
            prev = row.clone();
        }

        let filtered = filter_rows_parallel(Filter::Adaptive, bpp, &pairs);

        let mut prev = Vec::new();
        for (row, with_type) in rows.iter().zip(filtered.iter()) {
            let filter = crate::filter::RowFilter::from_u8(with_type[0]).expect("valid filter");
            let mut buf = with_type[1..].to_vec();
            unfilter(filter, bpp, &prev, &mut buf);
            assert_eq!(&buf, row);
            prev = row.clone();
        }
    }

    #[test]
    fn small_row_counts_stay_below_the_parallel_threshold() {
        assert!(!should_parallelize(0));
        assert!(!should_parallelize(MIN_ROWS_FOR_PARALLEL_FILTER - 1));
        assert!(should_parallelize(MIN_ROWS_FOR_PARALLEL_FILTER));
    }
}
