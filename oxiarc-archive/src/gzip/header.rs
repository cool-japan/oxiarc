//! GZIP header parsing and writing.

use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateWrapper, TrailingPolicy, WrappedInflate, deflate};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// GZIP magic bytes.
pub const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];

/// GZIP compression method: DEFLATE.
pub const CM_DEFLATE: u8 = 8;

/// GZIP header flags.
#[allow(dead_code)]
pub mod flags {
    /// Text file.
    pub const FTEXT: u8 = 0x01;
    /// Header CRC present.
    pub const FHCRC: u8 = 0x02;
    /// Extra field present.
    pub const FEXTRA: u8 = 0x04;
    /// Original filename present.
    pub const FNAME: u8 = 0x08;
    /// Comment present.
    pub const FCOMMENT: u8 = 0x10;
}

/// GZIP file header.
#[derive(Debug, Clone)]
pub struct GzipHeader {
    /// Compression method (should be 8 for DEFLATE).
    pub method: u8,
    /// Flags.
    pub flags: u8,
    /// Modification time (Unix timestamp).
    pub mtime: u32,
    /// Extra flags.
    pub xfl: u8,
    /// Operating system.
    pub os: u8,
    /// Original filename (if FNAME flag set).
    pub filename: Option<String>,
    /// Comment (if FCOMMENT flag set).
    pub comment: Option<String>,
    /// Header CRC16 (if FHCRC flag set).
    pub header_crc: Option<u16>,
}

impl Default for GzipHeader {
    fn default() -> Self {
        Self {
            method: CM_DEFLATE,
            flags: 0,
            mtime: 0,
            xfl: 0,
            os: 255, // Unknown OS
            filename: None,
            comment: None,
            header_crc: None,
        }
    }
}

impl GzipHeader {
    /// Create a new GZIP header with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a header with filename.
    pub fn with_filename(filename: &str) -> Self {
        Self {
            flags: flags::FNAME,
            filename: Some(filename.to_string()),
            ..Self::default()
        }
    }

    /// Set the modification time to now.
    #[must_use]
    pub fn with_mtime_now(mut self) -> Self {
        self.mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        self
    }

    /// Write the header to a writer.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        // Magic
        writer.write_all(&GZIP_MAGIC)?;

        // Method
        writer.write_all(&[self.method])?;

        // Flags
        writer.write_all(&[self.flags])?;

        // Modification time
        writer.write_all(&self.mtime.to_le_bytes())?;

        // XFL and OS
        writer.write_all(&[self.xfl, self.os])?;

        // Filename
        if self.flags & flags::FNAME != 0 {
            if let Some(ref filename) = self.filename {
                writer.write_all(filename.as_bytes())?;
                writer.write_all(&[0])?; // Null terminator
            }
        }

        // Comment
        if self.flags & flags::FCOMMENT != 0 {
            if let Some(ref comment) = self.comment {
                writer.write_all(comment.as_bytes())?;
                writer.write_all(&[0])?; // Null terminator
            }
        }

