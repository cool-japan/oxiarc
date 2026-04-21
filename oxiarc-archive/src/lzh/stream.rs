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

        // Read and decompress the entry's data.
        let mut compressed = vec![0u8; compressed_size];
        self.reader.read_exact(&mut compressed)?;

        let decompressed = if header.method == LzhMethod::Lh0 {
            compressed
        } else {
            decode_lzh(&compressed, header.method, header.original_size as u64).map_err(|e| {
                OxiArcError::corrupted(
                    0,
                    format!("LZH decompression failed for '{}': {}", header.filename, e),
                )
            })?
        };

        // Verify CRC-16.
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

        // Advance the running offset past the header bytes and compressed data.
        self.current_offset = header.data_offset + compressed_size as u64;

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

        let mut e0 = stream.next_entry().unwrap().unwrap();
        assert_eq!(e0.header.filename, "hello.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e0, &mut out).unwrap();
        assert_eq!(&out, b"Hello");
        drop(e0);

        let mut e1 = stream.next_entry().unwrap().unwrap();
        assert_eq!(e1.header.filename, "world.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e1, &mut out).unwrap();
        assert_eq!(&out, b"World");
        drop(e1);

        assert!(stream.next_entry().unwrap().is_none());
    }

    #[test]
    fn test_lzh_stream_drop_without_reading() {
        let buf = build_lzh(&[("skip.txt", b"skip this"), ("keep.txt", b"keep this")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor);

        // Drop first entry without reading
        let _ = stream.next_entry().unwrap().unwrap();

        let mut entry = stream.next_entry().unwrap().unwrap();
        assert_eq!(entry.header.filename, "keep.txt");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).unwrap();
        assert_eq!(&out, b"keep this");
    }

    #[test]
    fn test_lzh_stream_with_cancel() {
        let buf = build_lzh(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor).with_cancel(token_clone);

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
        while let Some(e) = stream.next_entry().unwrap() {
            drop(e);
        }
        assert_eq!(sink.count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_lzh_stream_noop_progress() {
        let buf = build_lzh(&[("x.txt", b"content")]);
        let cursor = Cursor::new(buf);
        let mut stream = LzhStreamReader::new(cursor).with_progress(noop_progress());
        let mut e = stream.next_entry().unwrap().unwrap();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut out).unwrap();
        assert_eq!(&out, b"content");
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
            while let Some(mut entry) = stream.next_entry().unwrap() {
                let name = entry.header.filename.clone();
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
                stream_results.push((name, data));
            }
        }

        // Read with LzhReader.
        let cursor = Cursor::new(buf);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();
        for (i, (name, expected)) in files.iter().enumerate() {
            assert_eq!(stream_results[i].0, *name);
            let entry = entries.iter().find(|e| &e.name == name).unwrap();
            let mut actual = Vec::new();
            reader.extract(entry, &mut actual).unwrap();
            assert_eq!(stream_results[i].1, actual);
            assert_eq!(actual, *expected);
        }
    }
}
