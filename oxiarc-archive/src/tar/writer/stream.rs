//! Streaming (write-as-you-go) TAR entry writer.
//!
//! [`TarWriter::add_stream`] and friends hand back a [`TarStreamWriter`] the
//! caller writes an entry's bytes into; the bytes go straight to the
//! archive's underlying [`Write`] as they are produced. See
//! [`TarStreamWriter`] for the format contract.

use super::super::header::TarHeader;
use super::{BLOCK_SIZE, TAR_NAME_MAX, TarWriter};
use oxiarc_core::error::{OxiArcError, Result};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest value the 12-byte octal `size` field of a UStar header can carry.
///
/// Eleven octal digits plus the NUL terminator: `0o77777777777` = 8 GiB - 1.
/// A stream whose declared size reaches [`TAR_SIZE_OCTAL_MAX`] + 1 therefore
/// carries its real size in a PAX `size` record instead, which every PAX-aware
/// reader (including this crate's) restores over the truncated field.
pub(crate) const TAR_SIZE_OCTAL_MAX: u64 = 0o777_7777_7777;

/// A TAR entry being written incrementally.
///
/// Returned by [`TarWriter::add_stream`], [`TarWriter::add_stream_with_mode`]
/// and [`TarWriter::add_stream_with_metadata`], and implementing [`Write`].
/// Every byte handed to it goes to the archive's underlying writer
/// immediately, so an entry of any size can be archived with a bounded
/// buffer — a `File` can be copied in chunks of whatever size the caller
/// likes, or piped through `std::io::copy`.
///
/// # Format contract
///
/// Unlike ZIP, a TAR header records the entry's size, and it is written before
/// the data — there is no data descriptor in the format and no way to go back
/// and fix one up. The declared size is therefore an *input* here: pass the
/// size you are about to write.
///
/// - Writing more than `size` bytes fails immediately (the header already
///   claims a shorter entry).
/// - Writing fewer and calling [`finish`](Self::finish) fails; the caller is
///   expected to write exactly `size` bytes.
/// - Dropping the entry without finishing it zero-fills the remainder of the
///   declared size and pads to the block boundary, the same recovery GNU tar
///   performs when a file shrinks mid-archive, so the archive stays
///   structurally valid. Errors during that best-effort finish are discarded.
///
/// Names longer than the UStar `name` field carries travel in a PAX extended
/// header emitted before the regular header, exactly as for the buffered
/// methods.
///
/// # Example
///
/// ```
/// use oxiarc_archive::TarWriter;
/// use std::io::{self, Write};
///
/// let mut buf = Vec::new();
/// {
///     let mut writer = TarWriter::new(&mut buf);
///     {
///         let mut entry = writer.add_stream("big.bin", 11)?;
///         entry.write_all(b"hello ")?;
///         entry.write_all(b"world")?;
///         entry.finish()?;
///     }
///     writer.finish()?;
/// }
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
pub struct TarStreamWriter<'w, W: Write> {
    /// The archive being written to. Held mutably, so the borrow checker
    /// enforces that at most one entry is open at a time.
    archive: &'w mut TarWriter<W>,
    /// Entry size declared in the header this stream was opened with.
    size: u64,
    /// Entry bytes written so far.
    written: u64,
    /// Entry name, kept for progress reporting and error messages.
    name: String,
    /// Whether the entry has been closed.
    finished: bool,
}

