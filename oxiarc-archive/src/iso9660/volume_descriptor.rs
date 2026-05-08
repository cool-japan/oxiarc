//! ISO 9660 Volume Descriptor parsing.
//!
//! Each volume descriptor occupies exactly 2048 bytes (one logical block).
//! Descriptors start at LBA 16 and are walked until a Terminator is found.
//!
//! Field offsets documented against ECMA-119 §8.

use crate::iso9660::joliet::decode_ucs2_be;

/// ISO 9660 escape sequences that identify a Joliet SVD.
const JOLIET_ESCAPES: &[[u8; 3]] = &[
    [0x25, 0x2F, 0x40], // %/@  UCS-2 Level 1
    [0x25, 0x2F, 0x43], // %/C  UCS-2 Level 2
    [0x25, 0x2F, 0x45], // %/E  UCS-2 Level 3
];

/// Parsed volume descriptor variants.
pub enum VolumeDescriptor {
    /// Primary Volume Descriptor (type 1).
    Primary {
        /// LBA of the root directory extent.
        root_dir_lba: u32,
        /// Size (in bytes) of the root directory extent.
        root_dir_size: u32,
        /// Volume identifier string (space-trimmed).
        volume_id: String,
    },
    /// Joliet Supplementary Volume Descriptor (type 2 with Joliet escapes).
    Joliet {
        /// LBA of the Joliet root directory extent.
        root_dir_lba: u32,
        /// Size (in bytes) of the Joliet root directory extent.
        root_dir_size: u32,
    },
    /// Volume Descriptor Set Terminator (type 255).
    Terminator,
    /// Any other descriptor type (ignored).
    Other,
}

/// Parse a 2048-byte volume descriptor sector.
///
/// Returns the appropriate [`VolumeDescriptor`] variant.
pub fn parse_volume_descriptor(sector: &[u8; 2048]) -> VolumeDescriptor {
    let vd_type = sector[0];

    // Verify the ISO 9660 / ECMA-119 identifier "CD001"
    if &sector[1..6] != b"CD001" {
        return VolumeDescriptor::Other;
    }

    match vd_type {
        1 => {
            // Primary Volume Descriptor — ECMA-119 §8.4
            let volume_id = String::from_utf8_lossy(&sector[40..72])
                .trim_end()
                .to_owned();

            let root_dir_lba = u32::from_le_bytes([
                sector[156 + 2],
                sector[156 + 3],
                sector[156 + 4],
                sector[156 + 5],
            ]);
            let root_dir_size = u32::from_le_bytes([
                sector[156 + 10],
                sector[156 + 11],
                sector[156 + 12],
                sector[156 + 13],
            ]);

            VolumeDescriptor::Primary {
                root_dir_lba,
                root_dir_size,
                volume_id,
            }
        }
        2 => {
            // Supplementary Volume Descriptor — check for Joliet escape sequences
            // Escape sequences are at bytes 88-90 for Joliet (ECMA-119 §8.5)
            let escape_seq = [sector[88], sector[89], sector[90]];
            let is_joliet = JOLIET_ESCAPES.contains(&escape_seq);

            if is_joliet {
                // Joliet root directory record is at the same offset as PVD's root dir record
                let root_dir_lba = u32::from_le_bytes([
                    sector[156 + 2],
                    sector[156 + 3],
                    sector[156 + 4],
                    sector[156 + 5],
                ]);
                let root_dir_size = u32::from_le_bytes([
                    sector[156 + 10],
                    sector[156 + 11],
                    sector[156 + 12],
                    sector[156 + 13],
                ]);

                VolumeDescriptor::Joliet {
                    root_dir_lba,
                    root_dir_size,
                }
            } else {
                // Non-Joliet SVD: not currently supported
                VolumeDescriptor::Other
            }
        }
        255 => VolumeDescriptor::Terminator,
        _ => VolumeDescriptor::Other,
    }
}

