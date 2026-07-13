//! Microsoft Cabinet (CAB) archive format support.
//!
//! This module implements reading of Microsoft Cabinet files (.cab).
//! CAB files are commonly used in Windows installations and software packages.
//!
//! ## Format Overview
//!
//! Cabinet files consist of:
//! - CFHEADER: Main header with file metadata
//! - CFFOLDER[]: Folder entries describing compression settings
//! - CFFILE[]: File entries with names and attributes
//! - CFDATA[]: Compressed data blocks
//!
//! ## Compression Methods
//!
//! - None (stored): No compression
//! - MSZIP: Deflate-based compression
//! - Quantum: Proprietary compression (not implemented)
//! - LZX: Dictionary-based compression (not implemented)
//!
//! ## Example
//!
//! ```no_run
//! use oxiarc_archive::CabReader;
//! use std::fs::File;
//! use std::io::BufReader;
//!
//! let file = File::open("archive.cab").unwrap();
//! let mut reader = CabReader::new(BufReader::new(file)).unwrap();
//!
//! for entry in reader.entries() {
//!     println!("{}: {} bytes", entry.name, entry.size);
//! }
//! ```

mod header;

use crate::ArchiveFormat;
use crate::lenient::{LenientWarning, LenientWarningKind};
use header::{CabFile, CabFolder, CabHeader, CompressionType};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::{CompressionMethod, Entry, EntryType, FileAttributes, OxiArcError, Result};
use oxiarc_deflate::Inflater;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

/// MSZIP LZ77 window size: the DEFLATE history persisted across CFDATA
/// blocks within a folder (per MS-CAB §MSZIP, only the Huffman tables
/// reset at block boundaries).
const MSZIP_WINDOW_SIZE: usize = 32 * 1024;

/// Cabinet archive reader.
pub struct CabReader<R> {
    reader: R,
    header: CabHeader,
    folders: Vec<CabFolder>,
    files: Vec<CabFile>,
    entries: Vec<Entry>,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
    /// When `true`, CFDATA checksum mismatches are recorded as warnings
    /// instead of aborting extraction.
    lenient: bool,
    /// Warnings accumulated in lenient mode.
    warnings: Vec<LenientWarning>,
    /// Decompressed folder data memoized by folder index, so N files
    /// sharing one folder cost one decompression instead of N.
    folder_cache: HashMap<usize, Vec<u8>>,
}

impl<R: Read + Seek> CabReader<R> {
    /// Create a new CAB reader from the given input.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read and parse header
        let header = CabHeader::read(&mut reader)?;

        // Read folder entries
        let mut folders = Vec::with_capacity(header.num_folders as usize);
        for _ in 0..header.num_folders {
            folders.push(CabFolder::read(&mut reader, header.folder_reserve_size)?);
        }

        // Seek to file entries
        reader.seek(SeekFrom::Start(header.files_offset as u64))?;

        // Read file entries
        let mut files = Vec::with_capacity(header.num_files as usize);
        for _ in 0..header.num_files {
            files.push(CabFile::read(&mut reader)?);
        }

        // Convert to Entry format
        let entries = files
            .iter()
            .map(|f| {
                let method = if f.folder_index < folders.len() as u16 {
                    match folders[f.folder_index as usize].compression_type {
                        CompressionType::None => CompressionMethod::Stored,
                        CompressionType::MsZip => CompressionMethod::Deflate,
                        CompressionType::Quantum => CompressionMethod::Unknown(0),
                        CompressionType::Lzx(_) => CompressionMethod::Unknown(0),
                        CompressionType::Unknown(code) => CompressionMethod::Unknown(code),
                    }
                } else {
                    CompressionMethod::Unknown(0)
                };

                // Build DOS attributes from CAB attributes
                let mut dos_attrs: u8 = 0;
                if f.is_readonly() {
                    dos_attrs |= 0x01;
                }
                if f.is_hidden() {
                    dos_attrs |= 0x02;
                }
                if f.is_system() {
                    dos_attrs |= 0x04;
                }

                Entry {
                    name: f.name.clone(),
                    entry_type: if f.is_directory() {
                        EntryType::Directory
                    } else {
                        EntryType::File
                    },
                    size: f.uncompressed_size as u64,
                    compressed_size: 0, // Not directly available per-file
                    method,
                    modified: f.modified_time(),
                    created: None,
                    accessed: None,
                    attributes: FileAttributes::new().with_dos(dos_attrs),
                    crc32: None,
                    comment: None,
                    link_target: None,
                    offset: 0,
                    extra: Vec::new(),
                }
            })
            .collect();

