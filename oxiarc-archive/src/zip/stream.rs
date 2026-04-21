//! Streaming ZIP reader — no `Seek` required.
//!
//! # Data-descriptor support (general-purpose flag bit 3)
//!
//! Entries using the data-descriptor flag have their CRC-32 and sizes stored
//! in a trailer *after* the compressed payload, not in the local file header.
//!
//! - **DEFLATE + bit 3**: supported. The inflater's end-of-stream marker is
//!   unambiguous (the last block has `BFINAL=1` and ends with the `256`
//!   end-of-block code), so we can locate the byte-aligned descriptor
//!   without scanning. The descriptor is parsed inline and its CRC-32 is
//!   validated against a CRC computed over the decompressed output.
//! - **Stored + bit 3**: rejected. With no compression and no length in the
//!   header, the only way to find the descriptor boundary is to scan for
//!   the `PK\x07\x08` signature — which is heuristic (the byte sequence may
//!   legitimately appear inside stored data). Such entries require `Seek`
//!   (via `ZipReader`) and return `OxiArcError::UnsupportedMethod`.
//! - **LZMA + bit 3**: rejected for the same streaming reason as Stored —
//!   LZMA's raw stream length is not recoverable here without the
//!   declared compressed_size.

use oxiarc_core::Crc32;
use oxiarc_core::bitstream::BitReader;
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_deflate::{Inflater, inflate};
use oxiarc_lzma::{LzmaProperties, decompress_raw as lzma_decompress_raw};
use std::io::{Cursor, Read};

use super::header::{CompressionMethod, LocalFileHeader};

/// ZIP data-descriptor signature (PK\x07\x08), per APPNOTE §4.3.9.
const DATA_DESCRIPTOR_SIG: u32 = 0x0807_4B50;

/// ZIP64 extra-field header id.
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;

/// ZIP local file header signature (PK\x03\x04).
const LOCAL_FILE_HEADER_SIG: u32 = 0x04034B50;

/// Metadata for a single entry in a streaming ZIP archive.
#[derive(Debug, Clone)]
pub struct ZipStreamEntryMeta {
    /// File name (UTF-8).
    pub name: String,
    /// Compression method.
    pub method: CompressionMethod,
    /// CRC-32 of the uncompressed data (may be 0 if data-descriptor was used).
    pub crc32: u32,
    /// Compressed size in bytes.
    pub compressed_size: u64,
    /// Uncompressed size in bytes.
    pub uncompressed_size: u64,
    /// Whether this is a directory entry (name ends with `/`).
    pub is_directory: bool,
}

/// Streaming ZIP reader requiring only `Read` — no `Seek` needed.
///
/// Entries must be processed in order. Each call to `next_entry` returns a
/// [`ZipStreamEntry`] whose `Read` impl yields the decompressed content.
/// Drop the entry (or read it to completion) before calling `next_entry` again.
///
/// # Supported methods
///
/// - `Store` (method 0): data is yielded as-is
/// - `Deflate` (method 8): data is inflated in memory before being yielded
///
/// # Data-descriptor entries
///
/// Entries with general-purpose flag bit 3 set are rejected immediately —
/// see module-level documentation for the reason.
pub struct ZipStreamReader<R: Read> {
    reader: R,
    done: bool,
    progress: Option<ProgressHandle>,
    cancel: Option<CancellationToken>,
    entry_index: u64,
    /// Bytes still to be drained from the current entry's raw (compressed) stream
    /// before the next `next_entry` call can start reading the next local header.
    pending_skip: u64,
}

