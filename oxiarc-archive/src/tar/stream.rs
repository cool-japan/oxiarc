//! Streaming TAR reader — no `Seek` required.

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
use std::collections::HashMap;
use std::io::Read;

use super::{BLOCK_SIZE, TarHeader};

/// Streaming TAR reader requiring only `Read` — no `Seek` needed.
///
/// Entries must be processed in order. Use [`TarStreamEntry`] (which implements
/// [`std::io::Read`]) to access each entry's content, then drop it before
/// calling `next_entry` again.
///
/// # Example
/// ```no_run
/// use oxiarc_archive::TarStreamReader;
/// use std::fs::File;
///
/// let f = File::open("archive.tar").expect("open");
/// let mut stream = TarStreamReader::new(f);
/// while let Some(entry) = stream.next_entry().expect("read entry") {
///     println!("{}", entry.header.name);
/// }
/// ```
pub struct TarStreamReader<R: Read> {
    pub(crate) reader: R,
    done: bool,
    /// Bytes remaining in the current entry (data + padding) that callers
    /// have not yet consumed. Maintained so that `next_entry` can skip them
    /// even when the caller drops `TarStreamEntry` without fully reading it.
    pub(crate) pending_skip: u64,
    progress: Option<ProgressHandle>,
    cancel: Option<CancellationToken>,
    entry_index: u64,
}

impl<R: Read> TarStreamReader<R> {
    /// Create a new streaming TAR reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            done: false,
            pending_skip: 0,
            progress: None,
            cancel: None,
            entry_index: 0,
        }
    }

    /// Attach a progress sink that will be notified for each entry and on
    /// every chunk read from the stream.
    pub fn with_progress(mut self, progress: ProgressHandle) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attach a cancellation token. If cancelled, `next_entry` will return
    /// `Err(OxiArcError::Cancelled)`.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Advance to the next entry.
    ///
    /// Returns `Ok(None)` at end-of-archive or if the underlying reader is
    /// exhausted. The returned [`TarStreamEntry`] borrows `self` mutably;
    /// drop it (or read it to completion) before calling `next_entry` again.
    ///
    /// # Note
    /// PAX global extended headers are treated as per-entry headers here
    /// (they apply only to the immediately following entry). Full global-scope
    /// semantics are handled by `TarReader` which requires `Seek`.
    pub fn next_entry(&mut self) -> Result<Option<TarStreamEntry<'_, R>>> {
        // Check for cancellation before doing any work.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Skip any bytes left over from the previous entry.
        if self.pending_skip > 0 {
            self.skip(self.pending_skip)?;
            self.pending_skip = 0;
        }

        if self.done {
            return Ok(None);
        }

        let mut pax_attrs: HashMap<String, String> = HashMap::new();
        let mut gnu_longname: Option<String> = None;
        let mut gnu_longlink: Option<String> = None;

        loop {
            // Check cancellation each iteration so long archives respond quickly.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            let mut block = [0u8; BLOCK_SIZE];
            match self.reader.read_exact(&mut block) {
                Ok(()) => {}
                Err(_) => {
                    self.done = true;
                    return Ok(None);
                }
            }

            match TarHeader::from_block(&block)? {
                None => {
                    self.done = true;
                    return Ok(None);
                }
                Some(mut header) => {
                    // --- Extension headers (consume their data inline) ---
                    if header.is_pax_header() || header.is_pax_global_header() {
                        let data = self.read_extension_data(header.size)?;
                        let attrs = TarHeader::parse_pax_data(&data);
                        pax_attrs.extend(attrs);
                        continue;
                    }
                    if header.is_gnu_longname() {
                        let data = self.read_extension_data(header.size)?;
                        gnu_longname = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );
                        continue;
                    }
                    if header.is_gnu_longlink() {
                        let data = self.read_extension_data(header.size)?;
                        gnu_longlink = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );
                        continue;
                    }

                    // --- Apply accumulated metadata ---
                    if !pax_attrs.is_empty() {
                        header.apply_pax_attrs(&pax_attrs);
                    }
                    if let Some(name) = gnu_longname.take() {
                        header.name = name;
                    }
                    if let Some(link) = gnu_longlink.take() {
                        header.linkname = link;
                    }

                    let data_size = header.size;
                    let padding =
                        (BLOCK_SIZE as u64 - (data_size % BLOCK_SIZE as u64)) % BLOCK_SIZE as u64;

                    // Track what the entry owns so Drop can skip it.
                    self.pending_skip = data_size + padding;

                    // Notify progress of the new entry.
                    let idx = self.entry_index;
                    self.entry_index += 1;
                    if let Some(ref sink) = self.progress {
                        sink.on_entry(&header.name, idx);
                    }

                    return Ok(Some(TarStreamEntry {
                        header,
                        stream: self,
                        remaining: data_size,
                        padding,
                        bytes_read: 0,
                    }));
                }
            }
        }
    }

    /// Read `size` bytes of extension-header data plus its block padding.
    fn read_extension_data(&mut self, size: u64) -> Result<Vec<u8>> {
        let mut data = vec![0u8; size as usize];
        self.reader.read_exact(&mut data)?;
        let padding = (BLOCK_SIZE - (size as usize % BLOCK_SIZE)) % BLOCK_SIZE;
        if padding > 0 {
            self.skip(padding as u64)?;
        }
        Ok(data)
    }

    /// Discard exactly `n` bytes from the inner reader.
    pub(crate) fn skip(&mut self, n: u64) -> Result<()> {
        let mut remaining = n;
        let mut buf = [0u8; 8192];
        while remaining > 0 {
            let to_read = remaining.min(buf.len() as u64) as usize;
            let got = self.reader.read(&mut buf[..to_read])?;
            if got == 0 {
                break; // EOF — treat as end of data
            }
            remaining -= got as u64;
        }
        Ok(())
    }
}

