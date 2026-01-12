//! 7z archive header parsing and reading.
//!
//! Based on 7z file format specification from LZMA SDK.

use oxiarc_core::crc::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use oxiarc_lzma::{Lzma2Decoder, LzmaProperties};
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 7z magic bytes: '7', 'z', 0xBC, 0xAF, 0x27, 0x1C
pub const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// Property IDs for 7z format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropertyId {
    End = 0x00,
    Header = 0x01,
    ArchiveProperties = 0x02,
    AdditionalStreamsInfo = 0x03,
    MainStreamsInfo = 0x04,
    FilesInfo = 0x05,
    PackInfo = 0x06,
    UnpackInfo = 0x07,
    SubStreamsInfo = 0x08,
    Size = 0x09,
    Crc = 0x0A,
    Folder = 0x0B,
    CodersUnpackSize = 0x0C,
    NumUnpackStream = 0x0D,
    EmptyStream = 0x0E,
    EmptyFile = 0x0F,
    Anti = 0x10,
    Name = 0x11,
    CTime = 0x12,
    ATime = 0x13,
    MTime = 0x14,
    WinAttributes = 0x15,
    Comment = 0x16,
    EncodedHeader = 0x17,
    StartPos = 0x18,
    Dummy = 0x19,
}

impl PropertyId {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x00 => Some(Self::End),
            0x01 => Some(Self::Header),
            0x02 => Some(Self::ArchiveProperties),
            0x03 => Some(Self::AdditionalStreamsInfo),
            0x04 => Some(Self::MainStreamsInfo),
            0x05 => Some(Self::FilesInfo),
            0x06 => Some(Self::PackInfo),
            0x07 => Some(Self::UnpackInfo),
            0x08 => Some(Self::SubStreamsInfo),
            0x09 => Some(Self::Size),
            0x0A => Some(Self::Crc),
            0x0B => Some(Self::Folder),
            0x0C => Some(Self::CodersUnpackSize),
            0x0D => Some(Self::NumUnpackStream),
            0x0E => Some(Self::EmptyStream),
            0x0F => Some(Self::EmptyFile),
            0x10 => Some(Self::Anti),
            0x11 => Some(Self::Name),
            0x12 => Some(Self::CTime),
            0x13 => Some(Self::ATime),
            0x14 => Some(Self::MTime),
            0x15 => Some(Self::WinAttributes),
            0x16 => Some(Self::Comment),
            0x17 => Some(Self::EncodedHeader),
            0x18 => Some(Self::StartPos),
            0x19 => Some(Self::Dummy),
            _ => None,
        }
    }
}

/// Codec IDs for 7z compression methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecId {
    /// No compression (copy).
    Copy,
    /// LZMA compression.
    Lzma,
    /// LZMA2 compression.
    Lzma2,
    /// Deflate compression.
    Deflate,
    /// BZip2 compression.
    BZip2,
    /// Delta filter.
    Delta,
    /// BCJ (x86) filter.
    BcjX86,
    /// BCJ2 filter.
    Bcj2,
    /// AES encryption.
    Aes,
    /// Unknown codec.
    Unknown(Vec<u8>),
}

impl CodecId {
    fn from_bytes(bytes: &[u8]) -> Self {
        match bytes {
            [0x00] => Self::Copy,
            [0x03, 0x01, 0x01] => Self::Lzma,
            [0x21] => Self::Lzma2,
            [0x04, 0x01, 0x08] => Self::Deflate,
            [0x04, 0x02, 0x02] => Self::BZip2,
            [0x03] => Self::Delta,
            [0x03, 0x03, 0x01, 0x03] => Self::BcjX86,
            [0x03, 0x03, 0x01, 0x1B] => Self::Bcj2,
            [0x06, 0xF1, 0x07, 0x01] => Self::Aes,
            _ => Self::Unknown(bytes.to_vec()),
        }
    }
}

/// A coder in a 7z folder.
#[derive(Debug, Clone)]
pub struct Coder {
    /// Codec ID.
    pub codec_id: CodecId,
    /// Number of input streams (used for complex coders).
    #[allow(dead_code)]
    pub num_in_streams: u64,
    /// Number of output streams.
    pub num_out_streams: u64,
    /// Codec properties.
    pub properties: Vec<u8>,
}