impl<R: Read> ZipStreamReader<R> {
    /// Create a new streaming ZIP reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            done: false,
            progress: None,
            cancel: None,
            entry_index: 0,
            pending_skip: 0,
        }
    }

    /// Attach a progress sink that will be notified for each entry.
    pub fn with_progress(mut self, progress: ProgressHandle) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attach a cancellation token.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Advance to the next entry.
    ///
    /// Returns `Ok(None)` when the first non-local-header signature is encountered
    /// (e.g., the start of the central directory or end-of-central-directory record).
    pub fn next_entry(&mut self) -> Result<Option<ZipStreamEntry<'_, R>>> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Drain any bytes left from the previous entry's compressed block.
        if self.pending_skip > 0 {
            self.skip(self.pending_skip)?;
            self.pending_skip = 0;
        }

        if self.done {
            return Ok(None);
        }

        // Peek at the 4-byte signature.
        let mut sig_buf = [0u8; 4];
        match self.reader.read_exact(&mut sig_buf) {
            Ok(()) => {}
            Err(_) => {
                self.done = true;
                return Ok(None);
            }
        }

        let sig = u32::from_le_bytes(sig_buf);
        if sig != LOCAL_FILE_HEADER_SIG {
            // Central directory or end-of-central-directory — we are done.
            self.done = true;
            return Ok(None);
        }

        // Read the rest of the local file header (30 bytes total, signature already consumed).
        // We reconstruct the full 30-byte buffer expected by `LocalFileHeader::read`.
        // Strategy: wrap a combined reader of [sig_bytes | remaining_stream].
        let remaining_lfh = ChainedReader::new(&sig_buf, &mut self.reader);
        let mut chain = remaining_lfh;
        let lfh = LocalFileHeader::read(&mut chain)?;

        let has_data_descriptor = lfh.has_data_descriptor();

        // Reject combinations of data-descriptor + non-DEFLATE method that
        // fundamentally need Seek (no unambiguous byte-boundary detection).
        if has_data_descriptor {
            match lfh.method {
                CompressionMethod::Stored => {
                    return Err(OxiArcError::unsupported_method(format!(
                        "Entry '{}': streaming Stored entries with data-descriptor \
                         (flag bit 3) requires Seek — locating the descriptor would \
                         require scanning for PK\\x07\\x08 which is ambiguous",
                        lfh.filename
                    )));
                }
                CompressionMethod::Lzma => {
                    return Err(OxiArcError::unsupported_method(format!(
                        "Entry '{}': streaming LZMA entries with data-descriptor \
                         (flag bit 3) requires Seek — raw LZMA stream length is \
                         not recoverable without the header size",
                        lfh.filename
                    )));
                }
                CompressionMethod::Deflate => {
                    // Supported — handled below via inflater EOF detection.
                }
                CompressionMethod::Unknown(m) => {
                    return Err(OxiArcError::unsupported_method(format!(
                        "Entry '{}': unsupported compression method {} with data-descriptor",
                        lfh.filename, m
                    )));
                }
            }
        }

        // Reject unknown compression methods (non-bit3 path).
        match lfh.method {
            CompressionMethod::Stored | CompressionMethod::Deflate | CompressionMethod::Lzma => {}
            CompressionMethod::Unknown(m) => {
                return Err(OxiArcError::unsupported_method(format!(
                    "Entry '{}': unsupported compression method {}",
                    lfh.filename, m
                )));
            }
        }

        // Decode either the bit-3 (data-descriptor) path or the
        // fixed-size-in-header path. The bit-3 path always goes through
        // DEFLATE here because the other two methods are rejected above.
        let (decompressed, compressed_size, uncompressed_size, final_crc32) = if has_data_descriptor
        {
            self.read_deflate_with_descriptor(&lfh)?
        } else {
            let compressed_size = lfh.actual_compressed_size();
            let uncompressed_size = lfh.actual_uncompressed_size();

            let mut compressed = vec![0u8; compressed_size as usize];
            self.reader.read_exact(&mut compressed)?;

            let decompressed = match lfh.method {
                CompressionMethod::Stored => compressed,
                CompressionMethod::Deflate => inflate(&compressed).map_err(|e| {
                    OxiArcError::corrupted(
                        0,
                        format!("DEFLATE inflate failed for '{}': {}", lfh.filename, e),
                    )
                })?,
                CompressionMethod::Lzma => decompress_lzma_stream(&compressed, uncompressed_size)
                    .map_err(|e| {
                    OxiArcError::corrupted(
                        0,
                        format!("LZMA decompress failed for '{}': {}", lfh.filename, e),
                    )
                })?,
                CompressionMethod::Unknown(_) => unreachable!(),
            };
            (decompressed, compressed_size, uncompressed_size, lfh.crc32)
        };

        let meta = ZipStreamEntryMeta {
            name: lfh.filename.clone(),
            method: lfh.method,
            crc32: final_crc32,
            compressed_size,
            uncompressed_size,
            is_directory: lfh.filename.ends_with('/'),
        };

        let idx = self.entry_index;
        self.entry_index += 1;

        if let Some(ref sink) = self.progress {
            sink.on_entry(&meta.name, idx);
        }

        Ok(Some(ZipStreamEntry {
            meta,
            data: Cursor::new(decompressed),
            stream: self,
            bytes_read: 0,
        }))
    }

    /// Discard exactly `n` bytes from the inner reader.
    fn skip(&mut self, n: u64) -> Result<()> {
        let mut remaining = n;
        let mut buf = [0u8; 8192];
        while remaining > 0 {
            let to_read = remaining.min(buf.len() as u64) as usize;
            let got = self.reader.read(&mut buf[..to_read])?;
            if got == 0 {
                break;
            }
            remaining -= got as u64;
        }
        Ok(())
    }

    /// Inflate a DEFLATE entry whose sizes are carried in a trailing
    /// data descriptor (general-purpose flag bit 3).
    ///
    /// Steps:
    ///   1. Wrap `&mut self.reader` in a `BitReader`.
    ///   2. Run `Inflater::inflate` — the inflater stops at the final
    ///      block's end-of-block marker, so no explicit length is needed.
    ///   3. Align the bit buffer to the next byte boundary.
    ///   4. Parse the data descriptor (optional `0x08074B50` signature,
    ///      then crc32, compressed_size, uncompressed_size — the latter
    ///      two being 8 bytes if ZIP64, 4 bytes otherwise).
    ///   5. Compute the CRC-32 of the decompressed data and cross-check
    ///      against the descriptor.
    ///
    /// Returns `(decompressed, compressed_size, uncompressed_size, crc32)`.
    fn read_deflate_with_descriptor(
        &mut self,
        lfh: &LocalFileHeader,
    ) -> Result<(Vec<u8>, u64, u64, u32)> {
        // Decide whether the descriptor is the ZIP64 (8-byte sizes) form.
        // Heuristics per APPNOTE §4.3.9.2 — a ZIP64 descriptor is used when
        // a ZIP64 extra field is present in the LFH or when the
        // version_needed >= 45. We check both to be robust.
        let is_zip64 = has_zip64_extra(&lfh.extra) || lfh.version_needed >= 45;

        // Own a BitReader around the stream so we can reliably consume
        // the descriptor bytes immediately after inflate finishes.
        let mut bit_reader = BitReader::new(&mut self.reader);
        let mut inflater = Inflater::new();
        let (decompressed, compressed_size) =
            inflater.inflate_consumed(&mut bit_reader).map_err(|e| {
                OxiArcError::corrupted(
                    0,
                    format!(
                        "DEFLATE inflate failed for '{}' (bit-3 path): {}",
                        lfh.filename, e
                    ),
                )
            })?;

        // Now read the data descriptor — entirely byte-aligned.
        let mut first_word = [0u8; 4];
        bit_reader.read_bytes(&mut first_word)?;
        let first_word_u32 = u32::from_le_bytes(first_word);

        let crc32 = if first_word_u32 == DATA_DESCRIPTOR_SIG {
            let mut crc_bytes = [0u8; 4];
            bit_reader.read_bytes(&mut crc_bytes)?;
            u32::from_le_bytes(crc_bytes)
        } else {
            // No signature — the first word IS the crc32.
            first_word_u32
        };

        let (desc_compressed, desc_uncompressed) = if is_zip64 {
            let mut buf = [0u8; 16];
            bit_reader.read_bytes(&mut buf)?;
            let comp = u64::from_le_bytes(take8(&buf, 0)?);
            let uncomp = u64::from_le_bytes(take8(&buf, 8)?);
            (comp, uncomp)
        } else {
            let mut buf = [0u8; 8];
            bit_reader.read_bytes(&mut buf)?;
            let comp = u32::from_le_bytes(take4(&buf, 0)?) as u64;
            let uncomp = u32::from_le_bytes(take4(&buf, 4)?) as u64;
            (comp, uncomp)
        };

        // Validate the CRC-32 against the decompressed data.
        let computed_crc = Crc32::compute(&decompressed);
        if crc32 != computed_crc {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "CRC-32 mismatch in data descriptor for '{}': descriptor={:08x}, computed={:08x}",
                    lfh.filename, crc32, computed_crc
                ),
            ));
        }

        // Cross-check the descriptor's declared sizes. The descriptor's
        // compressed_size should equal the inflater's byte count; the
        // declared uncompressed_size should equal the decompressed length.
        if desc_compressed != compressed_size {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "Data descriptor compressed_size mismatch for '{}': descriptor={}, actual={}",
                    lfh.filename, desc_compressed, compressed_size
                ),
            ));
        }
        if desc_uncompressed != decompressed.len() as u64 {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "Data descriptor uncompressed_size mismatch for '{}': descriptor={}, actual={}",
                    lfh.filename,
                    desc_uncompressed,
                    decompressed.len()
                ),
            ));
        }

        Ok((decompressed, compressed_size, desc_uncompressed, crc32))
    }
}

