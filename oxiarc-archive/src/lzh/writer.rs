//! LZH archive writer.

use crate::lzh::extensions;
use crate::lzh::extensions::LzhExtensionMetadata;
use oxiarc_core::Crc16;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_lzhuf::{LzhMethod, encode_lzh};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// LZH compression level for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LzhCompressionLevel {
    /// Store without compression (lh0).
    Store,
    /// LH5 compression (8KB window, most compatible).
    #[default]
    Lh5,
}

/// LZH archive writer.
pub struct LzhWriter<W: Write> {
    writer: W,
    compression: LzhCompressionLevel,
    finished: bool,
    /// LZH header level to write (0, 1, 2, or 3).
    header_level: u8,
    /// Entry index counter for progress reporting.
    entry_index: u64,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
}

impl<W: Write> LzhWriter<W> {
    /// Create a new LZH writer with default compression (lh5).
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            compression: LzhCompressionLevel::default(),
            finished: false,
            header_level: 1,
            entry_index: 0,
            progress: None,
        }
    }

    /// Add a file with per-entry Unix metadata encoded as level-3
    /// extension headers.
    ///
    /// Equivalent to [`LzhWriter::add_file`] but also emits the LZH
    /// extension headers corresponding to each populated field in
    /// `metadata`. Only supported when the writer is configured with
    /// header level 3 (see [`LzhWriter::with_header_level`]); other
    /// levels silently drop the metadata because the level-0/1/2 header
    /// structures do not have a compatible extension slot in our
    /// encoder.
    pub fn add_file_with_metadata(
        &mut self,
        name: &str,
        data: &[u8],
        metadata: &LzhExtensionMetadata,
    ) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, self.compression, Some(metadata))
    }

    /// Set the header level for subsequent entries.
    /// Panics if `level > 3` (programmer error).
    pub fn with_header_level(mut self, level: u8) -> Self {
        assert!(level <= 3, "LZH header level must be 0, 1, 2, or 3");
        self.header_level = level;
        self
    }

    /// Attach a progress callback handle.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: LzhCompressionLevel) {
        self.compression = level;
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, self.compression, None)
    }

    /// Add a file with specific compression.
    pub fn add_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        compression: LzhCompressionLevel,
    ) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, compression, None)
    }

    /// Unified entry point for file emission. Handles compression
    /// selection, progress accounting, header-level dispatch, and
    /// optional extension-header emission for level-3 headers.
    fn add_file_with_options_and_metadata(
        &mut self,
        name: &str,
        data: &[u8],
        compression: LzhCompressionLevel,
        metadata: Option<&LzhExtensionMetadata>,
    ) -> Result<()> {
        // Determine the actual method and compressed bytes in one pass,
        // falling back to Lh0 (stored) if compression does not reduce size.
        // Progress is emitted exactly once regardless of any fallback.
        let (method, compressed) = match compression {
            LzhCompressionLevel::Store => (LzhMethod::Lh0, data.to_vec()),
            LzhCompressionLevel::Lh5 => {
                let comp = encode_lzh(data, LzhMethod::Lh5)?;
                if comp.len() < data.len() {
                    (LzhMethod::Lh5, comp)
                } else {
                    // Fall back to stored — no recursion, so progress fires once
                    (LzhMethod::Lh0, data.to_vec())
                }
            }
        };

        let crc16 = Crc16::compute(data);
        let mtime = Self::current_unix_time();

        // Emit progress: entry start (exactly once per logical file)
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, idx);
        }
        self.entry_index += 1;

        let original_size_u64 = data.len() as u64;
        let compressed_size_u64 = compressed.len() as u64;

        // Build effective metadata: auto-inject 64-bit size headers when
        // either size exceeds u32::MAX so that large files are always
        // round-trippable even when the caller does not set these fields.
        let needs_size64 =
            original_size_u64 > u32::MAX as u64 || compressed_size_u64 > u32::MAX as u64;

        // `size64_override` holds an owned copy of metadata augmented with
        // 64-bit size fields when required. It must outlive `effective_meta`.
        let size64_override: Option<LzhExtensionMetadata> = if needs_size64 {
            let mut m = metadata.cloned().unwrap_or_default();
            if original_size_u64 > u32::MAX as u64 {
                m.uncompressed_size64 = Some(original_size_u64);
            }
            if compressed_size_u64 > u32::MAX as u64 {
                m.compressed_size64 = Some(compressed_size_u64);
            }
            Some(m)
        } else {
            None
        };

        // Prefer the augmented metadata when present, else pass through
        // the caller-supplied reference unchanged.
        let effective_meta: Option<&LzhExtensionMetadata> = size64_override
            .as_ref()
            .map(|m| m as &LzhExtensionMetadata)
            .or(metadata);

        // Clamp sizes to u32 for base-header fields; the 64-bit extension
        // headers carry the true value when sizes exceed the 32-bit range.
        let original_size_u32 = original_size_u64.min(u32::MAX as u64) as u32;

        // Write header based on header_level
        match self.header_level {
            3 => self.write_level3_header(
                name,
                &compressed,
                original_size_u32,
                crc16,
                mtime,
                method,
                effective_meta,
            )?,
            _ => self.write_level1_header(
                name,
                &compressed,
                original_size_u32,
                crc16,
                mtime,
                method,
            )?,
        }

        // Write compressed data
        self.writer.write_all(&compressed)?;

        // Emit progress: bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(compressed.len() as u64, None);
        }

        Ok(())
    }

    /// Add a file to the archive with pre-compressed data verbatim.
    ///
    /// This preserves the existing compression method and CRC-16, writing the
    /// raw compressed payload without decompressing or re-compressing. The
    /// header checksum is always recomputed over the new header bytes.
    ///
    /// Only supported for level 1 and level 3 headers (matching the writer's
    /// configured [`LzhWriter::with_header_level`]). Level 0 is also handled
    /// via the level-1 path.
    ///
    /// # Arguments
    ///
    /// * `name` – Entry name in the archive
    /// * `method` – LZH compression method of the source entry
    /// * `crc16` – CRC-16 of the *uncompressed* data from the source
    /// * `original_size` – Uncompressed size of the source entry
    /// * `compressed_data` – Raw compressed payload to write verbatim
    /// * `mtime` – Modification time as a Unix timestamp (`u32`)
    /// * `metadata` – Optional extension-header metadata (level-3 only)
    #[allow(clippy::too_many_arguments)]
    pub fn add_file_raw(
        &mut self,
        name: &str,
        method: LzhMethod,
        crc16: u16,
        original_size: u64,
        compressed_data: &[u8],
        mtime: u32,
        metadata: Option<&LzhExtensionMetadata>,
    ) -> Result<()> {
        // Emit progress: entry start
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, idx);
        }
        self.entry_index += 1;

        // Auto-inject 0x42/0x43 extension headers when sizes exceed u32::MAX.
        let compressed_size_u64 = compressed_data.len() as u64;
        let needs_size64 = original_size > u32::MAX as u64 || compressed_size_u64 > u32::MAX as u64;
        let size64_override: Option<LzhExtensionMetadata> = if needs_size64 {
            let mut m = metadata.cloned().unwrap_or_default();
            if original_size > u32::MAX as u64 {
                m.uncompressed_size64 = Some(original_size);
            }
            if compressed_size_u64 > u32::MAX as u64 {
                m.compressed_size64 = Some(compressed_size_u64);
            }
            Some(m)
        } else {
            None
        };
        let effective_meta = size64_override
            .as_ref()
            .map(|m| m as &LzhExtensionMetadata)
            .or(metadata);

        let original_size_u32 = original_size.min(u32::MAX as u64) as u32;

        // Write header based on header_level, then the raw data
        match self.header_level {
            3 => self.write_level3_header(
                name,
                compressed_data,
                original_size_u32,
                crc16,
                mtime,
                method,
                effective_meta,
            )?,
            _ => self.write_level1_header(
                name,
                compressed_data,
                original_size_u32,
                crc16,
                mtime,
                method,
            )?,
        }

        // Write the raw (pre-compressed) data verbatim
        self.writer.write_all(compressed_data)?;

        // Emit progress: bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(compressed_data.len() as u64, None);
        }

        Ok(())
    }

    /// Add a directory to the archive.
    pub fn add_directory(&mut self, name: &str) -> Result<()> {
        // Ensure directory name ends with /
        let dir_name = if name.ends_with('/') || name.ends_with('\\') {
            name.to_string()
        } else {
            format!("{}/", name)
        };

        let mtime = Self::current_unix_time();

        // Emit progress: entry start
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(&dir_name, idx);
        }
        self.entry_index += 1;

        // Write level 1 header for empty directory (level 3 dirs also use same approach)
        match self.header_level {
            3 => self.write_level3_header(&dir_name, &[], 0, 0, mtime, LzhMethod::Lh0, None)?,
            _ => self.write_level1_header(&dir_name, &[], 0, 0, mtime, LzhMethod::Lh0)?,
        }

        // Emit progress: 0 bytes
        if let Some(ref handle) = self.progress {
            handle.on_progress(0, None);
        }

        Ok(())
    }

    /// Write a level 3 header.
    ///
    /// Level 3 format (all fields are little-endian):
    ///   word_size(2) | method(5) | compressed_size(4) | original_size(4) |
    ///   mtime(4) | attribute(1) | level(1=3) | crc16(2) | os_id(1) |
    ///   total_header_size(4) | next_ext_size(4) | [ext_type(1) + data…]* | terminator(4 zeros)
    ///
    /// Extension header `next_ext_size` covers only `ext_type + data` (not the size field itself).
    ///
    /// If `metadata` is `Some`, emits each populated field as an
    /// extension header in canonical order: filename (0x01) first,
    /// then (0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50, 0x51, 0x54).
    #[allow(clippy::too_many_arguments)]
    fn write_level3_header(
        &mut self,
        filename: &str,
        compressed: &[u8],
        original_size: u32,
        crc16: u16,
        mtime: u32,
        method: LzhMethod,
        metadata: Option<&LzhExtensionMetadata>,
    ) -> Result<()> {
        let filename_bytes = filename.as_bytes();
        let compressed_size = compressed.len() as u32;

        // Canonical extension-header payload list. Each element is a
        // `[type + data]` byte vector; the writer wraps each with a
        // leading 4-byte size prefix.
        let mut payloads: Vec<Vec<u8>> = Vec::new();

        // Filename extension (0x01)
        let mut fname = Vec::with_capacity(1 + filename_bytes.len());
        fname.push(0x01u8);
        fname.extend_from_slice(filename_bytes);
        payloads.push(fname);

        // Metadata-derived extensions (0x40 / 0x41 / 0x42 / 0x43 / 0x44 / 0x46 / 0x50)
        if let Some(meta) = metadata {
            payloads.extend(extensions::encode_metadata_payloads(meta));
        }

        // Fixed fields size:
        //   word_size(2) + method(5) + compressed(4) + original(4) + mtime(4) +
        //   attr(1) + level(1) + crc16(2) + os_id(1) + total_header_size(4)
        // = 28 bytes
        //
        // Variable part: for each payload, 4-byte size prefix + payload;
        // then a 4-byte terminator (0).
        let payloads_total: u32 = payloads.iter().map(|p| 4 + p.len() as u32).sum::<u32>() + 4;
        let total_header_size: u32 = 28 + payloads_total;

        let mut header: Vec<u8> = Vec::with_capacity(total_header_size as usize);

        // word_size field (2 bytes) — always 0x0004 for level 3
        header.extend_from_slice(&4u16.to_le_bytes());

        // Method ID (5 bytes)
        header.extend_from_slice(method.id());

        // Compressed size (4 bytes)
        header.extend_from_slice(&compressed_size.to_le_bytes());

        // Original size (4 bytes)
        header.extend_from_slice(&original_size.to_le_bytes());

        // mtime (4 bytes)
        header.extend_from_slice(&mtime.to_le_bytes());

        // Attribute (1 byte) — 0x20 for archive
        header.push(0x20u8);

        // Level (1 byte) — 3
        header.push(3u8);

        // CRC-16 (2 bytes)
        header.extend_from_slice(&crc16.to_le_bytes());

        // OS ID (1 byte) — 'U' for Unix
        header.push(b'U');

        // Total header size (4 bytes)
        header.extend_from_slice(&total_header_size.to_le_bytes());

        // Emit each extension payload with its 4-byte LE size prefix.
        for payload in &payloads {
            let size = payload.len() as u32;
            header.extend_from_slice(&size.to_le_bytes());
            header.extend_from_slice(payload);
        }

        // Terminator: next_ext_size = 0 (4 bytes)
        header.extend_from_slice(&0u32.to_le_bytes());

        self.writer.write_all(&header)?;

        Ok(())
    }

    /// Write a level 1 header.
    fn write_level1_header(
        &mut self,
        filename: &str,
        compressed: &[u8],
        original_size: u32,
        crc16: u16,
        mtime: u32,
        method: LzhMethod,
    ) -> Result<()> {
        let filename_bytes = filename.as_bytes();
        if filename_bytes.len() > 255 {
            return Err(OxiArcError::invalid_header("Filename too long"));
        }

        let compressed_size = compressed.len() as u32;

        // Calculate header size
        // Base header: 22 bytes + filename_len + 2 (extended header size = 0)
        let header_len = 22 + filename_bytes.len();

        // Header size byte (excludes the first 2 bytes: size and checksum)
        let header_size = (header_len - 2) as u8;

        // Build header
        let mut header = Vec::with_capacity(header_len);

        // Header size (will be updated with checksum)
        header.push(header_size);
        header.push(0u8); // Checksum placeholder

        // Method ID (5 bytes)
        header.extend_from_slice(method.id());

        // Compressed size (4 bytes)
        header.extend_from_slice(&compressed_size.to_le_bytes());

        // Original size (4 bytes)
        header.extend_from_slice(&original_size.to_le_bytes());

        // Modification time (4 bytes, Unix timestamp)
        header.extend_from_slice(&mtime.to_le_bytes());

        // Attributes (1 byte)
        header.push(0x20); // Archive attribute

        // Level (1 byte)
        header.push(1); // Level 1

        // Filename length (1 byte)
        header.push(filename_bytes.len() as u8);

        // Filename
        header.extend_from_slice(filename_bytes);

        // CRC-16 (2 bytes)
        header.extend_from_slice(&crc16.to_le_bytes());

        // OS ID (1 byte) - 'U' for Unix
        header.push(b'U');

        // Extended header size (2 bytes) - 0 for no extended headers
        header.extend_from_slice(&0u16.to_le_bytes());

        // Calculate checksum (sum of bytes from offset 2 to end, modulo 256)
        let checksum: u8 = header[2..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        header[1] = checksum;

        // Write header
        self.writer.write_all(&header)?;

        Ok(())
    }

    /// Finish the archive.
    pub fn finish(&mut self) -> Result<()> {
        if !self.finished {
            // Write end marker (0 byte)
            self.writer.write_all(&[0u8])?;
            self.writer.flush()?;
            self.finished = true;
            if let Some(ref handle) = self.progress {
                handle.on_finish();
            }
        }
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.finish()?;
        let this = std::mem::ManuallyDrop::new(self);
        Ok(unsafe { std::ptr::read(&this.writer) })
    }

    /// Get current Unix timestamp.
    fn current_unix_time() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0)
    }
}

impl<W: Write> Drop for LzhWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
