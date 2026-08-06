//! 7z archive header parsing and reading.
//!
//! Implements the 7z container format per `7zFormat.txt` from the LZMA SDK:
//!
//! * Variable-length numbers carry their extra bytes in little-endian order,
//!   with the remaining bits of the first byte forming the most significant
//!   part of the value.
//! * `kSubStreamsInfo` provides per-substream sizes (the first `n - 1` sizes
//!   of each folder are stored; the last one is the residual of the folder
//!   unpack size) and CRC-32 digests.
//! * `kEmptyStream` / `kEmptyFile` / `kAnti` bit vectors distinguish
//!   directories, zero-byte files, and anti-items; none of them owns a
//!   substream, so extraction yields empty data instead of an error.
//! * Encoded (compressed) headers, linear multi-coder folders (e.g. filter +
//!   compressor chains bound by bind pairs), and folder/substream CRC
//!   verification are supported.

use oxiarc_core::crc::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use oxiarc_lzma::{Lzma2Decoder, LzmaProperties};
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 7z magic bytes: '7', 'z', 0xBC, 0xAF, 0x27, 0x1C
pub const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// Sanity bound for element counts read from headers.
///
/// Guards against memory exhaustion on corrupt or malicious headers.
const MAX_REASONABLE_COUNT: u64 = 16 * 1024 * 1024;

/// Maximum accepted size of the next header block.
const MAX_HEADER_SIZE: u64 = 1 << 31;

/// Maximum number of coders in a single folder.
///
/// The 7-Zip reference implementation caps a folder at 64 coders
/// (`kNumCodersMax`); real archives use at most 4 (filter chains plus
/// BCJ2). A crafted count beyond this drove a multi-GiB
/// `Vec::with_capacity` before a single coder was even parsed.
const MAX_CODERS_PER_FOLDER: usize = 64;

/// Maximum total input/output streams across the coders of one folder.
///
/// Complex coders declare their stream counts as unbounded varints; a
/// crafted value of 2^40 previously produced a capacity-overflow panic or
/// a TiB-scale reservation for the bind-pair vector (SEVENZ-01). The
/// reference implementation's bound is 64 (`kNumOutStreamsMax`).
const MAX_FOLDER_STREAMS: u64 = 64;

/// Maximum expansion factor a folder's whole coder chain may achieve over the
/// bytes actually read for its packed stream.
///
/// A single decompression stage over highly redundant data reaches roughly
/// 10^4; a chain that revisits coders multiplies those factors together, which
/// is how a few hundred bytes of crafted 7z metadata turn into terabytes of
/// output (SEVENZ-02). 10^6 leaves two orders of magnitude of headroom over any
/// real single-stage ratio while still cutting the multiplicative case off.
const MAX_FOLDER_EXPANSION_RATIO: u64 = 1_000_000;

/// Floor for the per-folder decoded-byte budget, so a small but legitimate
/// packed stream is never rejected by the ratio rule alone.
const MIN_FOLDER_DECODED_ALLOWANCE: u64 = 64 * 1024 * 1024;

/// Absolute ceiling on the bytes one folder's coder chain may produce in total.
///
/// `decode_folder_data` materialises the whole folder in memory, so anything
/// approaching this is already impractical; the cap exists so that a *large*
/// packed stream cannot license an unbounded chain via the ratio rule.
const MAX_FOLDER_DECODED_BYTES: u64 = 16 * 1024 * 1024 * 1024;

// `folder_decode_budget` clamps into [MIN_FOLDER_DECODED_ALLOWANCE,
// MAX_FOLDER_DECODED_BYTES]; `u64::clamp` panics when the bounds are inverted,
// so pin the ordering at compile time.
const _: () = assert!(MIN_FOLDER_DECODED_ALLOWANCE <= MAX_FOLDER_DECODED_BYTES);

/// Read exactly `declared_len` bytes starting at the reader's current
/// position, bounding the allocation against the number of bytes actually
/// remaining in the underlying stream.
///
/// Both the 7z "next header" size and per-folder "pack size" fields are
/// untrusted values taken directly from the archive header/metadata. A
/// crafted archive can declare an implausible size for what is really a
/// small or truncated file; blindly turning that into `vec![0u8; declared]`
/// would allocate (and zero-fill) up to a couple of GiB before a single
/// byte is even read, and a sufficiently adversarial `declared_len` could
/// overflow the allocator entirely and abort the process.
///
/// Here the declared length is first checked against the number of bytes
/// actually remaining from the current stream position to EOF, and the
/// allocation itself goes through `try_reserve_exact` so an
/// oversized-but-still-"remaining" declaration surfaces as a normal `Err`.
fn read_bounded_by_remaining<R: Read + Seek>(reader: &mut R, declared_len: u64) -> Result<Vec<u8>> {
    let current = reader.stream_position()?;
    let end = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(current))?;
    let remaining = end.saturating_sub(current);

    if declared_len > remaining {
        return Err(OxiArcError::invalid_header(format!(
            "7z declares a block of {declared_len} bytes but only {remaining} bytes remain in the stream"
        )));
    }

    let mut data = Vec::new();
    data.try_reserve_exact(declared_len as usize).map_err(|_| {
        OxiArcError::invalid_header(format!(
            "unable to allocate {declared_len} bytes while reading 7z stream data"
        ))
    })?;
    data.resize(declared_len as usize, 0);
    reader.read_exact(&mut data)?;
    Ok(data)
}

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