/// A single entry yielded by [`TarStreamReader`].
///
/// Implements [`std::io::Read`] so the caller can stream the entry's content
/// directly to a file or any other sink without buffering everything in memory.
/// Dropping the entry without fully reading it is safe — the [`Drop`] impl
/// skips the remaining bytes so [`TarStreamReader::next_entry`] will work
/// correctly afterward.
pub struct TarStreamEntry<'a, R: Read> {
    /// The parsed header for this entry.
    pub header: TarHeader,
    pub(crate) stream: &'a mut TarStreamReader<R>,
    /// Unread data bytes remaining for this entry.
    pub(crate) remaining: u64,
    /// Block-alignment padding that follows the entry's data.
    pub(crate) padding: u64,
    /// Total bytes read so far (for progress reporting).
    bytes_read: u64,
}

impl<R: Read> std::io::Read for TarStreamEntry<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(self.remaining as usize);
        let n = self.stream.reader.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        self.bytes_read += n as u64;
        // Keep pending_skip in sync so Drop skips only what is really left.
        self.stream.pending_skip = self.remaining + self.padding;
        // Report progress.
        if let Some(ref sink) = self.stream.progress {
            sink.on_progress(self.bytes_read, Some(self.bytes_read + self.remaining));
        }
        Ok(n)
    }
}