        Ok(Self {
            reader,
            header,
            folders,
            files,
            entries,
            progress: None,
            lenient: false,
            warnings: Vec::new(),
            folder_cache: HashMap::new(),
        })
    }

    /// Attach a progress callback handle.
    /// Progress is reported when `extract` or `extract_by_index` is called.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Enable or disable lenient mode.
    ///
    /// In lenient mode a CFDATA block whose stored checksum does not match
    /// the computed MS-CAB checksum is accepted, and the mismatch is
    /// recorded as a [`LenientWarning`] retrievable via
    /// [`CabReader::warnings`]. In strict mode (the default) the mismatch
    /// aborts extraction with a corruption error.
    #[must_use]
    pub fn lenient(mut self, enabled: bool) -> Self {
        self.lenient = enabled;
        self
    }

    /// Warnings recorded while reading in lenient mode.
    pub fn warnings(&self) -> &[LenientWarning] {
        &self.warnings
    }

    /// Get all entries in the archive.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Get the archive format.
    pub fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Cab
    }

    /// Get the cabinet version.
    pub fn version(&self) -> (u8, u8) {
        (self.header.version_major, self.header.version_minor)
    }

    /// Get the total cabinet size.
    pub fn cabinet_size(&self) -> u32 {
        self.header.cabinet_size
    }

    /// Get the number of folders.
    pub fn num_folders(&self) -> u16 {
        self.header.num_folders
    }

    /// Get the number of files.
    pub fn num_files(&self) -> u16 {
        self.header.num_files
    }

    /// Extract a file by entry.
    pub fn extract(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        // Find the file entry and its index
        let (index, file) = self
            .files
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == entry.name)
            .map(|(i, f)| (i, f.clone()))
            .ok_or_else(|| OxiArcError::corrupted(0, format!("File not found: {}", entry.name)))?;

        if let Some(ref handle) = self.progress {
            handle.on_entry(&entry.name, index as u64);
        }

        let data = self.extract_file(&file)?;

        if let Some(ref handle) = self.progress {
            handle.on_progress(data.len() as u64, Some(entry.size));
        }

        Ok(data)
    }

    /// Extract a file by index.
    pub fn extract_by_index(&mut self, index: usize) -> Result<Vec<u8>> {
        if index >= self.files.len() {
            return Err(OxiArcError::corrupted(
                0,
                format!("File index {} out of range", index),
            ));
        }
        let file = self.files[index].clone();

        if let Some(ref handle) = self.progress {
            handle.on_entry(&file.name, index as u64);
        }

        let data = self.extract_file(&file)?;

        if let Some(ref handle) = self.progress {
            handle.on_progress(data.len() as u64, None);
        }

        Ok(data)
    }

    /// Extract a specific file.
    fn extract_file(&mut self, file: &CabFile) -> Result<Vec<u8>> {
        // Handle special folder indices
        if file.folder_index >= 0xFFFD {
            return Err(OxiArcError::unsupported_method("Multi-cabinet spanning"));
        }

        let folder_idx = file.folder_index as usize;
        if folder_idx >= self.folders.len() {
            return Err(OxiArcError::corrupted(
                0,
                format!("Invalid folder index: {}", folder_idx),
            ));
        }

        // Decompress the folder data once and memoize it: N files sharing
        // a folder would otherwise re-decompress the entire folder N times.
        if !self.folder_cache.contains_key(&folder_idx) {
            let data = self.decompress_folder(folder_idx)?;
            self.folder_cache.insert(folder_idx, data);
        }
        let folder_data = self.folder_cache.get(&folder_idx).ok_or_else(|| {
            OxiArcError::corrupted(0, format!("folder cache miss for index {}", folder_idx))
        })?;

        // Extract the file's portion. Both fields are attacker-controlled
        // u32 values; use checked arithmetic so the addition cannot wrap on
        // 32-bit targets before the bounds check below.
        let start = file.folder_offset as usize;
        let end = start
            .checked_add(file.uncompressed_size as usize)
            .ok_or_else(|| {
                OxiArcError::corrupted(
                    0,
                    format!(
                        "File bounds overflow: offset {} + size {}",
                        file.folder_offset, file.uncompressed_size
                    ),
                )
            })?;

        if end > folder_data.len() {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "File extends beyond folder data: {} > {}",
                    end,
                    folder_data.len()
                ),
            ));
        }

        Ok(folder_data[start..end].to_vec())
    }

    /// Decompress all data blocks in a folder.
    fn decompress_folder(&mut self, folder_idx: usize) -> Result<Vec<u8>> {
        let folder = &self.folders[folder_idx];

        // Reject unsupported compression methods up front so a crafted
        // method code can never fall through to a raw-copy path.
        match folder.compression_type {
            CompressionType::Quantum => {
                return Err(OxiArcError::unsupported_method("Quantum compression"));
            }
            CompressionType::Lzx(_) => {
                return Err(OxiArcError::unsupported_method("LZX compression"));
            }
            CompressionType::Unknown(code) => {
                return Err(OxiArcError::unsupported_method(format!(
                    "CAB compression method {:#06x}",
                    code
                )));
            }
            CompressionType::None | CompressionType::MsZip => {}
        }

        // Seek to the folder's data offset
        self.reader
            .seek(SeekFrom::Start(folder.data_offset as u64))?;

        let mut output = Vec::new();

        // One long-lived Inflater per folder: MSZIP persists the 32 KiB
        // LZ77 window across CFDATA blocks (only the Huffman tables reset
        // per block), so each block after the first is decoded with the
        // last 32 KiB of cumulative folder output preloaded as dictionary.
        let mut inflater = Inflater::new();
        let compression = folder.compression_type;
        let num_blocks = folder.num_data_blocks;

        // Process each data block
        for block_index in 0..num_blocks {
            let block = CfData::read(&mut self.reader, self.header.data_reserve_size)?;

            // Read the block payload (bounded: compressed_size is u16).
            let mut data = vec![0u8; block.compressed_size as usize];
            self.reader.read_exact(&mut data)?;

            // Validate the MS-CAB rotate/XOR checksum. A stored value of 0
            // means "checksum not supplied" per the specification.
            if block.checksum != 0 {
                let computed =
                    cfdata_checksum(&data, block.compressed_size, block.uncompressed_size);
                if computed != block.checksum {
                    if self.lenient {
                        self.warnings.push(LenientWarning {
                            format: "CAB",
                            entry_name: None,
                            kind: LenientWarningKind::CrcMismatch {
                                expected: block.checksum,
                                computed,
                            },
                            message: format!(
                                "CFDATA block {} of folder {} checksum mismatch: stored {:08X}, computed {:08X}",
                                block_index, folder_idx, block.checksum, computed
                            ),
                        });
                    } else {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!(
                                "CFDATA block {} of folder {} checksum mismatch: stored {:08X}, computed {:08X}",
                                block_index, folder_idx, block.checksum, computed
                            ),
                        ));
                    }
                }
            }

            match compression {
                CompressionType::None => {
                    output.extend_from_slice(&data);
                }
                CompressionType::MsZip => {
                    // MSZIP blocks start with "CK" signature
                    if data.len() < 2 || &data[0..2] != b"CK" {
                        return Err(OxiArcError::corrupted(0, "Invalid MSZIP block signature"));
                    }

                    // Carry the LZ77 window across blocks: reset the
                    // per-block DEFLATE state, then preload the last 32 KiB
                    // of what this folder has produced so far.
                    inflater.reset();
                    if !output.is_empty() {
                        let dict_start = output.len().saturating_sub(MSZIP_WINDOW_SIZE);
                        inflater.set_dictionary(&output[dict_start..]);
                    }

                    let decompressed = inflater.inflate_reader(&mut &data[2..])?;

                    if decompressed.len() != block.uncompressed_size as usize {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!(
                                "MSZIP size mismatch: expected {}, got {}",
                                block.uncompressed_size,
                                decompressed.len()
                            ),
                        ));
                    }

                    output.extend_from_slice(&decompressed);
                }
                // Unreachable: rejected before the loop.
                CompressionType::Quantum
                | CompressionType::Lzx(_)
                | CompressionType::Unknown(_) => {
                    return Err(OxiArcError::unsupported_method("CAB compression"));
                }
            }
        }

        Ok(output)
    }
}

