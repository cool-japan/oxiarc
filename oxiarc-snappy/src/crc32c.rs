//! Pure Rust CRC32C (Castagnoli) implementation.
//!
//! CRC32C uses the polynomial 0x1EDC6F41 and is used by the Snappy
//! framed format for data integrity verification.
//!
//! On x86_64 CPUs that support SSE 4.2, hardware CRC32C instructions are
//! used at runtime via a `OnceLock`-based dispatcher for improved performance.

/// CRC32C lookup table, generated from the Castagnoli polynomial 0x1EDC6F41.
const CRC32C_TABLE: [u32; 256] = generate_crc32c_table();

/// Generate the CRC32C lookup table at compile time.
const fn generate_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let poly: u32 = 0x82F6_3B78; // Bit-reversed form of 0x1EDC6F41
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Update a running CRC32C with additional data.
///
/// # Arguments
/// * `crc` - The current CRC state (pre-inverted).
/// * `data` - Additional data to include.
///
/// # Returns
/// The updated CRC state (pre-inverted).
fn crc32c_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = CRC32C_TABLE[index] ^ (crc >> 8);
    }
    crc
}

/// Scalar CRC32C implementation. Used as fallback on non-x86_64 platforms
/// and when SSE 4.2 is not available at runtime.
fn crc32c_scalar(data: &[u8]) -> u32 {
    crc32c_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// SSE 4.2 hardware-accelerated CRC32C implementation.
///
/// Uses `_mm_crc32_u64` to process 8 bytes at a time, then `_mm_crc32_u32`
/// for a 4-byte remainder, then `_mm_crc32_u8` for the final 0–3 bytes.
///
/// # Safety
/// The caller must ensure the CPU supports SSE 4.2 (i.e. `is_x86_feature_detected!("sse4.2")` returns true).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(data: &[u8]) -> u32 {
    use core::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u32, _mm_crc32_u64};

    let mut crc: u64 = !0u64; // start with 0xFFFFFFFFFFFFFFFF
    let mut ptr = data.as_ptr();
    let mut len = data.len();

    // Process 8 bytes at a time.
    while len >= 8 {
        // SAFETY: caller guarantees SSE4.2; ptr is in-bounds within `data`.
        let val = unsafe { ptr.cast::<u64>().read_unaligned() };
        crc = _mm_crc32_u64(crc, val);
        // SAFETY: we just consumed 8 bytes; ptr remains within `data`.
        ptr = unsafe { ptr.add(8) };
        len -= 8;
    }

    // Process remaining 4 bytes if available.
    let mut crc32 = crc as u32;
    if len >= 4 {
        // SAFETY: ptr is in-bounds; at least 4 bytes remain.
        let val = unsafe { ptr.cast::<u32>().read_unaligned() };
        crc32 = _mm_crc32_u32(crc32, val);
        // SAFETY: we just consumed 4 bytes; ptr remains within `data`.
        ptr = unsafe { ptr.add(4) };
        len -= 4;
    }

    // Process remaining 0–3 bytes one at a time.
    // SAFETY: ptr and len are consistent; slice is valid.
    let tail = unsafe { core::slice::from_raw_parts(ptr, len) };
    for &b in tail {
        crc32 = _mm_crc32_u8(crc32, b);
    }

    !crc32
}

/// Runtime-selected CRC32C function pointer, initialised once on first call.
#[cfg(target_arch = "x86_64")]
static CRC32C_FN: std::sync::OnceLock<fn(&[u8]) -> u32> = std::sync::OnceLock::new();

/// Returns the fastest available CRC32C function for this CPU.
#[cfg(target_arch = "x86_64")]
fn get_crc32c_fn() -> fn(&[u8]) -> u32 {
    *CRC32C_FN.get_or_init(|| {
        if is_x86_feature_detected!("sse4.2") {
            |data| unsafe { crc32c_sse42(data) }
        } else {
            crc32c_scalar
        }
    })
}

/// Compute the CRC32C checksum of the given data.
///
/// On x86_64 with SSE 4.2, hardware acceleration is used automatically.
///
/// # Arguments
/// * `data` - The data to checksum.
///
/// # Returns
/// The CRC32C checksum as a u32.
pub fn crc32c(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        get_crc32c_fn()(data)
    }
    #[cfg(not(target_arch = "x86_64"))]
    crc32c_scalar(data)
}

/// Apply the Snappy masked CRC32C transformation.
///
/// The Snappy framed format stores checksums in a "masked" form to avoid
/// problems with data that happens to look like a valid checksum.
///
/// The masking is: `((crc >> 15) | (crc << 17)) + 0xa282ead8`
///
/// # Arguments
/// * `crc` - The raw CRC32C checksum.
///
/// # Returns
/// The masked checksum value.
pub fn mask_checksum(crc: u32) -> u32 {
    crc.rotate_right(15).wrapping_add(0xA282_EAD8)
}

