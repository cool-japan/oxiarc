//! XXHash64 implementation for Zstandard checksums.
//!
//! Zstandard uses the lower 32 bits of XXH64 with seed 0 for frame checksums.

/// XXH64 prime constants.
const PRIME64_1: u64 = 0x9E3779B185EBCA87;
const PRIME64_2: u64 = 0xC2B2AE3D27D4EB4F;
const PRIME64_3: u64 = 0x165667B19E3779F9;
const PRIME64_4: u64 = 0x85EBCA77C2B2AE63;
const PRIME64_5: u64 = 0x27D4EB2F165667C5;

/// Compute XXH64 hash of data with seed 0.
pub fn xxhash64(data: &[u8]) -> u64 {
    xxhash64_with_seed(data, 0)
}

/// Compute XXH64 hash with custom seed.
pub fn xxhash64_with_seed(data: &[u8], seed: u64) -> u64 {
    let len = data.len();

    let mut hash = if len >= 32 {
        // Process 32-byte chunks
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);

        let mut pos = 0;
        while pos + 32 <= len {
            v1 = round64(v1, read_u64_le(&data[pos..]));
            v2 = round64(v2, read_u64_le(&data[pos + 8..]));
            v3 = round64(v3, read_u64_le(&data[pos + 16..]));
            v4 = round64(v4, read_u64_le(&data[pos + 24..]));
            pos += 32;
        }

        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));

        h = merge_round64(h, v1);
        h = merge_round64(h, v2);
        h = merge_round64(h, v3);
        h = merge_round64(h, v4);

        h
    } else {
        seed.wrapping_add(PRIME64_5)
    };

    hash = hash.wrapping_add(len as u64);

    // Process remaining bytes
    let remaining = &data[len - (len % 32)..];
    let mut pos = 0;

    // Process 8-byte chunks
    while pos + 8 <= remaining.len() {
        let k = read_u64_le(&remaining[pos..]).wrapping_mul(PRIME64_2);
        hash ^= k.rotate_left(31).wrapping_mul(PRIME64_1);
        hash = hash
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        pos += 8;
    }

    // Process 4-byte chunk
    if pos + 4 <= remaining.len() {
        let k = (read_u32_le(&remaining[pos..]) as u64).wrapping_mul(PRIME64_1);
        hash ^= k;
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        pos += 4;
    }

    // Process remaining bytes
    while pos < remaining.len() {
        hash ^= (remaining[pos] as u64).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
        pos += 1;
    }

    // Final avalanche
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^= hash >> 32;

    hash
}

/// Compute the 32-bit checksum used by Zstandard.
pub fn xxhash64_checksum(data: &[u8]) -> u32 {
    xxhash64(data) as u32
}

/// Incremental (streaming) XXH64 hasher.
///
/// Produces exactly the same digest as [`xxhash64_with_seed`] applied to the
/// concatenation of every slice passed to [`update`](XxHash64::update), but
/// without ever retaining the data.  Zstandard frame checksums are computed
/// over the whole decompressed content, so a bounded-memory decoder needs
/// this incremental form: the content is hashed as it streams past and only
/// the 48-byte hasher state is kept.
///
/// # Example
///
/// ```rust
/// # use oxiarc_zstd::XxHash64;
/// let mut h = XxHash64::new();
/// h.update(b"Hello, ");
/// h.update(b"world!");
/// // The 32-bit form is what a Zstandard frame stores.
/// assert_eq!(h.finish_checksum(), h.finish() as u32);
/// ```
#[derive(Debug, Clone)]
pub struct XxHash64 {
    /// The four accumulator lanes.
    v: [u64; 4],
    /// Bytes not yet folded into the lanes (always fewer than 32).
    buf: [u8; 32],
    /// Number of valid bytes in `buf`.
    buf_len: usize,
    /// Total number of bytes fed so far.
    total_len: u64,
    /// The seed the hasher was created with.
    seed: u64,
}

impl XxHash64 {
    /// Create a hasher with seed 0 (the seed Zstandard uses).
    #[must_use]
    pub fn new() -> Self {
        Self::with_seed(0)
    }