/// Return `true` when the given LFH extra-field blob contains a ZIP64
/// extended-information record (header id `0x0001`).
fn has_zip64_extra(extra: &[u8]) -> bool {
    let mut offset = 0;
    while offset + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
        let size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;
        if id == ZIP64_EXTRA_FIELD_ID {
            return true;
        }
        offset = offset.saturating_add(4).saturating_add(size);
    }
    false
}

/// Copy 4 little-endian bytes out of `buf` starting at `offset`.
fn take4(buf: &[u8], offset: usize) -> Result<[u8; 4]> {
    if offset + 4 > buf.len() {
        return Err(OxiArcError::invalid_header(
            "Data descriptor: short read (need 4 bytes)",
        ));
    }
    let mut out = [0u8; 4];
    out.copy_from_slice(&buf[offset..offset + 4]);
    Ok(out)
}

/// Copy 8 little-endian bytes out of `buf` starting at `offset`.
fn take8(buf: &[u8], offset: usize) -> Result<[u8; 8]> {
    if offset + 8 > buf.len() {
        return Err(OxiArcError::invalid_header(
            "Data descriptor: short read (need 8 bytes)",
        ));
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&buf[offset..offset + 8]);
    Ok(out)
}

/// A single entry yielded by [`ZipStreamReader`].
///
/// The decompressed content is held in memory and served via `Read`.
pub struct ZipStreamEntry<'a, R: Read> {
    /// Entry metadata.
    pub meta: ZipStreamEntryMeta,
    data: Cursor<Vec<u8>>,
    stream: &'a mut ZipStreamReader<R>,
    bytes_read: u64,
}

