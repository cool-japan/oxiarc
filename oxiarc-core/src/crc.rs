//! CRC (Cyclic Redundancy Check) implementations.
//!
//! This module provides CRC implementations commonly used in archive formats:
//!
//! - **CRC-32 (ISO 3309)**: Used by ZIP, GZIP, PNG
//! - **CRC-64/ECMA-182**: Used by XZ format
//! - **CRC-16/ARC**: Used by LZH/LHA archives
//!
//! ## Performance Optimization
//!
//! Both CRC-32 and CRC-64 use the "slicing-by-8" technique for data ≥16 bytes,
//! processing 8 bytes at a time using 8 pre-computed lookup tables. This provides
//! significant speedup (typically 3-5x) over the traditional byte-at-a-time algorithm
//! while maintaining full compatibility.
//!
//! For smaller data (<16 bytes), a simpler single-table lookup is used to avoid
//! the overhead of the more complex slicing algorithm.
//!
//! ## Note on Hardware Acceleration
//!
//! The x86_64 SSE4.2 CRC32 instruction uses the Castagnoli polynomial (0x1EDC6F41),
//! which is different from the ISO 3309 polynomial (0xEDB88320) used by ZIP/GZIP.
//! Therefore, we use an optimized software implementation for maximum compatibility.

/// CRC-32 lookup table (polynomial 0xEDB88320, reflected).
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
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
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-32 slicing-by-8 lookup tables.
/// This pre-computes 8 tables, allowing us to process 8 bytes in parallel.
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

/// CRC-16/ARC lookup table (polynomial 0xA001, reflected).
const CRC16_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u16;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-32 calculator (ISO 3309).
///
/// This is the standard CRC-32 used by ZIP, GZIP, PNG, and many other formats.
///
/// - Polynomial: 0x04C11DB7 (reflected: 0xEDB88320)
/// - Initial value: 0xFFFFFFFF
/// - Final XOR: 0xFFFFFFFF
/// - Reflected input: Yes
/// - Reflected output: Yes
///
/// # Example
///
/// ```
/// use oxiarc_core::crc::Crc32;
///
/// let mut crc = Crc32::new();
/// crc.update(b"Hello, World!");
/// assert_eq!(crc.finalize(), 0xEC4AC3D0);
/// ```
#[derive(Debug, Clone)]
pub struct Crc32 {
    crc: u32,
}

impl Crc32 {
    /// Create a new CRC-32 calculator.
    pub fn new() -> Self {
        Self { crc: 0xFFFFFFFF }
    }

    /// Reset the CRC to its initial state.
    pub fn reset(&mut self) {
        self.crc = 0xFFFFFFFF;
    }