/// Bounds-checked cursor over the raw header bytes.
struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or_else(|| OxiArcError::corrupted(self.pos as u64, "7z header truncated"))?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| OxiArcError::corrupted(self.pos as u64, "7z header truncated"))?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u32_le(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64_le(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read a 7z variable-length number.
    ///
    /// The extra bytes are little-endian; the unused low bits of the first
    /// byte contribute the most significant part of the value (see
    /// `ReadNumber` in the 7-Zip reference implementation).
    fn number(&mut self) -> Result<u64> {
        let first = self.u8()?;
        let mut mask: u8 = 0x80;
        let mut value: u64 = 0;
        for i in 0..8 {
            if first & mask == 0 {
                let high = u64::from(first & mask.wrapping_sub(1));
                return Ok(value | (high << (8 * i)));
            }
            value |= u64::from(self.u8()?) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    /// Number of unread bytes left in the cursor.
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Read a number that is used as an element count, with a sanity bound.
    fn count(&mut self) -> Result<usize> {
        let value = self.number()?;
        if value > MAX_REASONABLE_COUNT {
            return Err(OxiArcError::invalid_header(format!(
                "7z header element count is implausible: {}",
                value
            )));
        }
        Ok(value as usize)
    }

    /// Read an element count that must additionally be *payload-plausible*:
    /// each counted element consumes at least one byte of this header, so
    /// a count exceeding the bytes remaining in the cursor is necessarily
    /// forged. This keeps the memory a crafted header can commit
    /// proportional to the bytes it actually supplies (a bare `count()`
    /// still allows a 5-byte varint to drive a 100+ MiB reservation
    /// through `Vec::with_capacity`-style pre-allocation in callers).
    fn count_bounded_by_remaining(&mut self) -> Result<usize> {
        let value = self.count()?;
        if value > self.remaining() {
            return Err(OxiArcError::invalid_header(format!(
                "7z header declares {value} elements but only {} bytes remain",
                self.remaining()
            )));
        }
        Ok(value)
    }

    /// Read an MSB-first bit vector of `count` bits.
    fn bits(&mut self, count: usize) -> Result<Vec<bool>> {
        let bytes = self.take(count.div_ceil(8))?;
        Ok((0..count)
            .map(|i| (bytes[i / 8] >> (7 - (i % 8))) & 1 != 0)
            .collect())
    }

    /// Read an `AllAreDefined`-prefixed bit vector.
    fn optional_bits(&mut self, count: usize) -> Result<Vec<bool>> {
        let all_defined = self.u8()?;
        if all_defined != 0 {
            Ok(vec![true; count])
        } else {
            self.bits(count)
        }
    }

    /// Read a CRC digest list (defined bit vector + little-endian u32 values).
    ///
    /// The `AllAreDefined` fast path is handled here directly instead of
    /// materializing `vec![true; count]` via [`ByteCursor::optional_bits`]:
    /// that byte consumes *no* input, so a crafted aggregate count (e.g.
    /// SubStreamsInfo `kCrc` summing per-folder counts) could reserve
    /// hundreds of MiB from a ~175-byte archive. When all digests are
    /// defined, each costs 4 bytes of header — bound the count by the
    /// bytes actually remaining before allocating anything.
    fn digests(&mut self, count: usize) -> Result<Vec<Option<u32>>> {
        let all_defined = self.u8()?;
        if all_defined != 0 {
            if count > self.remaining() / 4 {
                return Err(OxiArcError::invalid_header(format!(
                    "7z digest list declares {count} entries but only {} bytes remain",
                    self.remaining()
                )));
            }
            let mut digests = Vec::with_capacity(count);
            for _ in 0..count {
                digests.push(Some(self.u32_le()?));
            }
            Ok(digests)
        } else {
            // The defined bit vector consumes ceil(count / 8) bytes, so
            // `bits` (via `take`) naturally bounds `count` by the input.
            let defined = self.bits(count)?;
            defined
                .into_iter()
                .map(|is_defined| {
                    if is_defined {
                        Ok(Some(self.u32_le()?))
                    } else {
                        Ok(None)
                    }
                })
                .collect()
        }
    }
}

/// A coder in a 7z folder.
#[derive(Debug, Clone)]
pub struct Coder {
    /// Codec ID.
    pub codec_id: CodecId,
    /// Number of input streams (used for complex coders).
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
    /// Bind pairs (input stream index, output stream index).
    pub bind_pairs: Vec<(u64, u64)>,
    /// Packed stream indices.
    pub packed_indices: Vec<u64>,
    /// Unpack sizes for each coder output.
    pub unpack_sizes: Vec<u64>,
    /// CRC of unpacked data (if present).
    pub unpack_crc: Option<u32>,
}

impl Folder {
    /// Index of the final output stream (the one not consumed by a bind pair).
    fn main_output_index(&self) -> Option<usize> {
        (0..self.unpack_sizes.len())
            .find(|&out| !self.bind_pairs.iter().any(|&(_, o)| o as usize == out))
    }

    /// Get the total unpack size (final output).
    pub fn unpack_size(&self) -> u64 {
        self.main_output_index()
            .and_then(|out| self.unpack_sizes.get(out).copied())
            .unwrap_or(0)
    }
}

/// Parsed streams info (used for both the main streams and encoded headers).
struct StreamsInfo {
    /// Absolute offset of the first packed stream (includes the 32-byte
    /// signature header).
    pack_pos: u64,
    /// Packed stream sizes.
    pack_sizes: Vec<u64>,
    /// Folders.
    folders: Vec<Folder>,
    /// Number of substreams per folder.
    num_unpack_streams: Vec<u64>,
    /// Substream sizes in (folder, substream) order.
    substream_sizes: Vec<u64>,
    /// Substream CRC-32 digests in (folder, substream) order.
    substream_crcs: Vec<Option<u32>>,
}

impl StreamsInfo {
    fn empty() -> Self {
        Self {
            pack_pos: 32,
            pack_sizes: Vec::new(),
            folders: Vec::new(),
            num_unpack_streams: Vec::new(),
            substream_sizes: Vec::new(),
            substream_crcs: Vec::new(),
        }
    }

    /// (first packed stream index, packed stream count) of a folder.
    fn folder_pack_range(&self, folder_index: usize) -> (usize, usize) {
        let start: usize = self.folders[..folder_index]
            .iter()
            .map(|f| f.packed_indices.len())
            .sum();
        (start, self.folders[folder_index].packed_indices.len())
    }
}

/// Parsed `kFilesInfo` metadata.
struct FilesInfo {
    names: Vec<String>,
    empty_stream: Vec<bool>,
    empty_file: Vec<bool>,
    anti: Vec<bool>,
    mtimes: Vec<Option<SystemTime>>,
    ctimes: Vec<Option<SystemTime>>,
    atimes: Vec<Option<SystemTime>>,
    attributes: Vec<u32>,
}

impl FilesInfo {
    fn empty() -> Self {
        Self {
            names: Vec::new(),
            empty_stream: Vec::new(),
            empty_file: Vec::new(),
            anti: Vec::new(),
            mtimes: Vec::new(),
            ctimes: Vec::new(),
            atimes: Vec::new(),
            attributes: Vec::new(),
        }
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
    /// Parsed main streams info.
    streams: StreamsInfo,
    /// File entries.
    entries: Vec<SevenZEntry>,
    /// Cache of the most recently decoded folder (index, unpacked data).
    ///
    /// Speeds up sequential extraction of entries sharing a solid folder.
    folder_cache: Option<(usize, Vec<u8>)>,
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

        if next_header_size > MAX_HEADER_SIZE {
            return Err(OxiArcError::invalid_header(format!(
                "7z next header size is implausible: {}",
                next_header_size
            )));
        }

        // Seek to next header
        reader.seek(SeekFrom::Start(32 + next_header_offset))?;

        // Read next header
        let header_data = read_bounded_by_remaining(&mut reader, next_header_size)?;

        // Verify header CRC
        let computed_header_crc = Crc32::compute(&header_data);
        if computed_header_crc != next_header_crc {
            return Err(OxiArcError::crc_mismatch(
                next_header_crc,
                computed_header_crc,
            ));
        }

        // An empty archive carries no next header at all.
        if header_data.is_empty() {
            return Ok(Self {
                reader,
                streams: StreamsInfo::empty(),
                entries: Vec::new(),
                folder_cache: None,
            });
        }

        // Parse the header (decoding it first if it is compressed).
        let mut cursor = ByteCursor::new(&header_data);
        let header_type = cursor.u8()?;
        let (streams, files) = match PropertyId::from_u8(header_type) {
            Some(PropertyId::Header) => parse_header_body(&mut cursor)?,
            Some(PropertyId::EncodedHeader) => {
                let header_streams = parse_streams_info(&mut cursor)?;
                if header_streams.folders.is_empty() {
                    return Err(OxiArcError::invalid_header(
                        "7z encoded header has no folder",
                    ));
                }
                let decoded = decode_folder_data(&mut reader, &header_streams, 0)?;
                let mut inner = ByteCursor::new(&decoded);
                if inner.u8()? != PropertyId::Header as u8 {
                    return Err(OxiArcError::invalid_header(
                        "7z encoded header does not contain a header block",
                    ));
                }
                parse_header_body(&mut inner)?
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unexpected header type: 0x{:02X}",
                    header_type
                )));
            }
        };

        let entries = assemble_entries(&streams, files)?;

        Ok(Self {
            reader,
            streams,
            entries,
            folder_cache: None,
        })
    }

    /// Read a variable-length number (kept for compatibility with unit tests).
    #[cfg(test)]
    fn read_number(data: &[u8], pos: &mut usize) -> Result<u64> {
        let mut cursor = ByteCursor { data, pos: *pos };
        let value = cursor.number()?;
        *pos = cursor.pos;
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
    ///
    /// Directories, zero-byte files, and anti-items have no substream and
    /// yield empty data. Extracted bytes are verified against the stored
    /// CRC-32 when one is present.
    pub fn extract(&mut self, index: usize) -> Result<Vec<u8>> {
        let (folder_index, offset, size, crc) = {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| OxiArcError::corrupted(0, "Invalid entry index"))?;

            let Some(folder_index) = entry.folder_index else {
                // Directory, zero-byte file, or anti-item: nothing to decode.
                return Ok(Vec::new());
            };

            (
                folder_index,
                entry.offset_in_folder as usize,
                entry.size as usize,
                entry.crc,
            )
        };

        let cache_hit = matches!(&self.folder_cache, Some((cached, _)) if *cached == folder_index);
        if !cache_hit {
            let data = decode_folder_data(&mut self.reader, &self.streams, folder_index)?;
            self.folder_cache = Some((folder_index, data));
        }
        let Some((_, folder_data)) = &self.folder_cache else {
            return Err(OxiArcError::corrupted(0, "7z folder cache inconsistency"));
        };

        let end = offset
            .checked_add(size)
            .filter(|&end| end <= folder_data.len())
            .ok_or_else(|| {
                OxiArcError::corrupted(offset as u64, "7z entry exceeds decoded folder bounds")
            })?;
        let data = folder_data[offset..end].to_vec();

        if let Some(expected) = crc {
            let actual = Crc32::compute(&data);
            if actual != expected {
                return Err(OxiArcError::crc_mismatch(expected, actual));
            }
        }

        Ok(data)
    }
}