impl<R: Read> Drop for TarStreamEntry<'_, R> {
    fn drop(&mut self) {
        // Discard any unread data + block-alignment padding.
        let to_skip = self.remaining + self.padding;
        if to_skip > 0 {
            let _ = self.stream.skip(to_skip);
        }
        self.stream.pending_skip = 0;
        // Signal finish on the progress sink.
        if let Some(ref sink) = self.stream.progress {
            sink.on_finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::{TarReader, TarWriter};
    use oxiarc_core::EntryType;
    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::error::OxiArcError;
    use oxiarc_core::progress::{ProgressSink, noop_progress};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn test_tar_stream_reader_basic() {
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("hello.txt", b"Hello, streaming!")
                .expect("add_file hello.txt");
            w.add_directory("subdir").expect("add_directory subdir");
            w.add_file("subdir/world.txt", b"World")
                .expect("add_file subdir/world.txt");
            w.finish().expect("writer finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor);

        let entry0 = stream
            .next_entry()
            .expect("next_entry 0")
            .expect("entry 0 present");
        assert_eq!(entry0.header.name, "hello.txt");
        assert_eq!(entry0.header.size, 17);
        drop(entry0);

        let entry1 = stream
            .next_entry()
            .expect("next_entry 1")
            .expect("entry 1 present");
        assert!(entry1.header.entry_type() == EntryType::Directory);
        drop(entry1);

        let mut entry2 = stream
            .next_entry()
            .expect("next_entry 2")
            .expect("entry 2 present");
        assert_eq!(entry2.header.name, "subdir/world.txt");
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry2, &mut content).expect("read_to_end entry2");
        assert_eq!(&content, b"World");
        drop(entry2);

        assert!(stream.next_entry().expect("next_entry final").is_none());
    }

    #[test]
    fn test_tar_stream_reader_read_content() {
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("data.bin", &[42u8; 1024])
                .expect("add_file data.bin");
            w.finish().expect("writer finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor);
        let mut entry = stream
            .next_entry()
            .expect("next_entry")
            .expect("entry present");

        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).expect("read_to_end entry");
        assert_eq!(out.len(), 1024);
        assert!(out.iter().all(|&b| b == 42));
        drop(entry);

        assert!(stream.next_entry().expect("next_entry final").is_none());
    }

    #[test]
    fn test_tar_stream_reader_skip_without_reading() {
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("skip_me.txt", b"skip this content")
                .expect("add_file skip_me.txt");
            w.add_file("read_me.txt", b"read this content")
                .expect("add_file read_me.txt");
            w.finish().expect("writer finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor);

        // Drop the first entry without reading
        let _ = stream
            .next_entry()
            .expect("next_entry 0")
            .expect("entry 0 present");

        let mut entry = stream
            .next_entry()
            .expect("next_entry 1")
            .expect("entry 1 present");
        assert_eq!(entry.header.name, "read_me.txt");
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read_to_end entry");
        assert_eq!(&content, b"read this content");
    }

    #[test]
    fn test_tar_stream_reader_with_cancel_stops() {
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            for i in 0..5u8 {
                w.add_file(&format!("file{}.txt", i), &[i; 64])
                    .expect("add_file in loop");
            }
            w.finish().expect("writer finish");
        }

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor).with_cancel(token_clone);

        // Read the first entry normally.
        let entry = stream
            .next_entry()
            .expect("next_entry 0")
            .expect("entry 0 present");
        assert_eq!(entry.header.name, "file0.txt");
        drop(entry);

        // Cancel before reading further.
        token.cancel();

        let result = stream.next_entry();
        assert!(
            matches!(result, Err(OxiArcError::Cancelled)),
            "expected Cancelled error",
        );
    }

    #[test]
    fn test_tar_stream_reader_with_progress_reports_entries() {
        struct EntrySink {
            count: AtomicU64,
        }
        impl ProgressSink for EntrySink {
            fn on_progress(&self, _p: u64, _t: Option<u64>) {}
            fn on_entry(&self, _name: &str, _idx: u64) {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(EntrySink {
            count: AtomicU64::new(0),
        });
        let handle: oxiarc_core::ProgressHandle = sink.clone();

        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("a.txt", b"aaa").expect("add_file a.txt");
            w.add_file("b.txt", b"bbb").expect("add_file b.txt");
            w.finish().expect("writer finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor).with_progress(handle);

        while let Some(e) = stream.next_entry().expect("next_entry in progress loop") {
            drop(e);
        }

        assert_eq!(sink.count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_tar_stream_reader_progress_on_read() {
        let data = vec![0xABu8; 4096];
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("big.bin", &data).expect("add_file big.bin");
            w.finish().expect("writer finish");
        }

        struct ByteSink {
            total: AtomicU64,
        }
        impl ProgressSink for ByteSink {
            fn on_progress(&self, processed: u64, _t: Option<u64>) {
                self.total.store(processed, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(ByteSink {
            total: AtomicU64::new(0),
        });
        let handle: oxiarc_core::ProgressHandle = sink.clone();

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor).with_progress(handle);
        let mut entry = stream
            .next_entry()
            .expect("next_entry big.bin")
            .expect("big.bin present");

        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).expect("read_to_end big.bin");
        drop(entry);

        assert_eq!(sink.total.load(Ordering::SeqCst), 4096);
    }

    #[test]
    fn test_tar_stream_reader_gnu_longname() {
        // TarWriter uses PAX headers for names > 100 chars.
        // This test exercises PAX extension header handling in the stream path.
        let long_name = "a/".repeat(30) + "file.txt"; // > 100 chars
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("short.txt", b"Short")
                .expect("add_file short.txt");
            w.add_file(&long_name, b"LongPath")
                .expect("add_file long_name");
            w.finish().expect("writer finish");
        }

        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor);

        let e0 = stream
            .next_entry()
            .expect("next_entry e0")
            .expect("e0 present");
        assert_eq!(e0.header.name, "short.txt");
        drop(e0);

        let mut e1 = stream
            .next_entry()
            .expect("next_entry e1")
            .expect("e1 present");
        assert_eq!(e1.header.name, long_name);
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut e1, &mut content).expect("read_to_end e1");
        assert_eq!(&content, b"LongPath");
        drop(e1);

        assert!(stream.next_entry().expect("next_entry final").is_none());
    }

    #[test]
    fn test_tar_stream_reader_noop_progress() {
        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            w.add_file("x.txt", b"x").expect("add_file x.txt");
            w.finish().expect("writer finish");
        }

        let handle = noop_progress();
        let cursor = Cursor::new(buf);
        let mut stream = TarStreamReader::new(cursor).with_progress(handle);

        let mut entry = stream
            .next_entry()
            .expect("next_entry")
            .expect("entry present");
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut out).expect("read_to_end entry");
        drop(entry);
        assert_eq!(&out, b"x");
    }

    #[test]
    fn test_tar_stream_reader_matches_tar_reader() {
        // Verify streaming reader and TarReader agree on content for a multi-file archive.
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("alpha.txt", b"Alpha content".to_vec()),
            ("beta.bin", vec![0xBEu8; 256]),
            ("gamma.txt", b"Gamma content here".to_vec()),
        ];

        let mut buf = Vec::new();
        {
            let mut w = TarWriter::new(&mut buf);
            for (name, data) in &files {
                w.add_file(name, data).expect("add_file in loop");
            }
            w.finish().expect("writer finish");
        }

        // Read with streaming reader.
        let mut stream_results: Vec<(String, Vec<u8>)> = Vec::new();
        {
            let cursor = Cursor::new(buf.clone());
            let mut stream = TarStreamReader::new(cursor);
            while let Some(mut entry) = stream.next_entry().expect("next_entry in stream loop") {
                let name = entry.header.name.clone();
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data)
                    .expect("read_to_end in stream loop");
                stream_results.push((name, data));
            }
        }

        // Read with TarReader.
        let cursor = Cursor::new(buf);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");
        for (i, (name, expected)) in files.iter().enumerate() {
            assert_eq!(stream_results[i].0, *name);
            let actual = reader
                .extract_by_name(name)
                .expect("extract_by_name")
                .expect("entry present");
            assert_eq!(stream_results[i].1, actual);
            assert_eq!(actual, *expected);
        }
    }
}
