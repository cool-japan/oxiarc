//! Compression 1: uncompressed data.

use crate::error::Result;

/// Copies `src` into `dst`, returning the number of bytes written.
///
/// A source *longer* than the chunk geometry is padding, not corruption:
/// libtiff pads strips so every offset is even, and a recovered
/// `StripByteCounts` (edge case E1) can only over-estimate. The extra bytes are
/// ignored. A *shorter* source is reported to the caller, which knows whether
/// this is the last strip of a lenient read.
///
/// The compressed codecs keep the strict overrun check, because there an
/// over-long expansion really does mean corrupt data.
///
/// # Errors
/// Never fails; the signature matches the rest of the dispatch.
pub fn decode_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let take = src.len().min(dst.len());
    if let (Some(head), Some(body)) = (dst.get_mut(..take), src.get(..take)) {
        head.copy_from_slice(body);
    }
    Ok(take)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_exactly() {
        let mut dst = [0u8; 4];
        assert_eq!(decode_into(&[1, 2, 3, 4], &mut dst).expect("copy"), 4);
        assert_eq!(dst, [1, 2, 3, 4]);
    }

    #[test]
    fn a_short_source_leaves_the_tail_untouched() {
        let mut dst = [9u8; 4];
        assert_eq!(decode_into(&[1, 2], &mut dst).expect("short"), 2);
        assert_eq!(dst, [1, 2, 9, 9]);
    }

    #[test]
    fn an_over_long_source_is_treated_as_padding() {
        let mut dst = [0u8; 2];
        assert_eq!(decode_into(&[1, 2, 3], &mut dst).expect("padded"), 2);
        assert_eq!(dst, [1, 2]);
    }
}
