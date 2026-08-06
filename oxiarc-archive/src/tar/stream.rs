//! Streaming TAR reader — no `Seek` required.

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::collections::HashMap;
use std::io::Read;

use super::sparse::SparseMap;
use super::{BLOCK_SIZE, TarHeader};

/// Upper bound on the size of a single PAX extended-header or GNU
/// long-name/long-link record read via [`TarStreamReader::read_extension_data`].
///
/// `header.size` for these records comes straight from an untrusted TAR
/// header field. Because `TarStreamReader` only requires `Read` (no
/// `Seek`), it cannot cheaply learn how many bytes actually remain in the
/// underlying stream the way the seekable [`super::reader::TarReader`]
/// can, so a fixed sanity cap is used instead: real-world PAX headers and
/// long names/links are at most a few KiB, so 64 MiB is generous headroom
/// while still preventing a crafted archive from forcing an
/// arbitrarily large allocation before any data is validated.
const MAX_EXTENSION_DATA_SIZE: u64 = 64 * 1024 * 1024;

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
    #[must_use]
    pub fn with_progress(mut self, progress: ProgressHandle) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attach a cancellation token. If cancelled, `next_entry` will return
    /// `Err(OxiArcError::Cancelled)`.
    #[must_use]
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

                    // ---- GNU old-format sparse entry (typeflag 'S') ----
                    //
                    // The sparse map lives in the primary header's unused
                    // bytes, optionally extended by 512-byte continuation
                    // blocks; the payload is the concatenation of non-hole
                    // runs padded to BLOCK_SIZE. `header.size` holds the
                    // stored (on-disk) size, NOT the logical size, so the
                    // generic path below would both mis-size the entry and
                    // return raw run bytes as if they were contiguous
                    // content. Mirrors the seekable `TarReader` handling.
                    if header.typeflag == b'S' {
                        let map = SparseMap::parse_gnu_old_format(&block, &mut self.reader)?;
                        map.validate()?;
                        return Ok(Some(self.make_sparse_entry(header, map)));
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

                    // ---- PAX 0.1 sparse (`GNU.sparse.*` attributes) ----
                    //
                    // The map is carried by the preceding PAX header and the
                    // data entry is a regular '0' record whose `size` is the
                    // stored size; the canonical name is shadowed into
                    // `GNU.sparse.name`. Mirrors the seekable `TarReader`.
                    if pax_attrs.contains_key("GNU.sparse.map") {
                        let map = SparseMap::from_pax_attrs(&pax_attrs)?;
                        map.validate()?;
                        if let Some(real_name) = pax_attrs.get("GNU.sparse.name") {
                            header.name = real_name.clone();
                        }
                        return Ok(Some(self.make_sparse_entry(header, map)));
                    }

                    // ---- PAX 1.0 sparse (`GNU.sparse.major`/`.minor` = "1"/"0") ----
                    //
                    // Unlike 0.1, the map is not pax-attribute text — it is a
                    // decimal-ASCII preamble at the very start of this data
                    // entry's own payload, so it must be read directly off
                    // `self.reader` here (no `Seek` available). Once
                    // consumed, the reader sits exactly at the first run
                    // byte, which is exactly what `make_sparse_entry`
                    // expects (identical to the old-format 'S' case above,
                    // whose continuation blocks are consumed the same way).
                    if pax_attrs.get("GNU.sparse.major").map(String::as_str) == Some("1")
                        && pax_attrs.get("GNU.sparse.minor").map(String::as_str) == Some("0")
                    {
                        let map = SparseMap::parse_pax_1_0_preamble(&mut self.reader, &pax_attrs)?;
                        map.validate()?;
                        if let Some(real_name) = pax_attrs.get("GNU.sparse.name") {
                            header.name = real_name.clone();
                        }
                        return Ok(Some(self.make_sparse_entry(header, map)));
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
                        sparse: None,
                    }));
                }
            }
        }
    }

    /// Build a [`TarStreamEntry`] for a validated sparse map.
    ///
    /// The entry's `Read` impl serves the *logical* (realsize) view of the
    /// file: stored runs are read from the underlying stream and the holes
    /// between them are zero-filled on the fly, so no `realsize`-sized
    /// buffer is materialized. `header.size` is rewritten to the logical
    /// size to match what `read_to_end` will produce (and what the
    /// seekable `TarReader` reports for the same entry).
    fn make_sparse_entry(
        &mut self,
        mut header: TarHeader,
        map: SparseMap,
    ) -> TarStreamEntry<'_, R> {
        let stored = map.stored_size();
        let padding = map.padded_stored_size() - stored;
        header.size = map.realsize;

        // Track what the entry owns on the medium so Drop can skip it.
        self.pending_skip = stored + padding;

        let idx = self.entry_index;
        self.entry_index += 1;
        if let Some(ref sink) = self.progress {
            sink.on_entry(&header.name, idx);
        }

        TarStreamEntry {
            header,
            stream: self,
            remaining: stored,
            padding,
            bytes_read: 0,
            sparse: Some(SparseReadState {
                realsize: map.realsize,
                runs: map.runs,
                run_idx: 0,
                run_pos: 0,
                logical_pos: 0,
            }),
        }
    }

    /// Read `size` bytes of extension-header data plus its block padding.
    fn read_extension_data(&mut self, size: u64) -> Result<Vec<u8>> {
        if size > MAX_EXTENSION_DATA_SIZE {
            return Err(OxiArcError::invalid_header(format!(
                "TAR extension header declares size {size}, exceeding the {MAX_EXTENSION_DATA_SIZE}-byte sanity limit"
            )));
        }

        let mut data = Vec::new();
        data.try_reserve_exact(size as usize).map_err(|_| {
            OxiArcError::invalid_header(format!(
                "unable to allocate {size} bytes for TAR extension header data"
            ))
        })?;
        data.resize(size as usize, 0);
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
    /// Unread *stored* data bytes remaining for this entry (for sparse
    /// entries this counts run bytes on the medium, not logical bytes).
    pub(crate) remaining: u64,
    /// Block-alignment padding that follows the entry's data.
    pub(crate) padding: u64,
    /// Total bytes served to the caller so far (logical bytes; used for
    /// progress reporting).
    bytes_read: u64,
    /// Present for sparse entries: drives the logical realsize view with
    /// zero-filled holes interleaved between stored runs.
    sparse: Option<SparseReadState>,
}

