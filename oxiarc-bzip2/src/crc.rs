//! The bzip2-specific CRC-32.
//!
//! bzip2 does *not* use the reflected ISO-3309 CRC-32 (polynomial
//! `0xEDB88320`) used by ZIP/GZIP. It uses the non-reflected (MSB-first)
//! form of the same generator polynomial, `0x04C11DB7`, with initial value
//! `0xFFFFFFFF` and a final complement, processing each byte against the
//! *top* byte of the register. Using the standard reflected CRC here makes
//! every block CRC (and therefore the stream CRC) differ from real bzip2.

/// Generator polynomial (non-reflected form).
const BZ2_CRC_POLY: u32 = 0x04C1_1DB7;

/// Build the MSB-first CRC table at compile time.
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ BZ2_CRC_POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Precomputed MSB-first CRC table.
static BZ2_CRC_TABLE: [u32; 256] = build_table();

/// Incremental bzip2 block CRC calculator.
pub(crate) struct Bz2Crc {
    value: u32,
}

impl Bz2Crc {
    /// Create a new calculator with the initial register value.
    pub(crate) fn new() -> Self {
        Self { value: 0xFFFF_FFFF }
    }

    /// Reset the register for a new block.
    pub(crate) fn reset(&mut self) {
        self.value = 0xFFFF_FFFF;
    }

    /// Feed data into the CRC register.
    pub(crate) fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.value =
                (self.value << 8) ^ BZ2_CRC_TABLE[(((self.value >> 24) as u8) ^ byte) as usize];
        }
    }

    /// Finish and return the block CRC (complemented register).
    pub(crate) fn finish(&self) -> u32 {
        !self.value
    }

    /// Compute the CRC of a complete buffer in one call.
    pub(crate) fn compute(data: &[u8]) -> u32 {
        let mut crc = Self::new();
        crc.update(data);
        crc.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_of_empty_is_zero_complement() {
        // Register untouched: !0xFFFFFFFF == 0.
        assert_eq!(Bz2Crc::compute(b""), 0x0000_0000);
    }

    #[test]
    fn crc_matches_known_bzip2_block_crc() {
        // Block CRC extracted from a real libbz2 stream (bytes 10..14 of
        // `bz2.compress(b"hello bzip2 world\n" * 100)`): 0x2B907C8C.
        let data = b"hello bzip2 world\n".repeat(100);
        assert_eq!(Bz2Crc::compute(&data), 0x2B90_7C8C);
    }

    #[test]
    fn crc_differs_from_reflected_crc32() {
        // Guard against regressing to the ZIP/GZIP CRC-32.
        let data = b"123456789";
        assert_ne!(Bz2Crc::compute(data), 0xCBF4_3926);
    }

    #[test]
    fn crc_incremental_equals_oneshot() {
        let data = b"incremental bzip2 crc check";
        let mut crc = Bz2Crc::new();
        crc.update(&data[..7]);
        crc.update(&data[7..]);
        assert_eq!(crc.finish(), Bz2Crc::compute(data));
        crc.reset();
        crc.update(data);
        assert_eq!(crc.finish(), Bz2Crc::compute(data));
    }
}