/// Parse the body of a `kHeader` block (MainStreamsInfo + FilesInfo).
fn parse_header_body(cursor: &mut ByteCursor<'_>) -> Result<(StreamsInfo, FilesInfo)> {
    let mut streams = StreamsInfo::empty();
    let mut files = FilesInfo::empty();

    loop {
        let id = cursor.u8()?;
        match PropertyId::from_u8(id) {
            Some(PropertyId::End) => break,
            Some(PropertyId::MainStreamsInfo) => streams = parse_streams_info(cursor)?,
            Some(PropertyId::FilesInfo) => files = parse_files_info(cursor)?,
            Some(PropertyId::ArchiveProperties) => skip_archive_properties(cursor)?,
            Some(PropertyId::AdditionalStreamsInfo) => {
                // Parsed for cursor advancement; the data is not used.
                let _ = parse_streams_info(cursor)?;
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unknown 7z header property: 0x{:02X}",
                    id
                )));
            }
        }
    }

    Ok((streams, files))
}

/// Skip an `kArchiveProperties` block.
fn skip_archive_properties(cursor: &mut ByteCursor<'_>) -> Result<()> {
    loop {
        let prop_type = cursor.u8()?;
        if prop_type == PropertyId::End as u8 {
            return Ok(());
        }
        let size = cursor.count()?;
        cursor.take(size)?;
    }
}

/// Parse a `kStreamsInfo` block (PackInfo / UnpackInfo / SubStreamsInfo).
fn parse_streams_info(cursor: &mut ByteCursor<'_>) -> Result<StreamsInfo> {
    let mut info = StreamsInfo::empty();
    let mut substreams_seen = false;

    loop {
        let id = cursor.u8()?;
        match PropertyId::from_u8(id) {
            Some(PropertyId::End) => break,
            Some(PropertyId::PackInfo) => parse_pack_info(cursor, &mut info)?,
            Some(PropertyId::UnpackInfo) => parse_unpack_info(cursor, &mut info)?,
            Some(PropertyId::SubStreamsInfo) => {
                parse_substreams_info(cursor, &mut info)?;
                substreams_seen = true;
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unknown 7z StreamsInfo property: 0x{:02X}",
                    id
                )));
            }
        }
    }

    if !substreams_seen {
        // Without SubStreamsInfo every folder holds exactly one substream.
        info.num_unpack_streams = vec![1; info.folders.len()];
        info.substream_sizes = info.folders.iter().map(Folder::unpack_size).collect();
        info.substream_crcs = info.folders.iter().map(|f| f.unpack_crc).collect();
    }

    Ok(info)
}

/// Parse a `kPackInfo` block.
fn parse_pack_info(cursor: &mut ByteCursor<'_>, info: &mut StreamsInfo) -> Result<()> {
    info.pack_pos = 32u64
        .checked_add(cursor.number()?)
        .ok_or_else(|| OxiArcError::invalid_header("7z pack position overflows"))?;
    // Each pack stream must have a size varint (>= 1 byte) in this header.
    let num_pack_streams = cursor.count_bounded_by_remaining()?;

    loop {
        let id = cursor.u8()?;
        match PropertyId::from_u8(id) {
            Some(PropertyId::End) => break,
            Some(PropertyId::Size) => {
                info.pack_sizes = (0..num_pack_streams)
                    .map(|_| cursor.number())
                    .collect::<Result<_>>()?;
            }
            Some(PropertyId::Crc) => {
                // Pack stream CRCs are not used for extraction.
                let _ = cursor.digests(num_pack_streams)?;
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unknown 7z PackInfo property: 0x{:02X}",
                    id
                )));
            }
        }
    }

    if info.pack_sizes.len() != num_pack_streams {
        return Err(OxiArcError::invalid_header("7z pack sizes missing"));
    }
    Ok(())
}

