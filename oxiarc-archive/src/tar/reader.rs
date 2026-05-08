//! TAR archive reader with extraction support.

use crate::lenient::{LenientWarning, LenientWarningKind};
use oxiarc_core::Entry;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use super::header::TarHeader;
use super::sparse::{SparseMap, SparseMapTable};
use super::{BLOCK_SIZE, LENIENT_SCAN_MAX_BLOCKS};

/// TAR archive reader with extraction support.
pub struct TarReader<R: Read + Seek> {
    reader: R,
    entries: Vec<Entry>,
    /// Parallel raw TAR headers for each entry in `entries`.
    ///
    /// Kept so that callers (e.g. `oxiarc add`) can retrieve the full
    /// header metadata — uid, gid, uname, gname, mtime, linkname, mode —
    /// and pass it to [`super::writer::TarWriter::add_entry_from_header`] without loss.
    headers: Vec<TarHeader>,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
    /// Per-entry sparse maps, keyed by `entry.offset` (= data offset).
    ///
    /// Populated during `read_entries` for GNU old-format `'S'` entries and
    /// for PAX-marked sparse entries. Absent keys indicate a normal
    /// (non-sparse) entry. Wrapped so that `extract()` can short-circuit
    /// to the sparse materialization path.
    sparse_maps: SparseMapTable,
    /// When `true`, TAR header-checksum mismatches during scanning are
    /// recorded in [`TarReader::warnings`] and the reader scans forward
    /// to locate the next valid header. When `false` (default), a
    /// mismatch aborts [`TarReader::new`] with
    /// [`OxiArcError::InvalidHeader`].
    lenient: bool,
    /// Accumulated non-fatal warnings emitted while operating in
    /// lenient mode.
    warnings: Vec<LenientWarning>,
}

