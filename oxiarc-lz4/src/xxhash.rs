//! XXHash32 implementation for LZ4 frame checksums.
//!
//! The official LZ4 frame format uses XXHash32 for content checksums.

/// XXH32 prime constants.
const PRIME32_1: u32 = 0x9E3779B1;
const PRIME32_2: u32 = 0x85EBCA77;
const PRIME32_3: u32 = 0xC2B2AE3D;
const PRIME32_4: u32 = 0x27D4EB2F;
const PRIME32_5: u32 = 0x165667B1;

/// Compute XXH32 hash of data with seed 0.
#[inline]
pub fn xxhash32(data: &[u8]) -> u32 {
    xxhash32_with_seed(data, 0)
}

/// Compute XXH32 hash with custom seed.
pub fn xxhash32_with_seed(data: &[u8], seed: u32) -> u32 {
    let len = data.len();

    let mut h32 = if len >= 16 {
        // Process 16-byte chunks
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);

        let mut pos = 0;
        while pos + 16 <= len {
            v1 = round32(v1, read_u32_le(&data[pos..]));
            v2 = round32(v2, read_u32_le(&data[pos + 4..]));
            v3 = round32(v3, read_u32_le(&data[pos + 8..]));
            v4 = round32(v4, read_u32_le(&data[pos + 12..]));
            pos += 16;
        }

        v1.rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18))
    } else {
        seed.wrapping_add(PRIME32_5)
    };

    h32 = h32.wrapping_add(len as u32);

    // Process remaining bytes
    let remaining_start = len - (len % 16);
    let remaining = &data[remaining_start..];
    let mut pos = 0;

    // Process 4-byte chunks
    while pos + 4 <= remaining.len() {
        h32 = h32.wrapping_add(read_u32_le(&remaining[pos..]).wrapping_mul(PRIME32_3));
        h32 = h32.rotate_left(17).wrapping_mul(PRIME32_4);
        pos += 4;
    }

    // Process remaining bytes
    while pos < remaining.len() {
        h32 = h32.wrapping_add((remaining[pos] as u32).wrapping_mul(PRIME32_5));
        h32 = h32.rotate_left(11).wrapping_mul(PRIME32_1);
        pos += 1;
    }

    // Final avalanche
    h32 ^= h32 >> 15;
    h32 = h32.wrapping_mul(PRIME32_2);
    h32 ^= h32 >> 13;
    h32 = h32.wrapping_mul(PRIME32_3);
    h32 ^= h32 >> 16;

    h32
}

