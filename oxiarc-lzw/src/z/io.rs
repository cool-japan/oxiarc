//! `Read` and `Write` adapters for the UNIX `compress(1)` (`.Z`) format.

use std::io::{self, Read, Write};

use crate::error::{LzwError, Result};

use super::decode::ZDecoder;
use super::encode::ZEncoder;
use super::header::ZHeader;
use super::sink::VecZSink;

/// Compressed bytes pulled from the inner reader per decode step.
///
/// Bounds the working set: the reader holds this many compressed bytes plus
/// whatever they expand to, never the whole stream.
const READ_CHUNK: usize = 16 * 1024;

/// Uncompressed bytes buffered per `write` batch before they are handed to
/// the inner writer.
const WRITE_FLUSH: usize = 32 * 1024;

/// Streaming decoder for `.Z` data, implementing [`Read`].
///
/// This is a genuinely incremental reader: it pulls at most `16 KiB` of
/// compressed input at a time, decodes that much, and serves it, so peak
/// memory is bounded by one chunk's expansion rather than by the size of the
/// stream. The three-byte header is parsed lazily on the first `read`.
///
/// # How large is "one chunk's expansion"?
///
/// Unbounded, unless you bound it. 16 KiB of 16-bit codes is 8 192 codes,
/// and a code near the top of a full table can expand to tens of kilobytes,
/// so a *crafted* stream can make one step allocate hundreds of megabytes
/// even though the reader never holds the whole compressed body. For
/// anything that did not come from a trusted source, call
/// [`ZReader::with_max_output`]: the decoded buffer can then never exceed
/// the remaining budget. With it set, the whole reader's working set is the
/// code table (about 193 KiB at 16 bits) plus the 16 KiB input chunk plus at
/// most `max_output` bytes; `tests/z_memory.rs` measures it.
///
/// # Example
///
/// ```rust
/// use std::io::Read;
/// use oxiarc_lzw::z::{ZReader, compress};
///
/// let original = b"a .Z stream read through std::io::Read".repeat(20);
/// let stream = compress(&original, 16).expect("compress");
///
/// let mut out = Vec::new();
/// ZReader::new(&stream[..])
///     .read_to_end(&mut out)
///     .expect("decode");
/// assert_eq!(out, original);
/// ```
#[derive(Debug)]
pub struct ZReader<R: Read> {
    inner: R,
    decoder: Option<ZDecoder>,
    /// Bytes read for, but not yet consumed by, header parsing.
    header_bytes: Vec<u8>,
    buffer: Vec<u8>,
    position: usize,
    input: Vec<u8>,
    input_done: bool,
    produced: u64,
    max_output: Option<u64>,
}

impl<R: Read> ZReader<R> {
    /// Wrap `reader`, which must yield a complete `.Z` stream starting at
    /// its magic bytes.
    ///
    /// No I/O happens here; a malformed header surfaces from the first
    /// [`Read::read`] call.
    pub fn new(reader: R) -> Self {
        Self {
            inner: reader,
            decoder: None,
            header_bytes: Vec::with_capacity(ZHeader::LEN),
            buffer: Vec::new(),
            position: 0,
            input: vec![0u8; READ_CHUNK],
            input_done: false,
            produced: 0,
            max_output: None,
        }
    }

    /// Refuse to produce more than `max_output` bytes.
    ///
    /// The bound is enforced *during* decoding — as soon as a decoded chunk
    /// crosses it — so a `.Z` bomb never fully expands. A stream that
    /// exceeds it fails with [`LzwError::OutputLimitExceeded`] wrapped in an
    /// [`io::Error`].
    #[must_use]
    pub fn with_max_output(mut self, max_output: u64) -> Self {
        self.max_output = Some(max_output);
        self
    }

    /// The parsed header, available once the first `read` has succeeded.
    #[must_use]
    pub fn header(&self) -> Option<ZHeader> {
        self.decoder.as_ref().map(ZDecoder::header)
    }