/// A folder in a 7z archive (compression unit).
#[derive(Debug, Clone)]
pub struct Folder {
    /// Coders in this folder.
    pub coders: Vec<Coder>,
    /// Bind pairs (input stream -> output stream, used for complex coders).
    #[allow(dead_code)]
    pub bind_pairs: Vec<(u64, u64)>,
    /// Packed stream indices.
    pub packed_indices: Vec<u64>,
    /// Unpack sizes for each coder output.
    pub unpack_sizes: Vec<u64>,
    /// CRC of unpacked data (if present).
    pub unpack_crc: Option<u32>,
}

impl Folder {
    /// Get the total unpack size (final output).
    pub fn unpack_size(&self) -> u64 {
        self.unpack_sizes.last().copied().unwrap_or(0)
    }
}

/// An entry in a 7z archive.
#[derive(Debug, Clone)]
pub struct SevenZEntry {
    /// File name.
    pub name: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Whether this is an anti-item (deletion marker).
    pub is_anti: bool,
    /// Uncompressed size.
    pub size: u64,
    /// CRC-32 of the file.
    pub crc: Option<u32>,
    /// Modification time.
    pub mtime: Option<SystemTime>,
    /// Creation time.
    pub ctime: Option<SystemTime>,
    /// Access time.
    pub atime: Option<SystemTime>,
    /// Windows attributes.
    pub attributes: u32,
    /// Folder index (for compressed data).
    pub folder_index: Option<usize>,
    /// Offset within folder's unpacked data.
    pub offset_in_folder: u64,
}

impl SevenZEntry {
    /// Convert to core Entry type.
    pub fn to_entry(&self) -> Entry {
        let entry_type = if self.is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        };

        Entry {
            name: self.name.clone(),
            entry_type,
            size: self.size,
            compressed_size: 0, // Unknown at entry level
            method: oxiarc_core::entry::CompressionMethod::Unknown(0),
            modified: self.mtime,
            created: self.ctime,
            accessed: self.atime,
            attributes: FileAttributes::default(),
            crc32: self.crc,
            comment: None,
            link_target: None,
            offset: 0,
            extra: Vec::new(),
        }
    }
}

/// 7z archive reader.
pub struct SevenZReader<R: Read + Seek> {
    reader: R,
    /// Offset where packed streams start.
    pack_pos: u64,
    /// Packed stream sizes.
    pack_sizes: Vec<u64>,
    /// Folders.
    folders: Vec<Folder>,
    /// File entries.
    entries: Vec<SevenZEntry>,
    /// Number of substreams per folder.
    num_unpack_streams: Vec<u64>,
}

impl<R: Read + Seek> SevenZReader<R> {
    /// Create a new 7z reader.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read signature header
        let mut sig_header = [0u8; 32];
        reader.read_exact(&mut sig_header)?;

        // Verify magic
        if sig_header[0..6] != SEVENZ_MAGIC {
            return Err(OxiArcError::invalid_magic(
                SEVENZ_MAGIC.to_vec(),
                sig_header[0..6].to_vec(),
            ));
        }

        // Version
        let _major = sig_header[6];
        let _minor = sig_header[7];

        // Start header CRC
        let start_header_crc =
            u32::from_le_bytes([sig_header[8], sig_header[9], sig_header[10], sig_header[11]]);

        // Verify start header CRC
        let computed_crc = Crc32::compute(&sig_header[12..32]);
        if computed_crc != start_header_crc {
            return Err(OxiArcError::crc_mismatch(start_header_crc, computed_crc));
        }

        // Next header offset (from end of signature header)
        let next_header_offset = u64::from_le_bytes([
            sig_header[12],
            sig_header[13],
            sig_header[14],
            sig_header[15],
            sig_header[16],
            sig_header[17],
            sig_header[18],
            sig_header[19],
        ]);

        // Next header size
        let next_header_size = u64::from_le_bytes([
            sig_header[20],
            sig_header[21],
            sig_header[22],
            sig_header[23],
            sig_header[24],
            sig_header[25],
            sig_header[26],
            sig_header[27],
        ]);