    /// Create a hasher with an explicit seed.
    #[must_use]
    pub fn with_seed(seed: u64) -> Self {
        Self {
            v: [
                seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
                seed.wrapping_add(PRIME64_2),
                seed,
                seed.wrapping_sub(PRIME64_1),
            ],
            buf: [0u8; 32],
            buf_len: 0,
            total_len: 0,
            seed,
        }
    }

    /// Reset the hasher to its freshly-constructed state, keeping the seed.
    pub fn reset(&mut self) {
        *self = Self::with_seed(self.seed);
    }

    /// Number of bytes fed so far.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Feed `data` into the hasher.
    pub fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        let mut rest = data;

        // Top up a partially filled staging buffer first.
        if self.buf_len > 0 {
            let want = 32 - self.buf_len;
            if rest.len() < want {
                self.buf[self.buf_len..self.buf_len + rest.len()].copy_from_slice(rest);
                self.buf_len += rest.len();
                return;
            }
            let (head, tail) = rest.split_at(want);
            self.buf[self.buf_len..].copy_from_slice(head);
            let block = self.buf;
            self.fold_block(&block);
            self.buf_len = 0;
            rest = tail;
        }

        // Fold whole 32-byte stripes straight from the input.
        while rest.len() >= 32 {
            let (block, tail) = rest.split_at(32);
            self.fold_block(block);
            rest = tail;
        }

        // Stage the remainder.
        if !rest.is_empty() {
            self.buf[..rest.len()].copy_from_slice(rest);
            self.buf_len = rest.len();
        }
    }

    /// Fold one 32-byte stripe into the four lanes.
    fn fold_block(&mut self, block: &[u8]) {
        debug_assert_eq!(block.len(), 32);
        self.v[0] = round64(self.v[0], read_u64_le(&block[0..]));
        self.v[1] = round64(self.v[1], read_u64_le(&block[8..]));
        self.v[2] = round64(self.v[2], read_u64_le(&block[16..]));
        self.v[3] = round64(self.v[3], read_u64_le(&block[24..]));
    }

    /// Compute the 64-bit digest of everything fed so far.
    ///
    /// The hasher is not consumed and may keep receiving data afterwards.
    #[must_use]
    pub fn finish(&self) -> u64 {
        let mut hash = if self.total_len >= 32 {
            let mut h = self.v[0]
                .rotate_left(1)
                .wrapping_add(self.v[1].rotate_left(7))
                .wrapping_add(self.v[2].rotate_left(12))
                .wrapping_add(self.v[3].rotate_left(18));
            h = merge_round64(h, self.v[0]);
            h = merge_round64(h, self.v[1]);
            h = merge_round64(h, self.v[2]);
            h = merge_round64(h, self.v[3]);
            h
        } else {
            self.seed.wrapping_add(PRIME64_5)
        };

        hash = hash.wrapping_add(self.total_len);

        let remaining = &self.buf[..self.buf_len];
        let mut pos = 0;

        while pos + 8 <= remaining.len() {
            let k = read_u64_le(&remaining[pos..]).wrapping_mul(PRIME64_2);
            hash ^= k.rotate_left(31).wrapping_mul(PRIME64_1);
            hash = hash
                .rotate_left(27)
                .wrapping_mul(PRIME64_1)
                .wrapping_add(PRIME64_4);
            pos += 8;
        }

        if pos + 4 <= remaining.len() {
            let k = (read_u32_le(&remaining[pos..]) as u64).wrapping_mul(PRIME64_1);
            hash ^= k;
            hash = hash
                .rotate_left(23)
                .wrapping_mul(PRIME64_2)
                .wrapping_add(PRIME64_3);
            pos += 4;
        }

        while pos < remaining.len() {
            hash ^= (remaining[pos] as u64).wrapping_mul(PRIME64_5);
            hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
            pos += 1;
        }

        hash ^= hash >> 33;
        hash = hash.wrapping_mul(PRIME64_2);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(PRIME64_3);
        hash ^= hash >> 32;

        hash
    }

    /// Compute the 32-bit checksum a Zstandard frame stores (the low 32 bits
    /// of the 64-bit digest).
    #[must_use]
    pub fn finish_checksum(&self) -> u32 {
        self.finish() as u32
    }
}