impl<R: Read + Seek> TarReader<R> {
    /// Create a new TAR reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, headers, sparse_maps) =
            Self::read_entries(&mut reader, None, false, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            headers,
            progress: None,
            sparse_maps,
            lenient: false,
            warnings,
        })
    }

    /// Create a new lenient TAR reader.
    ///
    /// Equivalent to `TarReader::new(reader).lenient(true)` except that
    /// the initial entry scan is itself permitted to skip over corrupt
    /// blocks. Use this when the archive may contain mid-stream damage
    /// that the default (strict) scanner cannot tolerate.
    pub fn new_lenient(mut reader: R) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, headers, sparse_maps) =
            Self::read_entries(&mut reader, None, true, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            headers,
            progress: None,
            sparse_maps,
            lenient: true,
            warnings,
        })
    }

    /// Create a new TAR reader with progress reporting during entry scanning.
    pub fn new_with_progress(mut reader: R, handle: ProgressHandle) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, headers, sparse_maps) =
            Self::read_entries(&mut reader, Some(&handle), false, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            headers,
            progress: Some(handle),
            sparse_maps,
            lenient: false,
            warnings,
        })
    }

    /// Attach a progress callback handle.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Toggle lenient-mode **extraction**. Note: this does NOT re-run
    /// the initial entry scan. If you need lenient scanning to skip
    /// corrupt header blocks, use [`TarReader::new_lenient`] instead.
    pub fn lenient(mut self, enabled: bool) -> Self {
        self.lenient = enabled;
        self
    }

    /// Return the accumulated non-fatal warnings from lenient-mode
    /// operations.
    pub fn warnings(&self) -> &[LenientWarning] {
        &self.warnings
    }

    /// Read all entries, optionally reporting progress.
    ///
    /// Returns both the list of parsed entries and a side-channel table of
    /// sparse maps keyed by entry data offset. The map is consulted by
    /// `extract()` to switch into the sparse materialization path; entries
    /// absent from the map are extracted via plain sequential copy.
    ///
    /// When `lenient` is `true`, a header block whose POSIX checksum
    /// field does not match the computed checksum (and which is not an
    /// all-zero end-of-archive marker) is treated as a recoverable
    /// scanning error: the reader records a [`LenientWarning`] in
    /// `warnings`, advances 512 bytes, and probes the next candidate
    /// block. Recovery gives up after [`LENIENT_SCAN_MAX_BLOCKS`]
    /// consecutive failed probes and returns
    /// [`OxiArcError::InvalidHeader`]. Non-lenient mode preserves the
    /// legacy behavior of propagating the first parse error.
    fn read_entries(
        reader: &mut R,
        progress: Option<&ProgressHandle>,
        lenient: bool,
        warnings: &mut Vec<LenientWarning>,
    ) -> Result<(Vec<Entry>, Vec<TarHeader>, SparseMapTable)> {
        let mut entries = Vec::new();
        let mut headers: Vec<TarHeader> = Vec::new();
        let mut sparse_maps: SparseMapTable = HashMap::new();
        let mut offset = 0u64;
        let mut pax_attrs: HashMap<String, String> = HashMap::new();
        let mut global_pax_attrs: HashMap<String, String> = HashMap::new();
        let mut gnu_longname: Option<String> = None;
        let mut gnu_longlink: Option<String> = None;
        let mut index: u64 = 0;

        loop {
            let mut block = [0u8; BLOCK_SIZE];
            if reader.read_exact(&mut block).is_err() {
                break;
            }

            // Verify header checksum *before* attempting to parse.
            //
            // A fully-zero block is the legitimate end-of-archive
            // marker and is handled below by `TarHeader::from_block`
            // returning `None`. Every other block MUST pass the
            // checksum verification; if it does not, strict mode
            // returns an error and lenient mode scans forward.
            let is_zero_block = block.iter().all(|&b| b == 0);
            if !is_zero_block && !TarHeader::verify_checksum(&block) {
                if !lenient {
                    return Err(OxiArcError::invalid_header(format!(
                        "TAR header checksum mismatch at offset {offset}"
                    )));
                }

                // Lenient: scan forward up to LENIENT_SCAN_MAX_BLOCKS
                // blocks for the next block whose checksum matches and
                // whose `size` field is a parseable octal number
                // (otherwise we could easily snap onto random data).
                warnings.push(LenientWarning {
                    format: "TAR",
                    entry_name: None,
                    kind: LenientWarningKind::HeaderChecksumMismatch,
                    message: format!(
                        "TAR header checksum mismatch at offset {offset}; scanning forward"
                    ),
                });

                let scan_start = offset + BLOCK_SIZE as u64;
                let mut scanned_blocks = 0usize;
                let mut recovered = false;

                while scanned_blocks < LENIENT_SCAN_MAX_BLOCKS {
                    let mut probe = [0u8; BLOCK_SIZE];
                    if reader.read_exact(&mut probe).is_err() {
                        break;
                    }
                    scanned_blocks += 1;

                    let probe_offset = scan_start + (scanned_blocks as u64 - 1) * BLOCK_SIZE as u64;

                    let probe_is_zero = probe.iter().all(|&b| b == 0);
                    if probe_is_zero
                        || (TarHeader::verify_checksum(&probe)
                            && TarHeader::parse_octal_u64(&probe[124..136]).is_ok())
                    {
                        // Found either a valid header OR a zero block
                        // (genuine end-of-archive). In both cases,
                        // rewind one block so the outer loop re-reads
                        // the block and dispatches normally:
                        //   - valid header → parsed by `from_block`
                        //   - zero block   → `from_block` returns None
                        //     and the outer loop breaks
                        reader.seek(SeekFrom::Current(-(BLOCK_SIZE as i64)))?;
                        warnings.push(LenientWarning {
                            format: "TAR",
                            entry_name: None,
                            kind: LenientWarningKind::ScannedForward {
                                bytes: (scanned_blocks as u64) * BLOCK_SIZE as u64,
                            },
                            message: format!(
                                "TAR scan recovered a valid header/EOA after skipping {} bytes",
                                scanned_blocks * BLOCK_SIZE
                            ),
                        });
                        offset = probe_offset;
                        recovered = true;
                        break;
                    }
                }

                if !recovered {
                    return Err(OxiArcError::invalid_header(format!(
                        "TAR lenient scan gave up after {LENIENT_SCAN_MAX_BLOCKS} bad blocks starting at offset {offset}"
                    )));
                }

                // Continue outer loop — the next iteration will
                // `read_exact` the recovered header block.
                continue;
            }

            match TarHeader::from_block(&block)? {
                Some(mut header) => {
                    // Handle special header types
                    if header.is_pax_header() || header.is_pax_global_header() {
                        // Read PAX extended header data
                        let data = Self::read_header_data(reader, header.size)?;
                        let attrs = TarHeader::parse_pax_data(&data);

                        if header.is_pax_global_header() {
                            // Global headers apply to all subsequent entries
                            global_pax_attrs.extend(attrs);
                        } else {
                            // Regular PAX headers apply only to next entry
                            pax_attrs = attrs;
                        }

                        // Update offset and continue to next header
                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    if header.is_gnu_longname() {
                        // Read GNU LongName data
                        let data = Self::read_header_data(reader, header.size)?;
                        gnu_longname = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );

                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    if header.is_gnu_longlink() {
                        // Read GNU LongLink data
                        let data = Self::read_header_data(reader, header.size)?;
                        gnu_longlink = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );

                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    // ---- GNU old-format sparse entry (typeflag 'S') ----
                    //
                    // These have the sparse map encoded in the primary
                    // header's unused bytes, optionally extended by
                    // continuation 512-byte blocks. The payload that
                    // follows is the concatenation of non-hole runs,
                    // padded to `BLOCK_SIZE`.
                    if header.typeflag == b'S' {
                        // Parse the sparse map starting from the primary
                        // header; `parse_gnu_old_format` pulls extra
                        // 512-byte continuation blocks from `reader`
                        // whenever the `isextended` flag is set.
                        //
                        // We track how many continuation blocks were
                        // consumed by measuring the seek delta; this lets
                        // us update `offset` correctly without relying on
                        // an internal counter from the parser.
                        let start_pos = reader.stream_position()?;
                        let map = SparseMap::parse_gnu_old_format(&block, reader)?;
                        map.validate()?;
                        let cont_bytes = reader.stream_position()? - start_pos;

                        let realsize = map.realsize;
                        let data_offset = offset + BLOCK_SIZE as u64 + cont_bytes;
                        let mut entry = header.to_entry(data_offset);
                        entry.size = realsize;

                        if let Some(handle) = progress {
                            handle.on_entry(&entry.name, index);
                            handle.on_progress(entry.size, Some(entry.size));
                        }
                        index += 1;

                        let skip_bytes = map.padded_stored_size();
                        sparse_maps.insert(data_offset, map);

                        headers.push(header);
                        entries.push(entry);

                        reader.seek(SeekFrom::Current(skip_bytes as i64))?;

                        offset += BLOCK_SIZE as u64 + cont_bytes + skip_bytes;
                        continue;
                    }

                    // ---- Standard path (non-sparse or PAX-encoded sparse) ----

                    // Apply accumulated attributes
                    // First global, then local PAX (local overrides global)
                    if !global_pax_attrs.is_empty() {
                        header.apply_pax_attrs(&global_pax_attrs);
                    }

                    // Detect PAX-encoded sparse (`GNU.sparse.map` present in
                    // local PAX attrs) before `pax_attrs.clear()` drains
                    // them.
                    let is_pax_sparse = pax_attrs.contains_key("GNU.sparse.map");

                    if !pax_attrs.is_empty() {
                        header.apply_pax_attrs(&pax_attrs);
                    }

                    // Apply GNU long name/link
                    if let Some(name) = gnu_longname.take() {
                        header.name = name;
                    }
                    if let Some(link) = gnu_longlink.take() {
                        header.linkname = link;
                    }

                    let data_offset = offset + BLOCK_SIZE as u64;
                    let mut entry = header.to_entry(data_offset);

                    // Read count of bytes the payload occupies on the
                    // medium. For non-sparse entries this is `header.size`;
                    // for PAX-sparse entries we rederive it from the map.
                    let stored_bytes = if is_pax_sparse {
                        let map = SparseMap::from_pax_attrs(&pax_attrs)?;
                        map.validate()?;
                        // PAX sparse: logical size is realsize, stored
                        // bytes are sum of runs padded to BLOCK_SIZE.
                        entry.size = map.realsize;

                        // PAX 0.1 shadows the data-entry name: real archives
                        // emit dummy names like `./GNUSparseFile.XXXX/<real>`
                        // on the data header and supply the canonical name
                        // in `GNU.sparse.name`. Prefer that when present.
                        if let Some(real_name) = pax_attrs.get("GNU.sparse.name") {
                            entry.name = real_name.clone();
                        }

                        let padded = map.padded_stored_size();
                        sparse_maps.insert(data_offset, map);
                        padded
                    } else {
                        // Normal entry: skip header.size rounded up.
                        header.size.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64
                    };

                    // Done consuming PAX attrs for this entry.
                    pax_attrs.clear();

                    if let Some(handle) = progress {
                        handle.on_entry(&entry.name, index);
                        handle.on_progress(entry.size, Some(entry.size));
                    }
                    index += 1;

                    headers.push(header);
                    entries.push(entry);

                    // Use seek for efficiency
                    reader.seek(SeekFrom::Current(stored_bytes as i64))?;

                    offset += BLOCK_SIZE as u64 + stored_bytes;
                }
                None => break, // End of archive
            }
        }

        if let Some(handle) = progress {
            handle.on_finish();
        }

        Ok((entries, headers, sparse_maps))
    }

    /// Read header data (for PAX extended headers and GNU long name/link).
    fn read_header_data(reader: &mut R, size: u64) -> Result<Vec<u8>> {
        let mut data = vec![0u8; size as usize];
        reader.read_exact(&mut data)?;

        // Skip padding to block boundary
        let padding = (BLOCK_SIZE - (size as usize % BLOCK_SIZE)) % BLOCK_SIZE;
        if padding > 0 {
            let mut skip = [0u8; BLOCK_SIZE];
            reader.read_exact(&mut skip[..padding])?;
        }

        Ok(data)
    }

    /// Get entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Return the raw `TarHeader` that produced the given `Entry`, matched by
    /// the entry's data `offset`.
    ///
    /// Returns `None` when no header with a matching offset is found (e.g. the
    /// entry was not produced by this reader). The returned header preserves
    /// all metadata that is otherwise discarded when converting to the
    /// format-agnostic [`Entry`] type — including `uid`, `gid`, `uname`,
    /// `gname`, `mtime`, and `linkname`.
    pub fn header_for(&self, entry: &Entry) -> Option<&TarHeader> {
        // headers and entries are parallel — find by data offset match.
        self.entries
            .iter()
            .zip(self.headers.iter())
            .find(|(e, _)| e.offset == entry.offset)
            .map(|(_, h)| h)
    }

    /// Extract an entry to a writer.
    ///
    /// For sparse entries (GNU old-format `'S'` or PAX `GNU.sparse.*`), the
    /// logical content is materialized: non-hole runs are read from the
    /// data stream, and hole regions are filled with zero bytes. The full
    /// `entry.size` bytes (the logical size) are written to `writer`.
    pub fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64> {
        // Emit extraction progress
        if let Some(ref handle) = self.progress {
            handle.on_entry(&entry.name, 0);
        }

        // Seek to data offset
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Sparse entries go through the materialization helper so that
        // callers see a contiguous logical file rather than the packed
        // on-disk form.
        if let Some(map) = self.sparse_maps.get(&entry.offset).cloned() {
            let buf = super::sparse::extract_sparse(&mut self.reader, &map)?;
            writer.write_all(&buf)?;
            let written = buf.len() as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(written, Some(entry.size));
            }
            return Ok(written);
        }

        // Read and write data in chunks
        let mut remaining = entry.size;
        let mut buffer = [0u8; 8192];
        let mut written = 0u64;

        while remaining > 0 {
            let to_read = remaining.min(buffer.len() as u64) as usize;
            self.reader.read_exact(&mut buffer[..to_read])?;
            writer.write_all(&buffer[..to_read])?;
            remaining -= to_read as u64;
            written += to_read as u64;
        }

        if let Some(ref handle) = self.progress {
            handle.on_progress(written, Some(entry.size));
        }

        Ok(written)
    }

    /// Extract an entry to a Vec.
    pub fn extract_to_vec(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(entry.size as usize);
        self.extract(entry, &mut data)?;
        Ok(data)
    }

    /// Extract an entry by name.
    pub fn extract_by_name(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let entry = self.entries.iter().find(|e| e.name == name).cloned();
        match entry {
            Some(e) => Ok(Some(self.extract_to_vec(&e)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::sparse;
    use crate::tar::writer::TarWriter;
    use std::io::Cursor;

    #[test]
    fn test_tar_progress() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Sink {
            entries: Mutex<Vec<String>>,
            progress_calls: Mutex<u64>,
            finish_called: Mutex<bool>,
        }

        impl oxiarc_core::progress::ProgressSink for Sink {
            fn on_progress(&self, _processed: u64, _total: Option<u64>) {
                *self.progress_calls.lock().expect("progress_calls lock") += 1;
            }
            fn on_entry(&self, name: &str, _index: u64) {
                self.entries
                    .lock()
                    .expect("entries lock")
                    .push(name.to_string());
            }
            fn on_finish(&self) {
                *self.finish_called.lock().expect("finish_called lock") = true;
            }
        }

        let sink = Arc::new(Sink::default());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        // Write a test archive
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output).with_progress(handle);
            writer
                .add_file("first.txt", b"hello")
                .expect("add_file first.txt");
            writer
                .add_file("second.txt", b"world")
                .expect("add_file second.txt");
            writer.finish().expect("writer finish");
        }

        {
            let entries = sink.entries.lock().expect("entries lock");
            assert_eq!(entries.len(), 2, "expected on_entry called twice");
            assert_eq!(entries[0], "first.txt");
            assert_eq!(entries[1], "second.txt");
        }
        assert!(
            *sink.finish_called.lock().expect("finish_called lock"),
            "on_finish not called"
        );
    }

    #[test]
    fn test_parse_octal() {
        assert_eq!(
            TarHeader::parse_octal(b"0000644\0").expect("parse 0644"),
            0o644
        );
        assert_eq!(
            TarHeader::parse_octal(b"0001750\0").expect("parse 01750"),
            0o1750
        );
    }

    #[test]
    fn test_parse_string() {
        assert_eq!(TarHeader::parse_string(b"hello\0world"), "hello");
        assert_eq!(TarHeader::parse_string(b"test"), "test");
    }

    /// Create a minimal TAR archive for testing.
    fn create_test_tar() -> Vec<u8> {
        let mut tar = vec![0u8; BLOCK_SIZE * 4];

        // Header block for "test.txt"
        let name = b"test.txt";
        tar[..name.len()].copy_from_slice(name);

        // Mode: 0644
        tar[100..107].copy_from_slice(b"0000644");

        // UID: 1000
        tar[108..115].copy_from_slice(b"0001750");

        // GID: 1000
        tar[116..123].copy_from_slice(b"0001750");

        // Size: 13 bytes ("Hello, TAR!\n")
        tar[124..135].copy_from_slice(b"00000000015");

        // Mtime: some timestamp
        tar[136..147].copy_from_slice(b"14723456700");

        // Typeflag: regular file
        tar[156] = b'0';

        // Calculate checksum
        tar[148..156].copy_from_slice(b"        "); // Initialize with spaces
        let checksum: u32 = tar[..BLOCK_SIZE].iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        tar[148..156].copy_from_slice(checksum_str.as_bytes());

        // Data block
        let data = b"Hello, TAR!\n\0";
        tar[BLOCK_SIZE..BLOCK_SIZE + data.len()].copy_from_slice(data);

        // Two zero blocks mark end of archive
        // (already zeroed)

        tar
    }

    #[test]
    fn test_tar_reader() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = &reader.entries()[0];
        assert_eq!(entry.name, "test.txt");
        assert_eq!(entry.size, 13);
    }

    #[test]
    fn test_tar_extraction() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        let entry = reader.entries()[0].clone();
        let data = reader.extract_to_vec(&entry).expect("extract_to_vec");

        assert_eq!(data.len(), 13);
        assert_eq!(&data[..12], b"Hello, TAR!\n");
    }

    #[test]
    fn test_tar_extract_by_name() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        let data = reader
            .extract_by_name("test.txt")
            .expect("extract_by_name test.txt");
        assert!(data.is_some());
        assert_eq!(
            &data.expect("test.txt data present")[..12],
            b"Hello, TAR!\n"
        );

        let missing = reader
            .extract_by_name("nonexistent.txt")
            .expect("extract_by_name nonexistent.txt");
        assert!(missing.is_none());
    }

    #[test]
    fn test_tar_writer_single_file() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer
                .add_file("hello.txt", b"Hello, World!")
                .expect("add_file hello.txt");
            writer.finish().expect("writer finish");
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, 13);

        let data = reader.extract_to_vec(&entry).expect("extract_to_vec");
        assert_eq!(&data, b"Hello, World!");
    }

    #[test]
    fn test_tar_writer_multiple_files() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer
                .add_file("file1.txt", b"Content 1")
                .expect("add_file file1.txt");
            writer
                .add_file("file2.txt", b"Content 2 is longer")
                .expect("add_file file2.txt");
            writer
                .add_file("empty.txt", b"")
                .expect("add_file empty.txt");
            writer.finish().expect("writer finish");
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 3);
        assert_eq!(reader.entries()[0].name, "file1.txt");
        assert_eq!(reader.entries()[1].name, "file2.txt");
        assert_eq!(reader.entries()[2].name, "empty.txt");

        let data1 = reader
            .extract_by_name("file1.txt")
            .expect("extract file1.txt")
            .expect("file1.txt present");
        let data2 = reader
            .extract_by_name("file2.txt")
            .expect("extract file2.txt")
            .expect("file2.txt present");
        let data3 = reader
            .extract_by_name("empty.txt")
            .expect("extract empty.txt")
            .expect("empty.txt present");
        assert_eq!(&data1, b"Content 1");
        assert_eq!(&data2, b"Content 2 is longer");
        assert_eq!(&data3, b"");
    }

    #[test]
    fn test_tar_writer_directory() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_directory("mydir").expect("add_directory mydir");
            writer
                .add_file("mydir/file.txt", b"Inside directory")
                .expect("add_file mydir/file.txt");
            writer.finish().expect("writer finish");
        }

        // Read back
        let cursor = Cursor::new(output);
        let reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, "mydir/");
        assert!(reader.entries()[0].is_dir());
        assert_eq!(reader.entries()[1].name, "mydir/file.txt");
        assert!(reader.entries()[1].is_file());
    }

    #[test]
    fn test_tar_roundtrip() {
        // Create archive with various content
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_directory("docs").expect("add_directory docs");
            writer
                .add_file("docs/readme.txt", b"Read me first!")
                .expect("add_file docs/readme.txt");
            writer
                .add_file_with_mode("script.sh", b"#!/bin/sh\necho hello", 0o755)
                .expect("add_file_with_mode script.sh");
            writer.finish().expect("writer finish");
        }

        // Read and verify
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 3);

        // Verify directory
        assert!(entries[0].is_dir());
        assert_eq!(entries[0].name, "docs/");

        // Verify files
        let readme = reader
            .extract_by_name("docs/readme.txt")
            .expect("extract docs/readme.txt")
            .expect("docs/readme.txt present");
        assert_eq!(&readme, b"Read me first!");

        let script = reader
            .extract_by_name("script.sh")
            .expect("extract script.sh")
            .expect("script.sh present");
        assert_eq!(&script, b"#!/bin/sh\necho hello");

        // Check mode
        assert_eq!(entries[2].attributes.unix_mode, Some(0o755));
    }

    #[test]
    fn test_pax_record_format() {
        // Test the PAX record formatting
        // "path=test.txt\n" = 14 chars, plus "17 " = 17 total
        let record = TarWriter::<Vec<u8>>::format_pax_record("path", "test.txt");
        assert_eq!(record, "17 path=test.txt\n");
        assert_eq!(record.len(), 17);

        // Longer path
        let long_path = "a".repeat(200);
        let record = TarWriter::<Vec<u8>>::format_pax_record("path", &long_path);
        // "path=" + 200 chars + "\n" = 206, plus "209 " = 209... but 209 is 3 digits
        // Actually: 3 (digits) + 1 (space) + 4 (path) + 1 (=) + 200 (value) + 1 (\n) = 210
        assert!(record.starts_with("210 path="));
        assert_eq!(record.len(), 210);
    }

    #[test]
    fn test_parse_pax_data() {
        // Test single record first
        let pax_single = b"17 path=test.txt\n";
        let attrs_single = TarHeader::parse_pax_data(pax_single);
        assert_eq!(
            attrs_single.get("path").map(|s| s.as_str()),
            Some("test.txt")
        );

        // Test multiple records
        // "17 path=test.txt\n" = 17 bytes
        // "20 size=1234567890\n" = 20 bytes (2 + 1 + 4 + 1 + 10 + 1 = 19... wait that's 19)
        // Actually: "2" + "0" + " " + "size" + "=" + "1234567890" + "\n" = 2+1+4+1+10+1 = 19
        // So need length 19: "19 size=1234567890\n" = 19 bytes
        let pax_data = b"17 path=test.txt\n19 size=1234567890\n";
        let attrs = TarHeader::parse_pax_data(pax_data);

        assert_eq!(attrs.get("path").map(|s| s.as_str()), Some("test.txt"));
        assert_eq!(attrs.get("size").map(|s| s.as_str()), Some("1234567890"));
    }

    #[test]
    fn test_tar_pax_long_filename() {
        // Create a file with a very long filename (>100 chars)
        let long_name = "very_long_directory_name_that_exceeds_one_hundred_characters/\
            another_long_subdirectory_name/\
            and_finally_the_actual_filename.txt";

        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer
                .add_file(long_name, b"Content of file with long name")
                .expect("add_file long_name");
            writer.finish().expect("writer finish");
        }

        // Read back and verify
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        assert_eq!(reader.entries()[0].name, long_name);

        // Extract and verify content
        let data = reader
            .extract_to_vec(&reader.entries()[0].clone())
            .expect("extract_to_vec long_name");
        assert_eq!(&data, b"Content of file with long name");
    }

    #[test]
    fn test_tar_pax_roundtrip() {
        // Mix of normal and long filenames
        let short_name = "short.txt";
        let long_name = "this/is/a/very/long/path/name/that/definitely/exceeds/\
            the/one/hundred/character/limit/for/standard/tar/headers/file.txt";

        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer
                .add_file(short_name, b"Short content")
                .expect("add_file short_name");
            writer
                .add_file(long_name, b"Long path content")
                .expect("add_file long_name");
            writer.finish().expect("writer finish");
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, short_name);
        assert_eq!(reader.entries()[1].name, long_name);

        // Verify content
        let data1 = reader
            .extract_by_name(short_name)
            .expect("extract short_name")
            .expect("short_name present");
        let data2 = reader
            .extract_by_name(long_name)
            .expect("extract long_name")
            .expect("long_name present");
        assert_eq!(&data1, b"Short content");
        assert_eq!(&data2, b"Long path content");
    }

    // ---- Sparse file tests ----

    /// Append `data` plus zero-padding to reach the next 512-byte boundary.
    fn pad_to_block(dest: &mut Vec<u8>, data: &[u8]) {
        dest.extend_from_slice(data);
        let rem = dest.len() % BLOCK_SIZE;
        if rem != 0 {
            dest.extend(std::iter::repeat_n(0u8, BLOCK_SIZE - rem));
        }
    }

    #[test]
    fn test_tar_sparse_gnu_old_format() {
        // Sparse file: realsize=16_384, two runs
        //   (0, 100)  -> 100 bytes of 'A'
        //   (10_000, 500) -> 500 bytes of 'B'
        let realsize: u64 = 16_384;
        let runs: Vec<(u64, u64)> = vec![(0, 100), (10_000, 500)];

        let primary = sparse::build_gnu_sparse_primary("sparse.bin", realsize, &runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);

        // Data: 100 * 'A' || 500 * 'B', padded to 1024 bytes (2 blocks).
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(b'A', 100));
        data.extend(std::iter::repeat_n(b'B', 500));
        pad_to_block(&mut archive, &data);

        // Two zero blocks mark end of archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1, "expected one sparse entry");
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "sparse.bin");
        assert_eq!(entry.size, realsize, "entry.size must be realsize");

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..100].iter().all(|&b| b == b'A'), "run 0 content");
        assert!(
            out[100..10_000].iter().all(|&b| b == 0),
            "hole between runs must be zeros"
        );
        assert!(
            out[10_000..10_500].iter().all(|&b| b == b'B'),
            "run 1 content"
        );
        assert!(
            out[10_500..].iter().all(|&b| b == 0),
            "trailing hole must be zeros"
        );
    }

    #[test]
    fn test_tar_sparse_gnu_old_format_extended() {
        // Forces a continuation block: 5 runs total, only first 4 fit in
        // primary. realsize=1_048_576 (1 MiB).
        let realsize: u64 = 1_048_576;
        let primary_runs: Vec<(u64, u64)> =
            vec![(0, 200), (100_000, 300), (300_000, 150), (600_000, 400)];
        let cont_runs: Vec<(u64, u64)> = vec![(900_000, 250)];

        let primary = sparse::build_gnu_sparse_primary(
            "big_sparse.bin",
            realsize,
            &primary_runs,
            true, // isextended
        );
        let cont = sparse::build_gnu_sparse_continuation(&cont_runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);
        archive.extend_from_slice(&cont);

        // Concatenated run data (sum = 1300 bytes) padded to 1536 (3 blocks).
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(b'A', 200));
        data.extend(std::iter::repeat_n(b'B', 300));
        data.extend(std::iter::repeat_n(b'C', 150));
        data.extend(std::iter::repeat_n(b'D', 400));
        data.extend(std::iter::repeat_n(b'E', 250));
        pad_to_block(&mut archive, &data);

        // End-of-archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "big_sparse.bin");
        assert_eq!(entry.size, realsize);

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..200].iter().all(|&b| b == b'A'));
        assert!(out[200..100_000].iter().all(|&b| b == 0));
        assert!(out[100_000..100_300].iter().all(|&b| b == b'B'));
        assert!(out[100_300..300_000].iter().all(|&b| b == 0));
        assert!(out[300_000..300_150].iter().all(|&b| b == b'C'));
        assert!(out[600_000..600_400].iter().all(|&b| b == b'D'));
        assert!(out[900_000..900_250].iter().all(|&b| b == b'E'));
        assert!(out[900_250..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_tar_sparse_pax_format() {
        // PAX 0.1 sparse: extended header with GNU.sparse.{name,map,realsize},
        // followed by a regular '0' data entry whose name is the GNU-tar
        // sentinel `./GNUSparseFile.0/sparse.dat`. The reader must shadow
        // the sentinel name with `GNU.sparse.name=sparse.dat`.
        // Payload: 100 bytes of 'X' at offset 0, 200 bytes of 'Y' at 5000.
        let realsize: u64 = 10_000;
        let runs: Vec<(u64, u64)> = vec![(0, 100), (5_000, 200)];
        let stored: u64 = runs.iter().map(|(_, n)| *n).sum();

        // Build PAX payload.
        let mk_record =
            |k: &str, v: &str| -> String { TarWriter::<Vec<u8>>::format_pax_record(k, v) };
        let mut pax_payload = String::new();
        pax_payload.push_str(&mk_record("GNU.sparse.name", "sparse.dat"));
        pax_payload.push_str(&mk_record("GNU.sparse.realsize", &realsize.to_string()));
        pax_payload.push_str(&mk_record("GNU.sparse.map", "0,100,5000,200"));
        pax_payload.push_str(&mk_record("size", &stored.to_string()));

        let pax_bytes = pax_payload.as_bytes();
        let pax_hdr = sparse::build_pax_header_block(b'x', pax_bytes.len() as u64);

        // Build the data-entry header: typeflag '0', size = stored size,
        // name = the GNU-tar `./GNUSparseFile.XXXX/<file>` sentinel. A
        // real-world GNU-tar archive places this dummy name on the data
        // header; `GNU.sparse.name` holds the canonical name and the
        // reader must prefer it.
        let dummy_name = "./GNUSparseFile.12345/sparse.dat";
        let mut data_hdr = TarHeader::new_file(dummy_name, stored, 0o644);
        data_hdr.typeflag = b'0';
        let data_hdr_block = data_hdr.to_block().expect("data_hdr.to_block");

        let mut archive = Vec::new();
        archive.extend_from_slice(&pax_hdr);
        pad_to_block(&mut archive, pax_bytes);
        archive.extend_from_slice(&data_hdr_block);

        // Run payload.
        let mut payload = Vec::new();
        payload.extend(std::iter::repeat_n(b'X', 100));
        payload.extend(std::iter::repeat_n(b'Y', 200));
        pad_to_block(&mut archive, &payload);

        // End-of-archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(
            entry.name, "sparse.dat",
            "GNU.sparse.name must shadow the dummy GNUSparseFile path"
        );
        assert_eq!(
            entry.size, realsize,
            "entry.size must reflect GNU.sparse.realsize, not stored size"
        );

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..100].iter().all(|&b| b == b'X'));
        assert!(out[100..5_000].iter().all(|&b| b == 0));
        assert!(out[5_000..5_200].iter().all(|&b| b == b'Y'));
        assert!(out[5_200..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_tar_sparse_malformed_map() {
        // Two runs that overlap: (0,200) and (100,50). Parser should
        // reject at `validate()` time with InvalidHeader.
        let realsize: u64 = 1_000;
        let runs: Vec<(u64, u64)> = vec![(0, 200), (100, 50)];
        let primary = sparse::build_gnu_sparse_primary("bad.bin", realsize, &runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);
        // A block of filler data (not that we'd read it — validate() errors first).
        archive.extend_from_slice(&[0u8; BLOCK_SIZE]);

        let cursor = Cursor::new(archive);
        let err = TarReader::new(cursor)
            .err()
            .expect("overlapping sparse map must be rejected");
        match err {
            OxiArcError::InvalidHeader { .. } => {}
            other => panic!("unexpected error variant: {:?}", other),
        }
    }
}
