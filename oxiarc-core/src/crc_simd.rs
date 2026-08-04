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

/// Bit-reflected CRC-32 (ISO 3309) fold and reduction constants.
///
/// Shared by the x86_64 PCLMULQDQ and the aarch64 PMULL paths so the two
/// implementations can never drift apart. They are 33-bit pre-shifted values
/// for the reflected polynomial 0xEDB88320, taken from
/// [crc32fast](https://github.com/srijs/rust-crc32fast) and verified in this
/// crate against the scalar slicing-by-8 path (see the tests at the bottom of
/// this file).
///
/// * `K3` = x^(128+64) mod P(x) — fold-by-1, low lane
/// * `K4` = x^128 mod P(x)      — fold-by-1, high lane
/// * `K5` = x^96 mod P(x)       — 128 → 64 reduction
/// * `P_X` = P(x) with the x^32 term
/// * `U_PRIME` = floor(x^64 / P(x))
///
/// An earlier revision of the x86 path used the *non-reflected* Intel
/// whitepaper constants (0xE95C1271 / 0x104C11DB7 and friends) together with
/// reflected-style code and no input byte-swap, which produced wrong CRC-32
/// values; that is why the path was shipped disabled. It is now the same
/// arithmetic as the validated aarch64 path.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod reflected_constants {
    /// Fold-by-1 low lane constant: K3 = x^(128+64) mod P(x) reflected
    pub const K3: u64 = 0x1751997d0;
    /// Fold-by-1 high lane constant: K4 = x^128 mod P(x) reflected
    pub const K4: u64 = 0x0ccaa009e;
    /// 128→64 reduction constant: K5 = x^96 mod P(x) reflected
    pub const K5: u64 = 0x163cd6124;
    /// Barrett polynomial: P(x) with x^32 term
    pub const P_X: u64 = 0x1db710641;
    /// Barrett mu: floor(x^64 / P(x)), reflected
    pub const U_PRIME: u64 = 0x1f7011641;
}

/// x86_64 SIMD CRC-32 implementation using PCLMULQDQ
///
/// This is a direct translation of the validated aarch64 PMULL path
/// (`arm::crc32_pmull`) — same reflected constants, same fold-by-1 and Barrett
/// reduction steps, with `_mm_clmulepi64_si128` in place of `vmull_p64`.
#[cfg(target_arch = "x86_64")]
pub mod x86 {
    use super::CRC32_TABLE_SLICE;
    use core::arch::x86_64::*;

    use super::reflected_constants::{K3, K4, K5, P_X, U_PRIME};

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

        let mut ptr = data.as_ptr();
        // SAFETY: ptr + data.len() is within the slice bounds (data is a valid slice)
        let end = unsafe { ptr.add(data.len()) };

        // Fold-by-1 constants: lane 0 = K3, lane 1 = K4 (matches fold_128_arm,
        // which multiplies the low lane by K3 and the high lane by K4).
        let k3k4 = _mm_set_epi64x(K4 as i64, K3 as i64);

        // Initialize with the first 16 bytes XORed with the CRC in the low
        // 32 bits only (`_mm_cvtsi32_si128` zero-extends).
        // SAFETY: at least SIMD_THRESHOLD (>= 64) bytes are available.
        let mut x0 = unsafe { _mm_loadu_si128(ptr.cast()) };
        x0 = _mm_xor_si128(x0, _mm_cvtsi32_si128(crc as i32));
        // SAFETY: advancing by 16 is valid since data.len() >= SIMD_THRESHOLD
        ptr = unsafe { ptr.add(16) };