/// Parse a `kUnpackInfo` block (folder definitions).
fn parse_unpack_info(cursor: &mut ByteCursor<'_>, info: &mut StreamsInfo) -> Result<()> {
    if cursor.u8()? != PropertyId::Folder as u8 {
        return Err(OxiArcError::invalid_header("7z UnpackInfo lacks kFolder"));
    }
    // Each folder definition occupies several bytes; a count beyond the
    // remaining header bytes would only serve to pre-reserve memory.
    let num_folders = cursor.count_bounded_by_remaining()?;
    if cursor.u8()? != 0 {
        return Err(OxiArcError::unsupported_method(
            "7z external folder definitions",
        ));
    }
    info.folders = (0..num_folders)
        .map(|_| parse_folder(cursor))
        .collect::<Result<_>>()?;

    if cursor.u8()? != PropertyId::CodersUnpackSize as u8 {
        return Err(OxiArcError::invalid_header(
            "7z UnpackInfo lacks kCodersUnpackSize",
        ));
    }
    for folder in &mut info.folders {
        let num_out_streams: u64 = folder.coders.iter().map(|c| c.num_out_streams).sum();
        folder.unpack_sizes = (0..num_out_streams)
            .map(|_| cursor.number())
            .collect::<Result<_>>()?;
    }

    loop {
        let id = cursor.u8()?;
        match PropertyId::from_u8(id) {
            Some(PropertyId::End) => break,
            Some(PropertyId::Crc) => {
                let crcs = cursor.digests(info.folders.len())?;
                for (folder, crc) in info.folders.iter_mut().zip(crcs) {
                    folder.unpack_crc = crc;
                }
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unknown 7z UnpackInfo property: 0x{:02X}",
                    id
                )));
            }
        }
    }

    Ok(())
}

/// Parse a single folder (coders, bind pairs, packed stream indices).
fn parse_folder(cursor: &mut ByteCursor<'_>) -> Result<Folder> {
    let num_coders = cursor.count()?;
    if num_coders == 0 {
        return Err(OxiArcError::invalid_header("7z folder has no coders"));
    }
    if num_coders > MAX_CODERS_PER_FOLDER {
        return Err(OxiArcError::invalid_header(format!(
            "7z folder declares {num_coders} coders, exceeding the limit of \
             {MAX_CODERS_PER_FOLDER}"
        )));
    }

    let mut coders = Vec::with_capacity(num_coders);
    let mut total_in_streams: u64 = 0;
    let mut total_out_streams: u64 = 0;

    for _ in 0..num_coders {
        let main_byte = cursor.u8()?;
        let codec_id_size = (main_byte & 0x0F) as usize;
        let is_complex = (main_byte & 0x10) != 0;
        let has_attributes = (main_byte & 0x20) != 0;

        let codec_id = CodecId::from_bytes(cursor.take(codec_id_size)?);

        // Complex coders declare their stream counts as varints. Read them
        // through the *bounded* `count()` helper — the raw `number()` path
        // previously admitted values up to 2^63, which fed
        // `total_out_streams - 1` into `Vec::with_capacity` below
        // (capacity-overflow panic / TiB-scale reservation).
        let (num_in_streams, num_out_streams) = if is_complex {
            (cursor.count()? as u64, cursor.count()? as u64)
        } else {
            (1, 1)
        };
        if num_in_streams == 0 || num_out_streams == 0 {
            return Err(OxiArcError::invalid_header(
                "7z coder declares zero input or output streams",
            ));
        }
        total_in_streams = total_in_streams
            .checked_add(num_in_streams)
            .ok_or_else(|| OxiArcError::invalid_header("7z coder input count overflows"))?;
        total_out_streams = total_out_streams
            .checked_add(num_out_streams)
            .ok_or_else(|| OxiArcError::invalid_header("7z coder output count overflows"))?;
        if total_in_streams > MAX_FOLDER_STREAMS || total_out_streams > MAX_FOLDER_STREAMS {
            return Err(OxiArcError::invalid_header(format!(
                "7z folder declares more than {MAX_FOLDER_STREAMS} coder streams"
            )));
        }

        let properties = if has_attributes {
            let props_size = cursor.count()?;
            cursor.take(props_size)?.to_vec()
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
    let mut bind_pairs = Vec::with_capacity(num_bind_pairs as usize);
    for _ in 0..num_bind_pairs {
        let in_index = cursor.number()?;
        let out_index = cursor.number()?;
        bind_pairs.push((in_index, out_index));
    }

    // Read packed stream indices
    let num_packed = total_in_streams.saturating_sub(num_bind_pairs);
    let packed_indices = if num_packed == 1 {
        // Find the input stream not consumed by a bind pair.
        let unbound = (0..total_in_streams)
            .find(|i| !bind_pairs.iter().any(|&(in_idx, _)| in_idx == *i))
            .ok_or_else(|| {
                OxiArcError::invalid_header("7z folder packed stream index not found")
            })?;
        vec![unbound]
    } else {
        (0..num_packed)
            .map(|_| cursor.number())
            .collect::<Result<_>>()?
    };

    Ok(Folder {
        coders,
        bind_pairs,
        packed_indices,
        unpack_sizes: Vec::new(),
        unpack_crc: None,
    })
}

/// Parse a `kSubStreamsInfo` block.
fn parse_substreams_info(cursor: &mut ByteCursor<'_>, info: &mut StreamsInfo) -> Result<()> {
    let mut nums: Vec<u64> = vec![1; info.folders.len()];
    let mut sizes: Vec<u64> = Vec::new();
    let mut sizes_read = false;
    let mut crcs: Vec<Option<u32>> = Vec::new();
    let mut crcs_read = false;

    loop {
        let id = cursor.u8()?;
        match PropertyId::from_u8(id) {
            Some(PropertyId::End) => break,
            Some(PropertyId::NumUnpackStream) => {
                // Bound both each per-folder count and the aggregate: the
                // per-substream loops below (sizes, CRCs) and the entry
                // assembly all iterate the *sum*, which a crafted header
                // could otherwise push to folders x 16M.
                let mut total: u64 = 0;
                for num in nums.iter_mut() {
                    *num = cursor.number()?;
                    total = total
                        .checked_add(*num)
                        .filter(|&t| t <= MAX_REASONABLE_COUNT)
                        .ok_or_else(|| {
                            OxiArcError::invalid_header("7z substream count is implausible")
                        })?;
                }
            }
            Some(PropertyId::Size) => {
                // The first (n - 1) sizes of each folder are stored; the
                // last one is the residual of the folder unpack size.
                for (folder, &num) in info.folders.iter().zip(nums.iter()) {
                    if num == 0 {
                        continue;
                    }
                    let mut sum: u64 = 0;
                    for _ in 1..num {
                        let size = cursor.number()?;
                        sizes.push(size);
                        sum = sum.checked_add(size).ok_or_else(|| {
                            OxiArcError::invalid_header("7z substream size overflows")
                        })?;
                    }
                    let last = folder.unpack_size().checked_sub(sum).ok_or_else(|| {
                        OxiArcError::invalid_header(
                            "7z substream sizes exceed the folder unpack size",
                        )
                    })?;
                    sizes.push(last);
                }
                sizes_read = true;
            }
            Some(PropertyId::Crc) => {
                // Substreams whose folder CRC is already known (single
                // substream) are excluded from the digest list. Sum the
                // aggregate with overflow/plausibility checks — per-folder
                // counts are bounded but their sum across folders is not,
                // and it flows into an allocation inside `digests`.
                let mut unknown_count: u64 = 0;
                for (folder, &num) in info.folders.iter().zip(nums.iter()) {
                    if num == 1 && folder.unpack_crc.is_some() {
                        continue;
                    }
                    unknown_count = unknown_count
                        .checked_add(num)
                        .filter(|&t| t <= MAX_REASONABLE_COUNT)
                        .ok_or_else(|| {
                            OxiArcError::invalid_header(
                                "7z substream CRC digest count is implausible",
                            )
                        })?;
                }
                let digests = cursor.digests(unknown_count as usize)?;
                let mut digest_iter = digests.into_iter();
                for (folder, &num) in info.folders.iter().zip(nums.iter()) {
                    if num == 1 && folder.unpack_crc.is_some() {
                        crcs.push(folder.unpack_crc);
                    } else {
                        for _ in 0..num {
                            crcs.push(digest_iter.next().flatten());
                        }
                    }
                }
                crcs_read = true;
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unknown 7z SubStreamsInfo property: 0x{:02X}",
                    id
                )));
            }
        }
    }

    if !sizes_read {
        for (folder, &num) in info.folders.iter().zip(nums.iter()) {
            match num {
                0 => {}
                1 => sizes.push(folder.unpack_size()),
                _ => {
                    return Err(OxiArcError::invalid_header(
                        "7z substream sizes missing for a multi-stream folder",
                    ));
                }
            }
        }
    }
    if !crcs_read {
        for (folder, &num) in info.folders.iter().zip(nums.iter()) {
            for _ in 0..num {
                crcs.push(if num == 1 { folder.unpack_crc } else { None });
            }
        }
    }

    info.num_unpack_streams = nums;
    info.substream_sizes = sizes;
    info.substream_crcs = crcs;
    Ok(())
}