/// Incremental state for serving a sparse entry's logical content.
struct SparseReadState {
    /// Logical file size (realsize).
    realsize: u64,
    /// Validated `(offset, numbytes)` runs, monotonically ordered and
    /// non-overlapping (guaranteed by `SparseMap::validate`).
    runs: Vec<(u64, u64)>,
    /// Index of the run currently being (or next to be) served.
    run_idx: usize,
    /// Bytes of the current run already served.
    run_pos: u64,
    /// Current position in the logical (realsize) view.
    logical_pos: u64,
}

impl<R: Read> TarStreamEntry<'_, R> {
    /// Serve up to `buf.len()` logical bytes of a sparse entry.
    ///
    /// Holes are synthesized as zeros; stored runs are read from the
    /// underlying stream. A stream that ends mid-run is an error — the
    /// caller must never receive silently short or shifted content.
    fn read_sparse(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(sp) = self.sparse.as_mut() else {
            return Ok(0);
        };

        if sp.logical_pos >= sp.realsize {
            return Ok(0);
        }

        if sp.run_idx < sp.runs.len() {
            let (run_off, run_len) = sp.runs[sp.run_idx];

            if sp.logical_pos < run_off {
                // Hole before the current run: synthesize zeros.
                let hole = (run_off - sp.logical_pos).min(buf.len() as u64) as usize;
                buf[..hole].fill(0);
                sp.logical_pos += hole as u64;
                return Ok(hole);
            }

            // Inside the current run: read stored bytes from the stream.
            let left_in_run = run_len - sp.run_pos;
            let cap = (buf.len() as u64).min(left_in_run).min(self.remaining) as usize;
            let n = self.stream.reader.read(&mut buf[..cap])?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "TAR stream ended inside sparse run {} of '{}' ({} of {} run bytes read)",
                        sp.run_idx, self.header.name, sp.run_pos, run_len
                    ),
                ));
            }
            sp.run_pos += n as u64;
            sp.logical_pos += n as u64;
            self.remaining -= n as u64;
            if sp.run_pos == run_len {
                sp.run_idx += 1;
                sp.run_pos = 0;
            }
            // Keep pending_skip in sync so Drop skips only what is left.
            self.stream.pending_skip = self.remaining + self.padding;
            return Ok(n);
        }

        // Trailing hole after the final run.
        let hole = (sp.realsize - sp.logical_pos).min(buf.len() as u64) as usize;
        buf[..hole].fill(0);
        sp.logical_pos += hole as u64;
        Ok(hole)
    }
}