impl<W: Write> std::fmt::Debug for TarStreamWriter<'_, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TarStreamWriter")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("written", &self.written)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<'w, W: Write> TarStreamWriter<'w, W> {
    /// Open a streamed entry in `archive`, whose header has already been
    /// written for a `size`-byte entry.
    pub(super) fn new(archive: &'w mut TarWriter<W>, name: &str, size: u64) -> Self {
        Self {
            archive,
            size,
            written: 0,
            name: name.to_string(),
            finished: false,
        }
    }

    /// Declared entry size minus the bytes still to be written.
    pub fn remaining(&self) -> u64 {
        self.size - self.written
    }

    /// Whether [`finish`](Self::finish) has already run.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Zero-fill whatever is left of the declared size, pad to the block
    /// boundary, and close the entry.
    ///
    /// Idempotent: calling it more than once (including implicitly via
    /// [`Drop`]) writes the padding only once. Unlike
    /// [`finish`](Self::finish) this never fails on a short entry; it is what
    /// [`Drop`] uses to leave a well-formed archive behind.
    fn close(&mut self, allow_short: bool) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        if self.written != self.size {
            if !allow_short {
                return Err(OxiArcError::invalid_header(format!(
                    "streamed TAR entry '{}' declares {} bytes but only {} were written",
                    self.name, self.size, self.written
                )));
            }
            let missing = self.size - self.written;
            let filler = vec![0u8; usize::try_from(missing).unwrap_or(usize::MAX)];
            let mut written = 0usize;
            while written < filler.len() {
                let n = self
                    .archive
                    .writer_mut()?
                    .write(&filler[written..])
                    .map_err(io::Error::other)?;
                if n == 0 {
                    return Err(OxiArcError::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "TAR padding: underlying writer accepted no bytes",
                    )));
                }
                written += n;
            }
            self.written = self.size;
        }

        // Pad the entry body to a 512-byte boundary: the next header is only
        // allowed to start there, and readers skip to it by counting blocks
        // from the declared size.
        let block = BLOCK_SIZE as u64;
        let padding =
            usize::try_from((block - (self.written % block)) % block).unwrap_or(BLOCK_SIZE);
        if padding > 0 {
            self.archive.writer_mut()?.write_all(&vec![0u8; padding])?;
        }

        self.finished = true;

        // Emit progress: bytes written. A buffered `add_file` reports the
        // entry's full size the same way; a streaming writer cannot report it
        // any earlier, since only the declared size is known up front.
        if let Some(ref handle) = self.archive.progress {
            handle.on_progress(self.size, None);
        }

        Ok(())
    }

    /// Close the entry after exactly its declared number of bytes has been
    /// written.
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::Io`] if the underlying writer fails, and
    /// [`OxiArcError::InvalidHeader`] if fewer bytes than declared were
    /// written — the archive is then left unterminated, so the caller should
    /// abandon it rather than call [`TarWriter::finish`].
    pub fn finish(&mut self) -> Result<()> {
        self.close(false)
    }
}

impl<W: Write> Write for TarStreamWriter<'_, W> {
    /// Write entry bytes straight through to the archive.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.finished {
            return Err(std::io::Error::other("write to a finished TarStreamWriter"));
        }
        let len = buf.len() as u64;
        let remaining = self.size - self.written;
        if len > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "streamed TAR entry '{}' declares {remaining} more bytes but {len} were offered",
                    self.name
                ),
            ));
        }
        self.archive
            .writer_mut()
            .map_err(to_io_error)?
            .write_all(buf)?;
        self.written += len;
        Ok(buf.len())
    }

    /// Flush the archive's underlying writer.
    ///
    /// Deliberately does not close the entry: TAR has no way to reopen a
    /// header. Use [`finish`](Self::finish) to end the entry.
    fn flush(&mut self) -> std::io::Result<()> {
        self.archive.writer_mut().map_err(to_io_error)?.flush()
    }
}

impl<W: Write> Drop for TarStreamWriter<'_, W> {
    fn drop(&mut self) {
        let _ = self.close(true);
    }
}

/// Convert an [`OxiArcError`] into an [`std::io::Error`] for the [`Write`]
/// impl, which cannot return the crate's own error type.
fn to_io_error(err: OxiArcError) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

/// Open a streamed entry: emit the PAX extended header the name needs (and,
/// for a size beyond the octal field, a PAX `size` record), then the regular
/// UStar header carrying the declared size.
///
/// Returns the writer plus the name the UStar header actually used.
pub(super) fn open_stream_entry<'w, W: Write>(
    archive: &'w mut TarWriter<W>,
    name: &str,
    size: u64,
    mode: u32,
    mtime: SystemTime,
) -> Result<(TarStreamWriter<'w, W>, String)> {
    let mtime_secs = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Long names (and, when the octal size field cannot hold the value, the
    // size itself) travel in PAX extended attributes ahead of the header.
    let needs_pax_path = name.len() > TAR_NAME_MAX;
    let needs_pax_size = size > TAR_SIZE_OCTAL_MAX;
    if needs_pax_path || needs_pax_size {
        archive.write_pax_header_with_size(
            if needs_pax_path { name } else { "" },
            needs_pax_size.then_some(size),
        )?;
    }

    let header_name = if needs_pax_path {
        TarWriter::<W>::tar_fallback_name(name)
    } else {
        name.to_string()
    };
    let header = TarHeader::new_file_with_mtime(&header_name, size, mode, mtime_secs);
    archive.write_header(&header)?;

    // Progress: the entry has been announced by `open_stream_entry`'s caller.
    Ok((TarStreamWriter::new(archive, name, size), header_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::{BLOCK_SIZE, TarHeader, TarReader, TarStreamReader};
    use std::io::{Cursor, Read};
    use std::time::{Duration, UNIX_EPOCH};

    /// Compressible payload of `len` bytes.
    fn payload(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| b"streamed tar entry payload "[i % 27])
            .collect()
    }

    /// Write `data` as a single streamed entry, in `chunk`-sized writes, and
    /// return the whole archive.
    fn stream_one_archive(name: &str, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf);
            {
                let mut entry = writer
                    .add_stream_with_mode(name, data.len() as u64, 0o644)
                    .expect("add_stream_with_mode");
                let mut pos = 0;
                while pos < data.len() {
                    let end = (pos + chunk).min(data.len());
                    entry.write_all(&data[pos..end]).expect("write_all");
                    pos = end;
                }
                entry.finish().expect("finish entry");
            }
            writer.finish().expect("finish archive");
        }
        buf
    }

    /// Read every entry of `archive` back through the random-access reader as
    /// `(name, typeflag, contents)`.
    fn read_back(archive: &[u8]) -> Vec<(String, u8, Vec<u8>)> {
        let mut reader = TarReader::new(Cursor::new(archive)).expect("TarReader::new");
        let entries = reader.entries().to_vec();
        let mut out = Vec::new();
        for entry in &entries {
            let header = reader.header_for(entry).expect("header for entry");
            let name = header.name.clone();
            let typeflag = header.typeflag;
            let data = reader.extract_to_vec(entry).unwrap_or_else(|_| Vec::new());
            out.push((name, typeflag, data));
        }
        out
    }

    /// A streamed entry must round-trip through both TAR readers at any write
    /// granularity, and must be padded to the 512-byte block boundary so the
    /// next entry lands where a reader expects it.
    #[test]
    fn test_tar_stream_round_trips_at_any_write_granularity() {
        let data = payload(100_000);
        for chunk in [1, 13, BLOCK_SIZE, 5000, data.len()] {
            let archive = stream_one_archive("data.bin", &data, chunk);
            // 512-byte header + ceil(len / 512) data blocks + two terminator
            // blocks.
            let blocks = 1 + data.len().div_ceil(BLOCK_SIZE) + 2;
            assert_eq!(
                archive.len(),
                blocks * BLOCK_SIZE,
                "archive length for a {chunk} byte write chunk"
            );

            let entries = read_back(&archive);
            assert_eq!(entries.len(), 1, "expected exactly one entry");
            assert_eq!(entries[0].0, "data.bin");
            assert_eq!(entries[0].1, b'0');
            assert_eq!(entries[0].2, data, "payload mismatch");
        }
    }

    /// The sequential (Read-only) reader sees the same entry.
    #[test]
    fn test_tar_stream_round_trips_through_stream_reader() {
        let data = payload(20_000);
        let archive = stream_one_archive("s.bin", &data, 777);

        let mut reader = TarStreamReader::new(Cursor::new(&archive));
        let mut collected = Vec::new();
        while let Some(mut entry) = reader.next_entry().expect("next_entry") {
            let name = entry.header.name.clone();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).expect("read_to_end");
            collected.push((name, buf));
        }
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, "s.bin");
        assert_eq!(collected[0].1, data);
    }

    /// A zero-length streamed entry is a valid entry with an empty body.
    #[test]
    fn test_tar_stream_round_trips_empty_entry() {
        let archive = stream_one_archive("empty.bin", &[], BLOCK_SIZE);
        let entries = read_back(&archive);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "empty.bin");
        assert_eq!(entries[0].2, Vec::<u8>::new());
        assert_eq!(archive.len(), (1 + 2) * BLOCK_SIZE);
    }

    /// A streamed entry and the buffered `add_file` of the same payload
    /// produce byte-identical archives (modulo the mtime the buffered call
    /// stamps with "now", which is pinned by using the metadata variant).
    #[test]
    fn test_tar_stream_matches_buffered_bytes() {
        let data = payload(9_000);
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let mut streamed = Vec::new();
        {
            let mut writer = TarWriter::new(&mut streamed);
            {
                let mut entry = writer
                    .add_stream_with_metadata("x.bin", data.len() as u64, 0o640, mtime)
                    .expect("add_stream_with_metadata");
                for chunk in data.chunks(512) {
                    entry.write_all(chunk).expect("write_all");
                }
                entry.finish().expect("finish entry");
            }
            writer.finish().expect("finish archive");
        }

        let mut buffered = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buffered);
            writer
                .add_file_with_metadata("x.bin", &data, 0o640, mtime)
                .expect("add_file_with_metadata");
            writer.finish().expect("finish archive");
        }

        assert_eq!(
            streamed, buffered,
            "streaming an entry must not change the bytes add_file writes"
        );
    }

    /// Writing more than the declared size is refused immediately: the header
    /// already claims a shorter entry and cannot be taken back.
    #[test]
    fn test_tar_stream_rejects_writes_beyond_declared_size() {
        let mut buf = Vec::new();
        let mut writer = TarWriter::new(&mut buf);
        {
            let mut entry = writer.add_stream("short.bin", 4).expect("add_stream");
            entry.write_all(b"abcd").expect("write the declared size");
            let err = entry
                .write_all(b"e")
                .expect_err("writing past the declared size must fail");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        writer.finish().expect("finish");
    }

    /// Finishing with fewer bytes written than declared is an error too; the
    /// caller is expected to write exactly what it announced.
    #[test]
    fn test_tar_stream_rejects_short_entry_on_finish() {
        let mut buf = Vec::new();
        let mut writer = TarWriter::new(&mut buf);
        let mut entry = writer.add_stream("short.bin", 10).expect("add_stream");
        entry.write_all(b"abcd").expect("write");
        let err = entry
            .finish()
            .expect_err("finishing a short entry must fail");
        assert!(
            err.to_string().contains("declares 10 bytes"),
            "unexpected error: {err}"
        );
    }

    /// Dropping without finishing zero-fills the remainder of the declared
    /// size, so the archive stays well formed (GNU tar's recovery for a file
    /// that shrank mid-archive) and later entries are still readable.
    #[test]
    fn test_tar_stream_drop_zero_fills_and_keeps_archive_valid() {
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf);
            {
                let mut entry = writer.add_stream("partial.bin", 1000).expect("add_stream");
                entry.write_all(b"only a few bytes").expect("write");
                // Dropped here without finish().
            }
            writer
                .add_file("after.txt", b"still here")
                .expect("add_file after a dropped entry");
            writer.finish().expect("finish");
        }

        let entries = read_back(&buf);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "partial.bin");
        assert_eq!(entries[0].2.len(), 1000);
        assert_eq!(&entries[0].2[..16], b"only a few bytes");
        assert_eq!(&entries[0].2[16..], &vec![0u8; 984][..]);
        assert_eq!(entries[1].0, "after.txt");
        assert_eq!(entries[1].2, b"still here".to_vec());
    }

    /// Writing to a finished entry is an error rather than a silently
    //  misaligned archive.
    #[test]
    fn test_tar_stream_write_after_finish_errors() {
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf);
            {
                let mut entry = writer.add_stream("late.bin", 4).expect("add_stream");
                entry.write_all(b"done").expect("write");
                entry.finish().expect("finish");
                assert!(entry.write_all(b"more").is_err());
            }
            writer.finish().expect("finish");
        }
        let entries = read_back(&buf);
        assert_eq!(entries[0].2, b"done".to_vec());
    }

    /// A name longer than the UStar `name` field travels in a PAX extended
    /// header, for streamed and buffered entries alike.
    #[test]
    fn test_tar_stream_uses_pax_for_long_names() {
        let name = "a".repeat(120) + "/leaf.txt";
        let data = payload(1_500);
        let archive = stream_one_archive(&name, &data, 300);

        // PAX header + its data block + the real header + the data.
        assert!(archive.len() > data.len());
        let entries = read_back(&archive);
        assert_eq!(entries[0].0, name, "PAX must restore the full name");
        assert_eq!(entries[0].2, data);
    }

    /// A size past the 12-byte octal field travels in a PAX `size` record; the
    /// UStar header itself only fits 8 GiB - 1.
    #[test]
    fn test_tar_stream_uses_pax_for_sizes_past_the_octal_field() {
        let size = TAR_SIZE_OCTAL_MAX + 1;
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf);
            {
                // No payload is actually written; the entry is abandoned by
                // dropping it, which zero-fills only 8 GiB of padding... so
                // instead finish on a writer that rejects the writes.
                let mut entry = writer
                    .add_stream_with_metadata("huge.bin", size, 0o644, mtime)
                    .expect("add_stream_with_metadata");
                // Writing anything at all is fine; only the total matters.
                entry.write_all(b"x").expect("write");
                // Skip the zero-fill: finish() would refuse, and drop would
                // try to emit 8 GiB of zeros. Leak the archive instead.
                std::mem::forget(entry);
            }
            std::mem::forget(writer);
        }

        // The UStar header must carry the PAX placeholder...
        let mut first = [0u8; BLOCK_SIZE];
        first.copy_from_slice(&buf[..BLOCK_SIZE]);
        let header = TarHeader::from_block(&first)
            .expect("first header block")
            .expect("a header block");
        assert_eq!(header.typeflag, b'x', "first block must be the PAX header");
        assert_eq!(header.size as usize, pax_record_len(&buf));

        // ...whose records carry the real size, which the UStar header that
        // follows cannot express.
        let pax_len = header.size as usize;
        assert!(pax_len > 0);
        let mut second = [0u8; BLOCK_SIZE];
        second.copy_from_slice(&buf[BLOCK_SIZE * 2..BLOCK_SIZE * 3]);
        let ustar = TarHeader::from_block(&second)
            .expect("ustar header block")
            .expect("ustar header");
        assert_eq!(ustar.typeflag, b'0');
        assert!(
            ustar.size <= TAR_SIZE_OCTAL_MAX,
            "the octal field must not pretend it can hold {size}"
        );
        let records = String::from_utf8_lossy(&buf[BLOCK_SIZE..BLOCK_SIZE + pax_len]).to_string();
        assert!(
            records.contains(&format!("size={size}")),
            "PAX size record missing, got: {records:?}"
        );
    }

    /// Total length of the PAX records in the first (extended-header) block.
    fn pax_record_len(buf: &[u8]) -> usize {
        let mut block = [0u8; BLOCK_SIZE];
        block.copy_from_slice(&buf[..BLOCK_SIZE]);
        TarHeader::from_block(&block)
            .expect("pax header")
            .expect("pax header")
            .size as usize
    }

    /// Streamed entries interleave correctly with buffered ones, directories
    /// and symlinks: the running entry count keeps advancing and every entry
    /// stays readable.
    #[test]
    fn test_tar_stream_interleaves_with_other_entry_kinds() {
        let data = payload(3_000);
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf);
            writer.add_directory("dir").expect("add_directory");
            {
                let mut entry = writer
                    .add_stream("dir/streamed.bin", data.len() as u64)
                    .expect("add_stream");
                for chunk in data.chunks(101) {
                    entry.write_all(chunk).expect("write");
                }
                entry.finish().expect("finish entry");
            }
            writer
                .add_symlink("dir/link", "streamed.bin")
                .expect("add_symlink");
            writer
                .add_file("dir/buffered.txt", b"tail")
                .expect("add_file");
            writer.finish().expect("finish");
        }

        let entries = read_back(&buf);
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["dir/", "dir/streamed.bin", "dir/link", "dir/buffered.txt"]
        );
        assert_eq!(entries[1].2, data);
        assert_eq!(entries[3].2, b"tail".to_vec());
    }

    /// Progress hooks: `on_entry` when the entry opens and one `on_progress`
    /// carrying the entry's declared size when it closes, exactly as the
    /// buffered methods report it.
    #[test]
    fn test_tar_stream_reports_progress() {
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

        let data = payload(2_000);
        let mut buf = Vec::new();
        {
            let mut writer = TarWriter::new(&mut buf).with_progress(handle);
            {
                let mut entry = writer
                    .add_stream("p.bin", data.len() as u64)
                    .expect("add_stream");
                entry.write_all(&data).expect("write");
                entry.finish().expect("finish");
            }
            writer.finish().expect("finish");
        }

        assert_eq!(sink.entries.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64,
            "the progress notification must carry the entry's size"
        );
    }
}