    /// Consume the adapter and return the inner reader.
    ///
    /// The reader is **not** positioned at the end of the `.Z` stream: this
    /// adapter pulls up to [`READ_CHUNK`] bytes at a time, so it will
    /// normally have consumed some bytes past the last code. Anything still
    /// buffered — decoded output not yet handed out, and compressed input
    /// not yet decoded — is dropped. Use this to recover ownership, not to
    /// continue reading a container that has more data after the `.Z`
    /// member.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read the three header bytes and build the decoder.
    fn parse_header(&mut self) -> io::Result<()> {
        while self.header_bytes.len() < ZHeader::LEN {
            let mut byte = [0u8; ZHeader::LEN];
            let want = ZHeader::LEN - self.header_bytes.len();
            let read = self.inner.read(&mut byte[..want])?;
            if read == 0 {
                return Err(to_io(LzwError::ZTruncatedHeader {
                    len: self.header_bytes.len(),
                }));
            }
            self.header_bytes.extend_from_slice(&byte[..read]);
        }
        let header = ZHeader::parse(&self.header_bytes).map_err(to_io)?;
        self.decoder = Some(ZDecoder::new(header));
        Ok(())
    }

    /// Decode one chunk into `buffer`; returns `false` at end of stream.
    fn fill(&mut self) -> io::Result<bool> {
        if self.decoder.is_none() {
            self.parse_header()?;
        }
        // Destructure so the decoder and the input buffer can be borrowed
        // at the same time.
        let Self {
            inner,
            decoder,
            buffer,
            position,
            input,
            input_done,
            produced,
            max_output,
            ..
        } = self;
        let Some(decoder) = decoder.as_mut() else {
            return Ok(false);
        };

        buffer.clear();
        *position = 0;
        while buffer.is_empty() {
            if *input_done {
                return Ok(false);
            }
            let read = inner.read(input)?;
            if read == 0 {
                *input_done = true;
            }
            let limit = match *max_output {
                Some(max) => usize::try_from(max.saturating_sub(*produced)).unwrap_or(usize::MAX),
                None => usize::MAX,
            };
            let outcome = {
                let mut sink = VecZSink::new(buffer, limit);
                decoder.decode(&input[..read], &mut sink)
            };
            *produced += buffer.len() as u64;
            outcome.map_err(|error| match error {
                LzwError::OutputLimitExceeded { .. } => to_io(LzwError::OutputLimitExceeded {
                    limit: usize::try_from(max_output.unwrap_or_default()).unwrap_or(usize::MAX),
                }),
                other => to_io(other),
            })?;
        }
        Ok(true)
    }
}

impl<R: Read> Read for ZReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position >= self.buffer.len() {
            if !self.fill()? {
                return Ok(0);
            }
        }
        let available = self.buffer.len() - self.position;
        let take = available.min(out.len());
        out[..take].copy_from_slice(&self.buffer[self.position..self.position + take]);
        self.position += take;
        Ok(take)
    }
}

/// Streaming encoder for `.Z` data, implementing [`Write`].
///
/// The header is written on the first flush of encoded output (or by
/// [`ZWriter::finish`] for an empty stream), and code groups are handed to
/// the inner writer in batches of about 32 KiB, so the staging buffer stays
/// that size however large a slice is passed to a single
/// [`Write::write`] call.
///
/// **You must call [`ZWriter::finish`]**: the final code and the trailing
/// partial group are emitted there. Dropping the writer without finishing
/// leaves a truncated stream — and because `.Z` has no end-of-information
/// code, a reader cannot tell: such a stream decodes as a **successful short
/// read** (`Ok`, with a prefix of the data), never as an error. There is no
/// `Drop` impl that rescues this, so a lost `finish` is silent data loss.
///
/// # Failure of the inner writer poisons the adapter
///
/// For the same reason, an [`io::Error`] from the inner writer is *sticky*:
/// once a batch has failed to reach it, the stream has a hole in the middle
/// that no reader can detect, so every later `write`, `flush` and `finish`
/// fails with `ErrorKind::Other` instead of quietly completing a corrupt
/// stream. (Before 0.4.2 this was not the case: a transient
/// `ErrorKind::WouldBlock` from a non-blocking writer dropped a 32 KiB batch
/// and `finish()` still returned `Ok`.)
///
/// # Example
///
/// ```rust
/// use std::io::Write;
/// use oxiarc_lzw::z::{ZWriter, decompress};
///
/// let original = b"a .Z stream written through std::io::Write".repeat(20);
///
/// let mut writer = ZWriter::new(Vec::new(), 16).expect("9-16 bits");
/// writer.write_all(&original).expect("write");
/// let stream = writer.finish().expect("finish");
///
/// assert_eq!(&stream[..2], &[0x1F, 0x9D]);
/// assert_eq!(decompress(&stream).expect("decode"), original);
/// ```
#[derive(Debug)]
pub struct ZWriter<W: Write> {
    inner: Option<W>,
    encoder: ZEncoder,
    header: ZHeader,
    header_written: bool,
    pending: Vec<u8>,
    finished: bool,
    /// Set once a batch has failed to reach the inner writer; see the type
    /// docs. A `.Z` stream with a hole in it decodes as plausible garbage,
    /// so the adapter must never report success after one.
    failed: bool,
}