/// Parse a `kFilesInfo` block.
fn parse_files_info(cursor: &mut ByteCursor<'_>) -> Result<FilesInfo> {
    // Every file needs at least a 2-byte (NUL-terminated UTF-16) name in
    // the kName block of this same header, so a file count beyond the
    // remaining bytes is necessarily forged; rejecting it here keeps the
    // per-file metadata vectors below proportional to the input size.
    let num_files = cursor.count_bounded_by_remaining()?;

    let mut info = FilesInfo::empty();
    info.empty_stream = vec![false; num_files];
    info.mtimes = vec![None; num_files];
    info.ctimes = vec![None; num_files];
    info.atimes = vec![None; num_files];
    info.attributes = vec![0; num_files];

    loop {
        let id = cursor.u8()?;
        if id == PropertyId::End as u8 {
            break;
        }
        let size = cursor.count()?;
        let block = cursor.take(size)?;
        let mut sub = ByteCursor::new(block);

        match PropertyId::from_u8(id) {
            Some(PropertyId::EmptyStream) => {
                info.empty_stream = sub.bits(num_files)?;
            }
            Some(PropertyId::EmptyFile) => {
                let num_empty = info.empty_stream.iter().filter(|&&e| e).count();
                info.empty_file = sub.bits(num_empty)?;
            }
            Some(PropertyId::Anti) => {
                let num_empty = info.empty_stream.iter().filter(|&&e| e).count();
                info.anti = sub.bits(num_empty)?;
            }
            Some(PropertyId::Name) => {
                if sub.u8()? != 0 {
                    return Err(OxiArcError::unsupported_method("7z external file names"));
                }
                for _ in 0..num_files {
                    info.names.push(read_utf16_name(&mut sub)?);
                }
            }
            Some(PropertyId::MTime) => parse_file_times(&mut sub, num_files, &mut info.mtimes)?,
            Some(PropertyId::CTime) => parse_file_times(&mut sub, num_files, &mut info.ctimes)?,
            Some(PropertyId::ATime) => parse_file_times(&mut sub, num_files, &mut info.atimes)?,
            Some(PropertyId::WinAttributes) => {
                let defined = sub.optional_bits(num_files)?;
                let external = sub.u8()?;
                if external == 0 {
                    for (i, &is_defined) in defined.iter().enumerate() {
                        if is_defined {
                            info.attributes[i] = sub.u32_le()?;
                        }
                    }
                }
            }
            _ => {
                // Unknown or unhandled property: the sized block is skipped.
            }
        }
    }

    if info.names.len() != num_files {
        return Err(OxiArcError::invalid_header(
            "7z file name count does not match the file count",
        ));
    }
    Ok(info)
}

/// Parse a file-time property block into `out`.
fn parse_file_times(
    sub: &mut ByteCursor<'_>,
    num_files: usize,
    out: &mut [Option<SystemTime>],
) -> Result<()> {
    let defined = sub.optional_bits(num_files)?;
    let external = sub.u8()?;
    if external != 0 {
        // External timestamps are not supported; leave the defaults.
        return Ok(());
    }
    for (i, &is_defined) in defined.iter().enumerate() {
        if is_defined {
            let slot = out.get_mut(i).ok_or_else(|| {
                OxiArcError::invalid_header("7z time property index out of range")
            })?;
            *slot = filetime_to_system_time(sub.u64_le()?);
        }
    }
    Ok(())
}