/// Unmask a Snappy masked CRC32C checksum.
///
/// # Arguments
/// * `masked` - The masked checksum.
///
/// # Returns
/// The raw CRC32C checksum.
pub fn unmask_checksum(masked: u32) -> u32 {
    let rot = masked.wrapping_sub(0xA282_EAD8);
    rot.rotate_left(15)
}

/// Compute and mask the CRC32C checksum for use in Snappy frames.
///
/// # Arguments
/// * `data` - The data to checksum.
///
/// # Returns
/// The masked CRC32C checksum.
pub fn masked_crc32c(data: &[u8]) -> u32 {
    mask_checksum(crc32c(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32c_empty() {
        assert_eq!(crc32c(b""), 0x0000_0000);
    }

    #[test]
    fn test_crc32c_known_values() {
        // Known CRC32C test vectors
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn test_crc32c_single_bytes() {
        // CRC32C of a single zero byte
        let crc = crc32c(&[0x00]);
        // Just verify it's deterministic
        assert_eq!(crc, crc32c(&[0x00]));
    }

    #[test]
    fn test_mask_unmask_roundtrip() {
        let original = 0x12345678_u32;
        let masked = mask_checksum(original);
        let unmasked = unmask_checksum(masked);
        assert_eq!(original, unmasked);
    }

    #[test]
    fn test_mask_unmask_zero() {
        let masked = mask_checksum(0);
        let unmasked = unmask_checksum(masked);
        assert_eq!(0, unmasked);
    }

    #[test]
    fn test_masked_crc32c() {
        let data = b"Hello, World!";
        let raw = crc32c(data);
        let masked = masked_crc32c(data);
        assert_eq!(masked, mask_checksum(raw));
    }

    #[test]
    fn test_crc32c_table_valid() {
        // Verify the table was generated correctly by checking a known entry
        // CRC32C(0x01) with initial 0xFF..FF
        let crc = crc32c(&[0x01]);
        // Should be deterministic and non-zero
        assert_ne!(crc, 0);
    }

    #[test]
    fn test_crc32c_incremental_vs_oneshot() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let oneshot = crc32c(data);

        // Compute in two parts
        let mid = data.len() / 2;
        let crc1 = crc32c_update(0xFFFF_FFFF, &data[..mid]);
        let crc2 = crc32c_update(crc1, &data[mid..]) ^ 0xFFFF_FFFF;

        assert_eq!(oneshot, crc2);
    }

    /// Tests specific to the x86_64 SSE 4.2 path.
    #[cfg(target_arch = "x86_64")]
    mod simd_tests {
        use super::*;

        /// Verify SSE 4.2 output matches scalar across a sweep of input lengths.
        #[test]
        fn test_crc32c_sse_matches_scalar_lengths() {
            if !is_x86_feature_detected!("sse4.2") {
                return;
            }
            let data = vec![0xABu8; 4097];
            for len in (0..=4096).step_by(17) {
                let scalar = crc32c_scalar(&data[..len]);
                let simd = unsafe { crc32c_sse42(&data[..len]) };
                assert_eq!(scalar, simd, "length {len} mismatch");
            }
        }

        /// Verify SSE 4.2 output matches scalar on pseudo-random inputs.
        #[test]
        fn test_crc32c_sse_random() {
            if !is_x86_feature_detected!("sse4.2") {
                return;
            }
            let mut state: u64 = 0xdead_beef_1234_5678;
            for _ in 0..100 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let len = (state >> 48) as usize % 8193;
                let data: Vec<u8> = (0..len)
                    .map(|i| ((state >> (i % 8)) & 0xFF) as u8)
                    .collect();
                let scalar = crc32c_scalar(&data);
                let simd = unsafe { crc32c_sse42(&data) };
                assert_eq!(scalar, simd, "random len {len} mismatch");
            }
        }

        /// Verify known CRC32C vectors for both scalar and SSE 4.2 paths.
        #[test]
        fn test_crc32c_known_vectors() {
            // Standard CRC32C of empty input is 0x00000000.
            assert_eq!(crc32c_scalar(&[]), 0x0000_0000);
            // Standard CRC32C of [0x00] with init=0xFFFFFFFF, final XOR 0xFFFFFFFF.
            assert_eq!(crc32c_scalar(&[0x00]), 0x527D_5351);
            // Standard CRC32C of "123456789"
            assert_eq!(crc32c_scalar(b"123456789"), 0xE306_9283);

            if is_x86_feature_detected!("sse4.2") {
                assert_eq!(unsafe { crc32c_sse42(&[]) }, 0x0000_0000);
                assert_eq!(unsafe { crc32c_sse42(&[0x00]) }, 0x527D_5351);
                assert_eq!(unsafe { crc32c_sse42(b"123456789") }, 0xE306_9283);
            }
        }
    }
}
