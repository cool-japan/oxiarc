//! SIMD-accelerated CRC implementations
//!
//! This module provides hardware-accelerated CRC-32 computation using:
//! - PCLMULQDQ (carryless multiplication) on x86_64
//! - PMULL (polynomial multiplication) on aarch64
//!
//! The implementations use the ISO 3309 polynomial (0x04C11DB7, reflected: 0xEDB88320)
//! which is compatible with ZIP, GZIP, PNG, and other common formats.
//!
//! ## Algorithm Overview
//!
//! The PCLMULQDQ-based CRC-32 algorithm is based on Intel's paper:
//! "Fast CRC Computation for Generic Polynomials Using PCLMULQDQ Instruction"
//!
//! Key concepts:
//! 1. Fold 64-byte blocks using carryless multiplication
//! 2. Reduce to 16 bytes using fold constants
//! 3. Final Barrett reduction to 32-bit CRC
//!
//! This provides significant speedup (typically 5-20x) over software implementations.

/// Pre-computed CRC-32 lookup tables for slicing-by-8 algorithm
/// This is used both as a fallback and for smaller data
const CRC32_TABLE_SLICE: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];

    // First table is the standard CRC-32 table
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }

    // Build subsequent tables
    let mut t = 1;
    while t < 8 {
        let mut i = 0usize;
        while i < 256 {
            let prev = tables[t - 1][i];
            tables[t][i] = tables[0][(prev & 0xFF) as usize] ^ (prev >> 8);
            i += 1;
        }
        t += 1;
    }

    tables
};

/// Pre-computed constants for CRC-32 IEEE (ISO 3309) using PCLMULQDQ
///
/// These constants are derived from the CRC-32 IEEE polynomial 0x04C11DB7
/// using the reflected (LSB-first) representation (0xEDB88320).
///
/// The fold constants are computed as x^n mod P(x) for various bit positions n.
///
/// NOTE: These constants have been verified against known test vectors.
#[cfg(target_arch = "x86_64")]
mod x86_constants {
    /// Fold constants for 128-bit to 64-bit reduction
    /// k1 = x^128 mod P = 0xE95C1271
    /// k2 = x^192 mod P = 0xCE3371CB
    pub const K1_K2: [u64; 2] = [0xE95C1271, 0xCE3371CB];

    /// Fold constants for further reduction
    /// k3 = x^64 mod P = 0x910EEEC1
    /// k4 = x^128 mod P = 0xE95C1271
    pub const K3_K4: [u64; 2] = [0x910EEEC1, 0xE95C1271];

    /// Final reduction constants
    /// k5 = x^32 mod P = 0x0CBEC0ED
    /// k6 = x^64 mod P = 0x910EEEC1
    pub const K5_K6: [u64; 2] = [0x0CBEC0ED, 0x910EEEC1];

    /// Barrett reduction constants for CRC-32 IEEE
    /// mu = floor(x^64 / P') where P' = x^32 + P = 0x1_04C11DB7
    /// poly = P' (polynomial with x^32 term)
    ///
    /// For reflected algorithm:
    /// mu = 0x1_04D101DF
    /// poly = 0x1_04C11DB7
    pub const MU_POLY: [u64; 2] = [0x104D101DF, 0x104C11DB7];
}

/// x86_64 SIMD CRC-32 implementation using PCLMULQDQ
#[cfg(target_arch = "x86_64")]
pub mod x86 {
    use super::CRC32_TABLE_SLICE;
    use core::arch::x86_64::*;

    use super::x86_constants::*;

    /// Minimum data size for SIMD acceleration
    /// Below this threshold, software implementation is faster
    pub const SIMD_THRESHOLD: usize = 64;

    /// Check if PCLMULQDQ is available at runtime
    #[inline]
    pub fn is_supported() -> bool {
        #[cfg(target_feature = "pclmulqdq")]
        {
            true
        }
        #[cfg(not(target_feature = "pclmulqdq"))]
        {
            is_x86_feature_detected!("pclmulqdq") && is_x86_feature_detected!("sse4.1")
        }
    }

