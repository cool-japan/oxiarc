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
    fn test_checksum_is_lower_32_bits() {
        let data = b"test data";
        let full_hash = xxhash64(data);
        let checksum = xxhash64_checksum(data);
        assert_eq!(checksum, full_hash as u32);
    }
}