impl Default for XxHash64 {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn round64(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline]
fn merge_round64(mut acc: u64, val: u64) -> u64 {
    let val = round64(0, val);
    acc ^= val;
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

#[inline]
fn read_u64_le(data: &[u8]) -> u64 {
    u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ])
}

#[inline]
fn read_u32_le(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xxhash64_empty() {
        // Known value for empty input with seed 0
        let hash = xxhash64(&[]);
        assert_eq!(hash, 0xEF46DB3751D8E999);
    }

    #[test]
    fn test_xxhash64_hello() {
        // Known value for "Hello" with seed 0
        let hash = xxhash64(b"Hello");
        // This should be consistent
        assert_ne!(hash, 0);
    }

    #[test]
    fn test_xxhash64_long_data() {
        // Test with data longer than 32 bytes
        let data = vec![0x42u8; 100];
        let hash = xxhash64(&data);
        assert_ne!(hash, 0);
    }

    #[test]
    fn test_xxhash64_consistency() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let hash1 = xxhash64(data);
        let hash2 = xxhash64(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_incremental_matches_one_shot_random_splits() {
        // Deterministic pseudo-random corpus.
        let mut data = Vec::with_capacity(4096);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..4096 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 24) as u8);
        }

        for len in [
            0usize, 1, 3, 4, 7, 8, 15, 16, 31, 32, 33, 63, 64, 65, 127, 1000, 4096,
        ] {
            let slice = &data[..len];
            let expected = xxhash64(slice);
            // Single update.
            let mut h = XxHash64::new();
            h.update(slice);
            assert_eq!(h.finish(), expected, "single update, len {}", len);
            assert_eq!(h.total_len(), len as u64);

            // Byte-at-a-time.
            let mut h = XxHash64::new();
            for b in slice {
                h.update(std::slice::from_ref(b));
            }
            assert_eq!(h.finish(), expected, "byte-at-a-time, len {}", len);

            // Every possible two-way split.
            for split in 0..=len {
                let mut h = XxHash64::new();
                h.update(&slice[..split]);
                h.update(&slice[split..]);
                assert_eq!(h.finish(), expected, "split {} of len {}", split, len);
            }

            // Three-way splits at 32-byte-boundary-adjacent points.
            for a in [0usize, 1, 31, 32, 33] {
                for b in [0usize, 1, 31, 32, 33] {
                    let a = a.min(len);
                    let b = (a + b).min(len);
                    let mut h = XxHash64::new();
                    h.update(&slice[..a]);
                    h.update(&slice[a..b]);
                    h.update(&slice[b..]);
                    assert_eq!(h.finish(), expected, "3-way {}/{} of len {}", a, b, len);
                }
            }
        }
    }

    #[test]
    fn test_incremental_with_seed_and_reset() {
        let data = b"the quick brown fox jumps over the lazy dog, twice over";
        for seed in [0u64, 1, 0xDEAD_BEEF_CAFE_F00D] {
            let mut h = XxHash64::with_seed(seed);
            h.update(&data[..10]);
            h.update(&data[10..]);
            assert_eq!(h.finish(), xxhash64_with_seed(data, seed));
            h.reset();
            assert_eq!(h.total_len(), 0);
            h.update(data);
            assert_eq!(h.finish(), xxhash64_with_seed(data, seed));
        }
    }

    #[test]
    fn test_incremental_checksum_matches_frame_checksum() {
        let data = vec![0x5Au8; 100_000];
        let mut h = XxHash64::new();
        for chunk in data.chunks(7777) {
            h.update(chunk);
        }
        assert_eq!(h.finish_checksum(), xxhash64_checksum(&data));
    }

    #[test]
    fn test_incremental_empty_matches() {
        let h = XxHash64::new();
        assert_eq!(h.finish(), xxhash64(&[]));
        assert_eq!(h.finish(), 0xEF46DB3751D8E999);
    }

    #[test]
    fn test_checksum_is_lower_32_bits() {
        let data = b"test data";
        let full_hash = xxhash64(data);
        let checksum = xxhash64_checksum(data);
        assert_eq!(checksum, full_hash as u32);
    }
}