impl<R: Read> std::io::Read for TarStreamEntry<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let n = if self.sparse.is_some() {
            let n = self.read_sparse(buf)?;
            if n == 0 {
                return Ok(0);
            }
            n
        } else {
            if self.remaining == 0 {
                return Ok(0);
            }
            let cap = buf.len().min(self.remaining as usize);
            let n = self.stream.reader.read(&mut buf[..cap])?;
            self.remaining -= n as u64;
            // Keep pending_skip in sync so Drop skips only what is really left.
            self.stream.pending_skip = self.remaining + self.padding;
            n
        };

        self.bytes_read += n as u64;
        // Report progress against the logical total.
        if let Some(ref sink) = self.stream.progress {
            let total = match self.sparse {
                Some(ref sp) => sp.realsize,
                None => self.bytes_read + self.remaining,
            };
            sink.on_progress(self.bytes_read, Some(total));
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

    // ---- TAR-01 regression: GNU/PAX sparse entries in the stream reader ----

    use crate::tar::sparse;

    /// Materialize the expected logical content for a sparse layout.
    fn materialize(realsize: u64, runs: &[(u64, u64)], fill: impl Fn(usize) -> u8) -> Vec<u8> {
        let mut out = vec![0u8; realsize as usize];
        let mut cursor = 0usize;
        for &(off, len) in runs {
            for i in 0..len as usize {
                out[off as usize + i] = fill(cursor + i);
            }
            cursor += len as usize;
        }
        out
    }

    /// Build the stored payload (concatenated runs + block padding) for a
    /// sparse layout, using `fill` for byte values.
    fn stored_payload(runs: &[(u64, u64)], fill: impl Fn(usize) -> u8) -> Vec<u8> {
        let stored: u64 = runs.iter().map(|&(_, n)| n).sum();
        let mut data: Vec<u8> = (0..stored as usize).map(fill).collect();
        let pad = (BLOCK_SIZE - (data.len() % BLOCK_SIZE)) % BLOCK_SIZE;
        data.extend(std::iter::repeat_n(0u8, pad));
        data
    }

    /// GNU old-format 'S' entry: the stream reader must produce the full
    /// logical realsize content (holes zero-filled), byte-identical to the
    /// seekable TarReader, instead of returning the raw stored runs.
    #[test]
    fn test_tar_stream_gnu_sparse_old_format() {
        let realsize = 16_384u64;
        let runs = vec![(0u64, 100u64), (500, 200), (4_000, 50), (10_000, 250)];
        let fill = |i: usize| (i % 251) as u8;

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_gnu_sparse_primary(
            "sp.bin", realsize, &runs, false,
        ));
        archive.extend_from_slice(&stored_payload(&runs, fill));
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]); // end-of-archive

        // Stream reader.
        let mut stream = TarStreamReader::new(Cursor::new(archive.clone()));
        let mut entry = stream
            .next_entry()
            .expect("next_entry sparse")
            .expect("sparse entry present");
        assert_eq!(entry.header.name, "sp.bin");
        assert_eq!(
            entry.header.size, realsize,
            "sparse entry must report realsize, not stored size"
        );
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read sparse content");
        drop(entry);
        assert!(stream.next_entry().expect("final").is_none());

        let expected = materialize(realsize, &runs, fill);
        assert_eq!(content.len(), expected.len());
        assert_eq!(content, expected, "sparse logical content mismatch");

        // Differential vs the seekable TarReader on the same bytes.
        let mut reader = TarReader::new(Cursor::new(archive)).expect("TarReader::new");
        let seekable_entry = reader.entries()[0].clone();
        assert_eq!(seekable_entry.size, realsize);
        let seekable = reader
            .extract_by_name("sp.bin")
            .expect("extract_by_name")
            .expect("entry present");
        assert_eq!(content, seekable, "stream and seekable readers must agree");
    }

    /// GNU old-format 'S' entry whose map spills into a continuation block
    /// (`isextended`) must also decode correctly in the stream reader.
    #[test]
    fn test_tar_stream_gnu_sparse_with_continuation() {
        let realsize = 100_000u64;
        let primary_runs = vec![(0u64, 100u64), (500, 200), (4_000, 50), (10_000, 500)];
        let cont_runs = vec![(20_000u64, 300u64), (40_000, 600), (70_000, 1000)];
        let mut all_runs = primary_runs.clone();
        all_runs.extend_from_slice(&cont_runs);
        let fill = |i: usize| ((i * 7 + 3) % 253) as u8;

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_gnu_sparse_primary(
            "big.bin",
            realsize,
            &primary_runs,
            true,
        ));
        archive.extend_from_slice(&sparse::build_gnu_sparse_continuation(&cont_runs, false));
        archive.extend_from_slice(&stored_payload(&all_runs, fill));
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let mut stream = TarStreamReader::new(Cursor::new(archive));
        let mut entry = stream
            .next_entry()
            .expect("next_entry sparse+cont")
            .expect("entry present");
        assert_eq!(entry.header.size, realsize);
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read content");
        drop(entry);
        assert!(stream.next_entry().expect("final").is_none());

        assert_eq!(content, materialize(realsize, &all_runs, fill));
    }

    /// PAX 0.1 sparse (`GNU.sparse.*` attributes on a regular '0' data
    /// entry): the stream reader must produce logical content and shadow
    /// the dummy `GNUSparseFile` path with `GNU.sparse.name`.
    #[test]
    fn test_tar_stream_pax_sparse() {
        let realsize = 10_000u64;
        let runs = vec![(0u64, 100u64), (5_000, 200)];
        let fill = |i: usize| (i % 199) as u8;

        let mk_record =
            |k: &str, v: &str| -> String { TarWriter::<Vec<u8>>::format_pax_record(k, v) };
        let mut pax_payload = String::new();
        pax_payload.push_str(&mk_record("GNU.sparse.name", "sparse.dat"));
        pax_payload.push_str(&mk_record("GNU.sparse.realsize", &realsize.to_string()));
        pax_payload.push_str(&mk_record("GNU.sparse.map", "0,100,5000,200"));
        let pax_bytes = pax_payload.as_bytes();

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_pax_header_block(
            b'x',
            pax_bytes.len() as u64,
        ));
        archive.extend_from_slice(pax_bytes);
        let pad = (BLOCK_SIZE - (pax_bytes.len() % BLOCK_SIZE)) % BLOCK_SIZE;
        archive.extend(std::iter::repeat_n(0u8, pad));

        // Data-entry header: typeflag '0', size = stored size, dummy name.
        let stored: u64 = runs.iter().map(|&(_, n)| n).sum();
        let mut data_hdr = sparse::build_pax_header_block(b'0', stored);
        // Rewrite the name field to the GNUSparseFile dummy path.
        let dummy = b"./GNUSparseFile.42/sparse.dat";
        data_hdr[..100].fill(0);
        data_hdr[..dummy.len()].copy_from_slice(dummy);
        // Re-checksum after the name rewrite.
        data_hdr[148..156].copy_from_slice(b"        ");
        let checksum: u32 = data_hdr.iter().map(|&b| b as u32).sum();
        let s = format!("{:06o}\0 ", checksum);
        data_hdr[148..156].copy_from_slice(&s.as_bytes()[..8]);
        archive.extend_from_slice(&data_hdr);
        archive.extend_from_slice(&stored_payload(&runs, fill));
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let mut stream = TarStreamReader::new(Cursor::new(archive.clone()));
        let mut entry = stream
            .next_entry()
            .expect("next_entry pax sparse")
            .expect("entry present");
        assert_eq!(
            entry.header.name, "sparse.dat",
            "GNU.sparse.name must shadow the dummy path"
        );
        assert_eq!(entry.header.size, realsize);
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read content");
        drop(entry);
        assert!(stream.next_entry().expect("final").is_none());

        assert_eq!(content, materialize(realsize, &runs, fill));

        // Differential vs the seekable TarReader.
        let mut reader = TarReader::new(Cursor::new(archive)).expect("TarReader::new");
        let seekable = reader
            .extract_by_name("sparse.dat")
            .expect("extract_by_name")
            .expect("entry present");
        assert_eq!(content, seekable);
    }

    /// PAX 1.0 sparse (`GNU.sparse.major`/`.minor` = "1"/"0"): unlike 0.1,
    /// `GNU.sparse.map` is absent from the pax attributes — the map is a
    /// decimal-ASCII preamble read directly off the stream at the start of
    /// the data entry's own payload (no `Seek` available, unlike the
    /// seekable `TarReader`). The stream reader must still produce logical
    /// content and shadow the dummy `GNUSparseFile` path with
    /// `GNU.sparse.name`, exactly as for 0.1.
    #[test]
    fn test_tar_stream_pax_1_0_sparse() {
        let realsize = 10_000u64;
        let runs = vec![(0u64, 100u64), (5_000, 200)];
        let fill = |i: usize| (i % 199) as u8;

        let mk_record =
            |k: &str, v: &str| -> String { TarWriter::<Vec<u8>>::format_pax_record(k, v) };
        let mut pax_payload = String::new();
        pax_payload.push_str(&mk_record("GNU.sparse.name", "sparse10.dat"));
        pax_payload.push_str(&mk_record("GNU.sparse.major", "1"));
        pax_payload.push_str(&mk_record("GNU.sparse.minor", "0"));
        pax_payload.push_str(&mk_record("GNU.sparse.realsize", &realsize.to_string()));
        let pax_bytes = pax_payload.as_bytes();

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_pax_header_block(
            b'x',
            pax_bytes.len() as u64,
        ));
        archive.extend_from_slice(pax_bytes);
        let pad = (BLOCK_SIZE - (pax_bytes.len() % BLOCK_SIZE)) % BLOCK_SIZE;
        archive.extend(std::iter::repeat_n(0u8, pad));

        // Data-entry header: typeflag '0', size = total stored size
        // (preamble + runs, both BLOCK_SIZE-padded), dummy name.
        let preamble = sparse::build_pax_1_0_preamble(&runs);
        let stored_run_bytes: u64 = runs.iter().map(|&(_, n)| n).sum();
        let padded_run_bytes = stored_run_bytes.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64;
        let total_stored = preamble.len() as u64 + padded_run_bytes;

        let mut data_hdr = sparse::build_pax_header_block(b'0', total_stored);
        // Rewrite the name field to the GNUSparseFile dummy path.
        let dummy = b"./GNUSparseFile.43/sparse10.dat";
        data_hdr[..100].fill(0);
        data_hdr[..dummy.len()].copy_from_slice(dummy);
        // Re-checksum after the name rewrite.
        data_hdr[148..156].copy_from_slice(b"        ");
        let checksum: u32 = data_hdr.iter().map(|&b| b as u32).sum();
        let s = format!("{:06o}\0 ", checksum);
        data_hdr[148..156].copy_from_slice(&s.as_bytes()[..8]);
        archive.extend_from_slice(&data_hdr);
        // Preamble first (already BLOCK_SIZE-padded), then the run payload.
        archive.extend_from_slice(&preamble);
        archive.extend_from_slice(&stored_payload(&runs, fill));
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let mut stream = TarStreamReader::new(Cursor::new(archive.clone()));
        let mut entry = stream
            .next_entry()
            .expect("next_entry pax 1.0 sparse")
            .expect("entry present");
        assert_eq!(
            entry.header.name, "sparse10.dat",
            "GNU.sparse.name must shadow the dummy path"
        );
        assert_eq!(entry.header.size, realsize);
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read content");
        drop(entry);
        assert!(stream.next_entry().expect("final").is_none());

        assert_eq!(content, materialize(realsize, &runs, fill));

        // Differential vs the seekable TarReader.
        let mut reader = TarReader::new(Cursor::new(archive)).expect("TarReader::new");
        let seekable = reader
            .extract_by_name("sparse10.dat")
            .expect("extract_by_name")
            .expect("entry present");
        assert_eq!(content, seekable);
    }

    /// A sparse entry dropped without reading must not desync the stream:
    /// the following entry must still parse and extract correctly.
    #[test]
    fn test_tar_stream_sparse_skip_keeps_alignment() {
        let realsize = 8_192u64;
        let runs = vec![(0u64, 300u64), (4_096, 300)];
        let fill = |i: usize| (i % 97) as u8;

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_gnu_sparse_primary(
            "sp.bin", realsize, &runs, false,
        ));
        archive.extend_from_slice(&stored_payload(&runs, fill));
        // A regular entry afterwards.
        {
            let mut w = TarWriter::new(&mut archive);
            w.add_file("after.txt", b"after sparse").expect("add_file");
            w.finish().expect("finish");
        }

        let mut stream = TarStreamReader::new(Cursor::new(archive));
        // Drop the sparse entry unread.
        let sparse_entry = stream
            .next_entry()
            .expect("next_entry sparse")
            .expect("sparse present");
        assert_eq!(sparse_entry.header.name, "sp.bin");
        drop(sparse_entry);

        let mut entry = stream
            .next_entry()
            .expect("next_entry after")
            .expect("after present");
        assert_eq!(entry.header.name, "after.txt");
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("read after");
        assert_eq!(&content, b"after sparse");
    }

    /// A truncated sparse payload must surface as an error, never as
    /// silently short/wrong content.
    #[test]
    fn test_tar_stream_sparse_truncated_is_error() {
        let realsize = 16_384u64;
        let runs = vec![(0u64, 600u64)];
        let fill = |i: usize| (i % 251) as u8;

        let mut archive = Vec::new();
        archive.extend_from_slice(&sparse::build_gnu_sparse_primary(
            "sp.bin", realsize, &runs, false,
        ));
        let payload = stored_payload(&runs, fill);
        // Truncate mid-run.
        archive.extend_from_slice(&payload[..300]);

        let mut stream = TarStreamReader::new(Cursor::new(archive));
        let mut entry = stream
            .next_entry()
            .expect("next_entry sparse")
            .expect("entry present");
        let mut content = Vec::new();
        let result = std::io::Read::read_to_end(&mut entry, &mut content);
        assert!(
            result.is_err(),
            "truncated sparse run must be an error, got {} bytes",
            content.len()
        );
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
