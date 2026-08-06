//! Ring-buffer history and a greedy match finder shared by the legacy LArc
//! and PMarc codecs.
//!
//! The `-lzs-`, `-lz5-` and `-pm2-` formats all address their history by an
//! **absolute ring-buffer index**, not by a distance back from the current
//! position. That is the single most dangerous difference between them and
//! `-lh1-`/`-lh4-`..`-lh7-` (which are distance-addressed): swapping the two
//! conventions produces a codec that round-trips perfectly against itself and
//! is unreadable by every real LHA implementation. [`Ring`] therefore exposes
//! *only* absolute indexing, and the encoders convert a distance to an index
//! exactly once, in [`Ring::index_for_distance`].

/// A power-of-two ring buffer with a write cursor.
///
/// Reads are absolute (`byte_at`) and wrap with the buffer mask; writes go
/// through the cursor. Both the decoders and the encoders drive an identical
/// `Ring`, so the encoder always knows the index the decoder will see.
#[derive(Debug, Clone)]
pub(crate) struct Ring {
    buf: Vec<u8>,
    mask: usize,
    cursor: usize,
}

impl Ring {
    /// Create a ring of `size` bytes (must be a power of two) filled with
    /// `fill`, whose write cursor starts at `start`.
    pub(crate) fn new(size: usize, fill: u8, start: usize) -> Self {
        debug_assert!(size.is_power_of_two(), "ring size must be a power of two");
        Self {
            buf: vec![fill; size],
            mask: size - 1,
            cursor: start & (size - 1),
        }
    }

    /// Create a ring from an explicit initial image (used by `-lz5-`, whose
    /// history is pre-seeded with runs, ramps and padding).
    pub(crate) fn from_image(image: Vec<u8>, start: usize) -> Self {
        debug_assert!(
            image.len().is_power_of_two(),
            "ring size must be a power of two"
        );
        let mask = image.len() - 1;
        Self {
            buf: image,
            mask,
            cursor: start & mask,
        }
    }

    /// Current write cursor (used by the module's own layout tests).
    #[cfg(test)]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// Read the byte at absolute index `index` (wrapped).
    pub(crate) fn byte_at(&self, index: usize) -> u8 {
        self.buf[index & self.mask]
    }

    /// Append `byte` at the cursor and advance it.
    pub(crate) fn push(&mut self, byte: u8) {
        let slot = self.cursor;
        self.buf[slot] = byte;
        self.cursor = (slot + 1) & self.mask;
    }

    /// Absolute index that the decoder must be given to reproduce a match
    /// `distance` bytes back from the current cursor.
    pub(crate) fn index_for_distance(&self, distance: usize) -> usize {
        (self.cursor + self.buf.len() - distance) & self.mask
    }
}

/// Build the `-lz5-` pre-seeded history image (lhasa `fill_initial`).
///
/// LArc seeds the 4 KiB history with material an encoder is likely to want
/// before it has produced any output of its own: a 13-byte run of every byte
/// value (so `====…` style rules compress from the first byte), an ascending
/// and a descending ramp over all 256 values, a block of NULs, a block of
/// spaces, and 18 trailing NULs behind the start cursor.
pub(crate) fn lz5_initial_image() -> Vec<u8> {
    let mut image = Vec::with_capacity(4096);
    for value in 0u16..256 {
        image.extend(std::iter::repeat_n(value as u8, 13));
    }
    image.extend((0u16..256).map(|value| value as u8));
    image.extend((0u16..256).map(|value| (255 - value) as u8));
    image.extend(std::iter::repeat_n(0u8, 128));
    image.extend(std::iter::repeat_n(b' ', 110));
    image.extend(std::iter::repeat_n(0u8, 18));
    debug_assert_eq!(image.len(), 4096);
    image
}

/// Number of hash-chain candidates inspected per position.
///
/// The legacy formats cap matches at 17-18 bytes, so a long chain walk buys
/// almost nothing; this keeps encoding linear in practice.
const MAX_CHAIN: usize = 96;

/// Greedy hash-chain match finder over the *uncompressed* input.
///
/// Matches are reported as `(distance, length)` with `distance` measured back
/// from the current position, which the caller converts to a ring index. Only
/// history the encoder has actually produced is searched: the pre-seeded `-lz5-`
/// image is legal to reference but skipping it costs a little ratio and removes
/// a whole class of encoder/decoder disagreement.
pub(crate) struct GreedyMatcher {
    head: Vec<i32>,
    prev: Vec<i32>,
    hash_bits: u32,
    hash_len: usize,
    min_match: usize,
    max_match: usize,
    max_distance: usize,
}

impl GreedyMatcher {
    /// Create a matcher for `len` input bytes.
    pub(crate) fn new(len: usize, min_match: usize, max_match: usize, max_distance: usize) -> Self {
        let hash_bits = 15u32;
        Self {
            head: vec![-1; 1usize << hash_bits],
            prev: vec![-1; len.max(1)],
            hash_bits,
            hash_len: min_match.min(3),
            min_match,
            max_match,
            max_distance,
        }
    }

