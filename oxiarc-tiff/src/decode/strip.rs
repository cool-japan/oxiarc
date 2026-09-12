//! Strip geometry and the strip decode driver.
//!
//! Strips are row bands: strip `i` of a plane covers rows
//! `i * RowsPerStrip .. min((i + 1) * RowsPerStrip, height)`. The last strip is
//! **short**, never padded, so its decoded length must be computed from the
//! image height rather than from `RowsPerStrip`.
//!
//! ```
//! use oxiarc_tiff::decode::strip::{rows_in_strip, strip_of_row};
//!
//! // A 10-row image in strips of 4: strips 0 and 1 hold 4 rows, strip 2 holds 2.
//! assert_eq!(strip_of_row(9, 4), 2);
//! assert_eq!(rows_in_strip(2, 4, 10), 2);
//! assert_eq!(rows_in_strip(0, 4, 10), 4);
//! ```

use std::io::{Read, Seek};
use std::ops::Range;

use crate::byteorder::EndianReader;
use crate::compression::CodecRegistry;
use crate::error::Result;
use crate::image::{ChunkGeometry, ImageInfo, Rect};
use crate::limits::{Leniency, Limits, OutputBudget, Warnings};

use super::{ChunkBuffers, decode_chunk, place_chunk_in_rect};

/// The strip index (within a plane) that holds image row `row`.
#[must_use]
pub fn strip_of_row(row: u32, rows_per_strip: u32) -> u32 {
    if rows_per_strip == 0 {
        return 0;
    }
    row / rows_per_strip
}

/// How many rows strip `index` actually covers.
#[must_use]
pub fn rows_in_strip(index: u32, rows_per_strip: u32, height: u32) -> u32 {
    if rows_per_strip == 0 {
        return 0;
    }
    let start = index.saturating_mul(rows_per_strip);
    if start >= height {
        return 0;
    }
    rows_per_strip.min(height - start)
}

/// The strips of one plane that intersect `rect`.
#[must_use]
pub fn strips_for_rect(rows_per_strip: u32, height: u32, rect: Rect) -> Range<u32> {
    if rows_per_strip == 0 || rect.height == 0 {
        return 0..0;
    }
    let top = rect.y.min(height.saturating_sub(1));
    let bottom = rect.y.saturating_add(rect.height).min(height);
    if bottom == 0 {
        return 0..0;
    }
    let first = strip_of_row(top, rows_per_strip);
    let last = strip_of_row(bottom - 1, rows_per_strip);
    first..last.saturating_add(1)
}

/// The chunk indices, in decode order, of every strip across every plane
/// that intersects `rect`.
///
/// Factored out of [`decode_into`] so a parallel driver (the `rayon`
/// feature) enumerates *exactly* the chunks the serial path would, in the
/// same order -- there is only one place this list is computed.
#[must_use]
pub fn chunk_indices_for_rect(info: &ImageInfo, rect: Rect) -> Vec<u64> {
    let ChunkGeometry::Strips { rows_per_strip, .. } = &info.chunks else {
        return Vec::new();
    };
    let range = strips_for_rect(*rows_per_strip, info.height, rect);
    let per_plane = info.chunks_per_plane();
    let planes = u64::from(info.plane_count());
    let available = info.chunks.offsets().len() as u64;

    let mut indices = Vec::new();
    for plane in 0..planes {
        for strip in range.clone() {
            let index = plane * per_plane + u64::from(strip);
            if index < available {
                indices.push(index);
            }
        }
    }
    indices
}

/// Decodes every strip that intersects `rect` into `dst`.
///
/// `dst` is laid out as `rect.width * rect.height` interleaved pixels of the
/// image's `SamplesPerPixel` channels, in native slots.
///
/// # Errors
/// Every failure [`super::decode_chunk`] and [`place_chunk_in_rect`] produce.
#[allow(clippy::too_many_arguments)]
pub fn decode_into<R: Read + Seek>(
    reader: &mut EndianReader<R>,
    info: &ImageInfo,
    rect: Rect,
    dst: &mut [u8],
    limits: &Limits,
    leniency: Leniency,
    budget: &mut OutputBudget,
    registry: Option<&CodecRegistry>,
    warnings: &mut Warnings,
    buffers: &mut ChunkBuffers,
) -> Result<()> {
    for index in chunk_indices_for_rect(info, rect) {
        let shape = decode_chunk(
            reader, info, index, limits, leniency, budget, registry, warnings, buffers,
        )?;
        place_chunk_in_rect(
            buffers.native(),
            &shape,
            dst,
            rect,
            info.samples_per_pixel,
            info.planar,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_strip_is_short_not_padded() {
        assert_eq!(rows_in_strip(0, 4, 10), 4);
        assert_eq!(rows_in_strip(1, 4, 10), 4);
        assert_eq!(rows_in_strip(2, 4, 10), 2);
        assert_eq!(rows_in_strip(3, 4, 10), 0);
    }

    #[test]
    fn a_single_strip_covers_the_whole_image() {
        assert_eq!(rows_in_strip(0, u32::MAX, 100), 100);
        assert_eq!(strip_of_row(99, u32::MAX), 0);
    }

    #[test]
    fn row_to_strip_mapping() {
        assert_eq!(strip_of_row(0, 4), 0);
        assert_eq!(strip_of_row(3, 4), 0);
        assert_eq!(strip_of_row(4, 4), 1);
        assert_eq!(strip_of_row(11, 4), 2);
        assert_eq!(strip_of_row(5, 0), 0);
        assert_eq!(rows_in_strip(0, 0, 10), 0);
    }

    #[test]
    fn only_the_intersecting_strips_are_selected() {
        assert_eq!(strips_for_rect(4, 10, Rect::new(0, 0, 8, 10)), 0..3);
        assert_eq!(strips_for_rect(4, 10, Rect::new(0, 4, 8, 1)), 1..2);
        assert_eq!(strips_for_rect(4, 10, Rect::new(0, 3, 8, 2)), 0..2);
        assert_eq!(strips_for_rect(4, 10, Rect::new(0, 9, 8, 1)), 2..3);
        assert_eq!(strips_for_rect(4, 10, Rect::new(0, 0, 8, 0)), 0..0);
        assert_eq!(strips_for_rect(0, 10, Rect::new(0, 0, 8, 4)), 0..0);
    }
}