/// Read one NUL-terminated UTF-16LE name.
fn read_utf16_name(cursor: &mut ByteCursor<'_>) -> Result<String> {
    let mut units = Vec::new();
    loop {
        let bytes = cursor.take(2)?;
        let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    // 7z archives created on Windows may use '\' as the path separator.
    Ok(String::from_utf16_lossy(&units).replace('\\', "/"))
}

/// Convert a Windows FILETIME to `SystemTime`.
///
/// FILETIME counts 100-nanosecond intervals since 1601-01-01; times before
/// the Unix epoch yield `None`.
fn filetime_to_system_time(filetime: u64) -> Option<SystemTime> {
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let unix_100ns = filetime.checked_sub(EPOCH_DIFF)?;
    let secs = unix_100ns / 10_000_000;
    let nanos = ((unix_100ns % 10_000_000) * 100) as u32;
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

/// Assemble the entry list from streams info and files info.
fn assemble_entries(streams: &StreamsInfo, files: FilesInfo) -> Result<Vec<SevenZEntry>> {
    let num_files = files.names.len();
    let total_substreams: u64 = streams.num_unpack_streams.iter().sum();
    let stream_files = files.empty_stream.iter().filter(|&&e| !e).count() as u64;
    if stream_files != total_substreams {
        return Err(OxiArcError::invalid_header(format!(
            "7z stream count mismatch: {} stream files vs {} substreams",
            stream_files, total_substreams
        )));
    }

    let mut entries = Vec::with_capacity(num_files);
    let mut folder_index = 0usize;
    let mut within_folder = 0u64;
    let mut offset_in_folder = 0u64;
    let mut global_stream = 0usize;
    let mut empty_rank = 0usize;

    for i in 0..num_files {
        let name = files.names[i].clone();
        let mtime = files.mtimes.get(i).copied().flatten();
        let ctime = files.ctimes.get(i).copied().flatten();
        let atime = files.atimes.get(i).copied().flatten();
        let attributes = files.attributes.get(i).copied().unwrap_or(0);

        if files.empty_stream.get(i).copied().unwrap_or(false) {
            let is_empty_file = files.empty_file.get(empty_rank).copied().unwrap_or(false);
            let is_anti = files.anti.get(empty_rank).copied().unwrap_or(false);
            empty_rank += 1;
            entries.push(SevenZEntry {
                name,
                // No stream and no empty-file/anti marker means directory.
                is_dir: !is_empty_file && !is_anti,
                is_anti,
                size: 0,
                crc: None,
                mtime,
                ctime,
                atime,
                attributes,
                folder_index: None,
                offset_in_folder: 0,
            });
            continue;
        }

        // Advance to the folder holding the next substream.
        while folder_index < streams.folders.len()
            && within_folder
                >= streams
                    .num_unpack_streams
                    .get(folder_index)
                    .copied()
                    .unwrap_or(0)
        {
            folder_index += 1;
            within_folder = 0;
            offset_in_folder = 0;
        }
        if folder_index >= streams.folders.len() {
            return Err(OxiArcError::invalid_header(
                "7z folder assignment ran out of folders",
            ));
        }

        let size = streams
            .substream_sizes
            .get(global_stream)
            .copied()
            .ok_or_else(|| OxiArcError::invalid_header("7z substream size list too short"))?;
        let crc = streams.substream_crcs.get(global_stream).copied().flatten();

        entries.push(SevenZEntry {
            name,
            is_dir: false,
            is_anti: false,
            size,
            crc,
            mtime,
            ctime,
            atime,
            attributes,
            folder_index: Some(folder_index),
            offset_in_folder,
        });

        offset_in_folder = offset_in_folder
            .checked_add(size)
            .ok_or_else(|| OxiArcError::invalid_header("7z folder offset overflows"))?;
        within_folder += 1;
        global_stream += 1;
    }

    Ok(entries)
}

/// Read and decode the full unpacked data of one folder.
fn decode_folder_data<R: Read + Seek>(
    reader: &mut R,
    streams: &StreamsInfo,
    folder_index: usize,
) -> Result<Vec<u8>> {
    let folder = streams
        .folders
        .get(folder_index)
        .ok_or_else(|| OxiArcError::corrupted(0, "Invalid folder index"))?;

    if folder.packed_indices.len() != 1 {
        return Err(OxiArcError::unsupported_method(
            "7z multi-input coder graphs (e.g. BCJ2)",
        ));
    }
    for coder in &folder.coders {
        if coder.num_in_streams != 1 || coder.num_out_streams != 1 {
            return Err(OxiArcError::unsupported_method(
                "7z complex coder configurations",
            ));
        }
    }

    // Read this folder's packed stream.
    let (pack_start, _) = streams.folder_pack_range(folder_index);
    let pack_offset: u64 = streams.pack_pos
        + streams
            .pack_sizes
            .iter()
            .take(pack_start)
            .copied()
            .sum::<u64>();
    let pack_size = streams
        .pack_sizes
        .get(pack_start)
        .copied()
        .ok_or_else(|| OxiArcError::invalid_header("7z packed stream index out of range"))?;

    reader.seek(SeekFrom::Start(pack_offset))?;
    let packed = read_bounded_by_remaining(reader, pack_size)?;

    let data = decode_coder_chain(folder, packed)?;

    if let Some(expected) = folder.unpack_crc {
        let actual = Crc32::compute(&data);
        if actual != expected {
            return Err(OxiArcError::crc_mismatch(expected, actual));
        }
    }

    Ok(data)
}

/// Walk a folder's coder chain via its bind pairs and return the final output.
///
/// With 1-in/1-out coders, input stream `i` and output stream `i` both belong
/// to coder `i`, so the chain is a linear walk: start at the coder fed by the
/// packed stream, then repeatedly follow the bind pair whose *output* index is
/// the coder just decoded.
///
/// Two hardening rules apply, both driven by the fact that the bind-pair graph
/// is attacker-controlled metadata (SEVENZ-02):
///
/// 1. **Acyclic**: a coder may be decoded at most once per folder. A crafted
///    self-referential or cyclic bind-pair graph could otherwise re-enter the
///    same decompressor repeatedly, each pass expanding the previous pass's
///    output — a nested decompression bomb built from a tiny archive.
/// 2. **Cumulative output budget**: the total number of bytes produced across
///    *all* stages is bounded by [`folder_decode_budget`], checked both against
///    the declared per-stage size (before decoding) and against the bytes each
///    stage actually produced (after decoding). The declared size is only ever
///    used to reject early — never to authorise a larger allocation.
fn decode_coder_chain(folder: &Folder, packed: Vec<u8>) -> Result<Vec<u8>> {
    let start = *folder
        .packed_indices
        .first()
        .ok_or_else(|| OxiArcError::invalid_header("7z folder has no packed stream"))?
        as usize;
    if start >= folder.coders.len() {
        return Err(OxiArcError::invalid_header(
            "7z packed stream index out of coder range",
        ));
    }

    let budget = folder_decode_budget(packed.len() as u64);
    let mut produced: u64 = 0;
    let mut visited = vec![false; folder.coders.len()];
    visited[start] = true;

    let mut data = decode_chain_stage(
        &folder.coders[start],
        packed,
        folder.unpack_sizes.get(start).copied(),
        budget,
        &mut produced,
    )?;

    let mut current = start;
    for _ in 0..folder.coders.len() {
        match folder
            .bind_pairs
            .iter()
            .find(|&&(_, out)| out as usize == current)
        {
            Some(&(in_index, _)) => {
                let next = in_index as usize;
                if next >= folder.coders.len() {
                    return Err(OxiArcError::invalid_header(
                        "7z bind pair index out of coder range",
                    ));
                }
                if visited[next] {
                    return Err(OxiArcError::invalid_header(
                        "7z folder bind pairs form a cycle: coder chain revisits a coder",
                    ));
                }
                visited[next] = true;
                data = decode_chain_stage(
                    &folder.coders[next],
                    data,
                    folder.unpack_sizes.get(next).copied(),
                    budget,
                    &mut produced,
                )?;
                current = next;
            }
            None => break,
        }
    }

    Ok(data)
}

/// Cumulative decoded-byte budget for one folder's coder chain.
///
/// The chain is allowed to expand its packed input by up to
/// [`MAX_FOLDER_EXPANSION_RATIO`], with a floor of
/// [`MIN_FOLDER_DECODED_ALLOWANCE`] so that legitimately tiny packed streams
/// still decode, and an absolute ceiling of [`MAX_FOLDER_DECODED_BYTES`] so a
/// large packed stream cannot license an unbounded chain. Real archives sit far
/// below all three: even LZMA over highly redundant data tops out around a
/// 10^4 expansion in a single stage, whereas a bomb needs the *product* of
/// several stages.
fn folder_decode_budget(packed_len: u64) -> u64 {
    packed_len
        .saturating_mul(MAX_FOLDER_EXPANSION_RATIO)
        .clamp(MIN_FOLDER_DECODED_ALLOWANCE, MAX_FOLDER_DECODED_BYTES)
}

/// Decode one stage of a folder's coder chain under the cumulative budget.
fn decode_chain_stage(
    coder: &Coder,
    input: Vec<u8>,
    out_size: Option<u64>,
    budget: u64,
    produced: &mut u64,
) -> Result<Vec<u8>> {
    // Cheap pre-check: if the header *claims* an output size that already blows
    // the budget, reject before running the decompressor at all.
    if let Some(declared) = out_size {
        let projected = produced.saturating_add(declared);
        if projected > budget {
            return Err(budget_error(budget, projected));
        }
    }

    let output = decode_coder(coder, input, out_size)?;

    // Authoritative check: what the stage actually produced.
    let projected = produced.saturating_add(output.len() as u64);
    if projected > budget {
        return Err(budget_error(budget, projected));
    }
    *produced = projected;
    Ok(output)
}

/// Build the budget error, clamping the u64 counters into the error's `usize`
/// fields so the report is meaningful on 32-bit targets too.
fn budget_error(budget: u64, requested: u64) -> OxiArcError {
    OxiArcError::memory_budget_exceeded(
        usize::try_from(budget).unwrap_or(usize::MAX),
        usize::try_from(requested).unwrap_or(usize::MAX),
    )
}

/// Decode the output of one coder.
fn decode_coder(coder: &Coder, input: Vec<u8>, out_size: Option<u64>) -> Result<Vec<u8>> {
    let output = match &coder.codec_id {
        CodecId::Copy => input,
        CodecId::Lzma => {
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
            let cursor = std::io::Cursor::new(input);
            oxiarc_lzma::decompress_raw(cursor, props, dict_size, out_size)?
        }
        CodecId::Lzma2 => {
            if coder.properties.is_empty() {
                return Err(OxiArcError::invalid_header("Invalid LZMA2 properties"));
            }
            let dict_size = oxiarc_lzma::dict_size_from_props(coder.properties[0]);
            let mut decoder = Lzma2Decoder::new(dict_size);
            let mut cursor = std::io::Cursor::new(input);
            decoder.decode(&mut cursor)?
        }
        CodecId::Deflate => oxiarc_deflate::inflate(&input)?,
        CodecId::BZip2 => oxiarc_bzip2::decompress(input.as_slice())?,
        CodecId::Delta => {
            let distance = coder
                .properties
                .first()
                .map(|&d| d as usize + 1)
                .unwrap_or(1);
            let mut data = input;
            for i in distance..data.len() {
                data[i] = data[i].wrapping_add(data[i - distance]);
            }
            data
        }
        CodecId::BcjX86 => {
            let mut data = input;
            bcj_x86_decode(&mut data);
            data
        }
        CodecId::Aes => {
            return Err(OxiArcError::unsupported_method(
                "password-protected 7z archives",
            ));
        }
        CodecId::Bcj2 | CodecId::Unknown(_) => {
            return Err(OxiArcError::unsupported_method(format!(
                "Unsupported codec: {:?}",
                coder.codec_id
            )));
        }
    };

    if let Some(expected) = out_size {
        if output.len() as u64 != expected {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "7z coder output size mismatch: expected {}, got {}",
                    expected,
                    output.len()
                ),
            ));
        }
    }

    Ok(output)
}

