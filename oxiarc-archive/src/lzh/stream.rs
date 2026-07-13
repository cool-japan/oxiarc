//! Streaming LZH reader — no `Seek` required.

use oxiarc_core::Crc16;
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_lzhuf::{LzhMethod, decode_lzh};
use std::io::{Cursor, Read};

use super::LzhHeader;

/// Streaming LZH archive reader requiring only `Read` — no `Seek` needed.
///
/// Entries must be processed in order. Each call to `next_entry` returns an
/// [`LzhStreamEntry`] whose `Read` impl yields the decompressed content.
/// Drop the entry (or read it to completion) before calling `next_entry` again.
///
/// # Supported header levels
///
/// - Level 0, 1, 2 — all decoded.
/// - Level 3 — returns `Err(OxiArcError::InvalidHeader)` immediately.
///
/// # Supported methods
///
/// - `-lh0-` (`Lh0`): passthrough (no compression)
/// - `-lh5-`, `-lh6-`, `-lh7-`: LZSS+Huffman via `oxiarc-lzhuf`
///
/// All decompression is done in-memory: the compressed bytes for each entry
/// are buffered then decoded before the entry reader is returned.
pub struct LzhStreamReader<R: Read> {
    reader: R,
    done: bool,
    /// Running byte offset from the start of the archive.
    /// Used to pass the correct `offset` to `LzhHeader::read` so that the
    /// returned header's `data_offset` field is accurate (for informational
    /// purposes only — the streaming reader does not seek).
    current_offset: u64,
    progress: Option<ProgressHandle>,
    cancel: Option<CancellationToken>,
    entry_index: u64,
}

impl<R: Read> LzhStreamReader<R> {
    /// Create a new streaming LZH reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            done: false,
            current_offset: 0,
            progress: None,
            cancel: None,
            entry_index: 0,
        }
    }

    /// Attach a progress sink that will be notified for each entry.
    #[must_use]
    pub fn with_progress(mut self, progress: ProgressHandle) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attach a cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Advance to the next entry.
    ///
    /// Returns `Ok(None)` when the end-of-archive marker (a single zero byte
    /// that signals `header_size == 0`) is encountered.
    pub fn next_entry(&mut self) -> Result<Option<LzhStreamEntry<'_, R>>> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        if self.done {
            return Ok(None);
        }

        let header = match LzhHeader::read(&mut self.reader, self.current_offset)? {
            None => {
                self.done = true;
                return Ok(None);
            }
            Some(h) => h,
        };

        // After `LzhHeader::read`, the reader is positioned at the start of
        // the compressed data. Advance our offset tracking.
        // header.data_offset = self.current_offset + (header bytes consumed)
        // So the next offset after compressed data is data_offset + compressed_size.
        let compressed_size = header.compressed_size as usize;

        // Read the entry's compressed data. `compressed_size` is an
        // untrusted u32 (up to ~4 GiB), so never allocate it blindly:
        // pre-reserve a bounded amount and let `take + read_to_end` grow
        // the buffer only as bytes actually arrive — a short/truncated
        // stream then fails fast instead of first committing gigabytes.
        const PREALLOC_CAP: usize = 1 << 20; // 1 MiB up-front ceiling
        let mut compressed = Vec::new();
        compressed
            .try_reserve_exact(compressed_size.min(PREALLOC_CAP))
            .map_err(|_| {
                OxiArcError::corrupted(
                    header.data_offset,
                    format!(
                        "cannot allocate buffer for compressed entry '{}' ({} bytes)",
                        header.filename, compressed_size
                    ),
                )
            })?;
        let got = (&mut self.reader)
            .take(compressed_size as u64)
            .read_to_end(&mut compressed)?;
        if got != compressed_size {
            return Err(OxiArcError::corrupted(
                header.data_offset,
                format!(
                    "compressed data for '{}' truncated: expected {} bytes, got {}",
                    header.filename, compressed_size, got
                ),
            ));
        }

        // Advance the running offset past the header bytes and compressed
        // data *before* decoding, so a per-entry failure below leaves the
        // stream positioned at the next header and iteration can continue.
        self.current_offset = header.data_offset + compressed_size as u64;

        let decompressed = if header.method.is_directory() {
            Vec::new()
        } else if header.method == LzhMethod::Lh0 {
            compressed
        } else if let LzhMethod::Unknown(id) = header.method {
            // Unsupported method: skip this entry (its bytes are already
            // consumed) and report it; subsequent next_entry calls proceed.
            return Err(OxiArcError::unsupported_method(
                String::from_utf8_lossy(&id).into_owned(),
            ));
        } else {
            // Resolve the true uncompressed size the same way
            // `LzhHeader::to_entry` does: the 64-bit extension header 0x42
            // overrides the (possibly truncated) 32-bit base field.
            let uncompressed_size = header
                .uncompressed_size64
                .unwrap_or(header.original_size as u64);
            decode_lzh(&compressed, header.method, uncompressed_size).map_err(|e| {
                OxiArcError::corrupted(
                    0,
                    format!("LZH decompression failed for '{}': {}", header.filename, e),
                )
            })?
        };

        // Verify CRC-16 (directories carry no data and no meaningful CRC).
        if !header.method.is_directory() {
            let computed_crc = Crc16::compute(&decompressed);
            if computed_crc != header.crc16 {
                return Err(OxiArcError::corrupted(
                    header.data_offset,
                    format!(
                        "CRC-16 mismatch for '{}': expected {:04X}, computed {:04X}",
                        header.filename, header.crc16, computed_crc
                    ),
                ));
            }
        }

        let idx = self.entry_index;
        self.entry_index += 1;

        if let Some(ref sink) = self.progress {
            sink.on_entry(&header.filename, idx);
        }

        let uncompressed_size = decompressed.len() as u64;

        Ok(Some(LzhStreamEntry {
            header,
            data: Cursor::new(decompressed),
            stream: self,
            bytes_read: 0,
            uncompressed_size,
        }))
    }
}