    /// Hash of the `hash_len` bytes at `pos`; `None` when they do not fit.
    fn hash(&self, data: &[u8], pos: usize) -> Option<usize> {
        if pos + self.hash_len > data.len() {
            return None;
        }
        let mut value: u32 = 0;
        for &byte in &data[pos..pos + self.hash_len] {
            value = value
                .wrapping_mul(0x9E37_79B1)
                .wrapping_add(u32::from(byte));
        }
        Some((value >> (32 - self.hash_bits)) as usize)
    }

    /// Record `pos` in the chain so later positions can match against it.
    pub(crate) fn insert(&mut self, data: &[u8], pos: usize) {
        if let Some(slot) = self.hash(data, pos) {
            self.prev[pos] = self.head[slot];
            self.head[slot] = pos as i32;
        }
    }

    /// Best `(distance, length)` at `pos`, or `None` when no match reaches
    /// `min_match`.
    pub(crate) fn find(&self, data: &[u8], pos: usize) -> Option<(usize, usize)> {
        let remaining = data.len() - pos;
        if remaining < self.min_match {
            return None;
        }
        let limit = self.max_match.min(remaining);
        let slot = self.hash(data, pos)?;

        let mut candidate = self.head[slot];
        let mut best: Option<(usize, usize)> = None;
        let mut walked = 0usize;
        while candidate >= 0 && walked < MAX_CHAIN {
            walked += 1;
            let candidate_pos = candidate as usize;
            if candidate_pos >= pos {
                break;
            }
            let distance = pos - candidate_pos;
            if distance > self.max_distance {
                break;
            }
            let mut length = 0usize;
            while length < limit && data[candidate_pos + length] == data[pos + length] {
                length += 1;
            }
            if length >= self.min_match && best.is_none_or(|(_, best_len)| length > best_len) {
                best = Some((distance, length));
                if length == limit {
                    break;
                }
            }
            candidate = self.prev[candidate_pos];
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_indexes_absolutely_and_wraps() {
        let mut ring = Ring::new(8, b'.', 6);
        assert_eq!(ring.cursor(), 6);
        ring.push(b'a');
        ring.push(b'b');
        // Cursor wrapped past the end.
        assert_eq!(ring.cursor(), 0);
        assert_eq!(ring.byte_at(6), b'a');
        assert_eq!(ring.byte_at(7), b'b');
        assert_eq!(ring.byte_at(15), b'b', "reads must wrap with the mask");
        assert_eq!(ring.byte_at(0), b'.');
    }

    #[test]
    fn index_for_distance_matches_manual_walk() {
        let mut ring = Ring::new(16, 0, 0);
        for byte in b"abcdefgh" {
            ring.push(*byte);
        }
        // Distance 3 back from the cursor is 'f'.
        let index = ring.index_for_distance(3);
        assert_eq!(ring.byte_at(index), b'f');
        assert_eq!(ring.byte_at(index + 1), b'g');
        assert_eq!(ring.byte_at(index + 2), b'h');
    }

    #[test]
    fn lz5_image_matches_larc_layout() {
        let image = lz5_initial_image();
        assert_eq!(image.len(), 4096);
        // 13 copies of every byte value.
        assert_eq!(image[0], 0);
        assert_eq!(image[12], 0);
        assert_eq!(image[13], 1);
        assert_eq!(image[3327], 255);
        // Ascending then descending ramps.
        assert_eq!(image[3328], 0);
        assert_eq!(image[3583], 255);
        assert_eq!(image[3584], 255);
        assert_eq!(image[3839], 0);
        // NULs, spaces, then 18 trailing NULs.
        assert_eq!(image[3840], 0);
        assert_eq!(image[3967], 0);
        assert_eq!(image[3968], b' ');
        assert_eq!(image[4077], b' ');
        assert_eq!(image[4078], 0);
        assert_eq!(image[4095], 0);
    }

    #[test]
    fn matcher_finds_the_longest_recent_match() {
        let data = b"abcabcabcabcXYZ".to_vec();
        let mut matcher = GreedyMatcher::new(data.len(), 3, 18, 4095);
        for pos in 0..3 {
            matcher.insert(&data, pos);
        }
        let found = matcher.find(&data, 3).expect("match at position 3");
        assert_eq!(found.0, 3, "distance");
        assert!(found.1 >= 3, "length {} must reach min_match", found.1);
    }

    #[test]
    fn matcher_reports_none_without_history() {
        let data = b"abcdefgh".to_vec();
        let matcher = GreedyMatcher::new(data.len(), 3, 18, 4095);
        assert!(matcher.find(&data, 0).is_none());
    }
}