impl<R: Read> Read for ZipStreamEntry<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.read(buf)?;
        self.bytes_read += n as u64;
        if let Some(ref sink) = self.stream.progress {
            let total = self.meta.uncompressed_size;
            sink.on_progress(self.bytes_read, Some(total));
        }
        Ok(n)
    }
}

impl<R: Read> Drop for ZipStreamEntry<'_, R> {
    fn drop(&mut self) {
        if let Some(ref sink) = self.stream.progress {
            sink.on_finish();
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helper: chain a fixed slice prefix to a `&mut R` reader.
// ---------------------------------------------------------------------------

/// Decompress LZMA method-14 data from a stream entry.
///
/// Method-14 format per APPNOTE §5.8.8:
/// `[major_ver: u8][minor_ver: u8][props_size: u16_le][lzma_props: props_size bytes][lzma_stream: ...]`
fn decompress_lzma_stream(compressed_data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    if compressed_data.len() < 4 {
        return Err(OxiArcError::invalid_header(
            "LZMA method-14 header too short",
        ));
    }

    let _major_ver = compressed_data[0];
    let _minor_ver = compressed_data[1];
    let props_size = u16::from_le_bytes([compressed_data[2], compressed_data[3]]) as usize;

    if compressed_data.len() < 4 + props_size {
        return Err(OxiArcError::invalid_header("LZMA props truncated"));
    }
    if props_size < 5 {
        return Err(OxiArcError::invalid_header(
            "LZMA method-14 props_size too small (expected >= 5)",
        ));
    }

    let props_bytes = &compressed_data[4..4 + props_size];
    let stream = &compressed_data[4 + props_size..];

    let props = LzmaProperties::from_byte(props_bytes[0])
        .ok_or_else(|| OxiArcError::invalid_header("Invalid LZMA properties byte"))?;
    let dict_size = u32::from_le_bytes([
        props_bytes[1],
        props_bytes[2],
        props_bytes[3],
        props_bytes[4],
    ]);

    let size_hint = if uncompressed_size > 0 {
        Some(uncompressed_size)
    } else {
        None
    };

    lzma_decompress_raw(Cursor::new(stream), props, dict_size, size_hint)
}

/// A `Read` adapter that reads from a small prefix slice first,
/// then falls through to the underlying reader.
struct ChainedReader<'a, R: Read> {
    prefix: &'a [u8],
    offset: usize,
    inner: &'a mut R,
}

impl<'a, R: Read> ChainedReader<'a, R> {
    fn new(prefix: &'a [u8], inner: &'a mut R) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<R: Read> Read for ChainedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset < self.prefix.len() {
            let remaining = &self.prefix[self.offset..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.offset += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::ZipWriter;
    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::error::OxiArcError;
    use oxiarc_core::progress::{ProgressSink, noop_progress};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf);
            for (name, data) in files {
                w.add_file(name, data).expect("add_file");
            }
            w.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn test_zip_stream_basic_store() {
        let buf = build_zip(&[("hello.txt", b"Hello"), ("world.txt", b"World")]);
        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor);

        let mut e0 = stream.next_entry().unwrap().unwrap();
        assert_eq!(e0.meta.name, "hello.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e0, &mut out).unwrap();
        assert_eq!(&out, b"Hello");
        drop(e0);

        let mut e1 = stream.next_entry().unwrap().unwrap();
        assert_eq!(e1.meta.name, "world.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e1, &mut out).unwrap();
        assert_eq!(&out, b"World");
        drop(e1);

        assert!(stream.next_entry().unwrap().is_none());
    }

    #[test]
    fn test_zip_stream_deflate() {
        // ZipWriter compresses when data is large enough; force with Normal compression.
        let data = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(100);
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf);
            w.add_file("data.bin", &data).expect("add_file");
            w.finish().expect("finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor);
        let mut entry = stream.next_entry().unwrap().unwrap();

        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).unwrap();
        assert_eq!(&out, data.as_slice());
        drop(entry);

        assert!(stream.next_entry().unwrap().is_none());
    }

    #[test]
    fn test_zip_stream_drop_without_reading() {
        let buf = build_zip(&[("skip.txt", b"skip content"), ("keep.txt", b"keep content")]);
        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor);

        // Drop first entry without reading
        let _ = stream.next_entry().unwrap().unwrap();

        let mut entry = stream.next_entry().unwrap().unwrap();
        assert_eq!(entry.meta.name, "keep.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).unwrap();
        assert_eq!(&out, b"keep content");
    }

    #[test]
    fn test_zip_stream_directory_entry() {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf);
            w.add_directory("mydir/").expect("add_dir");
            w.add_file("mydir/file.txt", b"content").expect("add_file");
            w.finish().expect("finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor);

        let e0 = stream.next_entry().unwrap().unwrap();
        assert!(e0.meta.is_directory);
        drop(e0);

        let mut e1 = stream.next_entry().unwrap().unwrap();
        assert!(!e1.meta.is_directory);
        assert_eq!(e1.meta.name, "mydir/file.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e1, &mut out).unwrap();
        assert_eq!(&out, b"content");
    }

    #[test]
    fn test_zip_stream_with_cancel() {
        let buf = build_zip(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor).with_cancel(token_clone);

        let e0 = stream.next_entry().unwrap().unwrap();
        drop(e0);

        token.cancel();

        let result = stream.next_entry();
        assert!(
            matches!(result, Err(OxiArcError::Cancelled)),
            "expected Cancelled error",
        );
    }

    #[test]
    fn test_zip_stream_progress_entry_events() {
        struct CountSink {
            count: AtomicU64,
        }
        impl ProgressSink for CountSink {
            fn on_progress(&self, _p: u64, _t: Option<u64>) {}
            fn on_entry(&self, _n: &str, _i: u64) {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
        let sink = Arc::new(CountSink {
            count: AtomicU64::new(0),
        });
        let handle: oxiarc_core::ProgressHandle = sink.clone();

        let buf = build_zip(&[("f1.txt", b"1"), ("f2.txt", b"2"), ("f3.txt", b"3")]);
        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor).with_progress(handle);
        while let Some(e) = stream.next_entry().unwrap() {
            drop(e);
        }
        assert_eq!(sink.count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_zip_stream_noop_progress() {
        let buf = build_zip(&[("x.txt", b"content")]);
        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor).with_progress(noop_progress());
        let mut e = stream.next_entry().unwrap().unwrap();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut out).unwrap();
        assert_eq!(&out, b"content");
    }

    #[test]
    fn test_zip_stream_empty_archive() {
        let mut buf = Vec::new();
        {
            let mut w = ZipWriter::new(&mut buf);
            w.finish().expect("finish");
        }
        let cursor = Cursor::new(buf);
        let mut stream = ZipStreamReader::new(cursor);
        assert!(stream.next_entry().unwrap().is_none());
    }

    // ========================================================================
    // Data-descriptor (general-purpose flag bit 3) fixtures and tests.
    //
    // Because `ZipWriter` always writes fixed-size headers with flags=0, these
    // tests hand-craft minimal archives that exercise the bit-3 reader code
    // path without needing a streaming writer. Each fixture encodes one
    // entry plus an end-of-central-directory record sufficient for
    // `ZipStreamReader` (which stops on the first non-LFH signature).
    // ========================================================================

    const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
    const LOCAL_FILE_HEADER_SIG_LE: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
    const EOCD_SIG_LE: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    const DATA_DESCRIPTOR_SIG_LE: [u8; 4] = [0x50, 0x4B, 0x07, 0x08];

    /// Parameters for crafting a single-entry bit-3 ZIP archive.
    struct DataDescFixture<'a> {
        name: &'a str,
        raw_data: &'a [u8],
        /// Compression method written into the LFH. 0 = Stored, 8 = Deflate.
        method: u16,
        /// Compressed payload bytes as they appear after the LFH.
        /// For method 0 this equals `raw_data`; for method 8 this is a
        /// DEFLATE-encoded blob of `raw_data`.
        compressed_payload: &'a [u8],
        /// Whether to prefix the data descriptor with its optional
        /// signature (0x08074B50).
        with_sig: bool,
        /// When `true`, the descriptor uses 8-byte (ZIP64) sizes and the
        /// LFH records version_needed=45.
        zip64: bool,
    }

    /// Build a single-entry ZIP archive with bit 3 set in the LFH.
    fn craft_bit3_zip(f: &DataDescFixture<'_>) -> Vec<u8> {
        use oxiarc_core::Crc32;

        let mut out = Vec::new();
        let name_bytes = f.name.as_bytes();

        // --- Local file header ---
        out.extend_from_slice(&LOCAL_FILE_HEADER_SIG_LE);
        let version_needed: u16 = if f.zip64 { 45 } else { 20 };
        out.extend_from_slice(&version_needed.to_le_bytes());
        // flags: bit 3 set (data descriptor).
        out.extend_from_slice(&FLAG_DATA_DESCRIPTOR.to_le_bytes());
        out.extend_from_slice(&f.method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // mtime
        out.extend_from_slice(&0u16.to_le_bytes()); // mdate
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32 (zero for bit-3)
        out.extend_from_slice(&0u32.to_le_bytes()); // compressed size (zero)
        out.extend_from_slice(&0u32.to_le_bytes()); // uncompressed size (zero)
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        // extra field: for ZIP64 form we include the marker-only record
        // with zero data — this is sufficient to trigger the ZIP64 branch
        // via the `has_zip64_extra` probe.
        let extra_bytes: Vec<u8> = if f.zip64 {
            // Header id 0x0001, data size 0 (no sizes in extra — the LFH
            // sizes are already zero due to bit 3; sizes live only in
            // the trailing descriptor).
            vec![0x01, 0x00, 0x00, 0x00]
        } else {
            Vec::new()
        };
        out.extend_from_slice(&(extra_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&extra_bytes);

        // --- Compressed payload ---
        out.extend_from_slice(f.compressed_payload);

        // --- Data descriptor ---
        if f.with_sig {
            out.extend_from_slice(&DATA_DESCRIPTOR_SIG_LE);
        }
        let crc = Crc32::compute(f.raw_data);
        out.extend_from_slice(&crc.to_le_bytes());
        if f.zip64 {
            out.extend_from_slice(&(f.compressed_payload.len() as u64).to_le_bytes());
            out.extend_from_slice(&(f.raw_data.len() as u64).to_le_bytes());
        } else {
            out.extend_from_slice(&(f.compressed_payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(f.raw_data.len() as u32).to_le_bytes());
        }

        // --- Minimal end-of-central-directory record ---
        // ZipStreamReader only needs the signature to stop — it never
        // walks the central directory. A zero-entry EOCD is enough.
        out.extend_from_slice(&EOCD_SIG_LE);
        out.extend_from_slice(&0u16.to_le_bytes()); // disk #
        out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        out.extend_from_slice(&0u16.to_le_bytes()); // entries on disk
        out.extend_from_slice(&0u16.to_le_bytes()); // total entries
        out.extend_from_slice(&0u32.to_le_bytes()); // cd size
        out.extend_from_slice(&0u32.to_le_bytes()); // cd offset
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len

        out
    }

    #[test]
    fn test_stream_reader_data_descriptor_with_signature() {
        use oxiarc_deflate::deflate;

        let raw = b"Hello, data descriptor world!";
        let compressed = deflate(raw, 6).expect("deflate");

        let zip_bytes = craft_bit3_zip(&DataDescFixture {
            name: "hello.txt",
            raw_data: raw,
            method: 8,
            compressed_payload: &compressed,
            with_sig: true,
            zip64: false,
        });

        let cursor = Cursor::new(zip_bytes);
        let mut stream = ZipStreamReader::new(cursor);

        let mut entry = stream
            .next_entry()
            .expect("next_entry ok")
            .expect("entry present");
        assert_eq!(entry.meta.name, "hello.txt");
        assert_eq!(entry.meta.method, CompressionMethod::Deflate);
        assert_eq!(entry.meta.uncompressed_size, raw.len() as u64);
        assert_eq!(entry.meta.compressed_size, compressed.len() as u64);

        let mut extracted = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut extracted).expect("read_to_end");
        assert_eq!(&extracted, raw);
        drop(entry);

        assert!(stream.next_entry().expect("terminate").is_none());
    }

    #[test]
    fn test_stream_reader_data_descriptor_no_signature() {
        use oxiarc_deflate::deflate;

        let raw = b"Another data-descriptor payload without the optional signature.";
        let compressed = deflate(raw, 6).expect("deflate");

        let zip_bytes = craft_bit3_zip(&DataDescFixture {
            name: "nosig.txt",
            raw_data: raw,
            method: 8,
            compressed_payload: &compressed,
            with_sig: false,
            zip64: false,
        });

        let cursor = Cursor::new(zip_bytes);
        let mut stream = ZipStreamReader::new(cursor);

        let mut entry = stream
            .next_entry()
            .expect("next_entry ok")
            .expect("entry present");
        assert_eq!(entry.meta.name, "nosig.txt");
        assert_eq!(entry.meta.method, CompressionMethod::Deflate);

        let mut extracted = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut extracted).expect("read_to_end");
        assert_eq!(&extracted, raw);
    }

    #[test]
    fn test_stream_reader_stored_bit3_error() {
        let raw = b"stored payload";

        let zip_bytes = craft_bit3_zip(&DataDescFixture {
            name: "stored.bin",
            raw_data: raw,
            method: 0, // Stored
            compressed_payload: raw,
            with_sig: true,
            zip64: false,
        });

        let cursor = Cursor::new(zip_bytes);
        let mut stream = ZipStreamReader::new(cursor);

        let result = stream.next_entry();
        assert!(result.is_err(), "Stored + bit 3 must error");
        let err = result.err().expect("error");
        let msg = format!("{}", err);
        assert!(
            msg.contains("Seek"),
            "error must mention Seek limitation: {}",
            msg
        );
    }

    #[test]
    fn test_stream_reader_zip64_data_descriptor() {
        use oxiarc_deflate::deflate;

        // Small payload is fine — the ZIP64 descriptor form is triggered
        // by version_needed=45 and a ZIP64 extra-field record, not by the
        // raw byte count. This keeps the test fast while exercising the
        // 8-byte size decoding path.
        let raw = b"ZIP64 descriptor exercise with 8-byte sizes.".repeat(4);
        let compressed = deflate(&raw, 6).expect("deflate");

        let zip_bytes = craft_bit3_zip(&DataDescFixture {
            name: "zip64.bin",
            raw_data: &raw,
            method: 8,
            compressed_payload: &compressed,
            with_sig: true,
            zip64: true,
        });

        let cursor = Cursor::new(zip_bytes);
        let mut stream = ZipStreamReader::new(cursor);

        let mut entry = stream
            .next_entry()
            .expect("next_entry ok")
            .expect("entry present");
        assert_eq!(entry.meta.name, "zip64.bin");
        assert_eq!(entry.meta.method, CompressionMethod::Deflate);
        assert_eq!(entry.meta.uncompressed_size, raw.len() as u64);
        assert_eq!(entry.meta.compressed_size, compressed.len() as u64);

        let mut extracted = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut extracted).expect("read_to_end");
        assert_eq!(extracted, raw);
    }
}
