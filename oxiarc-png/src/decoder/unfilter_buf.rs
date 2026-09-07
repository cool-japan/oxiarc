//! The buffer the inflater writes decompressed scanline bytes into.
//!
//! [`UnfilterBuf`] is deliberately thin: a mutable slice plus a filled count.
//! The decoder hands it a window that is exactly the remainder of the current
//! scanline, which is what makes the decompression-bomb bound structural — the
//! inflater is never offered more output space than the image header says the
//! image data stream may produce.

use crate::filter::{RowFilter, unfilter};
use crate::header::BytesPerPixel;

/// A window the decompressor may write scanline bytes into.
///
/// ```
/// use oxiarc_png::UnfilterBuf;
/// let mut storage = [0u8; 8];
/// let mut filled = 0usize;
/// let mut buf = UnfilterBuf::new(&mut storage, &mut filled);
/// assert_eq!(buf.free_space(), 8);
/// buf.unfilled_mut()[..3].copy_from_slice(b"abc");
/// buf.advance(3);
/// assert_eq!(buf.filled(), 3);
/// assert_eq!(buf.free_space(), 5);
/// ```
#[derive(Debug)]
pub struct UnfilterBuf<'a> {
    data: &'a mut [u8],
    filled: &'a mut usize,
}

impl<'a> UnfilterBuf<'a> {
    /// Wrap `data`, treating its first `*filled` bytes as already written.
    pub fn new(data: &'a mut [u8], filled: &'a mut usize) -> UnfilterBuf<'a> {
        if *filled > data.len() {
            *filled = data.len();
        }
        UnfilterBuf { data, filled }
    }

    /// Total capacity of the window.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Bytes written so far.
    #[must_use]
    pub fn filled(&self) -> usize {
        *self.filled
    }

    /// Bytes still writable.
    #[must_use]
    pub fn free_space(&self) -> usize {
        self.data.len() - *self.filled
    }

    /// The writable tail of the window.
    pub fn unfilled_mut(&mut self) -> &mut [u8] {
        &mut self.data[*self.filled..]
    }

    /// Record that `n` more bytes were written.
    pub fn advance(&mut self, n: usize) {
        *self.filled = (*self.filled + n).min(self.data.len());
    }

    /// The bytes written so far.
    #[must_use]
    pub fn written(&self) -> &[u8] {
        &self.data[..*self.filled]
    }
}

/// A pair of adjacent scanlines to reconstruct in place.
///
/// The `png` crate exposes a type of this name whose shape is tied to its own
/// growable buffer; this one is a plain view over a previous and a current row
/// and is documented as a deliberate divergence.
#[derive(Debug)]
pub struct UnfilterRegion<'a> {
    /// The already-reconstructed row above, or empty for the first row of an
    /// image or Adam7 pass.
    pub previous: &'a [u8],
    /// The row to reconstruct, without its filter byte.
    pub current: &'a mut [u8],
    /// The filter stride.
    pub bpp: BytesPerPixel,
}

impl UnfilterRegion<'_> {
    /// Reconstruct [`UnfilterRegion::current`] in place.
    pub fn unfilter(self, filter: RowFilter) {
        unfilter(filter, self.bpp, self.previous, self.current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_accounting() {
        let mut storage = [0u8; 4];
        let mut filled = 0usize;
        {
            let mut buf = UnfilterBuf::new(&mut storage, &mut filled);
            assert_eq!(buf.capacity(), 4);
            assert_eq!(buf.free_space(), 4);
            buf.unfilled_mut()[0] = 1;
            buf.advance(1);
            assert_eq!(buf.written(), &[1]);
            buf.advance(99);
            assert_eq!(buf.filled(), 4);
            assert_eq!(buf.free_space(), 0);
            assert!(buf.unfilled_mut().is_empty());
        }
        assert_eq!(filled, 4);
    }

    #[test]
    fn an_oversized_filled_count_is_clamped() {
        let mut storage = [0u8; 2];
        let mut filled = 99usize;
        let buf = UnfilterBuf::new(&mut storage, &mut filled);
        assert_eq!(buf.filled(), 2);
    }

    #[test]
    fn region_reconstructs_in_place() {
        let mut row = [1u8, 1, 1];
        UnfilterRegion {
            previous: &[],
            current: &mut row,
            bpp: BytesPerPixel::One,
        }
        .unfilter(RowFilter::Sub);
        assert_eq!(row, [1, 2, 3]);
    }
}
