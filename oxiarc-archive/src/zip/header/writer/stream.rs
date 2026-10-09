//! Streaming (data-descriptor) ZIP entry writer.
//!
//! [`ZipWriter::add_stream`] hands back a [`ZipStreamWriter`] that the caller
//! writes entry bytes into; the bytes go straight to the archive's underlying
//! [`Write`] as they are produced, so a multi-gigabyte entry never has to be
//! held in memory. See [`ZipStreamWriter`] for the exact format contract.

use super::super::types::{CentralDirEntry, DATA_DESCRIPTOR_SIG, ZIP64_MARKER_32};
use super::{
    FLAG_DATA_DESCRIPTOR, LOCAL_FILE_HEADER_FIXED_LEN, LocalHeaderFields, VERSION_NEEDED_DEFLATE,
    VERSION_NEEDED_STORE, VERSION_NEEDED_ZIP64, ZipWriter, utf8_name_flag, write_local_header,
    zip64_extra_bytes,
};

/// Bytes a ZIP64 extended-information extra field occupies: the header ID and
/// data size, then the two 64-bit sizes.
const ZIP64_EXTRA_LEN: u64 = 20;
use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_deflate::Deflater;
use std::io::Write;
use std::time::SystemTime;

/// Options for a streamed ZIP entry.
///
/// Every field has a default that matches the behaviour of the buffered
/// [`ZipWriter::add_file`], so [`ZipStreamOptions::default`] plus
/// [`ZipWriter::add_stream`] is the plain case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipStreamOptions {
    /// Compression to apply to the entry's bytes.
    ///
    /// Note that a streamed entry commits to its method *before* any data is
    /// seen: unlike [`ZipWriter::add_file_with_options`] it cannot fall back
    /// to Stored once it discovers that deflating did not pay off, because the
    /// compression method is already recorded in the local file header that
    /// has been written by then. [`ZipCompressionLevel::Store`](super::ZipCompressionLevel::Store)
    /// still selects method 0 explicitly.
    pub compression: super::ZipCompressionLevel,

    /// Declare the entry as ZIP64 up front.
    ///
    /// A streamed entry's sizes are unknown when its local file header has to
    /// be written, so this flag decides *up front* whether the trailing data
    /// descriptor carries 32-bit or 64-bit sizes. Leave it `false` for entries
    /// that certainly stay under 4 GiB; set it for a file whose size you know
    /// to be 4 GiB or larger (or whose compressed size could reach that).
    ///
    /// With it set, the local header declares version-needed 4.5 and carries a
    /// ZIP64 extended-information extra field with placeholder zeros, which is
    /// how every other streaming ZIP writer (Info-ZIP, Java, Python's
    /// `force_zip64`) advertises a 64-bit data descriptor.
    pub zip64: bool,

    /// Modification time to stamp on the entry, or `None` for "now" (what
    /// every buffered writer method uses).
    pub mtime: Option<SystemTime>,
}

impl Default for ZipStreamOptions {
    fn default() -> Self {
        Self {
            compression: super::ZipCompressionLevel::Normal,
            zip64: false,
            mtime: None,
        }
    }
}