    /// Update the CRC with more data.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        // Use slicing-by-8 for better performance on large data
        if data.len() >= 16 {
            crc32_slice8(&mut self.crc, data);
        } else {
            crc32_sw(&mut self.crc, data);
        }
    }

    /// Get the current CRC value (without finalizing).
    #[inline(always)]
    pub fn value(&self) -> u32 {
        self.crc ^ 0xFFFFFFFF
    }

    /// Finalize and return the CRC value.
    #[inline(always)]
    pub fn finalize(self) -> u32 {
        self.crc ^ 0xFFFFFFFF
    }

    /// Compute CRC-32 for a slice in one call.
    #[inline]
    pub fn compute(data: &[u8]) -> u32 {
        let mut crc = Self::new();
        crc.update(data);
        crc.finalize()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Software CRC-32 implementation using single lookup table.
/// Best for small data (< 16 bytes).
#[inline]
fn crc32_sw(crc: &mut u32, data: &[u8]) {
    for &byte in data {
        let index = ((*crc ^ byte as u32) & 0xFF) as usize;
        *crc = CRC32_TABLE[index] ^ (*crc >> 8);
    }
}

/// Optimized CRC-32 using slicing-by-8 technique.
/// Processes 8 bytes at a time for better throughput on large data.
#[inline]
fn crc32_slice8(crc: &mut u32, data: &[u8]) {
    let mut c = *crc;
    let mut ptr = data.as_ptr();
    let end = unsafe { ptr.add(data.len()) };

    // Process 8 bytes at a time
    while unsafe { ptr.add(8) } <= end {
        // Read 8 bytes
        let bytes = unsafe { (ptr as *const [u8; 8]).read_unaligned() };

        // XOR the first 4 bytes with current CRC, then extract individual bytes
        let crc_xor = c ^ u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let b0 = (crc_xor & 0xFF) as usize;
        let b1 = ((crc_xor >> 8) & 0xFF) as usize;
        let b2 = ((crc_xor >> 16) & 0xFF) as usize;
        let b3 = ((crc_xor >> 24) & 0xFF) as usize;

        // Look up all 8 bytes using different tables
        c = CRC32_TABLE_SLICE[7][b0]
            ^ CRC32_TABLE_SLICE[6][b1]
            ^ CRC32_TABLE_SLICE[5][b2]
            ^ CRC32_TABLE_SLICE[4][b3]
            ^ CRC32_TABLE_SLICE[3][bytes[4] as usize]
            ^ CRC32_TABLE_SLICE[2][bytes[5] as usize]
            ^ CRC32_TABLE_SLICE[1][bytes[6] as usize]
            ^ CRC32_TABLE_SLICE[0][bytes[7] as usize];

        ptr = unsafe { ptr.add(8) };
    }

    // Process remaining bytes one at a time
    while ptr < end {
        let byte = unsafe { *ptr };
        c = CRC32_TABLE[((c ^ byte as u32) & 0xFF) as usize] ^ (c >> 8);
        ptr = unsafe { ptr.add(1) };
    }

    *crc = c;
}

/// CRC-64/ECMA-182 lookup table (polynomial 0xC96C5795D7870F42, reflected).
const CRC64_TABLE: [u64; 256] = {
    // ECMA-182 polynomial: 0x42F0E1EBA9EA3693 (normal)
    // Reflected: 0xC96C5795D7870F42
    let poly: u64 = 0xC96C5795D7870F42;
    let mut table = [0u64; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u64;
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
};

/// CRC-64 slicing-by-8 lookup tables.
/// This pre-computes 8 tables for processing 8 bytes in parallel.
const CRC64_TABLE_SLICE: [[u64; 256]; 8] = {
    let poly: u64 = 0xC96C5795D7870F42;
    let mut tables = [[0u64; 256]; 8];

    // First table is the standard CRC-64 table
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u64;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
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

/// CRC-64/ECMA-182 calculator.
///
/// This is the CRC-64 variant used by XZ format.
///
/// - Polynomial: 0x42F0E1EBA9EA3693 (reflected: 0xC96C5795D7870F42)
/// - Initial value: 0xFFFFFFFFFFFFFFFF
/// - Final XOR: 0xFFFFFFFFFFFFFFFF
/// - Reflected input: Yes
/// - Reflected output: Yes
///
/// # Example
///
/// ```
/// use oxiarc_core::crc::Crc64;
///
/// let mut crc = Crc64::new();
/// crc.update(b"123456789");
/// assert_eq!(crc.finalize(), 0x995DC9BBDF1939FA);
/// ```
#[derive(Debug, Clone)]
pub struct Crc64 {
    crc: u64,
}

impl Crc64 {
    /// Create a new CRC-64 calculator.
    pub fn new() -> Self {
        Self {
            crc: 0xFFFFFFFFFFFFFFFF,
        }
    }

    /// Reset the CRC to its initial state.
    pub fn reset(&mut self) {
        self.crc = 0xFFFFFFFFFFFFFFFF;
    }

    /// Update the CRC with more data.
    pub fn update(&mut self, data: &[u8]) {
        // Use slicing-by-8 for better performance on large data
        if data.len() >= 16 {
            crc64_slice8(&mut self.crc, data);
        } else {
            crc64_sw(&mut self.crc, data);
        }
    }

    /// Get the current CRC value (without finalizing).
    pub fn value(&self) -> u64 {
        self.crc ^ 0xFFFFFFFFFFFFFFFF
    }

    /// Finalize and return the CRC value.
    pub fn finalize(self) -> u64 {
        self.crc ^ 0xFFFFFFFFFFFFFFFF
    }

    /// Compute CRC-64 for a slice in one call.
    pub fn compute(data: &[u8]) -> u64 {
        let mut crc = Self::new();
        crc.update(data);
        crc.finalize()
    }
}

impl Default for Crc64 {
    fn default() -> Self {
        Self::new()
    }
}

/// Software CRC-64 implementation using single lookup table.
/// Best for small data (< 16 bytes).
#[inline]
fn crc64_sw(crc: &mut u64, data: &[u8]) {
    for &byte in data {
        let index = ((*crc ^ byte as u64) & 0xFF) as usize;
        *crc = CRC64_TABLE[index] ^ (*crc >> 8);
    }
}

/// Optimized CRC-64 using slicing-by-8 technique.
/// Processes 8 bytes at a time for better throughput on large data.
#[inline]
fn crc64_slice8(crc: &mut u64, data: &[u8]) {
    let mut c = *crc;
    let mut ptr = data.as_ptr();
    let end = unsafe { ptr.add(data.len()) };

    // Process 8 bytes at a time
    while unsafe { ptr.add(8) } <= end {
        // Read 8 bytes
        let bytes = unsafe { (ptr as *const [u8; 8]).read_unaligned() };

        // XOR the first 8 bytes with current CRC, then extract individual bytes
        let crc_xor = c ^ u64::from_le_bytes(bytes);
        let b0 = (crc_xor & 0xFF) as usize;
        let b1 = ((crc_xor >> 8) & 0xFF) as usize;
        let b2 = ((crc_xor >> 16) & 0xFF) as usize;
        let b3 = ((crc_xor >> 24) & 0xFF) as usize;
        let b4 = ((crc_xor >> 32) & 0xFF) as usize;
        let b5 = ((crc_xor >> 40) & 0xFF) as usize;
        let b6 = ((crc_xor >> 48) & 0xFF) as usize;
        let b7 = ((crc_xor >> 56) & 0xFF) as usize;

        // Look up all 8 bytes using different tables
        c = CRC64_TABLE_SLICE[7][b0]
            ^ CRC64_TABLE_SLICE[6][b1]
            ^ CRC64_TABLE_SLICE[5][b2]
            ^ CRC64_TABLE_SLICE[4][b3]
            ^ CRC64_TABLE_SLICE[3][b4]
            ^ CRC64_TABLE_SLICE[2][b5]
            ^ CRC64_TABLE_SLICE[1][b6]
            ^ CRC64_TABLE_SLICE[0][b7];

        ptr = unsafe { ptr.add(8) };
    }

    // Process remaining bytes one at a time
    while ptr < end {
        let byte = unsafe { *ptr };
        c = CRC64_TABLE[((c ^ byte as u64) & 0xFF) as usize] ^ (c >> 8);
        ptr = unsafe { ptr.add(1) };
    }

    *crc = c;
}

/// CRC-16/ARC calculator.
///
/// This is the CRC-16 variant used by LZH/LHA archives.
///
/// - Polynomial: 0x8005 (reflected: 0xA001)
/// - Initial value: 0x0000
/// - Final XOR: 0x0000
/// - Reflected input: Yes
/// - Reflected output: Yes
///
/// # Example
///
/// ```
/// use oxiarc_core::crc::Crc16;
///
/// let mut crc = Crc16::new();
/// crc.update(b"123456789");
/// assert_eq!(crc.finalize(), 0xBB3D);
/// ```
#[derive(Debug, Clone)]
pub struct Crc16 {
    crc: u16,
}

impl Crc16 {
    /// Create a new CRC-16 calculator.
    pub fn new() -> Self {
        Self { crc: 0x0000 }
    }

    /// Reset the CRC to its initial state.
    pub fn reset(&mut self) {
        self.crc = 0x0000;
    }

    /// Update the CRC with more data.
    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let index = ((self.crc ^ byte as u16) & 0xFF) as usize;
            self.crc = CRC16_TABLE[index] ^ (self.crc >> 8);
        }
    }

    /// Get the current CRC value.
    pub fn value(&self) -> u16 {
        self.crc
    }

    /// Finalize and return the CRC value.
    pub fn finalize(self) -> u16 {
        self.crc
    }

    /// Compute CRC-16 for a slice in one call.
    pub fn compute(data: &[u8]) -> u16 {
        let mut crc = Self::new();
        crc.update(data);
        crc.finalize()
    }
}

impl Default for Crc16 {
    fn default() -> Self {
        Self::new()
    }
}

/// Combined CRC calculator for streaming operations.
///
/// This is useful when you need to compute both CRC-32 and CRC-16
/// simultaneously during decompression.
#[derive(Debug, Clone)]
pub struct DualCrc {
    crc32: Crc32,
    crc16: Crc16,
}

impl DualCrc {
    /// Create a new dual CRC calculator.
    pub fn new() -> Self {
        Self {
            crc32: Crc32::new(),
            crc16: Crc16::new(),
        }
    }

    /// Reset both CRCs.
    pub fn reset(&mut self) {
        self.crc32.reset();
        self.crc16.reset();
    }

    /// Update both CRCs with data.
    /// This is optimized to compute both CRCs in a single pass for better cache locality.
    pub fn update(&mut self, data: &[u8]) {
        // For small data, use the dual path
        if data.len() < 16 {
            for &byte in data {
                let idx32 = ((self.crc32.crc ^ byte as u32) & 0xFF) as usize;
                self.crc32.crc = CRC32_TABLE[idx32] ^ (self.crc32.crc >> 8);

                let idx16 = ((self.crc16.crc ^ byte as u16) & 0xFF) as usize;
                self.crc16.crc = CRC16_TABLE[idx16] ^ (self.crc16.crc >> 8);
            }
        } else {
            // For larger data, use the optimized paths separately
            // (slicing-by-8 is more efficient despite two passes)
            self.crc32.update(data);
            self.crc16.update(data);
        }
    }

    /// Get the CRC-32 value.
    pub fn crc32(&self) -> u32 {
        self.crc32.value()
    }

    /// Get the CRC-16 value.
    pub fn crc16(&self) -> u16 {
        self.crc16.value()
    }
}

impl Default for DualCrc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_empty() {
        assert_eq!(Crc32::compute(b""), 0x00000000);
    }

    #[test]
    fn test_crc32_check() {
        // Standard CRC-32 check value for "123456789"
        assert_eq!(Crc32::compute(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_crc32_hello_world() {
        assert_eq!(Crc32::compute(b"Hello, World!"), 0xEC4AC3D0);
    }

    #[test]
    fn test_crc32_incremental() {
        let mut crc = Crc32::new();
        crc.update(b"Hello");
        crc.update(b", ");
        crc.update(b"World!");
        assert_eq!(crc.finalize(), 0xEC4AC3D0);
    }

    #[test]
    fn test_crc16_empty() {
        assert_eq!(Crc16::compute(b""), 0x0000);
    }

    #[test]
    fn test_crc16_check() {
        // Standard CRC-16/ARC check value for "123456789"
        assert_eq!(Crc16::compute(b"123456789"), 0xBB3D);
    }

    #[test]
    fn test_crc16_incremental() {
        let mut crc = Crc16::new();
        crc.update(b"12345");
        crc.update(b"6789");
        assert_eq!(crc.finalize(), 0xBB3D);
    }

    #[test]
    fn test_dual_crc() {
        let mut dual = DualCrc::new();
        dual.update(b"123456789");
        assert_eq!(dual.crc32(), 0xCBF43926);
        assert_eq!(dual.crc16(), 0xBB3D);
    }

    #[test]
    fn test_dual_crc_various_sizes() {
        // Test that DualCrc produces consistent results across different data sizes
        // and matches individual CRC computations
        for size in [1, 7, 8, 15, 16, 17, 31, 32, 64, 128, 256] {
            let data = vec![size as u8; size];

            // Compute with DualCrc
            let mut dual = DualCrc::new();
            dual.update(&data);
            let dual_crc32 = dual.crc32();
            let dual_crc16 = dual.crc16();

            // Compute individually
            let individual_crc32 = Crc32::compute(&data);
            let individual_crc16 = Crc16::compute(&data);

            assert_eq!(
                dual_crc32, individual_crc32,
                "CRC-32 mismatch for size {}",
                size
            );
            assert_eq!(
                dual_crc16, individual_crc16,
                "CRC-16 mismatch for size {}",
                size
            );
        }
    }

    #[test]
    fn test_dual_crc_incremental() {
        // Test incremental updates with mixed small/large chunks
        let mut dual = DualCrc::new();
        dual.update(b"12"); // Small chunk (< 16)
        dual.update(b"345678"); // Small chunk (< 16)
        dual.update(b"9"); // Small chunk (< 16)

        assert_eq!(dual.crc32(), 0xCBF43926);
        assert_eq!(dual.crc16(), 0xBB3D);

        // Test with larger chunks
        let mut dual2 = DualCrc::new();
        dual2.update(b"1234567890123456"); // Large chunk (≥ 16)
        let data = b"1234567890123456";
        assert_eq!(dual2.crc32(), Crc32::compute(data));
        assert_eq!(dual2.crc16(), Crc16::compute(data));
    }

    #[test]
    fn test_crc32_table_correctness() {
        // Verify a few known table entries
        assert_eq!(CRC32_TABLE[0], 0x00000000);
        assert_eq!(CRC32_TABLE[1], 0x77073096);
        assert_eq!(CRC32_TABLE[255], 0x2D02EF8D);
    }

    #[test]
    fn test_crc16_table_correctness() {
        // Verify a few known table entries
        assert_eq!(CRC16_TABLE[0], 0x0000);
        assert_eq!(CRC16_TABLE[1], 0xC0C1);
        assert_eq!(CRC16_TABLE[255], 0x4040);
    }

    #[test]
    fn test_crc64_empty() {
        assert_eq!(Crc64::compute(b""), 0x0000000000000000);
    }

    #[test]
    fn test_crc64_check() {
        // Standard CRC-64/ECMA-182 check value for "123456789"
        assert_eq!(Crc64::compute(b"123456789"), 0x995DC9BBDF1939FA);
    }

    #[test]
    fn test_crc64_incremental() {
        let mut crc = Crc64::new();
        crc.update(b"12345");
        crc.update(b"6789");
        assert_eq!(crc.finalize(), 0x995DC9BBDF1939FA);
    }

    #[test]
    fn test_crc64_table_correctness() {
        // Verify first and last table entries
        assert_eq!(CRC64_TABLE[0], 0x0000000000000000);
        // Entry for byte 0x01
        assert_eq!(CRC64_TABLE[1], 0xB32E4CBE03A75F6F);
    }

    #[test]
    fn test_crc64_slice8_table_correctness() {
        // Verify slicing tables are correctly generated
        // Table 0 should match the standard CRC table
        assert_eq!(CRC64_TABLE_SLICE[0][0], CRC64_TABLE[0]);
        assert_eq!(CRC64_TABLE_SLICE[0][1], CRC64_TABLE[1]);
        assert_eq!(CRC64_TABLE_SLICE[0][255], CRC64_TABLE[255]);

        // Each subsequent table is derived from the previous one
        for t in 1..8 {
            for i in 0..256 {
                let prev = CRC64_TABLE_SLICE[t - 1][i];
                let expected = CRC64_TABLE[(prev & 0xFF) as usize] ^ (prev >> 8);
                assert_eq!(
                    CRC64_TABLE_SLICE[t][i], expected,
                    "Table {} entry {} mismatch",
                    t, i
                );
            }
        }
    }

    #[test]
    fn test_crc64_large_data() {
        // Test with data large enough to trigger slicing-by-8
        let data = vec![0x42u8; 1024];
        let crc = Crc64::compute(&data);

        // Verify against incremental computation
        let mut crc2 = Crc64::new();
        for chunk in data.chunks(17) {
            // Use odd chunk size to test edge cases
            crc2.update(chunk);
        }

        assert_eq!(crc, crc2.finalize());
    }

    #[test]
    fn test_crc64_various_sizes() {
        // Test boundary conditions for slicing-by-8 threshold
        for size in [1, 7, 8, 15, 16, 17, 31, 32, 63, 64, 127, 128, 255, 256] {
            let data = vec![size as u8; size];
            let crc1 = Crc64::compute(&data);

            // Compute in small chunks to use non-optimized path
            let mut crc2 = Crc64::new();
            for &byte in &data {
                crc2.update(&[byte]);
            }

            assert_eq!(crc1, crc2.finalize(), "CRC mismatch for size {}", size);
        }
    }

    #[test]
    fn test_crc32_slice8_table_correctness() {
        // Verify slicing tables are correctly generated
        // Table 0 should match the standard CRC table
        assert_eq!(CRC32_TABLE_SLICE[0][0], CRC32_TABLE[0]);
        assert_eq!(CRC32_TABLE_SLICE[0][1], CRC32_TABLE[1]);
        assert_eq!(CRC32_TABLE_SLICE[0][255], CRC32_TABLE[255]);

        // Each subsequent table is derived from the previous one
        for t in 1..8 {
            for i in 0..256 {
                let prev = CRC32_TABLE_SLICE[t - 1][i];
                let expected = CRC32_TABLE[(prev & 0xFF) as usize] ^ (prev >> 8);
                assert_eq!(
                    CRC32_TABLE_SLICE[t][i], expected,
                    "Table {} entry {} mismatch",
                    t, i
                );
            }
        }
    }

    #[test]
    fn test_crc32_large_data() {
        // Test with data large enough to trigger slicing-by-8
        let data = vec![0x42u8; 1024];
        let crc = Crc32::compute(&data);

        // Verify against incremental computation
        let mut crc2 = Crc32::new();
        for chunk in data.chunks(17) {
            // Use odd chunk size to test edge cases
            crc2.update(chunk);
        }

        assert_eq!(crc, crc2.finalize());
    }

    #[test]
    fn test_crc32_various_sizes() {
        // Test boundary conditions for slicing-by-8 threshold
        for size in [1, 7, 8, 15, 16, 17, 31, 32, 63, 64, 127, 128, 255, 256] {
            let data = vec![size as u8; size];
            let crc1 = Crc32::compute(&data);

            // Compute in small chunks to use non-optimized path
            let mut crc2 = Crc32::new();
            for &byte in &data {
                crc2.update(&[byte]);
            }

            assert_eq!(crc1, crc2.finalize(), "CRC mismatch for size {}", size);
        }
    }
}