/// A single entry yielded by [`LzhStreamReader`].
///
/// The decompressed content is held in memory and served via `Read`.
pub struct LzhStreamEntry<'a, R: Read> {
    /// The parsed LZH header for this entry.
    pub header: LzhHeader,
    data: Cursor<Vec<u8>>,
    stream: &'a mut LzhStreamReader<R>,
    bytes_read: u64,
    uncompressed_size: u64,
}

impl<R: Read> Read for LzhStreamEntry<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.read(buf)?;
        self.bytes_read += n as u64;
        if let Some(ref sink) = self.stream.progress {
            sink.on_progress(self.bytes_read, Some(self.uncompressed_size));
        }
        Ok(n)
    }
}

impl<R: Read> Drop for LzhStreamEntry<'_, R> {
    fn drop(&mut self) {
        if let Some(ref sink) = self.stream.progress {
            sink.on_finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lzh::{LzhCompressionLevel, LzhWriter};
    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::error::OxiArcError;
    use oxiarc_core::progress::{ProgressSink, noop_progress};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn build_lzh(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = LzhWriter::new(&mut buf);
            w.set_compression(LzhCompressionLevel::Store);
            for (name, data) in files {
                w.add_file(name, data).expect("add_file");
            }
            w.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn test_lzh_stream_basic() {
        let buf = build_lzh(&[("hello.txt", b"Hello"), ("world.txt", b"World")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor);

        let mut e0 = stream
            .next_entry()
            .expect("next_entry e0")
            .expect("e0 present");
        assert_eq!(e0.header.filename, "hello.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e0, &mut out).expect("read_to_end e0");
        assert_eq!(&out, b"Hello");
        drop(e0);

        let mut e1 = stream
            .next_entry()
            .expect("next_entry e1")
            .expect("e1 present");
        assert_eq!(e1.header.filename, "world.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e1, &mut out).expect("read_to_end e1");
        assert_eq!(&out, b"World");
        drop(e1);

        assert!(stream.next_entry().expect("next_entry final").is_none());
    }

    #[test]
    fn test_lzh_stream_drop_without_reading() {
        let buf = build_lzh(&[("skip.txt", b"skip this"), ("keep.txt", b"keep this")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor);

        // Drop first entry without reading
        let _ = stream
            .next_entry()
            .expect("next_entry skip.txt")
            .expect("skip.txt present");

        let mut entry = stream
            .next_entry()
            .expect("next_entry keep.txt")
            .expect("keep.txt present");
        assert_eq!(entry.header.filename, "keep.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).expect("read_to_end keep.txt");
        assert_eq!(&out, b"keep this");
    }

    #[test]
    fn test_lzh_stream_with_cancel() {
        let buf = build_lzh(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor).with_cancel(token_clone);

        let e0 = stream
            .next_entry()
            .expect("next_entry e0")
            .expect("e0 present");
        drop(e0);

        token.cancel();

        let result = stream.next_entry();
        assert!(
            matches!(result, Err(OxiArcError::Cancelled)),
            "expected Cancelled error",
        );
    }

    #[test]
    fn test_lzh_stream_progress_entry_events() {
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

        let buf = build_lzh(&[("f1.txt", b"1"), ("f2.txt", b"2"), ("f3.txt", b"3")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor).with_progress(handle);
        while let Some(e) = stream.next_entry().expect("next_entry in progress loop") {
            drop(e);
        }
        assert_eq!(sink.count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_lzh_stream_noop_progress() {
        let buf = build_lzh(&[("x.txt", b"content")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor).with_progress(noop_progress());
        let mut e = stream
            .next_entry()
            .expect("next_entry x.txt")
            .expect("x.txt present");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut out).expect("read_to_end x.txt");
        assert_eq!(&out, b"content");
    }

    // ---- LZHUF-02 / LZHUF-03 regressions ----

    /// Hand-assemble a level-2 LZH header for `-lh5-` data with an
    /// arbitrary 32-bit `original_size` field plus a 0x42 extension header
    /// carrying the true 64-bit uncompressed size, followed by the
    /// compressed bytes.
    ///
    /// Layout (level 2): `[total u16][method 5][compressed u32]
    /// [original u32][mtime u32][attr][level=2][crc16][os_id]
    /// [first_ext_size u16][ext chain][data...]`, where each extension
    /// block is `[type][payload][next_size u16]` and its declared size
    /// includes the trailing next-size field.
    fn build_lzh_with_size64(
        name: &str,
        compressed: &[u8],
        original_size32: u32,
        size64: u64,
        crc16: u16,
    ) -> Vec<u8> {
        let name_bytes = name.as_bytes();
        let ext_name_len = 3 + name_bytes.len(); // type + payload + next_size
        let ext_size64_len = 3 + 8;
        let total_size = 21 + 2 + 1 + 2 + ext_name_len + ext_size64_len;

        let mut out = Vec::new();
        out.extend_from_slice(&(total_size as u16).to_le_bytes());
        out.extend_from_slice(b"-lh5-");
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&original_size32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // mtime
        out.push(0x20); // attributes
        out.push(2); // level
        out.extend_from_slice(&crc16.to_le_bytes());
        out.push(b'U'); // os_id
        // First extension size → 0x01 filename block
        out.extend_from_slice(&(ext_name_len as u16).to_le_bytes());
        out.push(0x01);
        out.extend_from_slice(name_bytes);
        // Next: 0x42 block
        out.extend_from_slice(&(ext_size64_len as u16).to_le_bytes());
        out.push(0x42);
        out.extend_from_slice(&size64.to_le_bytes());
        // End of chain
        out.extend_from_slice(&0u16.to_le_bytes());

        assert_eq!(out.len(), total_size, "header assembly mismatch");
        out.extend_from_slice(compressed);
        out.push(0); // end-of-archive marker
        out
    }

    /// LZHUF-02: the streaming reader must honor the 64-bit uncompressed
    /// size from extension header 0x42 instead of the raw 32-bit field.
    /// The archive here declares a *wrong* 32-bit size (too small) and the
    /// correct size only via 0x42 — exactly what the seekable `LzhReader`
    /// already resolves through `to_entry()`.
    #[test]
    fn test_lzh_stream_honors_size64_extension() {
        use oxiarc_core::Crc16;
        use oxiarc_lzhuf::LzhEncoder;

        let original: Vec<u8> = b"size64 override test "
            .iter()
            .cycle()
            .take(3000)
            .copied()
            .collect();
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder.compress_to_vec(&original).expect("lh5 encode");
        let crc = Crc16::compute(&original);

        // 32-bit field lies (100 bytes); 0x42 carries the true size.
        let archive = build_lzh_with_size64("a.bin", &compressed, 100, original.len() as u64, crc);

        let mut stream = LzhStreamReader::new(Cursor::new(archive));
        let mut entry = stream
            .next_entry()
            .expect("next_entry must succeed with 0x42 override")
            .expect("entry present");
        assert_eq!(entry.header.filename, "a.bin");
        assert_eq!(
            entry.header.uncompressed_size64,
            Some(original.len() as u64)
        );
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).expect("read entry");
        assert_eq!(out, original, "content must decode at the 0x42 size");
    }

    /// LZHUF-03: a header claiming far more compressed bytes than the
    /// stream contains must fail fast with a corruption error instead of
    /// pre-committing the full claimed allocation.
    #[test]
    fn test_lzh_stream_truncated_compressed_size_claim() {
        use oxiarc_core::Crc16;

        let payload = b"tiny";
        let crc = Crc16::compute(payload);
        // Claim ~64 MiB of compressed data but supply only 4 bytes.
        let mut archive = build_lzh_with_size64(
            "bomb.bin",
            payload,
            payload.len() as u32,
            payload.len() as u64,
            crc,
        );
        // Patch the compressed-size field (offset 7..11) to a huge value.
        archive[7..11].copy_from_slice(&(64u32 << 20).to_le_bytes());

        let mut stream = LzhStreamReader::new(Cursor::new(archive));
        let result = stream.next_entry();
        assert!(
            result.is_err(),
            "oversized compressed_size claim on a short stream must error"
        );
    }

    /// The streaming and seekable readers must agree on an archive whose
    /// entry carries a 0x42 extension (LZHUF-02 parity check).
    #[test]
    fn test_lzh_stream_size64_matches_seekable_reader() {
        use crate::lzh::LzhReader;
        use oxiarc_core::Crc16;
        use oxiarc_lzhuf::LzhEncoder;

        let original: Vec<u8> = (0u16..2048).map(|i| (i % 256) as u8).collect();
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder.compress_to_vec(&original).expect("lh5 encode");
        let crc = Crc16::compute(&original);
        let archive = build_lzh_with_size64("b.bin", &compressed, 7, original.len() as u64, crc);

        // Seekable reader.
        let mut reader = LzhReader::new(Cursor::new(archive.clone())).expect("LzhReader::new");
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.size, original.len() as u64);
        let mut seekable = Vec::new();
        reader
            .extract(&entry, &mut seekable)
            .expect("seekable extract");

        // Streaming reader.
        let mut stream = LzhStreamReader::new(Cursor::new(archive));
        let mut stream_entry = stream
            .next_entry()
            .expect("stream next_entry")
            .expect("entry present");
        let mut streamed = Vec::new();
        std::io::Read::read_to_end(&mut stream_entry, &mut streamed).expect("stream read");

        assert_eq!(streamed, seekable, "stream and seekable readers must agree");
        assert_eq!(streamed, original);
    }

    #[test]
    fn test_lzh_stream_matches_lzh_reader() {
        use crate::lzh::LzhReader;

        let files: Vec<(&str, Vec<u8>)> = vec![
            ("alpha.txt", b"Alpha content".to_vec()),
            ("beta.bin", vec![0xBEu8; 256]),
            ("gamma.txt", b"Gamma content here".to_vec()),
        ];

        let mut buf = Vec::new();
        {
            let mut w = LzhWriter::new(&mut buf);
            w.set_compression(LzhCompressionLevel::Store);
            for (name, data) in &files {
                w.add_file(name, data).expect("add_file");
            }
            w.finish().expect("finish");
        }

        // Read with streaming reader.
        let mut stream_results: Vec<(String, Vec<u8>)> = Vec::new();
        {
            let cursor = Cursor::new(buf.clone());
            let mut stream = LzhStreamReader::new(cursor);
            while let Some(mut entry) = stream.next_entry().expect("next_entry in stream loop") {
                let name = entry.header.filename.clone();
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data)
                    .expect("read_to_end in stream loop");
                stream_results.push((name, data));
            }
        }

        // Read with LzhReader.
        let cursor = Cursor::new(buf);
        let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
        let entries = reader.entries();
        for (i, (name, expected)) in files.iter().enumerate() {
            assert_eq!(stream_results[i].0, *name);
            let entry = entries
                .iter()
                .find(|e| &e.name == name)
                .expect("find entry by name");
            let mut actual = Vec::new();
            reader.extract(entry, &mut actual).expect("reader.extract");
            assert_eq!(stream_results[i].1, actual);
            assert_eq!(actual, *expected);
        }
    }
}
