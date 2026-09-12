//! Tile geometry and the tile decode driver.
//!
//! Unlike strips, **tiles are always coded at their full declared size and zero
//! padded** at the right and bottom edges of the image. The decoder therefore
//! always allocates a full tile, decodes into it, and copies only the valid
//! sub-rectangle out.
//!
//! ```
//! use oxiarc_tiff::decode::tile::{tile_index, tiles_across};
//!
//! // A 100x60 image in 32x32 tiles: 4 across, 2 down.
//! assert_eq!(tiles_across(100, 32), 4);
//! assert_eq!(tile_index(2, 1, 4), 6);
//! ```

use std::io::{Read, Seek};

use crate::byteorder::EndianReader;
use crate::compression::CodecRegistry;
use crate::error::Result;
use crate::image::{ChunkGeometry, ImageInfo, Rect};
use crate::limits::{Leniency, Limits, OutputBudget, Warnings};

use super::{ChunkBuffers, decode_chunk, place_chunk_in_rect};

/// Number of tiles across the image.
#[must_use]
pub fn tiles_across(width: u32, tile_width: u32) -> u32 {
    if tile_width == 0 {
        return 0;
    }
    width.div_ceil(tile_width)
}

/// Number of tiles down the image.
#[must_use]
pub fn tiles_down(height: u32, tile_length: u32) -> u32 {
    if tile_length == 0 {
        return 0;
    }
    height.div_ceil(tile_length)
}

/// The chunk index (within a plane) of the tile at column `col`, row `row`.
#[must_use]
pub fn tile_index(col: u32, row: u32, across: u32) -> u64 {
    u64::from(row) * u64::from(across) + u64::from(col)
}

/// The half-open column range of tiles that intersect `rect`.
#[must_use]
pub fn tile_cols_for_rect(tile_width: u32, width: u32, rect: Rect) -> (u32, u32) {
    if tile_width == 0 || rect.width == 0 {
        return (0, 0);
    }
    let left = rect.x.min(width.saturating_sub(1));
    let right = rect.x.saturating_add(rect.width).min(width);
    if right == 0 {
        return (0, 0);
    }
    (left / tile_width, ((right - 1) / tile_width) + 1)
}

/// The half-open row range of tiles that intersect `rect`.
#[must_use]
pub fn tile_rows_for_rect(tile_length: u32, height: u32, rect: Rect) -> (u32, u32) {
    if tile_length == 0 || rect.height == 0 {
        return (0, 0);
    }
    let top = rect.y.min(height.saturating_sub(1));
    let bottom = rect.y.saturating_add(rect.height).min(height);
    if bottom == 0 {
        return (0, 0);
    }
    (top / tile_length, ((bottom - 1) / tile_length) + 1)
}

/// The chunk indices, in decode order, of every tile across every plane that
/// intersects `rect`.
///
/// Factored out of [`decode_into`] so a parallel driver (the `rayon`
/// feature) enumerates *exactly* the chunks the serial path would, in the
/// same order -- there is only one place this list is computed.
#[must_use]
pub fn chunk_indices_for_rect(info: &ImageInfo, rect: Rect) -> Vec<u64> {
    let ChunkGeometry::Tiles {
        tile_width,
        tile_length,
        ..
    } = &info.chunks
    else {
        return Vec::new();
    };
    let across = tiles_across(info.width, *tile_width);
    let (col0, col1) = tile_cols_for_rect(*tile_width, info.width, rect);
    let (row0, row1) = tile_rows_for_rect(*tile_length, info.height, rect);
    let per_plane = info.chunks_per_plane();
    let planes = u64::from(info.plane_count());
    let available = info.chunks.offsets().len() as u64;

    let mut indices = Vec::new();
    for plane in 0..planes {
        for row in row0..row1 {
            for col in col0..col1 {
                let index = plane * per_plane + tile_index(col, row, across);
                if index < available {
                    indices.push(index);
                }
            }
        }
    }
    indices
}

/// Decodes every tile that intersects `rect` into `dst`.
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
    fn tile_counts_round_up() {
        assert_eq!(tiles_across(100, 32), 4);
        assert_eq!(tiles_across(128, 32), 4);
        assert_eq!(tiles_down(60, 32), 2);
        assert_eq!(tiles_across(1, 16), 1);
        assert_eq!(tiles_across(100, 0), 0);
        assert_eq!(tiles_down(100, 0), 0);
    }

    #[test]
    fn tile_indices_are_row_major() {
        assert_eq!(tile_index(0, 0, 4), 0);
        assert_eq!(tile_index(3, 0, 4), 3);
        assert_eq!(tile_index(0, 1, 4), 4);
        assert_eq!(tile_index(2, 1, 4), 6);
    }

    #[test]
    fn only_the_intersecting_tiles_are_selected() {
        assert_eq!(
            tile_cols_for_rect(32, 100, Rect::new(0, 0, 100, 60)),
            (0, 4)
        );
        assert_eq!(tile_cols_for_rect(32, 100, Rect::new(33, 0, 1, 1)), (1, 2));
        assert_eq!(tile_cols_for_rect(32, 100, Rect::new(31, 0, 2, 1)), (0, 2));
        assert_eq!(tile_rows_for_rect(32, 60, Rect::new(0, 32, 1, 28)), (1, 2));
        assert_eq!(tile_cols_for_rect(32, 100, Rect::new(0, 0, 0, 1)), (0, 0));
        assert_eq!(tile_rows_for_rect(0, 60, Rect::new(0, 0, 1, 1)), (0, 0));
    }
}