/// A ZIP entry being written incrementally.
///
/// Returned by [`ZipWriter::add_stream`] / [`ZipWriter::add_stream_with_options`]
/// and implementing [`Write`]: every byte handed to it is compressed (unless
/// the entry is Stored) and written to the archive's underlying writer
/// immediately, so peak memory is one DEFLATE window plus the caller's own
/// buffer, independent of the entry's size.
///
/// # Format contract
///
/// A ZIP local file header must carry the CRC-32 and both sizes, which are
/// only known once the data has been written, and this writer has no way to
/// seek back over a header it already emitted (its sink is a bare
/// [`Write`]). It therefore uses the streaming answer the format defines for
/// exactly this situation — **general-purpose bit flag bit 3**, the
/// "sizes/CRC follow the data" flag:
///
/// - the local file header is written with a zero CRC-32, zero sizes, and bit
///   3 set;
/// - after the entry's data, an optional-signature data descriptor
///   (`PK\x07\x08` + CRC-32 + compressed size + uncompressed size) carries the
///   real values;
/// - the central directory entry — which readers such as [`ZipReader`], Info-ZIP
///   and Python's `zipfile` actually consult — carries the same real values,
///   so both readers of the format agree on them.
///
/// [`ZipReader`]: crate::zip::ZipReader
///
/// ## ZIP64
///
/// The descriptor's width has to be decided before the data exists. Setting
/// [`ZipStreamOptions::zip64`] writes an 8-byte-size descriptor and advertises
/// it in the local header (version 4.5 + ZIP64 extra field with zero
/// placeholders); leaving it `false` writes the classic 4-byte-size
/// descriptor. If a non-ZIP64 streamed entry turns out to reach 4 GiB anyway,
/// [`finish`](Self::finish) fails with a typed error rather than emitting an
/// archive whose descriptor cannot describe it — pass `zip64: true` for such
/// entries.
///
/// This writer's own sequential reader ([`ZipStreamReader`]) resolves the same
/// question the same way (a ZIP64 extra field, or version-needed ≥ 4.5, in the
/// local header), so both descriptor forms round-trip through it.
///
/// [`ZipStreamReader`]: crate::zip::ZipStreamReader
///
/// ## Difference from the buffered path
///
/// The only behavioural difference from [`ZipWriter::add_file_with_options`] is
/// the one above: no "compressed data was not smaller, store it instead"
/// fallback, since the method field cannot be rewritten after the fact. The
/// deflate *bytes* themselves are identical — this writer feeds the same
/// [`Deflater`] the same data and closes it the same way, and that encoder is
/// documented to produce one continuous, byte-identical stream across
/// incremental calls.
///
/// # Example
///
/// ```
/// use oxiarc_archive::zip::ZipWriter;
/// use std::io::Write;
///
/// let mut output = Vec::new();
/// {
///     let mut writer = ZipWriter::new(&mut output);
///     {
///         let mut entry = writer.add_stream("big.bin")?;
///         entry.write_all(b"first half ")?;
///         entry.write_all(b"second half")?;
///         entry.finish()?;
///     }
///     writer.finish()?;
/// }
/// assert!(!output.is_empty());
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
///
/// # Drop
///
/// Dropping the entry finishes it best-effort, exactly as
/// [`ZipWriter`] itself does on drop, so an archive is never left with a
/// half-written entry. Call [`finish`](Self::finish) yourself when the error
/// matters.
pub struct ZipStreamWriter<'w, W: Write> {
    /// The archive being written to. Held mutably, so the borrow checker
    /// enforces that at most one entry is open at a time.
    archive: &'w mut ZipWriter<W>,
    /// Entry name, kept for the central directory record.
    name: String,
    /// Compression method written in both headers.
    method: u16,
    /// Version-needed written in the local header (and, unless ZIP64 forces a
    /// bump, in the central directory one).
    version_needed: u16,
    /// Whether the trailing data descriptor uses 64-bit sizes.
    zip64: bool,
    /// Deflate encoder, or `None` for a Stored entry.
    deflater: Option<Deflater>,
    /// Scratch buffer the deflater's output for one call lands in, so the
    /// compressed byte count can be tallied before it reaches the archive.
    scratch: Vec<u8>,
    /// Running CRC-32 of the uncompressed entry bytes.
    crc32: Crc32,
    /// Uncompressed bytes accepted so far.
    uncompressed_size: u64,
    /// Compressed bytes emitted so far.
    compressed_size: u64,
    /// DOS modification time/date words for both headers.
    mtime: u16,
    mdate: u16,
    /// Offset of this entry's local file header within the archive.
    local_header_offset: u64,
    /// Whether [`finish`](Self::finish) has already run.
    finished: bool,
}