/// Compute the MS-CAB CFDATA checksum for one block.
///
/// The algorithm XORs the payload as little-endian 32-bit words (with the
/// documented big-endian-ish fold of a 1-3 byte tail), then folds in the
/// `cbData`/`cbUncomp` header fields the same way. Order is immaterial
/// because the combine operation is XOR; this matches both the pseudo-code
/// in the MS-CAB specification and libmspack's `cabd_checksum`, and was
/// verified against cabinets produced by the independent `cabarchive`
/// writer.
fn cfdata_checksum(data: &[u8], compressed_size: u16, uncompressed_size: u16) -> u32 {
    let mut header_bytes = [0u8; 4];
    header_bytes[0..2].copy_from_slice(&compressed_size.to_le_bytes());
    header_bytes[2..4].copy_from_slice(&uncompressed_size.to_le_bytes());
    let seed = checksum_words(&header_bytes, 0);
    checksum_words(data, seed)
}

/// XOR-fold `data` into `seed` as little-endian u32 words plus the spec's
/// tail handling for the final 1-3 bytes.
fn checksum_words(data: &[u8], seed: u32) -> u32 {
    let mut csum = seed;
    let mut chunks = data.chunks_exact(4);
    for chunk in &mut chunks {
        csum ^= u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }

    let tail = chunks.remainder();
    let mut ul: u32 = 0;
    match tail.len() {
        3 => {
            ul |= u32::from(tail[0]) << 16;
            ul |= u32::from(tail[1]) << 8;
            ul |= u32::from(tail[2]);
        }
        2 => {
            ul |= u32::from(tail[0]) << 8;
            ul |= u32::from(tail[1]);
        }
        1 => {
            ul |= u32::from(tail[0]);
        }
        _ => {}
    }
    csum ^ ul
}