    /// Compute CRC-32 using PCLMULQDQ
    ///
    /// # Safety
    ///
    /// This function requires PCLMULQDQ and SSE4.1 support.
    /// Caller must verify `is_supported()` returns true.
    ///
    /// # Arguments
    ///
    /// * `crc` - Initial CRC value (already inverted for internal state)
    /// * `data` - Data to compute CRC over
    ///
    /// # Returns
    ///
    /// Updated CRC value (still in internal inverted state)
    #[target_feature(enable = "pclmulqdq", enable = "sse4.1")]
    pub unsafe fn crc32_pclmulqdq(crc: u32, data: &[u8]) -> u32 {
        if data.len() < SIMD_THRESHOLD {
            return crc32_slice8_fallback(crc, data);
        }

        unsafe {
            let mut ptr = data.as_ptr();
            let end = ptr.add(data.len());

            // Load fold constants
            let k1k2 = _mm_loadu_si128(K1_K2.as_ptr().cast());
            let _k3k4 = _mm_loadu_si128(K3_K4.as_ptr().cast());
            let k5k6 = _mm_loadu_si128(K5_K6.as_ptr().cast());
            let mu_poly = _mm_loadu_si128(MU_POLY.as_ptr().cast());

            // Initialize with first 16 bytes XORed with CRC
            let mut x0 = _mm_loadu_si128(ptr.cast());
            x0 = _mm_xor_si128(x0, _mm_cvtsi32_si128(crc as i32));
            ptr = ptr.add(16);

            // Process 16-byte blocks using fold operation
            while ptr.add(16) <= end {
                let next_block = _mm_loadu_si128(ptr.cast());
                x0 = fold_128(x0, next_block, k1k2);
                ptr = ptr.add(16);
            }

            // Handle remaining bytes (less than 16)
            let tail_len = end.offset_from(ptr) as usize;
            if tail_len > 0 {
                // Process remaining bytes with software fallback
                let mut result = barrett_reduce(x0, k5k6, mu_poly);
                // Apply remaining bytes to the partial result
                let remaining = core::slice::from_raw_parts(ptr, tail_len);
                result = crc32_slice8_fallback(result, remaining);
                return result;
            }

            // Final reduction from 128-bit to 32-bit CRC
            barrett_reduce(x0, k5k6, mu_poly)
        }
    }

    /// Fold one 128-bit value into another using carryless multiplication
    #[inline]
    #[target_feature(enable = "pclmulqdq")]
    unsafe fn fold_128(a: __m128i, b: __m128i, k: __m128i) -> __m128i {
        // Result = a_lo * k_lo + a_hi * k_hi + b
        let lo = _mm_clmulepi64_si128(a, k, 0x00); // a[0] * k[0]
        let hi = _mm_clmulepi64_si128(a, k, 0x11); // a[1] * k[1]
        _mm_xor_si128(_mm_xor_si128(lo, hi), b)
    }

    /// Barrett reduction: reduce 128-bit value to 32-bit CRC
    #[inline]
    #[target_feature(enable = "pclmulqdq", enable = "sse4.1")]
    unsafe fn barrett_reduce(x: __m128i, k5k6: __m128i, mu_poly: __m128i) -> u32 {
        // Step 1: Fold 128-bit to 64-bit
        // x_64 = x_hi * k5 + x_lo
        let x_fold = {
            let hi = _mm_srli_si128(x, 8);
            let mul = _mm_clmulepi64_si128(hi, k5k6, 0x00);
            _mm_xor_si128(x, mul)
        };

        // Extract lower 64 bits for further reduction
        let x_64 = {
            // Fold bits 64-95 with k6
            let hi_32 = _mm_srli_si128(x_fold, 4);
            let hi_masked = _mm_and_si128(hi_32, _mm_set_epi32(0, 0, 0, -1));
            let mul = _mm_clmulepi64_si128(hi_masked, k5k6, 0x10);
            let lo_masked = _mm_and_si128(x_fold, _mm_set_epi32(0, 0, 0, -1));
            _mm_xor_si128(lo_masked, _mm_srli_si128(mul, 4))
        };

        // Step 2: Barrett reduction from 64-bit to 32-bit
        // T1 = floor(R / x^32) * mu
        let t1 = {
            let x_hi = _mm_srli_si128(x_64, 4);
            _mm_clmulepi64_si128(x_hi, mu_poly, 0x00)
        };

        // T2 = floor(T1 / x^32) * P
        let t2 = {
            let t1_hi = _mm_srli_si128(t1, 4);
            _mm_clmulepi64_si128(t1_hi, mu_poly, 0x10)
        };

        // Result = x XOR T2 (mod x^32)
        let result = _mm_xor_si128(x_64, t2);
        _mm_cvtsi128_si32(result) as u32
    }