        // Next header CRC
        let next_header_crc = u32::from_le_bytes([
            sig_header[28],
            sig_header[29],
            sig_header[30],
            sig_header[31],
        ]);

        // Seek to next header
        reader.seek(SeekFrom::Start(32 + next_header_offset))?;

        // Read next header
        let mut header_data = vec![0u8; next_header_size as usize];
        reader.read_exact(&mut header_data)?;

        // Verify header CRC
        let computed_header_crc = Crc32::compute(&header_data);
        if computed_header_crc != next_header_crc {
            return Err(OxiArcError::crc_mismatch(
                next_header_crc,
                computed_header_crc,
            ));
        }

        // Parse the header
        let mut sevenz = Self {
            reader,
            pack_pos: 32, // Default to after signature header
            pack_sizes: Vec::new(),
            folders: Vec::new(),
            entries: Vec::new(),
            num_unpack_streams: Vec::new(),
        };

        sevenz.parse_header(&header_data)?;

        Ok(sevenz)
    }

    /// Parse the 7z header.
    fn parse_header(&mut self, data: &[u8]) -> Result<()> {
        let mut pos = 0;

        // First byte determines header type
        if pos >= data.len() {
            return Ok(());
        }

        let header_type = data[pos];
        pos += 1;

        match PropertyId::from_u8(header_type) {
            Some(PropertyId::Header) => {
                self.parse_header_content(data, &mut pos)?;
            }
            Some(PropertyId::EncodedHeader) => {
                // Header is compressed - need to decompress it first
                let decompressed = self.decompress_header(data, &mut pos)?;
                self.parse_header(&decompressed)?;
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unexpected header type: 0x{:02X}",
                    header_type
                )));
            }
        }

        Ok(())
    }

    /// Decompress an encoded header.
    fn decompress_header(&mut self, data: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
        // Read streams info for the encoded header
        self.parse_streams_info(data, pos)?;

        // The packed data for the header is at the beginning of the file
        // (right after the signature header)
        if self.pack_sizes.is_empty() || self.folders.is_empty() {
            return Err(OxiArcError::invalid_header(
                "No pack info for encoded header",
            ));
        }

        // Read and decompress the header
        let pack_size = self.pack_sizes[0];
        self.reader.seek(SeekFrom::Start(self.pack_pos))?;

        let mut packed = vec![0u8; pack_size as usize];
        self.reader.read_exact(&mut packed)?;

        // Decompress based on folder's coders
        let folder = &self.folders[0];
        self.decompress_folder(folder, &packed)
    }

    /// Decompress data from a folder.
    fn decompress_folder(&self, folder: &Folder, packed: &[u8]) -> Result<Vec<u8>> {
        if folder.coders.is_empty() {
            return Ok(packed.to_vec());
        }

        // For now, support single-coder folders with LZMA/LZMA2/Copy
        let coder = &folder.coders[0];
        match coder.codec_id {
            CodecId::Copy => Ok(packed.to_vec()),
            CodecId::Lzma => {
                // LZMA decompression
                if coder.properties.len() < 5 {
                    return Err(OxiArcError::invalid_header("Invalid LZMA properties"));
                }

                let props = LzmaProperties::from_byte(coder.properties[0])
                    .ok_or_else(|| OxiArcError::invalid_header("Invalid LZMA properties byte"))?;
                let dict_size = u32::from_le_bytes([
                    coder.properties[1],
                    coder.properties[2],
                    coder.properties[3],
                    coder.properties[4],
                ]);

                let unpack_size = folder.unpack_size();
                let cursor = std::io::Cursor::new(packed);
                oxiarc_lzma::decompress_raw(cursor, props, dict_size, Some(unpack_size))
            }
            CodecId::Lzma2 => {
                // LZMA2 decompression
                if coder.properties.is_empty() {
                    return Err(OxiArcError::invalid_header("Invalid LZMA2 properties"));
                }

                let dict_size = oxiarc_lzma::dict_size_from_props(coder.properties[0]);
                let mut decoder = Lzma2Decoder::new(dict_size);
                let mut cursor = std::io::Cursor::new(packed);
                decoder.decode(&mut cursor)
            }
            _ => Err(OxiArcError::unsupported_method(format!(
                "Unsupported codec: {:?}",
                coder.codec_id
            ))),
        }
    }

    /// Parse header content.
    fn parse_header_content(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::End) => break,
                Some(PropertyId::MainStreamsInfo) => {
                    self.parse_streams_info(data, pos)?;
                }
                Some(PropertyId::FilesInfo) => {
                    self.parse_files_info(data, pos)?;
                }
                Some(PropertyId::ArchiveProperties) => {
                    self.skip_archive_properties(data, pos)?;
                }
                Some(PropertyId::AdditionalStreamsInfo) => {
                    // Skip for now
                    self.parse_streams_info(data, pos)?;
                }
                _ => {
                    // Unknown property, try to skip
                    break;
                }
            }
        }

        Ok(())
    }

    /// Parse streams info.
    fn parse_streams_info(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::End) => break,
                Some(PropertyId::PackInfo) => {
                    self.parse_pack_info(data, pos)?;
                }
                Some(PropertyId::UnpackInfo) => {
                    self.parse_unpack_info(data, pos)?;
                }
                Some(PropertyId::SubStreamsInfo) => {
                    self.parse_substreams_info(data, pos)?;
                }
                _ => {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Parse pack info.
    fn parse_pack_info(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        // Pack position
        let pack_pos = Self::read_number(data, pos)?;
        self.pack_pos = 32 + pack_pos; // After signature header

        // Number of pack streams
        let num_pack_streams = Self::read_number(data, pos)?;

        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::End) => break,
                Some(PropertyId::Size) => {
                    // Read pack sizes
                    self.pack_sizes.clear();
                    for _ in 0..num_pack_streams {
                        let size = Self::read_number(data, pos)?;
                        self.pack_sizes.push(size);
                    }
                }
                Some(PropertyId::Crc) => {
                    // Skip CRCs for now
                    let all_defined = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;
                    if all_defined == 0 {
                        let bits_len = (num_pack_streams as usize).div_ceil(8);
                        *pos += bits_len;
                    }
                    // Skip CRC values
                    // Count defined CRCs
                    *pos += 4 * num_pack_streams as usize; // Approximate
                }
                _ => break,
            }
        }

        Ok(())
    }

    /// Parse unpack info (folders).
    fn parse_unpack_info(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::End) => break,
                Some(PropertyId::Folder) => {
                    let num_folders = Self::read_number(data, pos)?;

                    // External flag
                    let external = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;

                    if external != 0 {
                        // External folders - skip for now
                        let _data_index = Self::read_number(data, pos)?;
                    } else {
                        // Read folder definitions
                        for _ in 0..num_folders {
                            let folder = self.parse_folder(data, pos)?;
                            self.folders.push(folder);
                        }
                    }
                }
                Some(PropertyId::CodersUnpackSize) => {
                    // Read unpack sizes for each folder's coders
                    for folder in &mut self.folders {
                        let num_out_streams: usize = folder
                            .coders
                            .iter()
                            .map(|c| c.num_out_streams as usize)
                            .sum();
                        folder.unpack_sizes.clear();
                        for _ in 0..num_out_streams {
                            let size = Self::read_number(data, pos)?;
                            folder.unpack_sizes.push(size);
                        }
                    }
                }
                Some(PropertyId::Crc) => {
                    // Read folder CRCs
                    let all_defined = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;

                    let mut defined = vec![true; self.folders.len()];
                    if all_defined == 0 {
                        // Read defined bitmap
                        for (i, is_defined) in defined.iter_mut().enumerate() {
                            let byte_idx = i / 8;
                            let bit_idx = 7 - (i % 8);
                            if *pos + byte_idx < data.len() {
                                *is_defined = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                            }
                        }
                        let bits_len = self.folders.len().div_ceil(8);
                        *pos += bits_len;
                    }

                    for (i, folder) in self.folders.iter_mut().enumerate() {
                        if defined[i] && *pos + 4 <= data.len() {
                            let crc = u32::from_le_bytes([
                                data[*pos],
                                data[*pos + 1],
                                data[*pos + 2],
                                data[*pos + 3],
                            ]);
                            folder.unpack_crc = Some(crc);
                            *pos += 4;
                        }
                    }
                }
                _ => break,
            }
        }

        Ok(())
    }

    /// Parse a single folder.
    fn parse_folder(&mut self, data: &[u8], pos: &mut usize) -> Result<Folder> {
        let num_coders = Self::read_number(data, pos)?;
        let mut coders = Vec::new();
        let mut total_in_streams = 0u64;
        let mut total_out_streams = 0u64;

        for _ in 0..num_coders {
            let main_byte = data.get(*pos).copied().unwrap_or(0);
            *pos += 1;

            let codec_id_size = (main_byte & 0x0F) as usize;
            let is_complex = (main_byte & 0x10) != 0;
            let has_attributes = (main_byte & 0x20) != 0;

            // Read codec ID
            let codec_id_bytes = if *pos + codec_id_size <= data.len() {
                let bytes = data[*pos..*pos + codec_id_size].to_vec();
                *pos += codec_id_size;
                bytes
            } else {
                Vec::new()
            };

            let codec_id = CodecId::from_bytes(&codec_id_bytes);

            let (num_in_streams, num_out_streams) = if is_complex {
                let num_in = Self::read_number(data, pos)?;
                let num_out = Self::read_number(data, pos)?;
                (num_in, num_out)
            } else {
                (1, 1)
            };

            total_in_streams += num_in_streams;
            total_out_streams += num_out_streams;

            let properties = if has_attributes {
                let props_size = Self::read_number(data, pos)? as usize;
                if *pos + props_size <= data.len() {
                    let props = data[*pos..*pos + props_size].to_vec();
                    *pos += props_size;
                    props
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            coders.push(Coder {
                codec_id,
                num_in_streams,
                num_out_streams,
                properties,
            });
        }

        // Read bind pairs
        let num_bind_pairs = total_out_streams.saturating_sub(1);
        let mut bind_pairs = Vec::new();
        for _ in 0..num_bind_pairs {
            let in_index = Self::read_number(data, pos)?;
            let out_index = Self::read_number(data, pos)?;
            bind_pairs.push((in_index, out_index));
        }

        // Read packed stream indices
        let num_packed = total_in_streams.saturating_sub(num_bind_pairs);
        let mut packed_indices = Vec::new();
        if num_packed == 1 {
            // Find the unpaired input stream
            for i in 0..total_in_streams {
                let is_bound = bind_pairs.iter().any(|(idx, _)| *idx == i);
                if !is_bound {
                    packed_indices.push(i);
                    break;
                }
            }
        } else {
            for _ in 0..num_packed {
                let idx = Self::read_number(data, pos)?;
                packed_indices.push(idx);
            }
        }

        Ok(Folder {
            coders,
            bind_pairs,
            packed_indices,
            unpack_sizes: Vec::new(),
            unpack_crc: None,
        })
    }

    /// Parse substreams info.
    fn parse_substreams_info(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        // Initialize with 1 substream per folder
        self.num_unpack_streams = vec![1; self.folders.len()];

        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::End) => break,
                Some(PropertyId::NumUnpackStream) => {
                    for i in 0..self.folders.len() {
                        let num = Self::read_number(data, pos)?;
                        self.num_unpack_streams[i] = num;
                    }
                }
                Some(PropertyId::Size) => {
                    // Skip sizes for now
                    for num in &self.num_unpack_streams {
                        if *num > 1 {
                            for _ in 0..(*num - 1) {
                                let _ = Self::read_number(data, pos)?;
                            }
                        }
                    }
                }
                Some(PropertyId::Crc) => {
                    // Skip CRCs
                    let total_substreams: u64 = self.num_unpack_streams.iter().sum();
                    let all_defined = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;
                    if all_defined == 0 {
                        let bits_len = (total_substreams as usize).div_ceil(8);
                        *pos += bits_len;
                    }
                    // Skip CRC values (approximate)
                    *pos += 4 * total_substreams as usize;
                }
                _ => break,
            }
        }

        Ok(())
    }

    /// Parse files info.
    fn parse_files_info(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        let num_files = Self::read_number(data, pos)? as usize;

        // Initialize entries
        self.entries = vec![
            SevenZEntry {
                name: String::new(),
                is_dir: false,
                is_anti: false,
                size: 0,
                crc: None,
                mtime: None,
                ctime: None,
                atime: None,
                attributes: 0,
                folder_index: None,
                offset_in_folder: 0,
            };
            num_files
        ];

        // Track empty streams
        let mut empty_streams = vec![false; num_files];
        let mut empty_files = vec![false; num_files];

        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            if prop_id == PropertyId::End as u8 {
                break;
            }

            let size = Self::read_number(data, pos)? as usize;
            let end_pos = *pos + size;

            match PropertyId::from_u8(prop_id) {
                Some(PropertyId::Name) => {
                    // External flag
                    let external = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;

                    if external == 0 {
                        // Read UTF-16LE names
                        for entry in &mut self.entries {
                            let mut name_bytes = Vec::new();
                            while *pos + 2 <= end_pos {
                                let c = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
                                *pos += 2;
                                if c == 0 {
                                    break;
                                }
                                name_bytes.push(c);
                            }
                            entry.name = String::from_utf16_lossy(&name_bytes);
                        }
                    }
                }
                Some(PropertyId::EmptyStream) => {
                    // Bitmap of empty streams
                    for (i, empty) in empty_streams.iter_mut().enumerate() {
                        let byte_idx = i / 8;
                        let bit_idx = 7 - (i % 8);
                        if *pos + byte_idx < end_pos {
                            *empty = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                        }
                    }
                    *pos = end_pos;
                }
                Some(PropertyId::EmptyFile) => {
                    // Bitmap of empty files (within empty streams)
                    let mut empty_idx = 0;
                    for (empty_file, &is_empty_stream) in
                        empty_files.iter_mut().zip(empty_streams.iter())
                    {
                        if is_empty_stream {
                            let byte_idx = empty_idx / 8;
                            let bit_idx = 7 - (empty_idx % 8);
                            if *pos + byte_idx < end_pos {
                                *empty_file = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                            }
                            empty_idx += 1;
                        }
                    }
                    *pos = end_pos;
                }
                Some(PropertyId::Anti) => {
                    // Bitmap of anti items
                    let mut empty_idx = 0;
                    for (entry, &is_empty_stream) in
                        self.entries.iter_mut().zip(empty_streams.iter())
                    {
                        if is_empty_stream {
                            let byte_idx = empty_idx / 8;
                            let bit_idx = 7 - (empty_idx % 8);
                            if *pos + byte_idx < end_pos {
                                entry.is_anti = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                            }
                            empty_idx += 1;
                        }
                    }
                    *pos = end_pos;
                }
                Some(PropertyId::MTime) => {
                    self.parse_file_times(data, pos, end_pos, |e, t| e.mtime = Some(t))?;
                }
                Some(PropertyId::CTime) => {
                    self.parse_file_times(data, pos, end_pos, |e, t| e.ctime = Some(t))?;
                }
                Some(PropertyId::ATime) => {
                    self.parse_file_times(data, pos, end_pos, |e, t| e.atime = Some(t))?;
                }
                Some(PropertyId::WinAttributes) => {
                    // Read attributes bitmap and values
                    let all_defined = data.get(*pos).copied().unwrap_or(0);
                    *pos += 1;

                    let mut defined = vec![true; num_files];
                    if all_defined == 0 {
                        for (i, def) in defined.iter_mut().enumerate() {
                            let byte_idx = i / 8;
                            let bit_idx = 7 - (i % 8);
                            if *pos + byte_idx < end_pos {
                                *def = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                            }
                        }
                        let bits_len = num_files.div_ceil(8);
                        *pos += bits_len;
                    }

                    for (entry, &is_defined) in self.entries.iter_mut().zip(defined.iter()) {
                        if is_defined && *pos + 4 <= end_pos {
                            let attrs = u32::from_le_bytes([
                                data[*pos],
                                data[*pos + 1],
                                data[*pos + 2],
                                data[*pos + 3],
                            ]);
                            entry.attributes = attrs;
                            *pos += 4;
                        }
                    }
                }
                _ => {
                    // Skip unknown property
                    *pos = end_pos;
                }
            }

            *pos = end_pos;
        }

        // Mark directories
        for ((entry, &is_empty_stream), &is_empty_file) in self
            .entries
            .iter_mut()
            .zip(empty_streams.iter())
            .zip(empty_files.iter())
        {
            if is_empty_stream && !is_empty_file {
                entry.is_dir = true;
            }
        }

        // Assign folder indices and sizes
        self.assign_folder_info(&empty_streams);

        Ok(())
    }

    /// Parse file times.
    fn parse_file_times<F>(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        end_pos: usize,
        setter: F,
    ) -> Result<()>
    where
        F: Fn(&mut SevenZEntry, SystemTime),
    {
        let all_defined = data.get(*pos).copied().unwrap_or(0);
        *pos += 1;

        let num_entries = self.entries.len();
        let mut defined = vec![true; num_entries];
        if all_defined == 0 {
            for (i, def) in defined.iter_mut().enumerate() {
                let byte_idx = i / 8;
                let bit_idx = 7 - (i % 8);
                if *pos + byte_idx < end_pos {
                    *def = (data[*pos + byte_idx] >> bit_idx) & 1 != 0;
                }
            }
            let bits_len = num_entries.div_ceil(8);
            *pos += bits_len;
        }

        // External flag
        let external = data.get(*pos).copied().unwrap_or(0);
        *pos += 1;

        if external == 0 {
            for (entry, &is_defined) in self.entries.iter_mut().zip(defined.iter()) {
                if is_defined && *pos + 8 <= end_pos {
                    let filetime = u64::from_le_bytes([
                        data[*pos],
                        data[*pos + 1],
                        data[*pos + 2],
                        data[*pos + 3],
                        data[*pos + 4],
                        data[*pos + 5],
                        data[*pos + 6],
                        data[*pos + 7],
                    ]);
                    *pos += 8;

                    // Convert Windows FILETIME to Unix time
                    // FILETIME is 100-nanosecond intervals since Jan 1, 1601
                    // Unix epoch is Jan 1, 1970
                    // Difference: 11644473600 seconds
                    if filetime >= 116444736000000000 {
                        let unix_100ns = filetime - 116444736000000000;
                        let secs = unix_100ns / 10_000_000;
                        let nanos = ((unix_100ns % 10_000_000) * 100) as u32;
                        let time = UNIX_EPOCH + Duration::new(secs, nanos);
                        setter(entry, time);
                    }
                }
            }
        }

        Ok(())
    }

    /// Assign folder indices to file entries.
    fn assign_folder_info(&mut self, empty_streams: &[bool]) {
        let mut folder_idx = 0;
        let mut offset_in_folder = 0u64;
        let mut substream_idx = 0;

        for (entry, &is_empty_stream) in self.entries.iter_mut().zip(empty_streams.iter()) {
            if is_empty_stream {
                // Empty stream - no folder
                continue;
            }

            // Find the folder for this file
            while folder_idx < self.folders.len() {
                let num_substreams = self
                    .num_unpack_streams
                    .get(folder_idx)
                    .copied()
                    .unwrap_or(1);
                if substream_idx < num_substreams as usize {
                    break;
                }
                substream_idx = 0;
                offset_in_folder = 0;
                folder_idx += 1;
            }

            if folder_idx < self.folders.len() {
                entry.folder_index = Some(folder_idx);
                entry.offset_in_folder = offset_in_folder;

                // Get size from folder unpack sizes
                if let Some(folder) = self.folders.get(folder_idx) {
                    if !folder.unpack_sizes.is_empty() {
                        // For single substream, use folder's unpack size
                        let num_substreams = self
                            .num_unpack_streams
                            .get(folder_idx)
                            .copied()
                            .unwrap_or(1);
                        if num_substreams == 1 {
                            entry.size = folder.unpack_size();
                        }
                    }
                }

                entry.size = offset_in_folder; // Will be adjusted later
                substream_idx += 1;
            }
        }
    }

    /// Skip archive properties.
    fn skip_archive_properties(&mut self, data: &[u8], pos: &mut usize) -> Result<()> {
        loop {
            if *pos >= data.len() {
                break;
            }

            let prop_id = data[*pos];
            *pos += 1;

            if prop_id == PropertyId::End as u8 {
                break;
            }

            let size = Self::read_number(data, pos)? as usize;
            *pos += size;
        }

        Ok(())
    }

    /// Read a variable-length number.
    fn read_number(data: &[u8], pos: &mut usize) -> Result<u64> {
        if *pos >= data.len() {
            return Err(OxiArcError::corrupted(0, "Unexpected end of data"));
        }

        let first = data[*pos];
        *pos += 1;

        // Count leading ones
        let mask = !first;
        let extra_bytes = mask.leading_zeros() as usize;

        if extra_bytes == 0 {
            return Ok(first as u64);
        }

        let mut value = (first & (0xFF >> extra_bytes)) as u64;

        for _ in 0..extra_bytes {
            if *pos >= data.len() {
                return Err(OxiArcError::corrupted(0, "Truncated number"));
            }
            value = (value << 8) | (data[*pos] as u64);
            *pos += 1;
        }

        Ok(value)
    }

    /// Get the list of entries.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.iter().map(|e| e.to_entry()).collect()
    }

    /// Get the raw 7z entries.
    pub fn sevenz_entries(&self) -> &[SevenZEntry] {
        &self.entries
    }

    /// Extract a file by index.
    pub fn extract(&mut self, index: usize) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| OxiArcError::corrupted(0, "Invalid entry index"))?;

        if entry.is_dir {
            return Ok(Vec::new());
        }

        let folder_idx = entry
            .folder_index
            .ok_or_else(|| OxiArcError::corrupted(0, "No folder for entry"))?;

        // Calculate pack offset for this folder
        let mut pack_offset = self.pack_pos;
        for i in 0..folder_idx {
            if let Some(folder) = self.folders.get(i) {
                for idx in &folder.packed_indices {
                    if let Some(size) = self.pack_sizes.get(*idx as usize) {
                        pack_offset += size;
                    }
                }
            }
        }

        // Get folder and pack size
        let folder = self
            .folders
            .get(folder_idx)
            .ok_or_else(|| OxiArcError::corrupted(0, "Invalid folder index"))?
            .clone();

        let pack_size = folder
            .packed_indices
            .first()
            .and_then(|idx| self.pack_sizes.get(*idx as usize))
            .copied()
            .unwrap_or(0);

        // Read packed data
        self.reader.seek(SeekFrom::Start(pack_offset))?;
        let mut packed = vec![0u8; pack_size as usize];
        self.reader.read_exact(&mut packed)?;

        // Decompress
        let unpacked = self.decompress_folder(&folder, &packed)?;

        // Extract the portion for this entry
        let offset = entry.offset_in_folder as usize;
        let size = entry.size as usize;

        if offset + size <= unpacked.len() {
            Ok(unpacked[offset..offset + size].to_vec())
        } else if offset < unpacked.len() {
            Ok(unpacked[offset..].to_vec())
        } else {
            Ok(unpacked)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sevenz_magic() {
        assert_eq!(SEVENZ_MAGIC, [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]);
    }

    #[test]
    fn test_property_id_from_u8() {
        assert_eq!(PropertyId::from_u8(0x00), Some(PropertyId::End));
        assert_eq!(PropertyId::from_u8(0x01), Some(PropertyId::Header));
        assert_eq!(PropertyId::from_u8(0x17), Some(PropertyId::EncodedHeader));
        assert_eq!(PropertyId::from_u8(0xFF), None);
    }

    #[test]
    fn test_codec_id_from_bytes() {
        assert_eq!(CodecId::from_bytes(&[0x00]), CodecId::Copy);
        assert_eq!(CodecId::from_bytes(&[0x21]), CodecId::Lzma2);
        assert_eq!(CodecId::from_bytes(&[0x03, 0x01, 0x01]), CodecId::Lzma);
    }

    #[test]
    fn test_read_number_single_byte() {
        let data = [0x05, 0x00];
        let mut pos = 0;
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos).unwrap();
        assert_eq!(num, 5);
        assert_eq!(pos, 1);
    }

    #[test]
    fn test_read_number_two_bytes() {
        // 0x80 means 1 extra byte follows
        let data = [0x80, 0x05];
        let mut pos = 0;
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos).unwrap();
        assert_eq!(num, 5);
        assert_eq!(pos, 2);
    }
}
