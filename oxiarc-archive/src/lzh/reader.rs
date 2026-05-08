//! LZH archive reader with extraction support.

use crate::lenient::{LenientWarning, LenientWarningKind};
use crate::lzh::header::LzhHeader;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::{Crc16, Entry};
use oxiarc_lzhuf::{LzhMethod, decode_lzh};
use std::io::{Read, Seek, SeekFrom, Write};

/// Internal entry info for extraction.
#[derive(Debug, Clone)]
pub(crate) struct LzhEntryInfo {
    pub(crate) entry: Entry,
    pub(crate) method: LzhMethod,
    pub(crate) crc16: u16,
    pub(crate) compressed_size: u32,
}

/// LZH archive reader with extraction support.
pub struct LzhReader<R: Read + Seek> {
    pub(crate) reader: R,
    pub(crate) entries: Vec<LzhEntryInfo>,
    /// Optional progress handle for tracking extraction progress.
    pub(crate) progress: Option<ProgressHandle>,
    /// When `true`, CRC-16 mismatches during extraction are recorded in
    /// [`LzhReader::warnings`] instead of returning an error. Disabled
    /// by default; toggle via [`LzhReader::lenient`].
    pub(crate) lenient: bool,
    /// Accumulated non-fatal warnings emitted while operating in
    /// lenient mode. Empty unless [`LzhReader::lenient`] has been set
    /// to `true`.
    pub(crate) warnings: Vec<LenientWarning>,
}