#[inline]
fn round32(acc: u32, input: u32) -> u32 {
    acc.wrapping_add(input.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

#[inline]
fn read_u32_le(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}

/// Incremental XXHash32 hasher for streaming data.
#[derive(Clone)]
pub struct XxHash32 {
    seed: u32,
    v1: u32,
    v2: u32,
    v3: u32,
    v4: u32,
    buffer: [u8; 16],
    buffer_len: usize,
    total_len: u64,
    large: bool,
}

impl XxHash32 {
    /// Create a new hasher with default seed (0).
    pub fn new() -> Self {
        Self::with_seed(0)
    }

    /// Create a new hasher with custom seed.
    pub fn with_seed(seed: u32) -> Self {
        Self {
            seed,
            v1: seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2),
            v2: seed.wrapping_add(PRIME32_2),
            v3: seed,
            v4: seed.wrapping_sub(PRIME32_1),
            buffer: [0; 16],
            buffer_len: 0,
            total_len: 0,
            large: false,
        }
    }

    /// Update the hasher with new data.
    pub fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u64;
        let mut pos = 0;

        // Fill buffer if not empty
        if self.buffer_len > 0 {
            let to_copy = (16 - self.buffer_len).min(data.len());
            self.buffer[self.buffer_len..self.buffer_len + to_copy]
                .copy_from_slice(&data[..to_copy]);
            self.buffer_len += to_copy;
            pos = to_copy;

            if self.buffer_len == 16 {
                self.process_buffer();
                self.buffer_len = 0;
            }
        }

        // Process 16-byte chunks
        while pos + 16 <= data.len() {
            self.v1 = round32(self.v1, read_u32_le(&data[pos..]));
            self.v2 = round32(self.v2, read_u32_le(&data[pos + 4..]));
            self.v3 = round32(self.v3, read_u32_le(&data[pos + 8..]));
            self.v4 = round32(self.v4, read_u32_le(&data[pos + 12..]));
            self.large = true;
            pos += 16;
        }

        // Buffer remaining bytes
        let remaining = data.len() - pos;
        if remaining > 0 {
            self.buffer[..remaining].copy_from_slice(&data[pos..]);
            self.buffer_len = remaining;
        }
    }

    fn process_buffer(&mut self) {
        self.v1 = round32(self.v1, read_u32_le(&self.buffer[..]));
        self.v2 = round32(self.v2, read_u32_le(&self.buffer[4..]));
        self.v3 = round32(self.v3, read_u32_le(&self.buffer[8..]));
        self.v4 = round32(self.v4, read_u32_le(&self.buffer[12..]));
        self.large = true;
    }

    /// Finalize and return the hash value.
    pub fn finish(&self) -> u32 {
        let mut h32 = if self.large {
            self.v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18))
        } else {
            self.seed.wrapping_add(PRIME32_5)
        };

        h32 = h32.wrapping_add(self.total_len as u32);

        // Process buffered bytes
        let remaining = &self.buffer[..self.buffer_len];
        let mut pos = 0;

        // Process 4-byte chunks
        while pos + 4 <= remaining.len() {
            h32 = h32.wrapping_add(read_u32_le(&remaining[pos..]).wrapping_mul(PRIME32_3));
            h32 = h32.rotate_left(17).wrapping_mul(PRIME32_4);
            pos += 4;
        }

        // Process remaining bytes
        while pos < remaining.len() {
            h32 = h32.wrapping_add((remaining[pos] as u32).wrapping_mul(PRIME32_5));
            h32 = h32.rotate_left(11).wrapping_mul(PRIME32_1);
            pos += 1;
        }

        // Final avalanche
        h32 ^= h32 >> 15;
        h32 = h32.wrapping_mul(PRIME32_2);
        h32 ^= h32 >> 13;
        h32 = h32.wrapping_mul(PRIME32_3);
        h32 ^= h32 >> 16;

        h32
    }

    /// Reset the hasher to initial state.
    pub fn reset(&mut self) {
        self.v1 = self.seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        self.v2 = self.seed.wrapping_add(PRIME32_2);
        self.v3 = self.seed;
        self.v4 = self.seed.wrapping_sub(PRIME32_1);
        self.buffer = [0; 16];
        self.buffer_len = 0;
        self.total_len = 0;
        self.large = false;
    }
}

impl Default for XxHash32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xxhash32_empty() {
        // Known value for empty input with seed 0
        let hash = xxhash32(&[]);
        assert_eq!(hash, 0x02CC5D05);
    }

    #[test]
    fn test_xxhash32_hello() {
        // Test with "Hello"
        let hash = xxhash32(b"Hello");
        // Value should be consistent
        assert_ne!(hash, 0);
    }

    #[test]
    fn test_xxhash32_long_data() {
        // Test with data longer than 16 bytes
        let data = vec![0x42u8; 100];
        let hash = xxhash32(&data);
        assert_ne!(hash, 0);
    }

    #[test]
    fn test_xxhash32_consistency() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let hash1 = xxhash32(data);
        let hash2 = xxhash32(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_incremental_vs_one_shot() {
        let data = b"Hello, World! This is a test of incremental hashing.";

        let one_shot = xxhash32(data);

        let mut hasher = XxHash32::new();
        hasher.update(&data[..20]);
        hasher.update(&data[20..]);
        let incremental = hasher.finish();

        assert_eq!(one_shot, incremental);
    }

    #[test]
    fn test_incremental_small() {
        let data = b"Hi";

        let one_shot = xxhash32(data);

        let mut hasher = XxHash32::new();
        hasher.update(data);
        let incremental = hasher.finish();

        assert_eq!(one_shot, incremental);
    }

    #[test]
    fn test_incremental_large() {
        let data = vec![0x55u8; 1000];

        let one_shot = xxhash32(&data);

        let mut hasher = XxHash32::new();
        for chunk in data.chunks(17) {
            hasher.update(chunk);
        }
        let incremental = hasher.finish();

        assert_eq!(one_shot, incremental);
    }

    #[test]
    fn test_with_seed() {
        let data = b"test data";
        let hash0 = xxhash32_with_seed(data, 0);
        let hash1 = xxhash32_with_seed(data, 12345);
        // Different seeds should produce different hashes
        assert_ne!(hash0, hash1);
    }
}