        // Process 16-byte blocks using the fold-by-1 operation.
        // SAFETY-FIX: avoid speculative ptr.add(16) in the loop guard (same class of
        // bug as the aarch64 crc32_pmull loop and crc32_slice8_fallback below):
        // computing an out-of-bounds pointer is UB even if it is never dereferenced.
        while (end as usize) - (ptr as usize) >= 16 {
            // SAFETY: the loop guard proves 16 readable bytes remain at ptr.
            let next_block = unsafe { _mm_loadu_si128(ptr.cast()) };
            // SAFETY: fold_128 needs pclmulqdq, guaranteed by #[target_feature].
            x0 = unsafe { fold_128(x0, next_block, k3k4) };
            // SAFETY: within bounds, checked by the loop guard before the body ran.
            ptr = unsafe { ptr.add(16) };
        }

        // Handle remaining bytes (fewer than 16) after Barrett reduction.
        // SAFETY: both pointers come from the same allocation (`data`).
        let tail_len = unsafe { end.offset_from(ptr) } as usize;
        if tail_len > 0 {
            // SAFETY: barrett_reduce needs pclmulqdq+sse4.1, both enabled here.
            let mut result = unsafe { barrett_reduce(x0, k3k4) };
            // SAFETY: ptr is valid for tail_len bytes inside the original slice.
            let remaining = unsafe { core::slice::from_raw_parts(ptr, tail_len) };
            result = crc32_slice8_fallback(result, remaining);
            return result;
        }

