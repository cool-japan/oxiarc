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
        })
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
        // Find the file entry
        let file = self
            .files
            .iter()
            .find(|f| f.name == entry.name)
            .ok_or_else(|| OxiArcError::corrupted(0, format!("File not found: {}", entry.name)))?
            .clone();

        self.extract_file(&file)
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
        self.extract_file(&file)
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
}