/// CFDATA structure - compressed data block.
struct CfData {
    checksum: u32,
    compressed_size: u16,
    uncompressed_size: u16,
}

impl CfData {
    fn read<R: Read>(reader: &mut R, reserve_size: u8) -> Result<Self> {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf)?;

        let checksum = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let compressed_size = u16::from_le_bytes([buf[4], buf[5]]);
        let uncompressed_size = u16::from_le_bytes([buf[6], buf[7]]);

        // Skip reserved area
        if reserve_size > 0 {
            let mut skip = vec![0u8; reserve_size as usize];
            reader.read_exact(&mut skip)?;
        }

        Ok(Self {
            checksum,
            compressed_size,
            uncompressed_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cab_magic() {
        // MSCF magic number
        assert_eq!(header::MAGIC, *b"MSCF");
    }

    /// Build a minimal stored CAB in memory.
    ///
    /// Layout (84 bytes total):
    ///   CFHEADER  36 bytes  @ 0
    ///   CFFOLDER   8 bytes  @ 36
    ///   CFFILE    26 bytes  @ 44   (16 fixed + "hello.txt\0")
    ///   CFDATA    14 bytes  @ 70   (8 fixed + 6 data bytes)
    fn minimal_stored_cab() -> Vec<u8> {
        let mut cab = vec![0u8; 84];

        // --- CFHEADER (offset 0, 36 bytes) ---
        // Magic "MSCF"
        cab[0..4].copy_from_slice(b"MSCF");
        // reserved1 [4..8] = 0
        // cabinet_size [8..12]
        cab[8..12].copy_from_slice(&84u32.to_le_bytes());
        // reserved2 [12..16] = 0
        // files_offset [16..20] = 44
        cab[16..20].copy_from_slice(&44u32.to_le_bytes());
        // reserved3 [20..24] = 0
        // version_minor [24] = 3
        cab[24] = 3;
        // version_major [25] = 1
        cab[25] = 1;
        // num_folders [26..28] = 1
        cab[26..28].copy_from_slice(&1u16.to_le_bytes());
        // num_files [28..30] = 1
        cab[28..30].copy_from_slice(&1u16.to_le_bytes());
        // flags [30..32] = 0
        // set_id [32..34] = 0
        // cabinet_index [34..36] = 0

        // --- CFFOLDER (offset 36, 8 bytes) ---
        // data_offset [36..40] = 70  (CFDATA starts at byte 70)
        cab[36..40].copy_from_slice(&70u32.to_le_bytes());
        // num_data_blocks [40..42] = 1
        cab[40..42].copy_from_slice(&1u16.to_le_bytes());
        // compression_type [42..44] = 0 (stored / None)

        // --- CFFILE (offset 44, 16 fixed + 10 name bytes = 26) ---
        // uncompressed_size [44..48] = 6
        cab[44..48].copy_from_slice(&6u32.to_le_bytes());
        // folder_offset [48..52] = 0
        // folder_index [52..54] = 0
        // date [54..56] = 0
        // time [56..58] = 0
        // attributes [58..60] = 0x80 (ATTR_NAME_IS_UTF)
        cab[58..60].copy_from_slice(&0x0080u16.to_le_bytes());
        // name [60..70] = "hello.txt\0"
        cab[60..70].copy_from_slice(b"hello.txt\0");

        // --- CFDATA (offset 70, 8 fixed + 6 data = 14) ---
        // checksum [70..74] = 0
        // compressed_size [74..76] = 6
        cab[74..76].copy_from_slice(&6u16.to_le_bytes());
        // uncompressed_size [76..78] = 6
        cab[76..78].copy_from_slice(&6u16.to_le_bytes());
        // data [78..84] = "Hello!"
        cab[78..84].copy_from_slice(b"Hello!");

        cab
    }

    /// Build a CAB in memory with a single folder holding `blocks`
    /// CFDATA blocks (already-encoded payload + uncompressed size), a
    /// single file covering the folder's whole uncompressed extent, and
    /// the given compression-type code. Checksums are computed with the
    /// verified MS-CAB algorithm unless `zero_checksums` is set.
    fn build_cab(
        type_compress: u16,
        blocks: &[(Vec<u8>, u16)],
        total_uncompressed: u32,
        zero_checksums: bool,
    ) -> Vec<u8> {
        let name = b"payload.bin\0";
        let cfheader_len = 36usize;
        let cffolder_len = 8usize;
        let cffile_len = 16 + name.len();
        let data_offset = cfheader_len + cffolder_len + cffile_len;
        let data_len: usize = blocks.iter().map(|(d, _)| 8 + d.len()).sum();
        let total_len = data_offset + data_len;

        let mut cab = vec![0u8; total_len];
        cab[0..4].copy_from_slice(b"MSCF");
        cab[8..12].copy_from_slice(&(total_len as u32).to_le_bytes());
        cab[16..20].copy_from_slice(&((cfheader_len + cffolder_len) as u32).to_le_bytes());
        cab[24] = 3; // version minor
        cab[25] = 1; // version major
        cab[26..28].copy_from_slice(&1u16.to_le_bytes()); // folders
        cab[28..30].copy_from_slice(&1u16.to_le_bytes()); // files

        // CFFOLDER
        let fo = cfheader_len;
        cab[fo..fo + 4].copy_from_slice(&(data_offset as u32).to_le_bytes());
        cab[fo + 4..fo + 6].copy_from_slice(&(blocks.len() as u16).to_le_bytes());
        cab[fo + 6..fo + 8].copy_from_slice(&type_compress.to_le_bytes());

        // CFFILE
        let fi = fo + cffolder_len;
        cab[fi..fi + 4].copy_from_slice(&total_uncompressed.to_le_bytes());
        // folder_offset = 0, folder_index = 0, date/time = 0
        cab[fi + 14..fi + 16].copy_from_slice(&0x0080u16.to_le_bytes()); // UTF name
        cab[fi + 16..fi + 16 + name.len()].copy_from_slice(name);

        // CFDATA blocks
        let mut pos = data_offset;
        for (payload, uncomp) in blocks {
            let csum = if zero_checksums {
                0
            } else {
                cfdata_checksum(payload, payload.len() as u16, *uncomp)
            };
            cab[pos..pos + 4].copy_from_slice(&csum.to_le_bytes());
            cab[pos + 4..pos + 6].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            cab[pos + 6..pos + 8].copy_from_slice(&uncomp.to_le_bytes());
            cab[pos + 8..pos + 8 + payload.len()].copy_from_slice(payload);
            pos += 8 + payload.len();
        }

        cab
    }

    /// CAB-01: a spec-valid multi-block MSZIP cabinet whose second block
    /// back-references data emitted by the first block must decode. The
    /// second block is produced with the first block's output preloaded as
    /// a DEFLATE dictionary, exactly how cabarc/makecab exploit the
    /// persistent MSZIP window.
    #[test]
    fn test_cab_mszip_cross_block_back_reference() {
        use oxiarc_deflate::Deflater;

        // Block 1: distinctive patterned data.
        let d1: Vec<u8> = (0u32..4096).map(|i| ((i * 31 + 7) % 251) as u8).collect();
        // Block 2: byte-identical to block 1 so a dictionary-aware encoder
        // must emit back-references reaching across the block boundary.
        let d2 = d1.clone();

        let mut enc1 = Deflater::new(6);
        let mut s1 = b"CK".to_vec();
        s1.extend_from_slice(&enc1.compress_to_vec(&d1).expect("deflate block 1"));

        let mut enc2 = Deflater::with_dictionary(6, &d1);
        let s2_body = enc2.compress_to_vec(&d2).expect("deflate block 2");
        let mut s2 = b"CK".to_vec();
        s2.extend_from_slice(&s2_body);

        // Sanity: block 2 must actually be impossible to decode statelessly,
        // otherwise this test would not exercise the carried window.
        assert!(
            oxiarc_deflate::inflate(&s2_body).is_err()
                || oxiarc_deflate::inflate(&s2_body).expect("stateless decode") != d2,
            "block 2 must depend on the cross-block window"
        );

        let cab = build_cab(
            0x0001, // MSZIP
            &[(s1, d1.len() as u16), (s2, d2.len() as u16)],
            (d1.len() + d2.len()) as u32,
            false,
        );

        let mut reader = CabReader::new(std::io::Cursor::new(cab)).expect("CAB parse");
        let entry = reader.entries()[0].clone();
        let data = reader.extract(&entry).expect("multi-block MSZIP extract");
        let mut expected = d1;
        expected.extend_from_slice(&d2);
        assert_eq!(data, expected, "cross-block MSZIP decode mismatch");
    }

    /// CAB-02: a stored CAB with a corrupted CFDATA checksum must be
    /// rejected in strict mode and accepted-with-warning in lenient mode.
    #[test]
    fn test_cab_bad_cfdata_checksum_rejected() {
        let payload = b"Hello, cabinet!".to_vec();
        let uncomp = payload.len() as u16;
        let total = payload.len() as u32;
        let mut cab = build_cab(0x0000, &[(payload.clone(), uncomp)], total, false);

        // Corrupt one payload byte without updating the checksum.
        let last = cab.len() - 1;
        cab[last] ^= 0xFF;

        // Strict mode: extraction must fail.
        let mut reader = CabReader::new(std::io::Cursor::new(cab.clone())).expect("CAB parse");
        let entry = reader.entries()[0].clone();
        let err = reader.extract(&entry);
        assert!(err.is_err(), "bad CFDATA checksum must be rejected");

        // Lenient mode: extraction succeeds and the mismatch is recorded.
        let mut lenient_reader = CabReader::new(std::io::Cursor::new(cab))
            .expect("CAB parse")
            .lenient(true);
        let entry = lenient_reader.entries()[0].clone();
        let data = lenient_reader
            .extract(&entry)
            .expect("lenient mode must tolerate checksum mismatch");
        assert_eq!(data.len(), total as usize);
        assert_eq!(
            lenient_reader.warnings().len(),
            1,
            "lenient mode must record exactly one warning"
        );
    }

    /// CAB-02: a stored checksum of zero means "not supplied" and must not
    /// be validated (this is also what the module's other fixtures rely on).
    #[test]
    fn test_cab_zero_checksum_not_validated() {
        let payload = b"no checksum here".to_vec();
        let uncomp = payload.len() as u16;
        let total = payload.len() as u32;
        let cab = build_cab(0x0000, &[(payload.clone(), uncomp)], total, true);

        let mut reader = CabReader::new(std::io::Cursor::new(cab)).expect("CAB parse");
        let entry = reader.entries()[0].clone();
        let data = reader
            .extract(&entry)
            .expect("zero checksum must be accepted");
        assert_eq!(data, payload);
    }

    /// CAB-04: an unrecognized compression-method code must produce a clear
    /// unsupported-method error at extraction time, never a silent raw copy.
    /// Quantum and LZX (recognized but unimplemented) must behave the same.
    #[test]
    fn test_cab_unknown_and_unimplemented_methods_error() {
        for type_compress in [
            0x0004u16, 0x00FF, 0x0002, /* Quantum */
            0x0F03, /* LZX */
        ] {
            let payload = b"opaque bytes".to_vec();
            let uncomp = payload.len() as u16;
            let total = payload.len() as u32;
            let cab = build_cab(type_compress, &[(payload, uncomp)], total, true);

            let mut reader = CabReader::new(std::io::Cursor::new(cab))
                .unwrap_or_else(|e| panic!("CAB parse for method {type_compress:#06x}: {e}"));
            let entry = reader.entries()[0].clone();
            let result = reader.extract(&entry);
            assert!(
                matches!(result, Err(OxiArcError::UnsupportedMethod { .. })),
                "method {type_compress:#06x} must yield UnsupportedMethod, got {result:?}"
            );
        }
    }

    /// CAB-06: folder_offset/uncompressed_size whose sum overflows must be
    /// a clean corruption error (checked arithmetic), not a wrap/panic.
    #[test]
    fn test_cab_file_bounds_overflow_rejected() {
        let payload = b"tiny".to_vec();
        let uncomp = payload.len() as u16;
        let mut cab = build_cab(0x0000, &[(payload, uncomp)], 4, true);

        // Patch CFFILE: folder_offset = u32::MAX, size = u32::MAX.
        let fi = 36 + 8;
        cab[fi..fi + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        cab[fi + 4..fi + 8].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut reader = CabReader::new(std::io::Cursor::new(cab)).expect("CAB parse");
        let entry = reader.entries()[0].clone();
        let result = reader.extract(&entry);
        assert!(result.is_err(), "overflowing file bounds must be rejected");
    }

    /// CAB-03: repeated extraction from a shared folder decompresses the
    /// folder once (memoized), and both files still extract correctly.
    #[test]
    fn test_cab_folder_memoization_correctness() {
        let payload = b"AAAABBBB".to_vec();
        let uncomp = payload.len() as u16;
        let total = payload.len() as u32;
        let cab = build_cab(0x0000, &[(payload, uncomp)], total, false);

        let mut reader = CabReader::new(std::io::Cursor::new(cab)).expect("CAB parse");
        let entry = reader.entries()[0].clone();
        let first = reader.extract(&entry).expect("first extract");
        let second = reader.extract(&entry).expect("second extract (cached)");
        assert_eq!(first, second);
        assert_eq!(first, b"AAAABBBB");
        assert_eq!(
            reader.folder_cache.len(),
            1,
            "folder must be memoized after extraction"
        );
    }

    #[test]
    fn test_cab_progress() {
        use oxiarc_core::progress::ProgressSink;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct CountingSink {
            entries: Mutex<Vec<String>>,
            progress_calls: Mutex<u64>,
        }

        impl ProgressSink for CountingSink {
            fn on_progress(&self, _processed: u64, _total: Option<u64>) {
                *self.progress_calls.lock().expect("lock poisoned") += 1;
            }
            fn on_entry(&self, name: &str, _index: u64) {
                self.entries
                    .lock()
                    .expect("lock poisoned")
                    .push(name.to_string());
            }
            fn on_finish(&self) {}
        }

        let sink = Arc::new(CountingSink::default());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let cab_bytes = minimal_stored_cab();
        let cursor = std::io::Cursor::new(cab_bytes);
        let mut reader = CabReader::new(cursor)
            .expect("CAB parse failed")
            .with_progress(handle);

        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 1, "expected 1 entry");

        let data = reader.extract(&entries[0]).expect("extraction failed");
        assert_eq!(data, b"Hello!");

        {
            let entries_seen = sink.entries.lock().expect("lock poisoned");
            assert_eq!(entries_seen.len(), 1, "on_entry should fire exactly once");
            assert_eq!(entries_seen[0], "hello.txt");
        }
        assert_eq!(
            *sink.progress_calls.lock().expect("lock poisoned"),
            1,
            "on_progress should fire once"
        );
    }
}