impl<W: Write> ZWriter<W> {
    /// Wrap `writer`, producing a block-mode stream with `max_bits`-wide
    /// codes (`compress -b max_bits`).
    ///
    /// # Errors
    ///
    /// [`LzwError::ZUnsupportedMaxBits`] unless `9 <= max_bits <= 16`.
    pub fn new(writer: W, max_bits: u8) -> Result<Self> {
        Self::with_header(writer, ZHeader::new(max_bits, true)?)
    }

    /// Wrap `writer` with a fully specified header, so that non-block-mode
    /// streams (pre-4.3BSD `compress`) can be produced.
    ///
    /// # Errors
    ///
    /// Never fails today; the signature matches [`ZWriter::new`] so the two
    /// can be used interchangeably.
    pub fn with_header(writer: W, header: ZHeader) -> Result<Self> {
        Ok(Self {
            inner: Some(writer),
            encoder: ZEncoder::new(header),
            header,
            header_written: false,
            pending: Vec::with_capacity(WRITE_FLUSH),
            finished: false,
            failed: false,
        })
    }

    /// The header this writer emits.
    #[must_use]
    pub fn header(&self) -> ZHeader {
        self.header
    }

    /// Emit the final code, flush everything and return the inner writer.
    ///
    /// # Errors
    ///
    /// Any [`io::Error`] from the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        if self.failed {
            return Err(poisoned());
        }
        if !self.finished {
            self.finished = true;
            let mut out = core::mem::take(&mut self.pending);
            self.encoder.finish(&mut out);
            let result = self.emit(&out);
            out.clear();
            self.pending = out;
            result?;
        }
        let mut inner = self
            .inner
            .take()
            .ok_or_else(|| io::Error::other("inner writer already taken"))?;
        inner.flush()?;
        Ok(inner)
    }

    /// Write the header (once) followed by `bytes`.
    ///
    /// Any failure poisons the adapter: a `.Z` stream that is missing a
    /// batch from its middle is indistinguishable from a valid one, so
    /// carrying on would turn an I/O error into silent corruption.
    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        let header = self.header.to_bytes();
        let write_header = !self.header_written;
        let result = match self.inner.as_mut() {
            Some(inner) => {
                // No `?` here: every exit has to run the poisoning below.
                let mut outcome = if write_header {
                    inner.write_all(&header)
                } else {
                    Ok(())
                };
                if outcome.is_ok() && !bytes.is_empty() {
                    outcome = inner.write_all(bytes);
                }
                outcome
            }
            None => Err(io::Error::other("inner writer already taken")),
        };
        if result.is_err() {
            self.failed = true;
        } else {
            // Only now: a header write that failed must be retried, not
            // assumed done.
            self.header_written = true;
        }
        result
    }
}

impl<W: Write> Write for ZWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("ZWriter already finished"));
        }
        if self.failed {
            return Err(poisoned());
        }
        let mut out = core::mem::take(&mut self.pending);
        let mut result = Ok(());
        // Push in bounded pieces, so the staging buffer stays around
        // `WRITE_FLUSH` however large `data` is. `ZEncoder::push` is
        // chunk-invariant (see the `feeding_in_pieces_matches_feeding_at_once`
        // test in `z::encode`), so this cannot change a single output byte.
        //
        // On a failed batch the loop stops with part of `data` never fed to
        // the encoder, which would leave the encoder holding the state of a
        // *prefix* of what the caller wrote. That state can never escape:
        // `emit` has already set `failed`, and `finish` checks `failed`
        // before it calls `ZEncoder::finish`.
        for piece in data.chunks(WRITE_FLUSH) {
            self.encoder.push(piece, &mut out);
            if out.len() >= WRITE_FLUSH {
                result = self.emit(&out);
                out.clear();
                if result.is_err() {
                    break;
                }
            }
        }
        self.pending = out;
        result?;
        Ok(data.len())
    }

    /// Hand every completed code group to the inner writer and flush it.
    ///
    /// The `.Z` format has no synchronisation point, so this cannot make the
    /// bytes written so far independently decodable; it only stops the
    /// adapter from holding completed groups back.
    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(poisoned());
        }
        let mut out = core::mem::take(&mut self.pending);
        let result = self.emit(&out);
        out.clear();
        self.pending = out;
        result?;
        let Some(inner) = self.inner.as_mut() else {
            self.failed = true;
            return Err(io::Error::other("inner writer already taken"));
        };
        let flushed = inner.flush();
        if flushed.is_err() {
            self.failed = true;
        }
        flushed
    }
}