/// Read the volume identifier from a Primary Volume Descriptor sector.
///
/// Exposed for info display. Assumes the caller has verified type == 1.
pub fn read_volume_id_from_pvd(sector: &[u8; 2048]) -> String {
    String::from_utf8_lossy(&sector[40..72])
        .trim_end()
        .to_owned()
}

/// Read the logical block size from a volume descriptor sector (BWORD at bytes 128-131).
///
/// Always expected to be 2048 for valid ISO 9660 images.
pub fn read_logical_block_size(sector: &[u8; 2048]) -> u16 {
    u16::from_le_bytes([sector[128], sector[129]])
}

/// Read volume space size (total LBA count) from a volume descriptor.
pub fn read_volume_space_size(sector: &[u8; 2048]) -> u32 {
    u32::from_le_bytes([sector[80], sector[81], sector[82], sector[83]])
}

/// Decode a Joliet volume identifier from a Supplementary VD.
///
/// The volume identifier in an SVD is stored as UCS-2 BE at bytes 40-71.
pub fn read_joliet_volume_id(sector: &[u8; 2048]) -> String {
    decode_ucs2_be(&sector[40..72]).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pvd_basic() {
        let mut sector = [0u8; 2048];
        sector[0] = 1;
        sector[1..6].copy_from_slice(b"CD001");
        sector[6] = 1;
        // Volume ID: "TESTVOL " padded to 32 bytes
        sector[40..47].copy_from_slice(b"TESTVOL");
        for b in sector[47..72].iter_mut() {
            *b = b' ';
        }
        // Root dir record at bytes 156-189: LBA=20, size=128
        sector[156 + 2..156 + 6].copy_from_slice(&20u32.to_le_bytes());
        sector[156 + 6..156 + 10].copy_from_slice(&20u32.to_be_bytes());
        sector[156 + 10..156 + 14].copy_from_slice(&128u32.to_le_bytes());
        sector[156 + 14..156 + 18].copy_from_slice(&128u32.to_be_bytes());

        match parse_volume_descriptor(&sector) {
            VolumeDescriptor::Primary {
                root_dir_lba,
                root_dir_size,
                volume_id,
            } => {
                assert_eq!(root_dir_lba, 20);
                assert_eq!(root_dir_size, 128);
                assert_eq!(volume_id, "TESTVOL");
            }
            _ => panic!("expected Primary"),
        }
    }

    #[test]
    fn test_parse_joliet_svd() {
        let mut sector = [0u8; 2048];
        sector[0] = 2;
        sector[1..6].copy_from_slice(b"CD001");
        sector[6] = 1;
        // Joliet escape %/E
        sector[88] = 0x25;
        sector[89] = 0x2F;
        sector[90] = 0x45;
        // Root dir at LBA 21
        sector[156 + 2..156 + 6].copy_from_slice(&21u32.to_le_bytes());
        sector[156 + 6..156 + 10].copy_from_slice(&21u32.to_be_bytes());
        sector[156 + 10..156 + 14].copy_from_slice(&128u32.to_le_bytes());
        sector[156 + 14..156 + 18].copy_from_slice(&128u32.to_be_bytes());

        match parse_volume_descriptor(&sector) {
            VolumeDescriptor::Joliet {
                root_dir_lba,
                root_dir_size,
            } => {
                assert_eq!(root_dir_lba, 21);
                assert_eq!(root_dir_size, 128);
            }
            _ => panic!("expected Joliet"),
        }
    }

    #[test]
    fn test_parse_terminator() {
        let mut sector = [0u8; 2048];
        sector[0] = 255;
        sector[1..6].copy_from_slice(b"CD001");
        sector[6] = 1;
        match parse_volume_descriptor(&sector) {
            VolumeDescriptor::Terminator => {}
            _ => panic!("expected Terminator"),
        }
    }

    #[test]
    fn test_invalid_identifier() {
        let mut sector = [0u8; 2048];
        sector[0] = 1;
        sector[1..6].copy_from_slice(b"XXXXX"); // invalid
        match parse_volume_descriptor(&sector) {
            VolumeDescriptor::Other => {}
            _ => panic!("expected Other for invalid identifier"),
        }
    }
}
