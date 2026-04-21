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
use header::{CabFile, CabFolder, CabHeader, CompressionType};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::{CompressionMethod, Entry, EntryType, FileAttributes, OxiArcError, Result};
use oxiarc_deflate::inflate;
use std::io::{Read, Seek, SeekFrom};

/// Cabinet archive reader.
pub struct CabReader<R> {
    reader: R,
    header: CabHeader,
    folders: Vec<CabFolder>,
    files: Vec<CabFile>,
    entries: Vec<Entry>,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
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
        })
    }

    /// Attach a progress callback handle.
    /// Progress is reported when `extract` or `extract_by_index` is called.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
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

        // Decompress the folder data
        let folder_data = self.decompress_folder(folder_idx)?;

        // Extract the file's portion
        let start = file.folder_offset as usize;
        let end = start + file.uncompressed_size as usize;

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

        // Seek to the folder's data offset
        self.reader
            .seek(SeekFrom::Start(folder.data_offset as u64))?;

        let mut output = Vec::new();

        // Process each data block
        for _ in 0..folder.num_data_blocks {
            let block = CfData::read(&mut self.reader, self.header.data_reserve_size)?;

            match folder.compression_type {
                CompressionType::None => {
                    // Read raw data
                    let mut data = vec![0u8; block.compressed_size as usize];
                    self.reader.read_exact(&mut data)?;
                    output.extend_from_slice(&data);
                }
                CompressionType::MsZip => {
                    // MSZIP blocks start with "CK" signature
                    let mut compressed = vec![0u8; block.compressed_size as usize];
                    self.reader.read_exact(&mut compressed)?;

                    if compressed.len() < 2 || &compressed[0..2] != b"CK" {
                        return Err(OxiArcError::corrupted(0, "Invalid MSZIP block signature"));
                    }

                    // Decompress using Inflate (skip "CK" header)
                    let decompressed = decompress_mszip(&compressed[2..])?;

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
                CompressionType::Quantum => {
                    return Err(OxiArcError::unsupported_method("Quantum compression"));
                }
                CompressionType::Lzx(_) => {
                    return Err(OxiArcError::unsupported_method("LZX compression"));
                }
            }
        }

        Ok(output)
    }
}

/// CFDATA structure - compressed data block.
struct CfData {
    #[allow(dead_code)]
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

/// Decompress MSZIP data (raw deflate without zlib header).
fn decompress_mszip(data: &[u8]) -> Result<Vec<u8>> {
    // The inflate function handles raw deflate data
    inflate(data)
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