impl<W: Write> std::fmt::Debug for ZipStreamWriter<'_, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipStreamWriter")
            .field("name", &self.name)
            .field("method", &self.method)
            .field("uncompressed_size", &self.uncompressed_size)
            .field("compressed_size", &self.compressed_size)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<'w, W: Write> ZipStreamWriter<'w, W> {
    /// Open a streamed entry in `archive`: emit the entry's local file header
    /// (zero CRC-32, zero or ZIP64-marker sizes, general-purpose bit 3 set) and
    /// return the writer the caller feeds the entry's bytes to.
    pub(super) fn new(
        archive: &'w mut ZipWriter<W>,
        name: &str,
        options: ZipStreamOptions,
        method: u16,
        deflater: Option<Deflater>,
        mtime: u16,
        mdate: u16,
    ) -> Result<Self> {
        let zip64 = options.zip64;
        let version_needed = version_needed_for(method, zip64);
        let local_header_offset = archive.offset;
        write_local_header(
            archive.writer_mut()?,
            &LocalHeaderFields {
                name,
                version_needed,
                flags: FLAG_DATA_DESCRIPTOR | utf8_name_flag(name),
                method,
                mtime,
                mdate,
                crc32: 0,
                // With bit 3 set the sizes belong in the trailing data
                // descriptor. In the ZIP64 form the 32-bit fields carry the
                // marker and the real values live in the placeholder extra
                // field and the descriptor.
                compressed_size: size_placeholder(zip64),
                uncompressed_size: size_placeholder(zip64),
                extra: &if zip64 {
                    zip64_extra_bytes(0, 0)
                } else {
                    Vec::new()
                },
            },
        )?;
        archive.offset += LOCAL_FILE_HEADER_FIXED_LEN
            + name.len() as u64
            + if zip64 { ZIP64_EXTRA_LEN } else { 0 };

        Ok(Self {
            archive,
            name: name.to_string(),
            method,
            version_needed,
            zip64,
            deflater,
            scratch: Vec::new(),
            crc32: Crc32::new(),
            uncompressed_size: 0,
            compressed_size: 0,
            mtime,
            mdate,
            local_header_offset,
            finished: false,
        })
    }

    /// Bytes of entry data accepted so far (the uncompressed size).
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Bytes emitted to the archive for this entry so far (the compressed
    /// size, excluding the local header and the data descriptor).
    pub fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// Whether [`finish`](Self::finish) has already run.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Bytes the data descriptor will occupy once the entry is finished:
    /// signature + CRC-32 + the two sizes.
    fn descriptor_len(&self) -> u64 {
        if self.zip64 { 24 } else { 16 }
    }

    /// Build the error returned when a non-ZIP64 streamed entry outgrew the
    /// 32-bit descriptor it committed to.
    fn needs_zip64_error(name: &str, compressed_size: u64, uncompressed_size: u64) -> OxiArcError {
        OxiArcError::encoding_error(format!(
            "streamed ZIP entry '{name}' reached {uncompressed_size} uncompressed / \
             {compressed_size} compressed bytes, which does not fit the 32-bit data \
             descriptor its local file header committed to; stream it with \
             ZipStreamOptions {{ zip64: true, .. }} instead"
        ))
    }

    /// Close the DEFLATE stream (if any) and write the data descriptor,
    /// recording the central directory entry.
    ///
    /// Idempotent: calling it more than once (including implicitly via
    /// [`Drop`]) writes the descriptor only once.
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::Io`] if the underlying writer fails, and
    /// [`OxiArcError::EncodingError`] if a non-ZIP64 entry outgrew its
    /// 32-bit data descriptor — the local header is already on disk and
    /// cannot be widened, so the archive is left unterminated (the caller
    /// should abort it rather than call [`ZipWriter::finish`]).
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        // Terminate the deflate stream. A Stored entry has nothing to flush.
        if let Some(deflater) = self.deflater.as_mut() {
            self.scratch.clear();
            deflater.deflate(&[], &mut self.scratch, true)?;
            let produced = self.scratch.len() as u64;
            self.archive.writer_mut()?.write_all(&self.scratch)?;
            self.archive.offset += produced;
            self.compressed_size += produced;
        }

        let crc32 = self.crc32.value();
        if !self.zip64
            && (self.compressed_size >= ZIP64_MARKER_32 as u64
                || self.uncompressed_size >= ZIP64_MARKER_32 as u64)
        {
            return Err(Self::needs_zip64_error(
                &self.name,
                self.compressed_size,
                self.uncompressed_size,
            ));
        }

        // Data descriptor: optional signature, CRC-32, then the two sizes
        // (8 bytes each in the ZIP64 form).
        let mut descriptor = Vec::with_capacity(self.descriptor_len() as usize);
        descriptor.extend_from_slice(&DATA_DESCRIPTOR_SIG.to_le_bytes());
        descriptor.extend_from_slice(&crc32.to_le_bytes());
        if self.zip64 {
            descriptor.extend_from_slice(&self.compressed_size.to_le_bytes());
            descriptor.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        } else {
            descriptor.extend_from_slice(&(self.compressed_size as u32).to_le_bytes());
            descriptor.extend_from_slice(&(self.uncompressed_size as u32).to_le_bytes());
        }
        self.archive.writer_mut()?.write_all(&descriptor)?;
        self.archive.offset += descriptor.len() as u64;

        // Store central directory entry. `CentralDirEntry::write` adds the
        // ZIP64 extended information itself whenever a size or the local
        // header offset outgrows its 32-bit field.
        self.archive.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed: self.version_needed,
            flags: super::FLAG_DATA_DESCRIPTOR | super::utf8_name_flag(&self.name),
            method: self.method,
            mtime: self.mtime,
            mdate: self.mdate,
            crc32,
            compressed_size: self.compressed_size,
            uncompressed_size: self.uncompressed_size,
            filename: self.name.clone(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16,
            local_header_offset: self.local_header_offset,
        });

        self.finished = true;

        // Progress: notify about bytes written. A non-empty entry already
        // reported its size incrementally; a zero-length one has nothing to
        // report at write time, so announce it here.
        if self.uncompressed_size == 0 {
            if let Some(ref handle) = self.archive.progress {
                handle.on_progress(0, None);
            }
        }

        Ok(())
    }
}

impl<W: Write> Write for ZipStreamWriter<'_, W> {
    /// Compress (unless Stored) and write `buf` to the archive immediately.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.finished {
            return Err(std::io::Error::other("write to a finished ZipStreamWriter"));
        }

        self.crc32.update(buf);
        self.uncompressed_size += buf.len() as u64;

        // Disjoint field borrows: the deflater, the scratch buffer it writes
        // into and the archive sink are all separate fields.
        let Self {
            archive,
            deflater,
            scratch,
            compressed_size,
            ..
        } = self;

        // Write through, then advance the archive's running offset by exactly
        // what was emitted. That counter is what later local file headers and
        // the central directory use as their base, so it has to track the real
        // stream position even though the data is produced incrementally.
        match deflater {
            Some(deflater) => {
                scratch.clear();
                deflater.deflate(buf, scratch, false).map_err(to_io_error)?;
                *compressed_size += scratch.len() as u64;
                archive
                    .writer_mut()
                    .map_err(to_io_error)?
                    .write_all(scratch)?;
                archive.offset += scratch.len() as u64;
            }
            None => {
                archive.writer_mut().map_err(to_io_error)?.write_all(buf)?;
                *compressed_size += buf.len() as u64;
                archive.offset += buf.len() as u64;
            }
        }

        // Progress: report the entry's cumulative uncompressed size. `None` as
        // the total is what every other streaming writer in the workspace
        // reports; a streamed entry's size is not known in advance.
        if let Some(ref handle) = archive.progress {
            handle.on_progress(self.uncompressed_size, None);
        }

        Ok(buf.len())
    }

    /// Flush the archive's underlying writer.
    ///
    /// Deliberately does *not* close the DEFLATE stream: that would cut the
    /// entry's compressed data short. Use [`finish`](Self::finish) to end the
    /// entry.
    fn flush(&mut self) -> std::io::Result<()> {
        self.archive.writer_mut().map_err(to_io_error)?.flush()
    }
}

impl<W: Write> Drop for ZipStreamWriter<'_, W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Convert an [`OxiArcError`] into an [`std::io::Error`] for the [`Write`]
/// impl, which cannot return the crate's own error type.
fn to_io_error(err: OxiArcError) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

/// Version-needed for a streamed entry: 4.5 when the entry declares itself
/// ZIP64 (its data descriptor carries 64-bit sizes), 2.0 for deflate, 1.0 for
/// stored.
fn version_needed_for(method: u16, zip64: bool) -> u16 {
    if zip64 {
        VERSION_NEEDED_ZIP64
    } else if method == 8 {
        VERSION_NEEDED_DEFLATE
    } else {
        VERSION_NEEDED_STORE
    }
}

/// The 32-bit size field a streamed entry's local header carries: zero when
/// the real value goes into a classic 4-byte data descriptor, the ZIP64 marker
/// when it goes into the 8-byte one.
fn size_placeholder(zip64: bool) -> u32 {
    if zip64 { ZIP64_MARKER_32 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::ZipReader;
    use crate::zip::header::types::{
        CENTRAL_DIR_HEADER_SIG, DATA_DESCRIPTOR_SIG, END_OF_CENTRAL_DIR_SIG, FLAG_DATA_DESCRIPTOR,
        LOCAL_FILE_HEADER_SIG, ZIP64_EXTRA_FIELD_ID, ZIP64_MARKER_32,
    };
    use crate::zip::header::writer::ZipCompressionLevel;
    use crate::zip::stream::ZipStreamReader;
    use oxiarc_core::entry::CompressionMethod as CoreMethod;
    use std::io::{Cursor, Read};

    /// Compressible payload of `len` bytes.
    fn compressible(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| b"streaming zip entry payload "[i % 28])
            .collect()
    }

    /// Incompressible payload of `len` bytes (deterministic xorshift).
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// Build a one-entry archive whose payload was streamed in `chunk`-sized
    /// writes.
    fn stream_one_entry(data: &[u8], chunk: usize, options: ZipStreamOptions) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            {
                let mut entry = writer
                    .add_stream_with_options("payload.bin", options)
                    .expect("add_stream_with_options");
                let mut pos = 0;
                while pos < data.len() {
                    let end = (pos + chunk).min(data.len());
                    entry.write_all(&data[pos..end]).expect("write_all");
                    pos = end;
                }
                entry.finish().expect("entry finish");
            }
            writer.finish().expect("archive finish");
        }
        output
    }

    /// Read `output` back through the central-directory reader and return the
    /// single entry's decompressed bytes.
    fn extract_via_central_directory(output: &[u8]) -> Vec<u8> {
        let mut reader = ZipReader::new(Cursor::new(output)).expect("ZipReader::new");
        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 1, "expected exactly one entry");
        reader.extract(&entries[0]).expect("extract")
    }

    /// Read `output` back through the sequential (Read-only) streaming reader,
    /// which is the code path that has to parse the data descriptor.
    fn extract_via_stream_reader(output: &[u8]) -> Vec<u8> {
        let mut reader = ZipStreamReader::new(Cursor::new(output));
        let mut collected = Vec::new();
        while let Some(mut entry) = reader.next_entry().expect("next_entry") {
            assert_eq!(entry.meta.name, "payload.bin");
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).expect("read_to_end");
            collected.push(buf);
        }
        assert_eq!(collected.len(), 1, "expected exactly one entry");
        collected.pop().expect("one entry")
    }

    /// A streamed deflate entry must round-trip through both readers, whatever
    /// sizes the caller writes in.
    #[test]
    fn test_stream_round_trips_deflate_entry() {
        let data = compressible(200_000);
        for chunk in [1, 7, 4096, 65_537, data.len()] {
            let output = stream_one_entry(
                &data,
                chunk,
                ZipStreamOptions {
                    compression: ZipCompressionLevel::Normal,
                    ..ZipStreamOptions::default()
                },
            );
            assert_eq!(
                extract_via_central_directory(&output),
                data,
                "central-directory round trip failed for a {chunk} byte write chunk"
            );
            assert_eq!(
                extract_via_stream_reader(&output),
                data,
                "stream-reader round trip failed for a {chunk} byte write chunk"
            );
        }
    }

    /// An incompressible payload must round-trip too: deflate may emit more
    /// bytes than it consumed, and the descriptor/central-directory sizes have
    /// to describe that honestly.
    #[test]
    fn test_stream_round_trips_incompressible_entry() {
        let data = incompressible(120_000);
        let output = stream_one_entry(
            &data,
            3333,
            ZipStreamOptions {
                compression: ZipCompressionLevel::Best,
                ..ZipStreamOptions::default()
            },
        );
        assert_eq!(extract_via_central_directory(&output), data);
        assert_eq!(extract_via_stream_reader(&output), data);
    }

    /// A zero-length streamed entry is legal: CRC 0, both sizes 0, and a
    /// descriptor saying exactly that.
    #[test]
    fn test_stream_round_trips_empty_entry() {
        let output = stream_one_entry(&[], 4096, ZipStreamOptions::default());
        assert_eq!(extract_via_central_directory(&output), Vec::<u8>::new());

        let reader = ZipReader::new(Cursor::new(&output)).expect("ZipReader::new");
        let entries = reader.entries().to_vec();
        assert_eq!(entries[0].size, 0, "uncompressed size");
        assert_eq!(entries[0].crc32, Some(0), "CRC-32 of nothing");
        // A deflate entry still emits the two-byte empty final block, so its
        // compressed size is that — not zero.
        assert_eq!(entries[0].compressed_size, 2, "compressed size");
    }

    /// Stored entries stream verbatim; their data bytes must be exactly what
    /// the caller wrote.
    #[test]
    fn test_stream_round_trips_stored_entry() {
        let data = incompressible(50_000);
        let output = stream_one_entry(
            &data,
            1024,
            ZipStreamOptions {
                compression: ZipCompressionLevel::Store,
                ..ZipStreamOptions::default()
            },
        );

        let mut reader = ZipReader::new(Cursor::new(&output)).expect("ZipReader::new");
        let entries = reader.entries().to_vec();
        assert_eq!(entries[0].method, CoreMethod::Stored);
        assert_eq!(entries[0].compressed_size, data.len() as u64);
        assert_eq!(reader.extract_raw(&entries[0]).expect("extract_raw"), data);

        // The sequential reader cannot delimit a stored entry whose length
        // only appears after the data (bit 3 + method 0 is ambiguous), so it
        // must reject it rather than mis-parse — the central directory is the
        // supported reader for this shape.
        let mut stream = ZipStreamReader::new(Cursor::new(&output));
        let err = stream
            .next_entry()
            .err()
            .expect("stored + data descriptor must be rejected by the stream reader");
        assert!(
            err.to_string().contains("Stored"),
            "unexpected error: {err}"
        );
    }

    /// The ZIP64 form (8-byte descriptor sizes, marker sizes plus a ZIP64
    /// extra field in the local header) round-trips through both readers too,
    /// which is what makes entries of 4 GiB and beyond streamable.
    #[test]
    fn test_stream_round_trips_zip64_entry() {
        let data = compressible(70_000);
        let output = stream_one_entry(
            &data,
            999,
            ZipStreamOptions {
                zip64: true,
                ..ZipStreamOptions::default()
            },
        );

        // The local header advertises ZIP64 twice: version-needed 4.5 and a
        // ZIP64 extra field carrying placeholder zeros.
        assert_eq!(
            u16::from_le_bytes([output[4], output[5]]),
            super::VERSION_NEEDED_ZIP64
        );
        let extra_len = u16::from_le_bytes([output[28], output[29]]) as usize;
        assert_eq!(extra_len, 20, "expected the ZIP64 extra field");
        // The extra field is written after the file name.
        let name_len = u16::from_le_bytes([output[26], output[27]]) as usize;
        assert_eq!(
            u16::from_le_bytes([output[30 + name_len], output[31 + name_len]]),
            ZIP64_EXTRA_FIELD_ID,
            "the extra field must be the ZIP64 extended information record"
        );
        assert!(
            u32::from_le_bytes([output[18], output[19], output[20], output[21]]) == ZIP64_MARKER_32,
            "compressed size must carry the ZIP64 marker"
        );

        assert_eq!(extract_via_central_directory(&output), data);
        assert_eq!(extract_via_stream_reader(&output), data);
    }

    /// The deflate bytes a streamed entry produces are byte-identical to the
    /// ones the buffered `add_file` produces for the same payload — only the
    /// framing around them (zeroed header + data descriptor) differs.
    #[test]
    fn test_stream_deflate_payload_is_byte_identical_to_add_file() {
        let data = compressible(80_000);

        let mut buffered = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut buffered);
            writer
                .add_file_with_options("payload.bin", &data, ZipCompressionLevel::Normal)
                .expect("add_file_with_options");
            writer.finish().expect("finish");
        }
        let streamed = stream_one_entry(
            &data,
            512,
            ZipStreamOptions {
                compression: ZipCompressionLevel::Normal,
                ..ZipStreamOptions::default()
            },
        );

        let mut buffered_reader = ZipReader::new(Cursor::new(&buffered)).expect("reader");
        let mut streamed_reader = ZipReader::new(Cursor::new(&streamed)).expect("reader");
        let buffered_raw = buffered_reader
            .extract_raw(&buffered_reader.entries()[0].clone())
            .expect("extract_raw buffered");
        let streamed_raw = streamed_reader
            .extract_raw(&streamed_reader.entries()[0].clone())
            .expect("extract_raw streamed");

        assert_eq!(
            buffered_raw, streamed_raw,
            "incremental deflate must produce the same bytes as one-shot deflate"
        );
    }

    /// Regression guard for the buffered path: `add_file`/`add_file_stored`
    /// keep writing CRC and both sizes into the local file header, never set
    /// the data-descriptor flag, and emit no descriptor — i.e. the streaming
    /// path did not leak into (and change the bytes of) the existing API.
    #[test]
    fn test_buffered_entries_keep_their_original_framing() {
        let data = compressible(4096);

        let mut with_options = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut with_options);
            writer
                .add_file_with_options("a.txt", &data, ZipCompressionLevel::Normal)
                .expect("add_file_with_options");
            writer.finish().expect("finish");
        }
        let mut stored = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut stored);
            writer
                .add_file_stored("a.txt", &data)
                .expect("add_file_stored");
            writer.finish().expect("finish");
        }

        for (label, output) in [
            ("add_file", with_options.as_slice()),
            ("add_file_stored", stored.as_slice()),
        ] {
            assert_eq!(
                u32::from_le_bytes([output[0], output[1], output[2], output[3]]),
                LOCAL_FILE_HEADER_SIG,
                "{label}: local file header signature"
            );
            assert_eq!(
                u16::from_le_bytes([output[6], output[7]]) & FLAG_DATA_DESCRIPTOR,
                0,
                "{label}: the buffered path must not set general-purpose bit 3"
            );
            assert_eq!(
                u16::from_le_bytes([output[8], output[9]]),
                if label == "add_file_stored" { 0 } else { 8 },
                "{label}: compression method"
            );
            assert_ne!(
                u32::from_le_bytes([output[18], output[19], output[20], output[21]]),
                0,
                "{label}: the compressed size stays in the local header"
            );
            assert_ne!(
                u32::from_le_bytes([output[22], output[23], output[24], output[25]]),
                0,
                "{label}: the uncompressed size stays in the local header"
            );

            // No data descriptor: the bytes right after the entry's payload
            // are the central directory header.
            let cd_offset = u32::from_le_bytes([
                output[output.len() - 6],
                output[output.len() - 5],
                output[output.len() - 4],
                output[output.len() - 3],
            ]) as usize;
            assert_eq!(
                u32::from_le_bytes([
                    output[cd_offset],
                    output[cd_offset + 1],
                    output[cd_offset + 2],
                    output[cd_offset + 3]
                ]),
                CENTRAL_DIR_HEADER_SIG,
                "{label}: no data descriptor may follow the entry data"
            );
            assert_eq!(
                u32::from_le_bytes([
                    output[output.len() - 22],
                    output[output.len() - 21],
                    output[output.len() - 20],
                    output[output.len() - 19],
                ]),
                END_OF_CENTRAL_DIR_SIG,
                "{label}: end of central directory"
            );
        }
    }

    /// The data descriptor carries the optional `PK\x07\x08` signature and the
    /// same CRC-32 and sizes the central directory records.
    #[test]
    fn test_stream_descriptor_matches_central_directory() {
        let data = compressible(30_000);
        let output = stream_one_entry(
            &data,
            777,
            ZipStreamOptions {
                compression: ZipCompressionLevel::Fast,
                ..ZipStreamOptions::default()
            },
        );

        // Locate the descriptor: it is the last 16 bytes before the central
        // directory, which starts where the EOCD says it does.
        let cd_offset = u32::from_le_bytes([
            output[output.len() - 6],
            output[output.len() - 5],
            output[output.len() - 4],
            output[output.len() - 3],
        ]) as usize;
        let descriptor = &output[cd_offset - 16..cd_offset];
        assert_eq!(
            u32::from_le_bytes([descriptor[0], descriptor[1], descriptor[2], descriptor[3]]),
            DATA_DESCRIPTOR_SIG,
            "data descriptor signature"
        );

        let reader = ZipReader::new(Cursor::new(&output)).expect("ZipReader::new");
        let entry = reader.entries()[0].clone();
        assert_eq!(
            u32::from_le_bytes([descriptor[4], descriptor[5], descriptor[6], descriptor[7]]),
            entry.crc32.expect("crc32"),
            "descriptor CRC must match the central directory"
        );
        assert_eq!(
            u32::from_le_bytes([descriptor[8], descriptor[9], descriptor[10], descriptor[11]])
                as u64,
            entry.compressed_size,
            "descriptor compressed size must match the central directory"
        );
        assert_eq!(
            u32::from_le_bytes([
                descriptor[12],
                descriptor[13],
                descriptor[14],
                descriptor[15]
            ]) as u64,
            entry.size,
            "descriptor uncompressed size must match the central directory"
        );
    }

    /// Non-ASCII names keep the EFS flag, and an explicit modification time is
    /// the one that lands in both headers.
    #[test]
    fn test_stream_preserves_name_encoding_and_explicit_mtime() {
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            {
                let mut entry = writer
                    .add_stream_with_options(
                        "文書/資料.txt",
                        ZipStreamOptions {
                            compression: ZipCompressionLevel::Store,
                            zip64: false,
                            mtime: Some(mtime),
                        },
                    )
                    .expect("add_stream_with_options");
                entry.write_all(b"data").expect("write");
                entry.finish().expect("finish");
            }
            writer.finish().expect("finish");
        }

        assert_ne!(
            u16::from_le_bytes([output[6], output[7]]) & 0x0800,
            0,
            "non-ASCII name must set the EFS flag"
        );

        let mut reader = ZipReader::new(Cursor::new(&output)).expect("ZipReader::new");
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "文書/資料.txt");
        assert_eq!(reader.extract(&entry).expect("extract"), b"data".to_vec());
        let recorded = entry.modified.expect("modified time");
        // DOS timestamps have two-second granularity.
        assert!(
            recorded
                .duration_since(std::time::UNIX_EPOCH)
                .expect("post-epoch")
                .as_secs()
                / 2
                * 2
                == 1_700_000_000,
            "explicit mtime not preserved: {recorded:?}"
        );
    }

    /// Progress hooks: one `on_entry` when the entry opens, cumulative
    /// `on_progress` calls while it is written, and the central directory
    /// still lands at the right offset (which the reader round trip proves).
    #[test]
    fn test_stream_reports_progress() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            entries: AtomicU64,
            last_processed: AtomicU64,
        }
        impl ProgressSink for CountingSink {
            fn on_progress(&self, processed: u64, _total: Option<u64>) {
                self.last_processed.store(processed, Ordering::SeqCst);
            }
            fn on_entry(&self, _name: &str, _index: u64) {
                self.entries.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountingSink {
            entries: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        let data = compressible(10_000);
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output).with_progress(handle);
            {
                let mut entry = writer.add_stream("p.bin").expect("add_stream");
                entry.write_all(&data[..4000]).expect("write");
                entry.write_all(&data[4000..]).expect("write");
                entry.finish().expect("finish");
            }
            writer.finish().expect("finish");
        }

        assert_eq!(sink.entries.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64,
            "the last progress notification must carry the entry's full size"
        );
        assert_eq!(extract_via_central_directory(&output), data);
    }

    /// Entries written before and after a streamed one must all stay readable:
    /// the streaming path's offset bookkeeping (header + data + descriptor) has
    /// to leave the central directory pointing at real local headers.
    #[test]
    fn test_stream_interleaves_with_buffered_entries() {
        let streamed_data = compressible(20_000);
        let buffered_data = b"buffered entry".to_vec();

        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer
                .add_file_stored("first.txt", &buffered_data)
                .expect("add_file_stored");
            {
                let mut entry = writer.add_stream("middle.bin").expect("add_stream");
                entry.write_all(&streamed_data).expect("write");
                entry.finish().expect("finish");
            }
            writer.add_directory("dir").expect("add_directory");
            writer.finish().expect("finish");
        }

        let mut reader = ZipReader::new(Cursor::new(&output)).expect("ZipReader::new");
        let entries = reader.entries().to_vec();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["first.txt", "middle.bin", "dir/"]);
        assert_eq!(reader.extract(&entries[0]).expect("extract"), buffered_data);
        assert_eq!(reader.extract(&entries[1]).expect("extract"), streamed_data);
    }

    /// Dropping a stream writer without calling `finish()` still records a
    /// readable entry (best-effort finish, like `ZipWriter`'s own drop).
    #[test]
    fn test_stream_drop_finishes_entry() {
        let data = compressible(5_000);
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            {
                let mut entry = writer.add_stream("dropped.bin").expect("add_stream");
                entry.write_all(&data).expect("write");
            }
            writer.finish().expect("finish");
        }
        assert_eq!(extract_via_central_directory(&output), data);
    }

    /// Writing to a finished entry is an error rather than a silent second
    /// descriptor.
    #[test]
    fn test_stream_write_after_finish_errors() {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            {
                let mut entry = writer.add_stream("late.bin").expect("add_stream");
                entry.write_all(b"first").expect("write");
                entry.finish().expect("finish");
                assert!(entry.write_all(b"second").is_err());
                // Finishing again is a harmless no-op.
                entry.finish().expect("second finish is a no-op");
            }
            writer.finish().expect("archive finish");
        }
        assert_eq!(extract_via_central_directory(&output), b"first".to_vec());
    }
}
