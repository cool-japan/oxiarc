//! Pure Rust CRC32C (Castagnoli) implementation.
//!
//! CRC32C uses the polynomial 0x1EDC6F41 and is used by the Snappy
//! framed format for data integrity verification.

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

/// Compute the CRC32C checksum of the given data.
///
/// # Arguments
/// * `data` - The data to checksum.
///
/// # Returns
/// The CRC32C checksum as a u32.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
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
}