/// Decode the BCJ (x86) filter (same algorithm as Bra86 in xz / 7-Zip).
fn bcj_x86_decode(buf: &mut [u8]) {
    const MASK_TO_ALLOWED_STATUS: [bool; 8] = [true, true, true, false, true, false, false, false];
    const MASK_TO_BIT_NUMBER: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];

    #[inline(always)]
    fn test_ms_byte(b: u8) -> bool {
        b == 0x00 || b == 0xFF
    }

    let len = buf.len();
    if len < 5 {
        return;
    }
    let end = len - 5;
    let ip: u32 = 5; // Instruction-pointer adjustment from the stream start.
    let mut prev_pos: isize = -1;
    let mut prev_mask: u32 = 0;
    let mut i: usize = 0;

    while i <= end {
        let b = buf[i];
        if b != 0xE8 && b != 0xE9 {
            i += 1;
            continue;
        }
        prev_pos = i as isize - prev_pos;
        if (prev_pos & !3) != 0 {
            prev_mask = 0;
        } else {
            prev_mask = (prev_mask << (prev_pos - 1)) & 7;
            if prev_mask != 0 {
                let bit_number = MASK_TO_BIT_NUMBER[prev_mask as usize] as usize;
                if !MASK_TO_ALLOWED_STATUS[prev_mask as usize]
                    || test_ms_byte(buf[i + 4 - bit_number])
                {
                    prev_pos = i as isize;
                    prev_mask = ((prev_mask << 1) & 7) | 1;
                    i += 1;
                    continue;
                }
            }
        }
        prev_pos = i as isize;

        if test_ms_byte(buf[i + 4]) {
            let mut src = u32::from_le_bytes([buf[i + 1], buf[i + 2], buf[i + 3], buf[i + 4]]);
            let mut dest: u32;
            loop {
                dest = src.wrapping_sub(ip.wrapping_add(i as u32));
                if prev_mask == 0 {
                    break;
                }
                let index = MASK_TO_BIT_NUMBER[prev_mask as usize] * 8;
                let check = ((dest >> (24 - index)) & 0xFF) as u8;
                if !test_ms_byte(check) {
                    break;
                }
                src = dest ^ ((1u32 << (32 - index)).wrapping_sub(1));
            }
            buf[i + 1] = dest as u8;
            buf[i + 2] = (dest >> 8) as u8;
            buf[i + 3] = (dest >> 16) as u8;
            buf[i + 4] = if dest & 0x0100_0000 != 0 { 0xFF } else { 0x00 };
            i += 5;
        } else {
            prev_mask = ((prev_mask << 1) & 7) | 1;
            i += 1;
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
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos)
            .expect("read_number single byte");
        assert_eq!(num, 5);
        assert_eq!(pos, 1);
    }

    #[test]
    fn test_read_number_two_bytes() {
        // 0x80 means 1 extra byte follows
        let data = [0x80, 0x05];
        let mut pos = 0;
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos)
            .expect("read_number two bytes");
        assert_eq!(num, 5);
        assert_eq!(pos, 2);
    }

    #[test]
    fn test_read_number_first_byte_high_bits() {
        // 0x81 0x2C: one extra byte (0x2C) plus high bits from the first
        // byte: (0x01 << 8) | 0x2C = 300.
        let data = [0x81, 0x2C];
        let mut pos = 0;
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos)
            .expect("read_number with high bits");
        assert_eq!(num, 300);
        assert_eq!(pos, 2);
    }

    #[test]
    fn test_read_number_multi_byte_little_endian() {
        // Per 7zFormat.txt the extra bytes are little-endian: 0xC0 declares
        // two extra bytes, so [0x34, 0x12] decodes to 0x1234 (not 0x3412).
        let data = [0xC0, 0x34, 0x12];
        let mut pos = 0;
        let num = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos)
            .expect("read_number three bytes");
        assert_eq!(num, 0x1234);
        assert_eq!(pos, 3);
    }

    #[test]
    fn test_read_number_truncated() {
        let data = [0x80];
        let mut pos = 0;
        let result = SevenZReader::<std::io::Cursor<Vec<u8>>>::read_number(&data, &mut pos);
        assert!(result.is_err());
    }

    /// `read_bounded_by_remaining` must reject a declared length larger
    /// than the bytes actually remaining in the stream, rather than
    /// attempting a `vec![0u8; declared_len]`-style allocation up front.
    #[test]
    fn test_read_bounded_by_remaining_rejects_oversized_declaration() {
        let mut cursor = std::io::Cursor::new(vec![1u8, 2, 3, 4]);
        // Declare a size far beyond both the 4 bytes present here and any
        // reasonable archive size.
        let result = read_bounded_by_remaining(&mut cursor, u32::MAX as u64 * 4);
        assert!(result.is_err());
    }

    /// A declared length that is in range must still be read correctly.
    #[test]
    fn test_read_bounded_by_remaining_reads_valid_declaration() {
        let mut cursor = std::io::Cursor::new(vec![10u8, 20, 30, 40, 50]);
        let data = read_bounded_by_remaining(&mut cursor, 3).expect("read within bounds");
        assert_eq!(data, vec![10, 20, 30]);
    }

    /// A `SevenZReader::new` call over a signature header that declares an
    /// implausible `next_header_size` (larger than the bytes actually
    /// remaining after the signature header) must fail cleanly instead of
    /// attempting to allocate/zero-fill that many bytes.
    #[test]
    fn test_sevenz_reader_new_rejects_oversized_next_header_size() {
        // 32-byte signature header: 6-byte magic, 2-byte version, 4-byte
        // start-header CRC, 8-byte next_header_offset, 8-byte
        // next_header_size, 4-byte next_header_crc.
        let mut sig = vec![0u8; 32];
        sig[0..6].copy_from_slice(&SEVENZ_MAGIC);
        sig[6] = 0; // major version
        sig[7] = 4; // minor version

        let next_header_offset: u64 = 0;
        let next_header_size: u64 = 1 << 20; // declared, but no bytes follow

        sig[12..20].copy_from_slice(&next_header_offset.to_le_bytes());
        sig[20..28].copy_from_slice(&next_header_size.to_le_bytes());

        let start_header_crc = Crc32::compute(&sig[12..32]);
        sig[8..12].copy_from_slice(&start_header_crc.to_le_bytes());

        // No bytes follow the 32-byte signature header at all, so the
        // declared 1 MiB next-header size vastly exceeds what remains.
        let cursor = std::io::Cursor::new(sig);
        let result = SevenZReader::new(cursor);
        assert!(
            result.is_err(),
            "oversized next_header_size with no remaining bytes must error"
        );
    }

    /// Build a folder made of `count` `Copy` coders with the given bind pairs.
    fn copy_folder(count: usize, bind_pairs: Vec<(u64, u64)>, unpack_sizes: Vec<u64>) -> Folder {
        Folder {
            coders: (0..count)
                .map(|_| Coder {
                    codec_id: CodecId::Copy,
                    num_in_streams: 1,
                    num_out_streams: 1,
                    properties: Vec::new(),
                })
                .collect(),
            bind_pairs,
            packed_indices: vec![0],
            unpack_sizes,
            unpack_crc: None,
        }
    }

    /// A linear two-coder chain must still decode end to end.
    #[test]
    fn test_coder_chain_linear_chain_still_decodes() {
        let payload = b"7z coder chain payload".to_vec();
        let len = payload.len() as u64;
        // Bind pair (in=1, out=0): coder 1 consumes coder 0's output.
        let folder = copy_folder(2, vec![(1, 0)], vec![len, len]);

        let out = decode_coder_chain(&folder, payload.clone()).expect("linear chain must decode");
        assert_eq!(out, payload);
    }

    /// Regression test for SEVENZ-02: a self-referential bind pair must be
    /// rejected instead of re-entering the same coder.
    #[test]
    fn test_coder_chain_rejects_self_referential_bind_pair() {
        let payload = b"payload".to_vec();
        let len = payload.len() as u64;
        // Bind pair (in=0, out=0): coder 0 feeds itself.
        let folder = copy_folder(1, vec![(0, 0)], vec![len]);

        let err = decode_coder_chain(&folder, payload)
            .expect_err("self-referential bind pair must be rejected");
        assert!(
            matches!(err, OxiArcError::InvalidHeader { .. }),
            "expected InvalidHeader, got {err:?}"
        );
    }

    /// Regression test for SEVENZ-02: a cyclic two-coder bind-pair graph lets a
    /// crafted archive run the decompressor chain repeatedly over its own
    /// output. The walk must stop at the first revisit.
    #[test]
    fn test_coder_chain_rejects_cyclic_bind_pairs() {
        let payload = b"payload".to_vec();
        let len = payload.len() as u64;
        // (in=1, out=0) and (in=0, out=1): 0 -> 1 -> 0 -> ...
        let folder = copy_folder(2, vec![(1, 0), (0, 1)], vec![len, len]);

        let err =
            decode_coder_chain(&folder, payload).expect_err("cyclic bind pairs must be rejected");
        assert!(
            matches!(err, OxiArcError::InvalidHeader { .. }),
            "expected InvalidHeader, got {err:?}"
        );
    }

    /// A declared per-stage output size beyond the folder budget must be
    /// refused *before* the coder runs, so the header cannot authorise a huge
    /// allocation from a few bytes of packed input.
    #[test]
    fn test_coder_chain_rejects_oversized_declared_output() {
        let payload = b"tiny".to_vec();
        let folder = copy_folder(1, Vec::new(), vec![1 << 40]);

        let err = decode_coder_chain(&folder, payload)
            .expect_err("declared output beyond the budget must be rejected");
        assert!(
            matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
            "expected MemoryBudgetExceeded, got {err:?}"
        );
    }

    /// The budget scales with the packed size, but never below the floor and
    /// never above the absolute ceiling.
    #[test]
    fn test_folder_decode_budget_bounds() {
        assert_eq!(folder_decode_budget(0), MIN_FOLDER_DECODED_ALLOWANCE);
        assert_eq!(folder_decode_budget(1), MIN_FOLDER_DECODED_ALLOWANCE);
        // In the linear middle band the ratio rule applies verbatim.
        assert_eq!(
            folder_decode_budget(1024),
            1024 * MAX_FOLDER_EXPANSION_RATIO
        );
        // Large packed streams are clamped by the absolute ceiling.
        assert_eq!(folder_decode_budget(1024 * 1024), MAX_FOLDER_DECODED_BYTES);
        assert_eq!(folder_decode_budget(u64::MAX), MAX_FOLDER_DECODED_BYTES);
    }
}