        // Final reduction from 128-bit to 32-bit CRC
        // SAFETY: barrett_reduce needs pclmulqdq+sse4.1, both enabled here.
        unsafe { barrett_reduce(x0, k3k4) }
    }

    /// Fold one 128-bit value into another using carryless multiplication.
    ///
    /// Computes `result = (a_low × K3) XOR (a_high × K4) XOR b`, the SSE
    /// counterpart of `fold_128_arm`.
    #[inline]
    #[target_feature(enable = "pclmulqdq")]
    unsafe fn fold_128(a: __m128i, b: __m128i, k3k4: __m128i) -> __m128i {
        let lo = _mm_clmulepi64_si128(a, k3k4, 0x00); // a[63:0]   × K3
        let hi = _mm_clmulepi64_si128(a, k3k4, 0x11); // a[127:64] × K4
        _mm_xor_si128(_mm_xor_si128(lo, hi), b)
    }

    /// Barrett reduction: reduce a 128-bit fold accumulator to the 32-bit CRC.
    ///
    /// Step-for-step translation of `barrett_reduce_arm` (whose doc comment
    /// transcribes this very SSE sequence from crc32fast):
    ///
    /// ```text
    /// x = clmul(x, k3k4, 0x10) XOR srli(x, 8)          // 128 -> 64
    /// x = clmul(x[31:0], K5, 0x00) XOR srli(x, 4)      // fold last 32 bits
    /// t1 = x[31:0]  × U_PRIME
    /// t2 = t1[31:0] × P_X
    /// crc = extract_epi32(x XOR t2, 1)                 // bits [63:32]
    /// ```
    #[inline]
    #[target_feature(enable = "pclmulqdq", enable = "sse4.1")]
    unsafe fn barrett_reduce(x: __m128i, k3k4: __m128i) -> u32 {
        // Mask keeping only the low 32 bits of the 128-bit register.
        let mask32 = _mm_set_epi32(0, 0, 0, -1);

        // --- Step 1: 128-bit -> 64-bit fold ---
        // (x_lo × K4) XOR (x_hi moved into the low 64 bits)
        let x = _mm_xor_si128(_mm_clmulepi64_si128(x, k3k4, 0x10), _mm_srli_si128(x, 8));

        // --- Step 2: fold the remaining 32 bits with K5 ---
        let k5 = _mm_set_epi64x(0, K5 as i64);
        let x = _mm_xor_si128(
            _mm_clmulepi64_si128(_mm_and_si128(x, mask32), k5, 0x00),
            _mm_srli_si128(x, 4),
        );

        // --- Step 3: Barrett reduction 64-bit -> 32-bit ---
        // Lane 0 = U_PRIME (mu), lane 1 = P_X (polynomial).
        let pu = _mm_set_epi64x(P_X as i64, U_PRIME as i64);
        let t1 = _mm_clmulepi64_si128(_mm_and_si128(x, mask32), pu, 0x00); // x[31:0]  × U_PRIME
        let t2 = _mm_clmulepi64_si128(_mm_and_si128(t1, mask32), pu, 0x10); // t1[31:0] × P_X

        // Bits [63:32] of (x XOR t2) — dword lane 1, not lane 0.
        _mm_extract_epi32(_mm_xor_si128(x, t2), 1) as u32
    }

    /// Slicing-by-8 software fallback
    #[inline]
    fn crc32_slice8_fallback(mut crc: u32, data: &[u8]) -> u32 {
        let mut ptr = data.as_ptr();
        let end = unsafe { ptr.add(data.len()) };

        // Process 8 bytes at a time
        // SAFETY-FIX: compare addresses instead of calling `ptr.add(8)` speculatively.
        // `ptr.add(n)` is itself UB when the result would land more than one byte past
        // the end of the allocation, even if the pointer is only compared and never
        // dereferenced. Address subtraction avoids constructing an out-of-bounds pointer.
        while (end as usize) - (ptr as usize) >= 8 {
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

/// aarch64 SIMD CRC-32 implementation using PMULL
///
/// Algorithm: "Fast CRC Computation for Generic Polynomials Using PCLMULQDQ"
/// (Intel whitepaper), adapted for NEON PMULL with bit-reflected constants
/// verified against crc32fast (<https://github.com/srijs/rust-crc32fast>).
#[cfg(target_arch = "aarch64")]
pub mod arm {
    use super::CRC32_TABLE_SLICE;
    use super::reflected_constants::{K3, K4, K5, P_X, U_PRIME};
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
        // SAFETY: ptr + data.len() is within the slice bounds (data is a valid slice)
        let end = unsafe { ptr.add(data.len()) };

        // Initialize with first 16 bytes XORed with CRC in the low 32 bits only.
        // vcreate_u64(crc as u64) puts CRC in bits [31:0] of the low 64-bit lane.
        // This is the ARM equivalent of SSE2 _mm_cvtsi32_si128 — CRC in low 32 bits only.
        // SAFETY: vld1q_u8 is safe when ptr is valid for 16 bytes (SIMD_THRESHOLD >= 64)
        let mut x0 = unsafe { vld1q_u8(ptr) };
        let crc_lo = vcombine_u64(vcreate_u64(crc as u64), vcreate_u64(0));
        x0 = veorq_u8(x0, vreinterpretq_u8_u64(crc_lo));
        // SAFETY: advancing by 16 is valid since data.len() >= SIMD_THRESHOLD (>= 64)
        ptr = unsafe { ptr.add(16) };

        // Fold-by-1: process remaining 16-byte blocks using K3/K4 constants.
        // SAFETY: ptr.add(16) stays within [data.as_ptr(), end] due to the loop guard
        // SAFETY-FIX: avoid speculative ptr.add(16) in the loop guard (same class of bug
        // as crc32_slice8_fallback: computing an out-of-bounds pointer is UB even when
        // it is only compared, never dereferenced).
        while (end as usize) - (ptr as usize) >= 16 {
            // SAFETY: vld1q_u8 is safe when ptr is valid for 16 bytes (loop guard ensures this)
            let next_block = unsafe { vld1q_u8(ptr) };
            // SAFETY: fold_128_arm requires neon+aes features which we have (target_feature)
            x0 = unsafe { fold_128_arm(x0, next_block) };
            // SAFETY: ptr.add(16) is within bounds (loop guard checked this before body ran)
            ptr = unsafe { ptr.add(16) };
        }

        // Handle remaining bytes (< 16) with scalar fallback after Barrett reduction.
        // SAFETY: offset_from requires both pointers from same allocation — both are within data
        let tail_len = unsafe { end.offset_from(ptr) } as usize;
        if tail_len > 0 {
            // SAFETY: barrett_reduce_arm requires neon+aes features which we have
            let mut result = unsafe { barrett_reduce_arm(x0) };
            // SAFETY: ptr is valid for tail_len bytes within the original slice allocation
            let remaining = unsafe { core::slice::from_raw_parts(ptr, tail_len) };
            result = crc32_slice8_fallback(result, remaining);
            return result;
        }

        // SAFETY: barrett_reduce_arm requires neon+aes features which we have
        unsafe { barrett_reduce_arm(x0) }
    }

    /// Fold one 128-bit value into another using PMULL (fold-by-1 step).
    ///
    /// Computes: result = (a_low × K3) XOR (a_high × K4) XOR b
    ///
    /// This matches the PCLMULQDQ `reduce128(a, b, k3k4)` from crc32fast where
    /// K3 is the low-lane constant and K4 is the high-lane constant.
    #[inline]
    #[target_feature(enable = "neon", enable = "aes")]
    unsafe fn fold_128_arm(a: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
        let a_u64 = vreinterpretq_u64_u8(a);
        // SAFETY: lane index 0/1 are valid for 2-lane u64x2
        let a_low = vgetq_lane_u64(a_u64, 0);
        let a_high = vgetq_lane_u64(a_u64, 1);

        // low  lane × K3, high lane × K4 — fold-by-1 constants
        let lo = vmull_p64(a_low, K3);
        let hi = vmull_p64(a_high, K4);

        let result = veorq_u8(vreinterpretq_u8_p128(lo), vreinterpretq_u8_p128(hi));
        veorq_u8(result, b)
    }

    /// Barrett reduction from 128-bit to 32-bit CRC.
    ///
    /// Direct ARM PMULL translation of crc32fast's PCLMULQDQ path for the
    /// bit-reflected ISO 3309 polynomial.
    ///
    /// SSE reference (crc32fast):
    /// ```text
    /// // Step 1: 128-bit → 64-bit
    /// x = clmulepi64(x, k3k4, 0x10) XOR srli(x, 8)
    ///   = (x_lo × K4)[127:0] XOR (x_hi in bits [63:0])
    ///   → x[63:0]   = (x_lo × K4)[63:0] XOR x_hi
    ///     x[127:64] = (x_lo × K4)[127:64]
    ///
    /// // Step 2: fold remaining 32 bits
    /// x = (x[31:0] × K5)[127:0] XOR srli(x, 4)
    ///   → x[63:0]   = (x[31:0] × K5)[63:0] XOR x_prev[95:32]
    ///     x[127:64] = (x[31:0] × K5)[127:64] XOR x_prev[127:96]
    ///
    /// // Step 3: Barrett 64-bit → 32-bit
    /// t1 = x[31:0] × U_PRIME          (low 32 of x × mu)
    /// t2 = t1[31:0] × P_X             (low 32 of t1 × poly)
    /// result = extract_i32(x XOR t2, 1)  = bits [63:32]
    /// ```
    #[inline]
    #[target_feature(enable = "neon", enable = "aes")]
    unsafe fn barrett_reduce_arm(x: uint8x16_t) -> u32 {
        let x_u64 = vreinterpretq_u64_u8(x);
        // All intrinsics here are safe inside #[target_feature] — no extra unsafe{} needed.
        let x_lo = vgetq_lane_u64(x_u64, 0);
        let x_hi = vgetq_lane_u64(x_u64, 1);

        // --- Step 1: 128-bit → 64-bit fold ---
        // SSE: clmulepi64(x, k3k4, 0x10) XOR srli(x, 8)
        //      = (x_lo × K4) XOR (x_hi placed at bits [63:0])
        // Result layout after XOR with shifted x_hi:
        //   new[63:0]   = (x_lo × K4)[63:0] XOR x_hi
        //   new[127:64] = (x_lo × K4)[127:64]
        let fold_p128 = vmull_p64(x_lo, K4);
        let fold_u64 = vreinterpretq_u64_p128(fold_p128);
        let fold_lo = vgetq_lane_u64(fold_u64, 0);
        let fold_hi = vgetq_lane_u64(fold_u64, 1);
        // XOR x_hi into the *low* 64 bits (matches srli(x,8) + xor in SSE)
        let x_new_lo = fold_lo ^ x_hi;
        let x_new_hi = fold_hi;

        // --- Step 2: fold remaining 32 bits with K5 ---
        // SSE: clmul(x[31:0], K5, 0x00) XOR srli(x, 4)
        // srli(x, 4) shifts by 4 bytes = 32 bits:
        //   shift_lo = x_new[95:32] = (x_new_lo >> 32) | (x_new_hi << 32)
        //   shift_hi = x_new[127:96] = x_new_hi >> 32
        let x_new_lo32 = x_new_lo & 0xFFFF_FFFF;
        let k5_p128 = vmull_p64(x_new_lo32, K5);
        let k5_u64 = vreinterpretq_u64_p128(k5_p128);
        let k5_lo = vgetq_lane_u64(k5_u64, 0);
        // Only the low 64 bits feed the Barrett reduction; high bits are discarded.
        let shift_lo = (x_new_lo >> 32) | (x_new_hi << 32);
        let x2_lo = k5_lo ^ shift_lo;

        // --- Step 3: Barrett reduction 64-bit → 32-bit ---
        // t1 = (x2[31:0] × U_PRIME) — low 32 bits of x2_lo as input
        let x2_lo32 = x2_lo & 0xFFFF_FFFF;
        let t1_p128 = vmull_p64(x2_lo32, U_PRIME);
        let t1_u64 = vreinterpretq_u64_p128(t1_p128);
        let t1_val = vgetq_lane_u64(t1_u64, 0);
        // t2 = (t1[31:0] × P_X) — low 32 bits of t1
        let t1_lo32 = t1_val & 0xFFFF_FFFF;
        let t2_p128 = vmull_p64(t1_lo32, P_X);
        let t2_u64 = vreinterpretq_u64_p128(t2_p128);
        let t2_val = vgetq_lane_u64(t2_u64, 0);

        // Result = bits [63:32] of (x2_lo XOR t2_lo)
        // Equivalent to extract_epi32(x XOR t2, 1) = bits [63:32]
        ((x2_lo ^ t2_val) >> 32) as u32
    }

    /// Slicing-by-8 software fallback
    #[inline]
    fn crc32_slice8_fallback(mut crc: u32, data: &[u8]) -> u32 {
        let mut ptr = data.as_ptr();
        let end = unsafe { ptr.add(data.len()) };

        // Process 8 bytes at a time
        // SAFETY-FIX: compare addresses instead of calling `ptr.add(8)` speculatively.
        // `ptr.add(n)` is itself UB when the result would land more than one byte past
        // the end of the allocation, even if the pointer is only compared and never
        // dereferenced. Address subtraction avoids constructing an out-of-bounds pointer.
        while (end as usize) - (ptr as usize) >= 8 {
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
/// ## Acceleration status
///
/// Both SIMD paths use the same bit-reflected ISO 3309 constants
/// (`reflected_constants`: K3/K4/K5/P_X/U_PRIME, 33-bit pre-shifted values
/// derived from [crc32fast](https://github.com/srijs/rust-crc32fast)) and the
/// same fold-by-1 plus Barrett reduction shape.
///
/// **aarch64 (Apple Silicon / Cortex-A):** PMULL path is **enabled** when
/// `is_aarch64_feature_detected!("aes")` returns true at runtime (AES implies
/// PMULL on all known aarch64 microarchitectures). Verified against the scalar
/// slicing-by-8 path for all lengths 0–4096 bytes and 100 random inputs.
///
/// **x86_64 (PCLMULQDQ):** **enabled** when `pclmulqdq` and `sse4.1` are
/// detected at runtime. The path was corrected and validated in 0.4.1: it
/// previously combined non-reflected constants with reflected-mode code and
/// returned the wrong dword lane, which is why dispatch used to be hardcoded
/// off. Verification was performed by executing the `x86_64-apple-darwin`
/// test suite under Rosetta 2 translation (which provides PCLMULQDQ/SSE4.1)
/// and comparing against the scalar path over the fixed ISO 3309 check
/// vectors, the 1 MiB stress buffers, every length 0–4096, and 100 random
/// inputs with both zero and non-zero seed CRCs — see
/// `test_pclmulqdq_matches_scalar_vectors`, `test_pclmulqdq_length_sweep` and
/// `test_pclmulqdq_random_inputs`. It has not been run on native x86_64
/// silicon in this repository.
///
/// **Other architectures:** always use slicing-by-8 software fallback.
pub struct SimdCrc32Dispatcher {
    #[cfg(target_arch = "x86_64")]
    use_pclmulqdq: bool,
    #[cfg(target_arch = "aarch64")]
    use_pmull: bool,
}

impl SimdCrc32Dispatcher {
    /// Create a new dispatcher, enabling SIMD acceleration when available.
    ///
    /// On aarch64, enables PMULL if `is_aarch64_feature_detected!("aes")` returns true.
    /// On x86_64, enables PCLMULQDQ if both `pclmulqdq` and `sse4.1` are detected.
    pub fn new() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            use_pclmulqdq: x86::is_supported(),
            #[cfg(target_arch = "aarch64")]
            use_pmull: arm::is_supported(),
        }
    }

    /// Create a dispatcher with SIMD disabled (for testing/benchmarking).
    ///
    /// Always uses the slicing-by-8 software path, regardless of what the CPU
    /// supports.
    pub fn software_only() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            use_pclmulqdq: false,
            #[cfg(target_arch = "aarch64")]
            use_pmull: false,
        }
    }

    /// Check if SIMD acceleration is available and enabled for this dispatcher.
    ///
    /// This reflects what [`Self::update`] will actually do, so it differs from
    /// [`Self::is_simd_supported`] only for a dispatcher built by
    /// [`Self::software_only`].
    #[inline]
    pub fn is_simd_available(&self) -> bool {
        #[cfg(target_arch = "aarch64")]
        {
            self.use_pmull
        }
        #[cfg(target_arch = "x86_64")]
        {
            self.use_pclmulqdq
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            false
        }
    }

    /// Check if the CPU supports SIMD instructions (even if currently disabled)
    #[inline]
    pub fn is_simd_supported(&self) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            self.use_pclmulqdq
        }
        #[cfg(target_arch = "aarch64")]
        {
            self.use_pmull
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            false
        }
    }

    /// Compute CRC-32 using best available implementation.
    ///
    /// On aarch64 with AES/PMULL extensions, uses `arm::crc32_pmull`.
    /// On x86_64 with PCLMULQDQ + SSE4.1, uses `x86::crc32_pclmulqdq`.
    /// On other architectures, always uses slicing-by-8.
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
        #[cfg(target_arch = "aarch64")]
        {
            if self.use_pmull {
                // SAFETY: use_pmull is only true when `arm::is_supported()` returned true,
                // which requires the AES (and therefore PMULL) CPU feature to be present.
                return unsafe { arm::crc32_pmull(crc, data) };
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            if self.use_pclmulqdq {
                // SAFETY: use_pclmulqdq is only true when `x86::is_supported()` returned
                // true, which requires both the PCLMULQDQ and SSE4.1 CPU features.
                return unsafe { x86::crc32_pclmulqdq(crc, data) };
            }
        }
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
    // SAFETY-FIX: see crc32_slice8_fallback above — avoid speculative ptr.add(8).
    while (end as usize) - (ptr as usize) >= 8 {
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
    /// This test used to be `#[ignore]`d because `x86::crc32_pclmulqdq` mixed
    /// the *non-reflected* Intel whitepaper constants into reflected-mode code
    /// (and extracted the result from the wrong dword lane), so it produced
    /// wrong CRC-32 values. The path now uses the same reflected constants and
    /// the same fold/Barrett steps as the validated aarch64 PMULL path, and
    /// this test — together with `test_pclmulqdq_length_sweep` and
    /// `test_pclmulqdq_random_inputs` — asserts equality with the scalar
    /// reference on every x86_64 test run.
    #[cfg(target_arch = "x86_64")]
    #[test]
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

    /// Length sweep across every alignment class of the PCLMULQDQ path:
    /// below the SIMD threshold, exact multiples of 16, and tails of 1..15.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_pclmulqdq_length_sweep() {
        if !x86::is_supported() {
            eprintln!("PCLMULQDQ not available on this CPU; skipping.");
            return;
        }
        let data: Vec<u8> = (0..8192u32)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        for len in 0..=4096usize {
            let scalar = software_crc32(0, &data[..len]);
            // SAFETY: is_supported() verified PCLMULQDQ + SSE4.1 are present.
            let simd = unsafe { x86::crc32_pclmulqdq(0, &data[..len]) };
            assert_eq!(scalar, simd, "length {len} mismatch");
        }
    }

    /// Randomised cross-check of the PCLMULQDQ path against the scalar path.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_pclmulqdq_random_inputs() {
        if !x86::is_supported() {
            eprintln!("PCLMULQDQ not available on this CPU; skipping.");
            return;
        }
        // Simple LCG for deterministic pseudo-random inputs.
        let mut state: u64 = 0xdeadbeefcafe1234;
        for _ in 0..100 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 48) as usize % 8193;
            let data: Vec<u8> = (0..len)
                .map(|i| ((state >> (i % 8)) & 0xFF) as u8)
                .collect();
            let scalar = software_crc32(0, &data);
            // SAFETY: is_supported() verified PCLMULQDQ + SSE4.1 are present.
            let simd = unsafe { x86::crc32_pclmulqdq(0, &data) };
            assert_eq!(scalar, simd, "random input len {len} mismatch");

            // Non-zero seed CRC (continuation) must agree as well.
            let scalar_seeded = software_crc32(0x1234_5678, &data);
            // SAFETY: as above.
            let simd_seeded = unsafe { x86::crc32_pclmulqdq(0x1234_5678, &data) };
            assert_eq!(scalar_seeded, simd_seeded, "seeded len {len} mismatch");
        }
    }

    /// Verify PMULL CRC-32 matches both the published check values and
    /// the scalar reference across fixed + stress vectors.
    ///
    /// Compiled on aarch64; skipped at runtime if PMULL (AES crypto) is unavailable.
    #[cfg(target_arch = "aarch64")]
    #[test]
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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_pmull_length_sweep() {
        if !arm::is_supported() {
            return;
        }
        let data = vec![0x42u8; 8192];
        for len in [
            0, 1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 1023, 1024, 4095, 4096,
        ] {
            let scalar = software_crc32(0, &data[..len]);
            // SAFETY: is_supported() verified AES/PMULL CPU feature is present.
            let simd = unsafe { arm::crc32_pmull(0, &data[..len]) };
            assert_eq!(scalar, simd, "length {len} mismatch");
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn test_pmull_random_inputs() {
        if !arm::is_supported() {
            return;
        }
        // Simple LCG for deterministic pseudo-random
        let mut state: u64 = 0xdeadbeefcafe1234;
        for _ in 0..100 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 48) as usize % 8193;
            let data: Vec<u8> = (0..len)
                .map(|i| ((state >> (i % 8)) & 0xFF) as u8)
                .collect();
            let scalar = software_crc32(0, &data);
            // SAFETY: is_supported() verified AES/PMULL CPU feature is present.
            let simd = unsafe { arm::crc32_pmull(0, &data) };
            assert_eq!(scalar, simd, "random input len {len} mismatch");
        }
    }
}