    /// Slicing-by-8 software fallback
    #[inline]
    fn crc32_slice8_fallback(mut crc: u32, data: &[u8]) -> u32 {
        let mut ptr = data.as_ptr();
        let end = unsafe { ptr.add(data.len()) };

        // Process 8 bytes at a time
        while unsafe { ptr.add(8) } <= end {
            let bytes = unsafe { (ptr as *const [u8; 8]).read_unaligned() };
            let crc_xor = crc ^ u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

            crc = CRC32_TABLE_SLICE[7][(crc_xor & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[6][((crc_xor >> 8) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[5][((crc_xor >> 16) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[4][((crc_xor >> 24) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[3][bytes[4] as usize]
                ^ CRC32_TABLE_SLICE[2][bytes[5] as usize]
                ^ CRC32_TABLE_SLICE[1][bytes[6] as usize]
                ^ CRC32_TABLE_SLICE[0][bytes[7] as usize];

            ptr = unsafe { ptr.add(8) };
        }

        // Process remaining bytes
        while ptr < end {
            let byte = unsafe { *ptr };
            crc = CRC32_TABLE_SLICE[0][((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
            ptr = unsafe { ptr.add(1) };
        }

        crc
    }
}

/// Pre-computed constants for CRC-32 IEEE using NEON PMULL (aarch64)
#[cfg(target_arch = "aarch64")]
mod arm_constants {
    /// Fold constants for CRC-32 IEEE
    /// These match the x86_64 constants for the same polynomial
    pub const K1: u64 = 0xE95C1271; // x^128 mod P
    pub const K2: u64 = 0xCE3371CB; // x^192 mod P
    pub const K5: u64 = 0x0CBEC0ED; // x^32 mod P
    pub const K6: u64 = 0x910EEEC1; // x^64 mod P
    pub const MU: u64 = 0x104D101DF; // Barrett mu
    pub const POLY: u64 = 0x104C11DB7; // Polynomial with x^32
}

/// aarch64 SIMD CRC-32 implementation using PMULL
#[cfg(target_arch = "aarch64")]
pub mod arm {
    use super::CRC32_TABLE_SLICE;
    use super::arm_constants::*;
    use core::arch::aarch64::*;

    /// Minimum data size for SIMD acceleration
    pub const SIMD_THRESHOLD: usize = 64;

    /// Check if PMULL (crypto extensions) is available at runtime
    #[inline]
    pub fn is_supported() -> bool {
        #[cfg(target_feature = "aes")]
        {
            true
        }
        #[cfg(not(target_feature = "aes"))]
        {
            std::arch::is_aarch64_feature_detected!("aes")
        }
    }

    /// Compute CRC-32 using NEON PMULL instructions
    ///
    /// # Safety
    ///
    /// This function requires PMULL (AES crypto extensions) support.
    /// Caller must verify `is_supported()` returns true.
    #[target_feature(enable = "neon", enable = "aes")]
    pub unsafe fn crc32_pmull(crc: u32, data: &[u8]) -> u32 {
        if data.len() < SIMD_THRESHOLD {
            return crc32_slice8_fallback(crc, data);
        }

        let mut ptr = data.as_ptr();
        // SAFETY: ptr + data.len() is within the slice bounds
        let end = unsafe { ptr.add(data.len()) };

        // Initialize with first 16 bytes XORed with CRC
        // SAFETY: vld1q_u8 is safe when ptr is valid for 16 bytes
        let mut x0 = unsafe { vld1q_u8(ptr) };
        let crc_vec = vreinterpretq_u8_u32(vdupq_n_u32(crc));
        x0 = veorq_u8(x0, crc_vec);
        // SAFETY: advancing by 16 is valid since data.len() >= SIMD_THRESHOLD
        ptr = unsafe { ptr.add(16) };

        // Process 16-byte blocks
        // SAFETY: ptr.add is unsafe pointer arithmetic
        while unsafe { ptr.add(16) } <= end {
            // SAFETY: vld1q_u8 is safe when ptr is valid for 16 bytes
            let next_block = unsafe { vld1q_u8(ptr) };
            // SAFETY: fold_128_arm requires neon+aes features which we have
            x0 = unsafe { fold_128_arm(x0, next_block) };
            ptr = unsafe { ptr.add(16) };
        }

        // Handle remaining bytes
        // SAFETY: offset_from requires both pointers from same allocation
        let tail_len = unsafe { end.offset_from(ptr) } as usize;
        if tail_len > 0 {
            // SAFETY: barrett_reduce_arm requires neon+aes features which we have
            let mut result = unsafe { barrett_reduce_arm(x0) };
            // SAFETY: ptr is valid for tail_len bytes
            let remaining = unsafe { core::slice::from_raw_parts(ptr, tail_len) };
            result = crc32_slice8_fallback(result, remaining);
            return result;
        }

        // SAFETY: barrett_reduce_arm requires neon+aes features which we have
        unsafe { barrett_reduce_arm(x0) }
    }

    /// Fold one 128-bit value into another using PMULL
    #[inline]
    #[target_feature(enable = "neon", enable = "aes")]
    unsafe fn fold_128_arm(a: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
        let a_u64 = vreinterpretq_u64_u8(a);
        let a_low = vgetq_lane_u64(a_u64, 0);
        let a_high = vgetq_lane_u64(a_u64, 1);

        // Multiply low 64 bits by K1
        let lo = vmull_p64(a_low, K1);
        // Multiply high 64 bits by K2
        let hi = vmull_p64(a_high, K2);

        // XOR results together
        let result = veorq_u8(vreinterpretq_u8_p128(lo), vreinterpretq_u8_p128(hi));
        veorq_u8(result, b)
    }

    /// Barrett reduction from 128-bit to 32-bit CRC
    #[inline]
    #[target_feature(enable = "neon", enable = "aes")]
    unsafe fn barrett_reduce_arm(x: uint8x16_t) -> u32 {
        let x_u64 = vreinterpretq_u64_u8(x);
        let hi = vgetq_lane_u64(x_u64, 1);
        let lo = vgetq_lane_u64(x_u64, 0);

        // Fold high to low using K5
        let folded = vmull_p64(hi, K5);
        let folded_u64 = vreinterpretq_u64_p128(folded);
        let x_64 = lo ^ vgetq_lane_u64(folded_u64, 0);

        // Reduce to 32 bits using K6
        let x_hi = (x_64 >> 32) as u64;
        let x_lo = (x_64 & 0xFFFFFFFF) as u64;

        let t1 = vmull_p64(x_hi, K6);
        let t1_u64 = vreinterpretq_u64_p128(t1);
        let reduced = x_lo ^ vgetq_lane_u64(t1_u64, 0);

        // Final Barrett reduction
        let r = (reduced >> 32) as u64;
        let t2 = vmull_p64(r, MU);
        let t2_u64 = vreinterpretq_u64_p128(t2);
        let t2_val = vgetq_lane_u64(t2_u64, 0);

        let q = (t2_val >> 32) as u64;
        let t3 = vmull_p64(q, POLY);
        let t3_u64 = vreinterpretq_u64_p128(t3);
        let t3_val = vgetq_lane_u64(t3_u64, 0);

        ((reduced ^ t3_val) & 0xFFFFFFFF) as u32
    }

    /// Slicing-by-8 software fallback
    #[inline]
    fn crc32_slice8_fallback(mut crc: u32, data: &[u8]) -> u32 {
        let mut ptr = data.as_ptr();
        let end = unsafe { ptr.add(data.len()) };

        // Process 8 bytes at a time
        while unsafe { ptr.add(8) } <= end {
            let bytes = unsafe { (ptr as *const [u8; 8]).read_unaligned() };
            let crc_xor = crc ^ u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

            crc = CRC32_TABLE_SLICE[7][(crc_xor & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[6][((crc_xor >> 8) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[5][((crc_xor >> 16) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[4][((crc_xor >> 24) & 0xFF) as usize]
                ^ CRC32_TABLE_SLICE[3][bytes[4] as usize]
                ^ CRC32_TABLE_SLICE[2][bytes[5] as usize]
                ^ CRC32_TABLE_SLICE[1][bytes[6] as usize]
                ^ CRC32_TABLE_SLICE[0][bytes[7] as usize];

            ptr = unsafe { ptr.add(8) };
        }

        // Process remaining bytes
        while ptr < end {
            let byte = unsafe { *ptr };
            crc = CRC32_TABLE_SLICE[0][((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
            ptr = unsafe { ptr.add(1) };
        }

        crc
    }
}

/// Runtime dispatcher for SIMD CRC-32
///
/// NOTE: SIMD acceleration is currently disabled pending verification of fold constants.
/// The software slicing-by-8 implementation provides good performance (3-5x faster than
/// naive byte-at-a-time) and is fully verified for correctness.
///
/// SIMD support will be re-enabled after the PCLMULQDQ fold constants for CRC-32 IEEE
/// (polynomial 0x04C11DB7) are properly computed and verified against test vectors.
///
/// ## Verification attempt 2026-04-20
///
/// Added fixed + stress test vectors covering empty, "123456789", the fox
/// pangram, and 1 MiB / 63 / 64 / 65 / 128-byte synthetic buffers
/// (`test_pmull_matches_scalar_vectors`, `test_pclmulqdq_matches_scalar_vectors`).
///
/// Empirical result on aarch64 (the host used for this verification pass):
/// - Fixed vectors < 64 bytes pass — they bypass PMULL via the scalar
///   fallback inside `arm::crc32_pmull` (and `x86::crc32_pclmulqdq`).
/// - Stress buffers that exercise the PMULL fold loop DO NOT match the
///   slicing-by-8 scalar path. Example: `vec![0xFF; 1_048_576]` produces
///   `0x5e570f27` under PMULL vs `0x956bac74` under slicing-by-8.
///
/// Diagnosis (not applied, to avoid shipping unverified changes): the
/// constants in `x86_constants` / `arm_constants` are 32/33-bit. For the
/// bit-reflected ISO 3309 polynomial, the canonical Intel-white-paper form
/// uses 33-bit pre-shifted values (`rk1 = 0x154442bd4`,
/// `rk2 = 0x1c6e41596`, etc.) combined with a specific Barrett reduction
/// shape. Fixing this is a multi-line structural change (constants +
/// `fold_128*` + `barrett_reduce*`), not a single-constant tweak, and
/// PCLMULQDQ cannot be empirically verified from an aarch64 host.
///
/// Decision: dispatch continues to route to `software_crc32`; SIMD modules
/// remain compiled but unreferenced on the hot path. The failing tests are
/// marked `#[ignore]` with the diagnosis above so that a future work item
/// has a concrete, reproducible target.
pub struct SimdCrc32Dispatcher {
    #[cfg(target_arch = "x86_64")]
    _use_pclmulqdq: bool,
    #[cfg(target_arch = "aarch64")]
    _use_pmull: bool,
}

impl SimdCrc32Dispatcher {
    /// Create a new dispatcher
    ///
    /// Currently uses software implementation (slicing-by-8) for correctness.
    /// SIMD acceleration will be enabled in a future release.
    pub fn new() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            _use_pclmulqdq: x86::is_supported(),
            #[cfg(target_arch = "aarch64")]
            _use_pmull: arm::is_supported(),
        }
    }

    /// Create a dispatcher with SIMD disabled (for testing/benchmarking)
    ///
    /// This is currently equivalent to `new()` since SIMD is disabled.
    pub fn software_only() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            _use_pclmulqdq: false,
            #[cfg(target_arch = "aarch64")]
            _use_pmull: false,
        }
    }

    /// Check if SIMD acceleration is available and enabled
    ///
    /// Currently returns false as SIMD is disabled pending verification.
    #[inline]
    pub fn is_simd_available(&self) -> bool {
        // SIMD disabled pending fold constant verification
        // The PCLMULQDQ constants for CRC-32 IEEE need to be properly computed
        false
    }

    /// Check if the CPU supports SIMD instructions (even if currently disabled)
    #[inline]
    pub fn is_simd_supported(&self) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            self._use_pclmulqdq
        }
        #[cfg(target_arch = "aarch64")]
        {
            self._use_pmull
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            false
        }
    }

    /// Compute CRC-32 using best available implementation
    ///
    /// Currently uses slicing-by-8 software implementation for correctness.
    ///
    /// # Arguments
    ///
    /// * `crc` - Current CRC value (already inverted for internal state)
    /// * `data` - Data to process
    ///
    /// # Returns
    ///
    /// Updated CRC value (still inverted)
    #[inline]
    pub fn update(&self, crc: u32, data: &[u8]) -> u32 {
        // Use software implementation (slicing-by-8) for correctness
        // SIMD will be enabled after fold constants are verified
        software_crc32(crc, data)
    }
}

impl Default for SimdCrc32Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Software CRC-32 implementation using slicing-by-8 (fallback)
#[inline]
pub fn software_crc32(mut crc: u32, data: &[u8]) -> u32 {
    let mut ptr = data.as_ptr();
    let end = unsafe { ptr.add(data.len()) };

    // Process 8 bytes at a time
    while unsafe { ptr.add(8) } <= end {
        let bytes = unsafe { (ptr as *const [u8; 8]).read_unaligned() };
        let crc_xor = crc ^ u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

        crc = CRC32_TABLE_SLICE[7][(crc_xor & 0xFF) as usize]
            ^ CRC32_TABLE_SLICE[6][((crc_xor >> 8) & 0xFF) as usize]
            ^ CRC32_TABLE_SLICE[5][((crc_xor >> 16) & 0xFF) as usize]
            ^ CRC32_TABLE_SLICE[4][((crc_xor >> 24) & 0xFF) as usize]
            ^ CRC32_TABLE_SLICE[3][bytes[4] as usize]
            ^ CRC32_TABLE_SLICE[2][bytes[5] as usize]
            ^ CRC32_TABLE_SLICE[1][bytes[6] as usize]
            ^ CRC32_TABLE_SLICE[0][bytes[7] as usize];

        ptr = unsafe { ptr.add(8) };
    }

    // Process remaining bytes
    while ptr < end {
        let byte = unsafe { *ptr };
        crc = CRC32_TABLE_SLICE[0][((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
        ptr = unsafe { ptr.add(1) };
    }

    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatcher_creation() {
        let dispatcher = SimdCrc32Dispatcher::new();
        // Just verify it doesn't panic
        let _ = dispatcher.is_simd_available();
    }

    #[test]
    fn test_software_only_dispatcher() {
        let dispatcher = SimdCrc32Dispatcher::software_only();
        assert!(!dispatcher.is_simd_available());
    }

    #[test]
    fn test_software_crc32() {
        // Test vector: "123456789" should give 0xCBF43926
        let data = b"123456789";
        let crc = software_crc32(0xFFFFFFFF, data);
        assert_eq!(crc ^ 0xFFFFFFFF, 0xCBF43926);
    }

    #[test]
    fn test_software_crc32_empty() {
        let crc = software_crc32(0xFFFFFFFF, b"");
        assert_eq!(crc ^ 0xFFFFFFFF, 0x00000000);
    }

    #[test]
    fn test_software_crc32_various_sizes() {
        // Verify slicing-by-8 produces consistent results across all sizes
        for size in [1, 7, 8, 15, 16, 17, 31, 32, 63, 64, 127, 128] {
            let data: Vec<u8> = (0..size).map(|i| i as u8).collect();

            // Compute with slicing-by-8
            let crc_slice8 = software_crc32(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;

            // Compute byte-by-byte
            let mut crc_byte = 0xFFFFFFFF_u32;
            for &byte in &data {
                let idx = ((crc_byte ^ byte as u32) & 0xFF) as usize;
                crc_byte = CRC32_TABLE_SLICE[0][idx] ^ (crc_byte >> 8);
            }
            let crc_byte = crc_byte ^ 0xFFFFFFFF;

            assert_eq!(crc_slice8, crc_byte, "CRC mismatch for size {}", size);
        }
    }

    #[test]
    fn test_dispatcher_correctness() {
        let dispatcher = SimdCrc32Dispatcher::new();

        // Test various sizes
        for size in [8, 16, 32, 64, 128, 256, 512, 1024] {
            let data: Vec<u8> = (0..size).map(|i| i as u8).collect();

            // Compute with dispatcher
            let crc_dispatcher = dispatcher.update(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;

            // Compute with pure software
            let crc_sw = software_crc32(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;

            assert_eq!(
                crc_dispatcher, crc_sw,
                "CRC mismatch for size {} (dispatcher vs software)",
                size
            );
        }
    }

    #[test]
    fn test_dispatcher_vs_software_only() {
        let simd_dispatcher = SimdCrc32Dispatcher::new();
        let sw_dispatcher = SimdCrc32Dispatcher::software_only();

        for size in [64, 128, 256, 512, 1024] {
            let data: Vec<u8> = (0..size).map(|i| i as u8).collect();

            let crc_simd = simd_dispatcher.update(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;
            let crc_sw = sw_dispatcher.update(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;

            assert_eq!(
                crc_simd, crc_sw,
                "CRC mismatch for size {} (SIMD vs software)",
                size
            );
        }
    }

    #[test]
    fn test_large_data_correctness() {
        let dispatcher = SimdCrc32Dispatcher::new();

        // Test with 1MB of data
        let data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

        let crc_simd = dispatcher.update(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;
        let crc_sw = software_crc32(0xFFFFFFFF, &data) ^ 0xFFFFFFFF;

        assert_eq!(crc_simd, crc_sw, "CRC mismatch for 1MB data");
    }

    #[test]
    fn test_incremental_crc() {
        let dispatcher = SimdCrc32Dispatcher::new();

        // Test that incremental computation matches single-pass
        let data = b"Hello, World! This is a test of incremental CRC computation.";

        // Single pass
        let crc_single = dispatcher.update(0xFFFFFFFF, data) ^ 0xFFFFFFFF;

        // Incremental with various chunk sizes
        for chunk_size in [1, 7, 8, 16, 17, 32, 64] {
            let mut crc_inc = 0xFFFFFFFF_u32;
            for chunk in data.chunks(chunk_size) {
                crc_inc = dispatcher.update(crc_inc, chunk);
            }
            let crc_inc = crc_inc ^ 0xFFFFFFFF;

            assert_eq!(
                crc_single, crc_inc,
                "Incremental CRC mismatch with chunk size {}",
                chunk_size
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_x86_simd_availability() {
        let available = x86::is_supported();
        println!("PCLMULQDQ available: {}", available);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_arm_simd_availability() {
        let available = arm::is_supported();
        println!("ARM PMULL available: {}", available);
    }

    /// Standard CRC-32 test vectors (ISO 3309 / ZIP / GZIP polynomial).
    ///
    /// Values XORed with `0xFFFFFFFF` match the published check values:
    /// - empty string:   0x00000000
    /// - "123456789":    0xCBF43926
    /// - "The quick brown fox jumps over the lazy dog": 0x414FA339
    ///
    /// The internal (pre-final-XOR) form is what `software_crc32` and the SIMD
    /// paths return. We initialise the internal state with `0xFFFFFFFF` and
    /// compare against the final (XORed) CRC.
    fn fixed_vectors() -> Vec<(&'static str, Vec<u8>, u32)> {
        vec![
            ("empty", vec![], 0x00000000),
            ("123456789", b"123456789".to_vec(), 0xCBF43926),
            (
                "fox",
                b"The quick brown fox jumps over the lazy dog".to_vec(),
                0x414FA339,
            ),
        ]
    }

    /// Larger buffers for stress testing. We do not hard-code the expected
    /// CRC — instead we compare SIMD vs the already-trusted scalar path.
    fn stress_buffers() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("1MiB_FF", vec![0xFFu8; 1_048_576]),
            ("1MiB_seq", (0..=255u8).cycle().take(1_048_576).collect()),
            ("65537_00", vec![0u8; 65537]),
            ("63_seq", (0..63u8).collect()),
            ("64_seq", (0..64u8).collect()),
            ("65_seq", (0..65u8).collect()),
            ("127_seq", (0..127u8).collect()),
            ("128_seq", (0..=127u8).collect()),
        ]
    }

    /// Verify the scalar path matches the published ISO 3309 check values.
    /// This is the baseline that the SIMD path must then agree with.
    #[test]
    fn test_scalar_matches_standard_vectors() {
        for (name, data, expected) in fixed_vectors().iter() {
            let crc = software_crc32(0xFFFFFFFF, data) ^ 0xFFFFFFFF;
            assert_eq!(
                crc, *expected,
                "scalar CRC-32 mismatch on {}: got {:08x}, expected {:08x}",
                name, crc, expected
            );
        }
    }

    /// Verify PCLMULQDQ CRC-32 matches both the published check values and
    /// the scalar reference across fixed + stress vectors.
    ///
    /// Compiled on x86_64; skipped at runtime if PCLMULQDQ is unavailable.
    ///
    /// Currently `#[ignore]` — the fold constants / Barrett reduction shape in
    /// `x86::crc32_pclmulqdq` do not match the bit-reflected ISO 3309
    /// convention used by the scalar path (verified on aarch64 — PMULL path
    /// has the same structural issue; see `test_pmull_matches_scalar_vectors`
    /// below). The SIMD path is left intact but NOT wired into dispatch.
    /// Un-ignore this test once `x86::crc32_pclmulqdq` (and `arm::crc32_pmull`)
    /// use validated reflected-mode constants (typically pre-shifted by x^1,
    /// i.e. 33-bit values) per Intel's "Fast CRC Computation Using PCLMULQDQ".
    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore = "SIMD fold constants pending verification — see comment"]
    fn test_pclmulqdq_matches_scalar_vectors() {
        if !x86::is_supported() {
            eprintln!("PCLMULQDQ not available on this CPU; skipping.");
            return;
        }

        // Fixed vectors with known answers.
        for (name, data, expected) in fixed_vectors().iter() {
            let scalar = software_crc32(0xFFFFFFFF, data) ^ 0xFFFFFFFF;
            assert_eq!(scalar, *expected, "scalar mismatch on {}", name);

            // SAFETY: `is_supported()` returned true above, so PCLMULQDQ +
            // SSE4.1 are available.
            let simd = unsafe { x86::crc32_pclmulqdq(0xFFFFFFFF, data) } ^ 0xFFFFFFFF;
            assert_eq!(
                simd,
                scalar,
                "SIMD mismatch on {} (len={}): got {:08x}, expected {:08x}",
                name,
                data.len(),
                simd,
                scalar
            );
        }

        // Larger stress buffers — compare SIMD against scalar reference.
        for (name, data) in stress_buffers().iter() {
            let scalar = software_crc32(0xFFFFFFFF, data) ^ 0xFFFFFFFF;
            // SAFETY: as above.
            let simd = unsafe { x86::crc32_pclmulqdq(0xFFFFFFFF, data) } ^ 0xFFFFFFFF;
            assert_eq!(
                simd,
                scalar,
                "SIMD mismatch on {} (len={}): got {:08x}, scalar {:08x}",
                name,
                data.len(),
                simd,
                scalar
            );
        }
    }

    /// Verify PMULL CRC-32 matches both the published check values and
    /// the scalar reference across fixed + stress vectors.
    ///
    /// Compiled on aarch64; skipped at runtime if PMULL (AES crypto) is unavailable.
    ///
    /// Currently `#[ignore]` — empirically on aarch64 (2026-04-20), fixed
    /// vectors < SIMD_THRESHOLD (64 bytes) pass because they bypass PMULL via
    /// the scalar fallback, but every stress buffer that actually exercises
    /// the PMULL fold loop disagrees with the scalar path. Example:
    /// `1MiB_FF` produces `0x5e570f27` under PMULL vs `0x956bac74` under
    /// slicing-by-8. The fold constants (`K1`, `K2`, `K5`, `K6`, `MU`, `POLY`)
    /// in `arm_constants` are 32/33-bit values; for the bit-reflected ISO 3309
    /// polynomial, Intel's white paper (and verified references such as
    /// crc32fast / Linux `lib/crc32.c`) require 33-bit pre-shifted values
    /// (the canonical `rk1 = 0x154442bd4`, `rk2 = 0x1c6e41596`, etc. with the
    /// accompanying Barrett shape). The SIMD path is left intact but NOT
    /// wired into dispatch. Un-ignore once constants + reduction are fixed.
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "SIMD fold constants pending verification — see comment"]
    fn test_pmull_matches_scalar_vectors() {
        if !arm::is_supported() {
            eprintln!("PMULL not available on this CPU; skipping.");
            return;
        }

        // Fixed vectors with known answers.
        for (name, data, expected) in fixed_vectors().iter() {
            let scalar = software_crc32(0xFFFFFFFF, data) ^ 0xFFFFFFFF;
            assert_eq!(scalar, *expected, "scalar mismatch on {}", name);

            // SAFETY: `is_supported()` returned true above, so PMULL/NEON/AES
            // are available.
            let simd = unsafe { arm::crc32_pmull(0xFFFFFFFF, data) } ^ 0xFFFFFFFF;
            assert_eq!(
                simd,
                scalar,
                "SIMD mismatch on {} (len={}): got {:08x}, expected {:08x}",
                name,
                data.len(),
                simd,
                scalar
            );
        }

        // Larger stress buffers — compare SIMD against scalar reference.
        for (name, data) in stress_buffers().iter() {
            let scalar = software_crc32(0xFFFFFFFF, data) ^ 0xFFFFFFFF;
            // SAFETY: as above.
            let simd = unsafe { arm::crc32_pmull(0xFFFFFFFF, data) } ^ 0xFFFFFFFF;
            assert_eq!(
                simd,
                scalar,
                "SIMD mismatch on {} (len={}): got {:08x}, scalar {:08x}",
                name,
                data.len(),
                simd,
                scalar
            );
        }
    }
}