impl<R: Read + Seek> LzhReader<R> {
    /// Create a new LZH reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader, None)?;
        Ok(Self {
            reader,
            entries,
            progress: None,
            lenient: false,
            warnings: Vec::new(),
        })
    }

    /// Create a new LZH reader with progress reporting during entry scanning.
    pub fn new_with_progress(mut reader: R, handle: ProgressHandle) -> Result<Self> {
        let entries = Self::read_entries(&mut reader, Some(&handle))?;
        Ok(Self {
            reader,
            entries,
            progress: Some(handle),
            lenient: false,
            warnings: Vec::new(),
        })
    }

    /// Attach a progress callback handle (for extraction progress only;
    /// does not retroactively replay entry-scan progress).
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Enable or disable lenient-mode extraction.
    ///
    /// When enabled, CRC-16 mismatches during extraction are recorded
    /// in [`LzhReader::warnings`] and the (possibly corrupted) payload
    /// is returned to the caller anyway. When disabled (default),
    /// CRC-16 mismatches abort the extraction with
    /// [`OxiArcError::CorruptedData`].
    pub fn lenient(mut self, enabled: bool) -> Self {
        self.lenient = enabled;
        self
    }

    /// Return the accumulated non-fatal warnings from lenient-mode
    /// operations. Empty unless [`LzhReader::lenient`] has been set to
    /// `true`.
    pub fn warnings(&self) -> &[LenientWarning] {
        &self.warnings
    }

    /// Read all entries, optionally reporting progress.
    fn read_entries(
        reader: &mut R,
        progress: Option<&ProgressHandle>,
    ) -> Result<Vec<LzhEntryInfo>> {
        let mut entries = Vec::new();
        let mut offset = 0u64;
        let mut index: u64 = 0;

        while let Some(header) = LzhHeader::read(reader, offset)? {
            let entry = header.to_entry();
            let method = header.method;
            let crc16 = header.crc16;
            let compressed_size = header.compressed_size;

            if let Some(handle) = progress {
                handle.on_entry(&entry.name, index);
                handle.on_progress(entry.size, Some(entry.size));
            }

            // Skip compressed data using seek
            reader.seek(SeekFrom::Current(header.compressed_size as i64))?;

            offset = header.data_offset + header.compressed_size as u64;
            index += 1;

            entries.push(LzhEntryInfo {
                entry,
                method,
                crc16,
                compressed_size,
            });
        }

        if let Some(handle) = progress {
            handle.on_finish();
        }

        Ok(entries)
    }

    /// Get entries.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.iter().map(|e| e.entry.clone()).collect()
    }

    /// Extract an entry to a writer.
    pub fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64> {
        // Find the entry info
        let info = self
            .entries
            .iter()
            .find(|e| e.entry.offset == entry.offset)
            .ok_or_else(|| OxiArcError::invalid_header("Entry not found"))?
            .clone();

        // Emit extraction progress start
        if let Some(ref handle) = self.progress {
            handle.on_entry(&entry.name, 0);
        }

        // Seek to data offset
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read compressed data
        let mut compressed = vec![0u8; info.compressed_size as usize];
        self.reader.read_exact(&mut compressed)?;

        // Decompress
        let decompressed = if info.method == LzhMethod::Lh0 {
            // Stored (no compression)
            compressed
        } else {
            decode_lzh(&compressed, info.method, entry.size)?
        };

        // Verify CRC-16
        let computed_crc = Crc16::compute(&decompressed);
        if computed_crc != info.crc16 {
            if self.lenient {
                self.warnings.push(LenientWarning {
                    format: "LZH",
                    entry_name: Some(entry.name.clone()),
                    kind: LenientWarningKind::CrcMismatch {
                        expected: info.crc16 as u32,
                        computed: computed_crc as u32,
                    },
                    message: format!(
                        "CRC-16 mismatch for entry {:?} at offset {}: expected {:04X}, computed {:04X}",
                        entry.name, entry.offset, info.crc16, computed_crc
                    ),
                });
            } else {
                return Err(OxiArcError::corrupted(
                    entry.offset,
                    format!(
                        "CRC-16 mismatch: expected {:04X}, computed {:04X}",
                        info.crc16, computed_crc
                    ),
                ));
            }
        }

        // Write to output
        writer.write_all(&decompressed)?;

        // Emit extraction progress completion
        if let Some(ref handle) = self.progress {
            handle.on_progress(decompressed.len() as u64, Some(entry.size));
        }

        Ok(decompressed.len() as u64)
    }

    /// Extract an entry to a Vec.
    pub fn extract_to_vec(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(entry.size as usize);
        self.extract(entry, &mut data)?;
        Ok(data)
    }

    /// Extract an entry by name.
    pub fn extract_by_name(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let entry = self.entries.iter().find(|e| e.entry.name == name).cloned();
        match entry {
            Some(info) => Ok(Some(self.extract_to_vec(&info.entry)?)),
            None => Ok(None),
        }
    }

    /// Read the raw compressed payload for an entry without decompressing.
    ///
    /// Returns `(method, raw_compressed_bytes, crc16)`. The CRC-16 is the
    /// original value stored in the header and is **not** verified — the
    /// caller receives exactly `entry.compressed_size` bytes from disk.
    ///
    /// This is the LZH counterpart of `ZipReader::extract_raw` and is used
    /// by `oxiarc add` to preserve byte-fidelity when rewriting archives.
    pub fn read_raw_method_data(&mut self, entry: &Entry) -> Result<(LzhMethod, Vec<u8>, u16)> {
        let info = self
            .entries
            .iter()
            .find(|e| e.entry.offset == entry.offset)
            .ok_or_else(|| OxiArcError::invalid_header("Entry not found in LZH reader"))?
            .clone();

        self.reader.seek(SeekFrom::Start(entry.offset))?;
        let mut compressed = vec![0u8; info.compressed_size as usize];
        self.reader.read_exact(&mut compressed)?;

        Ok((info.method, compressed, info.crc16))
    }
}

/// Open a LZH archive using memory-mapped I/O for efficient large-file reading.
///
/// This is a convenience wrapper around [`LzhReader::new`] that opens the file
/// at `path` with [`oxiarc_core::mmap::MmapReader`], avoiding a full read into
/// memory while still offering random-access semantics.
///
/// # Errors
/// Returns an error if the file cannot be opened, cannot be memory-mapped, or
/// does not contain a valid LZH archive.
///
/// # Example
///
/// ```no_run
/// use oxiarc_archive::lzh::open_lzh_mmap;
///
/// let reader = open_lzh_mmap("large_archive.lzh").unwrap();
/// for entry in reader.entries() {
///     println!("{}", entry.name);
/// }
/// ```
#[cfg(feature = "mmap")]
pub fn open_lzh_mmap<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<LzhReader<oxiarc_core::mmap::MmapReader>> {
    let reader = oxiarc_core::mmap::MmapReader::open(path)?;
    LzhReader::new(reader)
}