        Ok(())
    }

    /// Read a GZIP header from a reader.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 10];
        reader.read_exact(&mut buf)?;

        // Check magic
        if buf[0..2] != GZIP_MAGIC {
            return Err(OxiArcError::invalid_magic(
                GZIP_MAGIC.to_vec(),
                buf[0..2].to_vec(),
            ));
        }

        let method = buf[2];
        if method != CM_DEFLATE {
            return Err(OxiArcError::unsupported_method(format!(
                "GZIP method {}",
                method
            )));
        }

        let flags = buf[3];
        let mtime = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let xfl = buf[8];
        let os = buf[9];

        // Read optional fields
        let mut filename = None;
        let mut comment = None;
        let mut header_crc = None;

        // Extra field
        if flags & flags::FEXTRA != 0 {
            let mut xlen_buf = [0u8; 2];
            reader.read_exact(&mut xlen_buf)?;
            let xlen = u16::from_le_bytes(xlen_buf) as usize;
            let mut extra = vec![0u8; xlen];
            reader.read_exact(&mut extra)?;
        }

        // Filename
        if flags & flags::FNAME != 0 {
            filename = Some(Self::read_null_terminated(reader)?);
        }

        // Comment
        if flags & flags::FCOMMENT != 0 {
            comment = Some(Self::read_null_terminated(reader)?);
        }

        // Header CRC
        if flags & flags::FHCRC != 0 {
            let mut crc_buf = [0u8; 2];
            reader.read_exact(&mut crc_buf)?;
            header_crc = Some(u16::from_le_bytes(crc_buf));
        }

        Ok(Self {
            method,
            flags,
            mtime,
            xfl,
            os,
            filename,
            comment,
            header_crc,
        })
    }

    /// Read a null-terminated string.
    fn read_null_terminated<R: Read>(reader: &mut R) -> Result<String> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1];

        loop {
            reader.read_exact(&mut buf)?;
            if buf[0] == 0 {
                break;
            }
            bytes.push(buf[0]);
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Staging buffer size for [`GzipReader::decompress`], both directions.
///
/// Matches `oxiarc_deflate::InflateReader`'s own 64 KiB staging buffer.
const GZIP_STAGE: usize = 64 * 1024;

/// A [`Read`] adapter that copies every byte it hands out into a sink.
///
/// [`GzipReader::new`] parses the first member's header eagerly (so
/// [`GzipReader::header`] can be read before any decoding, and so a
/// non-gzip input fails at construction), but the streaming decoder used by
/// [`GzipReader::decompress`] re-parses the framing itself. Those header
/// bytes are gone from the underlying reader by then, so they are captured
/// here and replayed in front of it.
struct TeeReader<'a, R: Read> {
    inner: &'a mut R,
    sink: &'a mut Vec<u8>,
}

impl<R: Read> Read for TeeReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let filled = self.inner.read(buf)?;
        if let Some(fresh) = buf.get(..filled) {
            self.sink.extend_from_slice(fresh);
        }
        Ok(filled)
    }
}

/// GZIP reader that decompresses data.
///
/// Concatenated multi-member streams (RFC 1952 §2.2 — `cat a.gz b.gz`,
/// `pigz`, `bgzip`, rsyncable gzips, and oxiarc's own
/// `compress_gzip_parallel` output) decode to the concatenation of every
/// member's contents. Trailing `0x00` padding after the last member is
/// tolerated, as the `gzip` CLI and CPython's `gzip` module do.
pub struct GzipReader<R: Read> {
    /// Underlying reader, positioned just past the first member's header.
    reader: R,
    /// Parsed header of the first member.
    header: GzipHeader,
    /// The first member's header bytes verbatim, replayed into the
    /// streaming decoder by [`GzipReader::decompress`].
    header_bytes: Vec<u8>,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
    /// Optional cap on the total decompressed size, enforced *during*
    /// decoding. See [`GzipReader::with_max_output`].
    max_output: Option<u64>,
}