/// The error every operation on a poisoned [`ZWriter`] returns.
fn poisoned() -> io::Error {
    io::Error::other(
        "ZWriter is poisoned: an earlier batch failed to reach the inner writer, so the \
         stream would have an undetectable hole in it",
    )
}

/// Wrap an [`LzwError`] as an [`io::Error`], preserving a real I/O error.
fn to_io(error: LzwError) -> io::Error {
    match error {
        LzwError::Io(inner) => inner,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader that hands out at most `step` bytes per call, to exercise the
    /// adapters' partial-read handling.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let take = self.data.len().min(self.step).min(out.len());
            out[..take].copy_from_slice(&self.data[..take]);
            self.data = &self.data[take..];
            Ok(take)
        }
    }

    #[test]
    fn the_reader_survives_one_byte_at_a_time() {
        let payload = b"trickle-fed compress stream. ".repeat(300);
        let stream = crate::z::compress(&payload, 16).expect("compress");
        for step in [1usize, 2, 3, 5, 17] {
            let mut out = Vec::new();
            ZReader::new(Trickle {
                data: &stream,
                step,
            })
            .read_to_end(&mut out)
            .expect("decode");
            assert_eq!(out, payload, "step {step}");
        }
    }

    #[test]
    fn the_reader_exposes_the_header_after_the_first_read() {
        let stream = crate::z::compress(b"header check", 13).expect("compress");
        let mut reader = ZReader::new(&stream[..]);
        assert_eq!(reader.header(), None);
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).expect("first byte");
        let header = reader.header().expect("header after first read");
        assert_eq!(header.max_bits, 13);
        assert!(header.block_mode);
    }

    #[test]
    fn the_reader_enforces_its_output_cap_during_decoding() {
        let payload = vec![b'z'; 4 * 1024 * 1024];
        let stream = crate::z::compress(&payload, 16).expect("compress");
        assert!(stream.len() < 64 * 1024, "the bomb must be small");
        let mut out = Vec::new();
        let error = ZReader::new(&stream[..])
            .with_max_output(64 * 1024)
            .read_to_end(&mut out)
            .expect_err("the cap must fire");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(out.len() <= 64 * 1024, "wrote {} bytes", out.len());
    }

    #[test]
    fn a_bad_header_surfaces_from_the_first_read() {
        let mut out = Vec::new();
        let error = ZReader::new(&b"not a .Z file"[..])
            .read_to_end(&mut out)
            .expect_err("bad magic");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(out.is_empty());

        let mut out = Vec::new();
        let error = ZReader::new(&b"\x1f"[..])
            .read_to_end(&mut out)
            .expect_err("truncated header");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn writer_output_matches_the_one_shot_encoder() {
        let payload = b"streamed exactly like the one-shot path. ".repeat(700);
        for max_bits in 9u8..=16 {
            let one_shot = crate::z::compress(&payload, max_bits).expect("compress");
            for chunk in [1usize, 7, 4096, payload.len()] {
                let mut writer = ZWriter::new(Vec::new(), max_bits).expect("writer");
                for piece in payload.chunks(chunk) {
                    writer.write_all(piece).expect("write");
                }
                let streamed = writer.finish().expect("finish");
                assert_eq!(streamed, one_shot, "max_bits {max_bits}, chunk {chunk}");
            }
        }
    }

    #[test]
    fn an_empty_stream_is_just_the_header() {
        let writer = ZWriter::new(Vec::new(), 16).expect("writer");
        let stream = writer.finish().expect("finish");
        assert_eq!(stream, vec![0x1F, 0x9D, 0x90]);
        assert!(
            crate::z::decompress(&stream)
                .expect("decode empty")
                .is_empty()
        );
    }

    #[test]
    fn flush_does_not_corrupt_the_stream() {
        let payload = b"flushed midway through. ".repeat(500);
        let mut writer = ZWriter::new(Vec::new(), 14).expect("writer");
        for piece in payload.chunks(101) {
            writer.write_all(piece).expect("write");
            writer.flush().expect("flush");
        }
        let stream = writer.finish().expect("finish");
        assert_eq!(stream, crate::z::compress(&payload, 14).expect("one shot"));
    }

    #[test]
    fn writing_after_finish_is_refused() {
        let mut writer = ZWriter::new(Vec::new(), 16).expect("writer");
        writer.write_all(b"before").expect("write");
        writer.finished = true;
        assert!(writer.write(b"after").is_err());
    }

    /// Writer that fails exactly once, on its `fail_at`-th `write` call.
    struct FailOnce {
        calls: usize,
        fail_at: usize,
        out: Vec<u8>,
    }

    impl Write for FailOnce {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == self.fail_at {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "not now"));
            }
            self.out.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Regression: a transient failure of the inner writer used to drop one
    /// ~32 KiB batch and let `finish()` return `Ok`, producing a stream with
    /// an undetectable hole in it (`.Z` has no checksum and no
    /// end-of-information code, so such a stream decodes to plausible
    /// garbage). Every later operation must fail instead.
    #[test]
    fn a_failed_batch_poisons_the_writer_instead_of_corrupting_the_stream() {
        // Incompressible, so a single `write_all` really does produce many
        // batches for the inner writer.
        let mut state = 0x2468_ACE0u32;
        let payload: Vec<u8> = (0..600_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 7) as u8
            })
            .collect();

        for fail_at in [1usize, 2, 3, 5, 9] {
            let inner = FailOnce {
                calls: 0,
                fail_at,
                out: Vec::new(),
            };
            let mut writer = ZWriter::new(inner, 16).expect("writer");
            let wrote = writer.write_all(&payload);
            let flushed = writer.flush();
            let finished = writer.finish();
            assert!(
                wrote.is_err() || flushed.is_err(),
                "fail_at {fail_at}: the failure must reach the caller"
            );
            assert!(
                finished.is_err(),
                "fail_at {fail_at}: finish() must not report success for a stream with a hole"
            );
        }

        // The same, but calling `finish` straight after the failed write
        // with no `flush` in between: `finish` must consult the poison flag
        // *before* `finished`, or the encoder's prefix-only state would be
        // emitted as a complete, internally consistent, silently short
        // stream — worse than the hole this poisoning replaced.
        for fail_at in [1usize, 2, 3, 5, 9] {
            let inner = FailOnce {
                calls: 0,
                fail_at,
                out: Vec::new(),
            };
            let mut writer = ZWriter::new(inner, 16).expect("writer");
            let wrote = writer.write_all(&payload);
            assert!(wrote.is_err(), "fail_at {fail_at}: write must fail");
            assert!(
                writer.finish().is_err(),
                "fail_at {fail_at}: finish() straight after a failed write must fail too"
            );
        }

        // And a writer that never fails is unaffected.
        let inner = FailOnce {
            calls: 0,
            fail_at: usize::MAX,
            out: Vec::new(),
        };
        let mut writer = ZWriter::new(inner, 16).expect("writer");
        writer.write_all(&payload).expect("write");
        let done = writer.finish().expect("finish");
        assert_eq!(
            done.out,
            crate::z::compress(&payload, 16).expect("one shot"),
            "the healthy path must still be byte-identical to the one-shot encoder"
        );
    }

    #[test]
    fn a_failed_header_write_is_not_recorded_as_done() {
        // The header goes out with the first batch. If that write fails the
        // adapter must not remember the header as written (the batch behind
        // it is gone, so the writer is poisoned as well — but the flag has
        // to track reality either way).
        let inner = FailOnce {
            calls: 0,
            fail_at: 1,
            out: Vec::new(),
        };
        let mut writer = ZWriter::new(inner, 12).expect("writer");
        assert!(!writer.header_written);
        let _ = writer.write_all(b"anything");
        let _ = writer.flush();
        assert!(
            !writer.header_written,
            "a header write that failed must still be pending"
        );
    }

    #[test]
    fn non_block_mode_streams_round_trip_through_the_adapters() {
        let payload = b"no block mode here. ".repeat(400);
        let header = ZHeader::new(12, false).expect("header");
        let mut writer = ZWriter::with_header(Vec::new(), header).expect("writer");
        writer.write_all(&payload).expect("write");
        let stream = writer.finish().expect("finish");
        assert_eq!(stream[2] & 0x80, 0, "block-mode flag must be clear");

        let mut out = Vec::new();
        ZReader::new(&stream[..])
            .read_to_end(&mut out)
            .expect("decode");
        assert_eq!(out, payload);
    }
}