impl<R: Read> GzipReader<R> {
    /// Create a new GZIP reader.
    ///
    /// # Errors
    ///
    /// [`OxiArcError::InvalidMagic`] if the input does not start with the
    /// gzip magic, [`OxiArcError::UnsupportedMethod`] for a compression
    /// method other than DEFLATE, and [`OxiArcError::Io`] for a header that
    /// ends early.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut header_bytes = Vec::new();
        let header = {
            let mut tee = TeeReader {
                inner: &mut reader,
                sink: &mut header_bytes,
            };
            GzipHeader::read(&mut tee)?
        };
        Ok(Self {
            reader,
            header,
            header_bytes,
            progress: None,
            max_output: None,
        })
    }

    /// Attach a progress callback handle.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Refuse to decode more than `limit` bytes in total, across every
    /// member of the stream.
    ///
    /// The cap is enforced *inside* a DEFLATE block, so a decompression
    /// bomb is rejected with [`OxiArcError::MemoryBudgetExceeded`] before
    /// its expansion is ever allocated — unlike a post-decode size check.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_archive::{GzipReader, gzip};
    /// use oxiarc_core::error::OxiArcError;
    ///
    /// let bomb = gzip::compress(&vec![0u8; 1 << 20], 9).expect("compress");
    /// let mut reader = GzipReader::new(std::io::Cursor::new(bomb))
    ///     .expect("header")
    ///     .with_max_output(64 * 1024);
    /// assert!(matches!(
    ///     reader.decompress(),
    ///     Err(OxiArcError::MemoryBudgetExceeded { .. })
    /// ));
    /// ```
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.max_output = Some(limit);
        self
    }

    /// Get the header of the stream's first member.
    pub fn header(&self) -> &GzipHeader {
        &self.header
    }

    /// Decompress the data.
    ///
    /// Streams the input through `oxiarc_deflate`'s resumable gzip core
    /// rather than buffering the whole compressed file, and decodes **every**
    /// member of a concatenated stream (RFC 1952 §2.2), concatenating their
    /// contents. Each member's CRC-32 and `ISIZE` trailer is verified, as is
    /// its `FHCRC` header checksum when present.
    ///
    /// # Errors
    ///
    /// [`OxiArcError::CrcMismatch`] for a bad CRC-32 or `FHCRC`,
    /// [`OxiArcError::CorruptedData`] for an `ISIZE` mismatch or a malformed
    /// DEFLATE stream, [`OxiArcError::UnexpectedEof`] for a truncated
    /// member, [`OxiArcError::InvalidMagic`] for non-zero trailing garbage,
    /// and [`OxiArcError::MemoryBudgetExceeded`] when
    /// [`GzipReader::with_max_output`] is exceeded.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_archive::{GzipReader, gzip};
    ///
    /// // Two independently-compressed members, concatenated.
    /// let mut stream = gzip::compress(b"first ", 6).expect("compress");
    /// stream.extend_from_slice(&gzip::compress(b"second", 6).expect("compress"));
    ///
    /// let mut reader = GzipReader::new(std::io::Cursor::new(stream)).expect("header");
    /// assert_eq!(reader.decompress().expect("decompress"), b"first second");
    /// ```
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        // Emit entry start progress
        let stream_name = self
            .header
            .filename
            .as_deref()
            .unwrap_or("<stream>")
            .to_string();
        if let Some(ref handle) = self.progress {
            handle.on_entry(&stream_name, 0);
        }

        let mut core = WrappedInflate::new(InflateWrapper::Gzip)
            .multi_member(true)
            .trailing_policy(TrailingPolicy::AllowZeros);
        if let Some(limit) = self.max_output {
            core = core.with_max_output(limit);
        }

        // The first member's header was consumed by `new`; replay it in
        // front of whatever is left of the reader.
        let head = std::mem::take(&mut self.header_bytes);
        let mut source = std::io::Cursor::new(head).chain(&mut self.reader);

        let mut in_buf = vec![0u8; GZIP_STAGE];
        let mut out_buf = vec![0u8; GZIP_STAGE];
        let mut output = Vec::new();
        let mut filled = 0usize;
        let mut pos = 0usize;
        let mut eof = false;
        let mut idle = 0u8;

        loop {
            if pos == filled && !eof {
                pos = 0;
                filled = 0;
                loop {
                    match source.read(&mut in_buf) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(read) => {
                            filled = read;
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
            }

            // At source EOF the whole stream is in hand, so `Finish`: a
            // member that ends early is an error rather than a request for
            // more input.
            let flush = if eof {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = core.inflate(
                in_buf.get(pos..filled).unwrap_or_default(),
                &mut out_buf,
                flush,
            )?;
            pos += progress.consumed;
            if let Some(fresh) = out_buf.get(..progress.produced) {
                output.extend_from_slice(fresh);
            }
            if progress.status == InflateStatus::StreamEnd {
                break;
            }
            if progress.consumed == 0 && progress.produced == 0 {
                idle += 1;
                if idle >= 2 {
                    return Err(OxiArcError::corrupted(
                        output.len() as u64,
                        "gzip decoder made no progress",
                    ));
                }
            } else {
                idle = 0;
            }
        }

        // Emit completion progress
        if let Some(ref handle) = self.progress {
            handle.on_progress(output.len() as u64, None);
            handle.on_finish();
        }

        Ok(output)
    }
}

/// GZIP writer that compresses data.
pub struct GzipWriter {
    /// Header to use.
    header: GzipHeader,
    /// Compression level (0-9).
    level: u8,
}

impl GzipWriter {
    /// Create a new GZIP writer with default settings.
    pub fn new() -> Self {
        Self {
            header: GzipHeader::new(),
            level: 6,
        }
    }

    /// Create a writer with a specific header.
    pub fn with_header(header: GzipHeader) -> Self {
        Self { header, level: 6 }
    }

    /// Set compression level (0-9).
    #[must_use]
    pub fn level(mut self, level: u8) -> Self {
        self.level = level.min(9);
        // Set XFL based on level
        self.header.xfl = match self.level {
            0..=1 => 4, // Fastest
            9 => 2,     // Maximum compression
            _ => 0,     // Default
        };
        self
    }

    /// Compress data and write to a writer.
    pub fn compress<W: Write>(&self, data: &[u8], writer: &mut W) -> Result<()> {
        // Write header
        self.header.write(writer)?;

        // Compress with DEFLATE
        let compressed = deflate(data, self.level)?;
        writer.write_all(&compressed)?;

        // Write trailer (CRC32 + ISIZE)
        let crc = Crc32::compute(data);
        writer.write_all(&crc.to_le_bytes())?;

        let isize = (data.len() as u32).to_le_bytes();
        writer.write_all(&isize)?;

        Ok(())
    }

    /// Compress data and return as Vec.
    pub fn compress_to_vec(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.compress(data, &mut output)?;
        Ok(output)
    }
}

impl Default for GzipWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Compress data to GZIP format.
pub fn compress(data: &[u8], level: u8) -> Result<Vec<u8>> {
    GzipWriter::new().level(level).compress_to_vec(data)
}

/// Compress data to GZIP format with filename.
pub fn compress_with_filename(data: &[u8], filename: &str, level: u8) -> Result<Vec<u8>> {
    let header = GzipHeader::with_filename(filename).with_mtime_now();
    GzipWriter::with_header(header)
        .level(level)
        .compress_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_gzip_magic() {
        assert_eq!(GZIP_MAGIC, [0x1F, 0x8B]);
    }

    #[test]
    fn test_gzip_header_default() {
        let header = GzipHeader::new();
        assert_eq!(header.method, CM_DEFLATE);
        assert_eq!(header.flags, 0);
    }

    #[test]
    fn test_gzip_header_with_filename() {
        let header = GzipHeader::with_filename("test.txt");
        assert_eq!(header.flags & flags::FNAME, flags::FNAME);
        assert_eq!(header.filename, Some("test.txt".to_string()));
    }

    #[test]
    fn test_gzip_roundtrip() {
        let original = b"Hello, GZIP World! This is a test of compression.";

        // Compress
        let compressed = compress(original, 6).expect("compress");

        // Decompress
        let mut reader = GzipReader::new(Cursor::new(compressed)).expect("GzipReader::new");
        let decompressed = reader.decompress().expect("decompress");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_roundtrip_with_filename() {
        let original = b"Test data with filename";

        // Compress with filename
        let compressed =
            compress_with_filename(original, "data.txt", 6).expect("compress_with_filename");

        // Decompress and check filename
        let mut reader = GzipReader::new(Cursor::new(compressed)).expect("GzipReader::new");
        assert_eq!(reader.header().filename, Some("data.txt".to_string()));

        let decompressed = reader.decompress().expect("decompress");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_empty() {
        let original: &[u8] = b"";
        let compressed = compress(original, 6).expect("compress empty");

        let mut reader = GzipReader::new(Cursor::new(compressed)).expect("GzipReader::new");
        let decompressed = reader.decompress().expect("decompress empty");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_repeated() {
        let original = vec![b'A'; 10000];
        let compressed = compress(&original, 9).expect("compress repeated");

        // Should compress well
        assert!(compressed.len() < original.len() / 10);

        let mut reader = GzipReader::new(Cursor::new(compressed)).expect("GzipReader::new");
        let decompressed = reader.decompress().expect("decompress repeated");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_progress() {
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

        let original = b"Hello, progress world!";
        let compressed =
            compress_with_filename(original, "hello.txt", 6).expect("compress_with_filename");

        let mut reader = GzipReader::new(Cursor::new(compressed))
            .expect("GzipReader::new")
            .with_progress(handle);
        let decompressed = reader.decompress().expect("decompress");

        assert_eq!(&decompressed, original);

        // on_entry should have been called once with the filename
        {
            let entries = sink.entries.lock().expect("entries lock");
            assert_eq!(entries.len(), 1, "expected on_entry called once");
            assert_eq!(entries[0], "hello.txt");
        }
        assert!(
            *sink.finish_called.lock().expect("finish_called lock"),
            "on_finish not called"
        );
        assert_eq!(*sink.progress_calls.lock().expect("progress_calls lock"), 1);
    }

    /// FINALGATE F1 regression: a valid RFC 1952 §2.2 concatenated stream
    /// (`cat a.gz b.gz`, `pigz`, `bgzip`) must decode to the concatenation
    /// of every member. Before this fix `decompress` read the whole file,
    /// treated its *last* 8 bytes as the only trailer and inflated
    /// everything before them as one member, so every multi-member `.gz`
    /// failed with a spurious `CRC mismatch`.
    #[test]
    fn test_gzip_multi_member_concatenated() {
        let mut stream = compress(b"first member;", 6).expect("compress first");
        stream.extend_from_slice(&compress(b"second member;", 6).expect("compress second"));
        stream.extend_from_slice(&compress(b"third member", 9).expect("compress third"));

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        let decompressed = reader.decompress().expect("decompress multi-member");
        assert_eq!(decompressed, b"first member;second member;third member");
    }

    /// The first member's header is still what `header()` reports, even
    /// though later members carry their own.
    #[test]
    fn test_gzip_multi_member_reports_first_header() {
        let mut stream =
            compress_with_filename(b"alpha", "a.txt", 6).expect("compress_with_filename");
        stream.extend_from_slice(
            &compress_with_filename(b"beta", "b.txt", 6).expect("compress_with_filename"),
        );

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        assert_eq!(reader.header().filename, Some("a.txt".to_string()));
        assert_eq!(reader.decompress().expect("decompress"), b"alphabeta");
    }

    /// Trailing NUL padding after the last member is tolerated (tape blocks),
    /// exactly as the `gzip` CLI and CPython's `gzip` module do.
    #[test]
    fn test_gzip_trailing_zero_padding_is_tolerated() {
        let mut stream = compress(b"padded payload", 6).expect("compress");
        stream.extend_from_slice(&[0u8; 512]);

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        assert_eq!(reader.decompress().expect("decompress"), b"padded payload");
    }

    /// Non-zero trailing garbage is still rejected.
    #[test]
    fn test_gzip_trailing_garbage_is_rejected() {
        let mut stream = compress(b"payload", 6).expect("compress");
        stream.extend_from_slice(b"garbage");

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        assert!(
            reader.decompress().is_err(),
            "trailing garbage must be rejected"
        );
    }

    /// A corrupt payload still fails with `CrcMismatch`, not silently.
    #[test]
    fn test_gzip_corrupt_payload_is_a_crc_mismatch() {
        let original = vec![b'z'; 4096];
        let mut stream = compress(&original, 6).expect("compress");
        let last = stream.len() - 9;
        stream[last] ^= 0x01;

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        let error = reader.decompress().expect_err("corruption must be caught");
        assert!(
            matches!(
                error,
                OxiArcError::CrcMismatch { .. } | OxiArcError::CorruptedData { .. }
            ),
            "unexpected error: {error:?}"
        );
    }

    /// A stream cut short mid-member is an error, never a short read.
    #[test]
    fn test_gzip_truncated_member_is_an_error() {
        let stream = compress(&vec![7u8; 100_000], 6).expect("compress");
        for cut in [10, 20, stream.len() / 2, stream.len() - 1] {
            let mut reader =
                GzipReader::new(Cursor::new(stream[..cut].to_vec())).expect("GzipReader::new");
            assert!(
                reader.decompress().is_err(),
                "truncation at {cut} must be rejected"
            );
        }
    }

    /// A truncated *second* member is an error too: the multi-member loop
    /// must not stop silently at a member boundary.
    #[test]
    fn test_gzip_truncated_second_member_is_an_error() {
        let mut stream = compress(b"complete first member", 6).expect("compress");
        let second = compress(&vec![3u8; 50_000], 6).expect("compress");
        stream.extend_from_slice(&second[..second.len() - 5]);

        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        assert!(
            reader.decompress().is_err(),
            "a truncated later member must be rejected"
        );
    }

    /// `with_max_output` refuses a bomb *during* decoding.
    #[test]
    fn test_gzip_max_output_rejects_a_bomb() {
        let bomb = compress(&vec![0u8; 4 << 20], 9).expect("compress bomb");
        let mut reader = GzipReader::new(Cursor::new(bomb))
            .expect("GzipReader::new")
            .with_max_output(64 * 1024);
        let error = reader.decompress().expect_err("the cap must be reported");
        assert!(
            matches!(error, OxiArcError::MemoryBudgetExceeded { .. }),
            "unexpected error: {error:?}"
        );
    }

    /// The budget spans the whole stream, not one member: three members of
    /// 40 KB each must trip a 64 KB cap.
    #[test]
    fn test_gzip_max_output_is_cumulative_across_members() {
        let member = vec![1u8; 40 * 1024];
        let mut stream = compress(&member, 6).expect("compress");
        stream.extend_from_slice(&compress(&member, 6).expect("compress"));
        stream.extend_from_slice(&compress(&member, 6).expect("compress"));

        let mut reader = GzipReader::new(Cursor::new(stream))
            .expect("GzipReader::new")
            .with_max_output(64 * 1024);
        let error = reader.decompress().expect_err("the cap must be reported");
        assert!(
            matches!(error, OxiArcError::MemoryBudgetExceeded { .. }),
            "unexpected error: {error:?}"
        );
    }

    /// An in-budget payload is unaffected by the cap (no false positives).
    #[test]
    fn test_gzip_max_output_allows_an_in_budget_payload() {
        let payload = vec![9u8; 32 * 1024];
        let stream = compress(&payload, 6).expect("compress");
        let mut reader = GzipReader::new(Cursor::new(stream))
            .expect("GzipReader::new")
            .with_max_output(64 * 1024);
        assert_eq!(reader.decompress().expect("decompress"), payload);
    }

    /// A member whose `FHCRC` header checksum is corrupt is rejected, as
    /// `gzip -d` rejects it. (0.4.1 skipped the field entirely.)
    #[test]
    fn test_gzip_bad_header_crc_is_rejected() {
        let payload = b"header-crc guarded";
        let inner = compress(payload, 6).expect("compress");
        // Rebuild the stream with FLG.FHCRC set and a correct CRC16, then
        // flip it.
        let mut header = inner[..10].to_vec();
        header[3] |= flags::FHCRC;
        let crc16 = (Crc32::compute(&header) & 0xFFFF) as u16;
        let mut good = header.clone();
        good.extend_from_slice(&crc16.to_le_bytes());
        good.extend_from_slice(&inner[10..]);

        let mut reader = GzipReader::new(Cursor::new(good.clone())).expect("GzipReader::new");
        assert_eq!(reader.decompress().expect("decompress"), payload);

        let mut bad = good;
        bad[10] ^= 0xFF;
        let mut reader = GzipReader::new(Cursor::new(bad)).expect("GzipReader::new");
        let error = reader.decompress().expect_err("a bad FHCRC must be caught");
        assert!(
            matches!(error, OxiArcError::CrcMismatch { .. }),
            "unexpected error: {error:?}"
        );
    }

    /// The streaming decoder must agree with `oxiarc_deflate`'s own
    /// one-shot gzip path on every shape tested here.
    #[test]
    fn test_gzip_reader_agrees_with_deflate_one_shot() {
        let mut stream = compress(b"agreement, member one. ", 6).expect("compress");
        stream.extend_from_slice(&compress(&vec![42u8; 70_000], 9).expect("compress"));
        stream.extend_from_slice(&compress(b"", 6).expect("compress"));

        let mut reader = GzipReader::new(Cursor::new(stream.clone())).expect("GzipReader::new");
        let streamed = reader.decompress().expect("streaming decompress");
        let one_shot = oxiarc_deflate::gzip_decompress(&stream).expect("one-shot decompress");
        assert_eq!(streamed, one_shot);
    }

    /// A payload larger than the 64 KiB staging buffer exercises the refill
    /// loop in both directions.
    #[test]
    fn test_gzip_payload_larger_than_the_staging_buffer() {
        // Deliberately incompressible (a xorshift PRNG), so the *compressed*
        // stream is bigger than one staging buffer and the refill loop runs.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let payload: Vec<u8> = (0..400_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let stream = compress(&payload, 6).expect("compress");
        assert!(stream.len() > GZIP_STAGE, "fixture must span refills");
        let mut reader = GzipReader::new(Cursor::new(stream)).expect("GzipReader::new");
        assert_eq!(reader.decompress().expect("decompress"), payload);
    }
}
