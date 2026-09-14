//! Raw in-memory streaming compression and decompression: flate2's
//! [`Compress`] / [`Decompress`] over oxiarc-deflate's [`Deflater`] and
//! [`InflateStream`].
//!
//! # Semantics
//!
//! The contract mirrors flate2 1.x (and therefore zlib's `deflate(3)` /
//! `inflate(3)`):
//!
//! * every call consumes a prefix of `input`, produces a prefix of `output`,
//!   and advances [`Compress::total_in`] / [`Compress::total_out`] by exactly
//!   those amounts — callers compute progress from the counter deltas;
//! * a call that can make no progress at all returns [`Status::BufError`]
//!   (never an error), just like zlib's `Z_BUF_ERROR`;
//! * compressed bytes that do not fit into `output` are kept and handed out
//!   by later calls before any new input is looked at;
//! * [`Decompress`] never consumes bytes past the end of the stream: after
//!   [`Status::StreamEnd`], `total_in` is exactly the stream length, so a
//!   gzip trailer or the next member stays in the caller's buffer.

use std::error::Error;
use std::fmt;
use std::io;
use std::mem::MaybeUninit;

use oxiarc_core::Crc32;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{Adler32, Deflater, InflateStatus, InflateStream};

use crate::Compression;
use crate::gz::{GzHeader, parse_header};

/// Largest history window a DEFLATE stream can reference.
const WINDOW: usize = 32 * 1024;

/// Values which indicate the form of flushing to be used when compressing
/// in-memory data.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum FlushCompress {
    /// A typical parameter for passing to compression/decompression
    /// functions, this indicates that the underlying stream to decide how
    /// much data to accumulate before producing output in order to maximize
    /// compression.
    None = 0,
    /// All pending output is flushed to the output buffer, but the output is
    /// not aligned to a byte boundary (zlib's `Z_PARTIAL_FLUSH`).
    Partial = 1,
    /// All pending output is flushed to the output buffer and the output is
    /// aligned on a byte boundary so that the decompressor can get all input
    /// data available so far (an empty stored block, `00 00 FF FF`).
    Sync = 2,
    /// Like [`FlushCompress::Sync`], and the compression state is reset so
    /// that decompression can restart from this point.
    Full = 3,
    /// Pending input is processed and pending output is flushed; the stream
    /// is terminated (final block, plus the zlib/gzip trailer).
    Finish = 4,
}

/// Values which indicate the form of flushing to be used when decompressing
/// in-memory data.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum FlushDecompress {
    /// Decompress as much as possible.
    None = 0,
    /// Flush as much output as possible (identical to `None` here: the
    /// decoder always emits everything it can).
    Sync = 2,
    /// Hint that `input` holds the rest of the stream. A stream that is
    /// still incomplete after that is reported as [`Status::BufError`] (no
    /// progress) or [`Status::Ok`], never as a hard error — the zlib
    /// behaviour flate2 exposes.
    Finish = 4,
}

/// Possible status results of compressing some data or successfully
/// decompressing a block of data.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Status {
    /// Indicates success. Means that more input may be needed but isn't
    /// available and/or there's more output to be written but the output
    /// buffer is full.
    Ok,
    /// Indicates that forward progress is not possible due to input or
    /// output buffers being empty.
    BufError,
    /// Indicates that all input has been consumed and all output bytes have
    /// been written. Decompression/compression should not be called again.
    StreamEnd,
}

#[derive(Clone, Debug)]
enum DecompressErrorInner {
    General { msg: String },
    NeedsDictionary(u32),
}

/// Error returned when a decompression object finds that the input stream of
/// bytes was not a valid input stream of bytes.
#[derive(Clone, Debug)]
pub struct DecompressError(DecompressErrorInner);

impl DecompressError {
    /// Indicates whether decompression failed due to requiring a dictionary.
    ///
    /// The resulting integer is the Adler-32 checksum of the dictionary
    /// required.
    pub fn needs_dictionary(&self) -> Option<u32> {
        match self.0 {
            DecompressErrorInner::NeedsDictionary(adler) => Some(adler),
            DecompressErrorInner::General { .. } => None,
        }
    }

    /// Retrieve the implementation's message about why the operation failed,
    /// if one exists.
    pub fn message(&self) -> Option<&str> {
        match &self.0 {
            DecompressErrorInner::General { msg } => Some(msg.as_str()),
            DecompressErrorInner::NeedsDictionary(_) => None,
        }
    }

    fn general(msg: impl Into<String>) -> Self {
        DecompressError(DecompressErrorInner::General { msg: msg.into() })
    }
}

impl Error for DecompressError {}

impl From<DecompressError> for io::Error {
    fn from(data: DecompressError) -> io::Error {
        io::Error::other(data)
    }
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match &self.0 {
            DecompressErrorInner::General { msg } => Some(msg.as_str()),
            DecompressErrorInner::NeedsDictionary(_) => Some("requires a dictionary"),
        };
        match msg {
            Some(msg) => write!(f, "deflate decompression error: {msg}"),
            None => write!(f, "deflate decompression error"),
        }
    }
}

/// Error returned when a compression object is used incorrectly or otherwise
/// generates an error.
#[derive(Clone, Debug)]
pub struct CompressError {
    msg: Option<String>,
}

impl CompressError {
    /// Retrieve the implementation's message about why the operation failed,
    /// if one exists.
    pub fn message(&self) -> Option<&str> {
        self.msg.as_deref()
    }

    fn new(msg: impl Into<String>) -> Self {
        CompressError {
            msg: Some(msg.into()),
        }
    }
}

impl Error for CompressError {}

impl From<CompressError> for io::Error {
    fn from(data: CompressError) -> io::Error {
        io::Error::other(data)
    }
}

impl fmt::Display for CompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.msg {
            Some(msg) => write!(f, "deflate compression error: {msg}"),
            None => write!(f, "deflate compression error"),
        }
    }
}

/// Container around the DEFLATE payload.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Format {
    Raw,
    Zlib,
    Gzip,
}

/// zlib's flush ranking (`RANK()` in `deflate.c`), used to suppress
/// duplicate consecutive flushes that have nothing new to flush.
fn flush_rank(flush: FlushCompress) -> u8 {
    match flush {
        FlushCompress::None => 0,
        FlushCompress::Partial => 2,
        FlushCompress::Sync => 4,
        FlushCompress::Full => 6,
        FlushCompress::Finish => 8,
    }
}

/// Clamp a flate2 level (0..=10 is accepted by flate2's miniz backend) to
/// the 0..=9 range the encoder implements.
fn encoder_level(level: Compression) -> u8 {
    level.level().min(9) as u8
}

/// The zlib `FLEVEL` bits zlib itself writes for `level`.
fn zlib_flevel(level: u8) -> u8 {
    match level {
        0 | 1 => 0,
        2..=5 => 1,
        6 => 2,
        _ => 3,
    }
}

/// Raw in-memory compression stream for blocks of data.
///
/// This type is the building block for the I/O streams in the rest of this
/// crate. It requires more management than the `Read`/`Write` API but is
/// maximally flexible in terms of accepting input from any source and being
/// able to produce output to any memory location.
pub struct Compress {
    deflater: Deflater,
    format: Format,
    level: u8,
    /// Container bits still owed before the payload (zlib or gzip header).
    header_pending: bool,
    /// Compressed bytes produced but not yet handed to the caller.
    pending: Vec<u8>,
    pending_pos: usize,
    /// `Finish` has run: the final block and trailer are in `pending`.
    finished: bool,
    adler: Adler32,
    crc: Crc32,
    /// Adler-32 of a preset dictionary, written as `DICTID` (zlib only).
    dict_id: Option<u32>,
    /// Rank of the most recent flush and whether data arrived since, for
    /// zlib's "avoid duplicate consecutive flushes" rule.
    last_flush_rank: u8,
    data_since_flush: bool,
    total_in: u64,
    total_out: u64,
    /// The last [`WINDOW`] input bytes, so [`Compress::set_level`] can carry
    /// the history into a new encoder instead of losing compression.
    history: Vec<u8>,
}

impl fmt::Debug for Compress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compress")
            .field("format", &self.format)
            .field("level", &self.level)
            .field("total_in", &self.total_in)
            .field("total_out", &self.total_out)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Compress {
    fn with_format(level: Compression, format: Format) -> Compress {
        let level = encoder_level(level);
        Compress {
            deflater: Deflater::new(level),
            format,
            level,
            header_pending: format != Format::Raw,
            pending: Vec::new(),
            pending_pos: 0,
            finished: false,
            adler: Adler32::new(),
            crc: Crc32::new(),
            dict_id: None,
            last_flush_rank: 0,
            data_since_flush: false,
            total_in: 0,
            total_out: 0,
            history: Vec::new(),
        }
    }

    /// Creates a new object ready for compressing data that it's given.
    ///
    /// The `level` argument here indicates what level of compression is
    /// going to be performed, and the `zlib_header` argument indicates
    /// whether the output data should have a zlib header or not.
    pub fn new(level: Compression, zlib_header: bool) -> Compress {
        let format = if zlib_header {
            Format::Zlib
        } else {
            Format::Raw
        };
        Compress::with_format(level, format)
    }

    /// Creates a new object ready for compressing data that it's given.
    ///
    /// `window_bits` must be in `9..=15`. The encoder always searches the
    /// full 32 KiB DEFLATE window, so a zlib header declares `window_bits
    /// == 15` regardless: that stream is valid for, and decodes with, any
    /// inflater configured with the default window.
    ///
    /// # Panics
    ///
    /// Never; out-of-range values are clamped, where flate2 would panic.
    pub fn new_with_window_bits(
        level: Compression,
        zlib_header: bool,
        window_bits: u8,
    ) -> Compress {
        let _ = window_bits.clamp(9, 15);
        Compress::new(level, zlib_header)
    }

    /// Creates a new object ready for compressing data that it's given,
    /// producing a gzip (RFC 1952) stream with a minimal header.
    pub fn new_gzip(level: Compression, window_bits: u8) -> Compress {
        let _ = window_bits.clamp(9, 15);
        Compress::with_format(level, Format::Gzip)
    }

    /// Returns the total number of input bytes which have been processed by
    /// this compression object.
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Returns the total number of output bytes which have been produced by
    /// this compression object.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Specifies the compression dictionary to use.
    ///
    /// Returns the Adler-32 checksum of the dictionary. Must be called
    /// before any data is compressed; for a zlib stream the header then
    /// carries `FDICT` + `DICTID`, as zlib's `deflateSetDictionary` does.
    ///
    /// # Errors
    ///
    /// When data has already been compressed, or for a gzip stream (RFC
    /// 1952 has no dictionary field).
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<u32, CompressError> {
        if self.format == Format::Gzip {
            return Err(CompressError::new("gzip streams cannot use a dictionary"));
        }
        if self.total_in > 0
            || self.finished
            || (self.format == Format::Zlib && !self.header_pending)
        {
            return Err(CompressError::new(
                "dictionary must be set before compressing",
            ));
        }
        let adler = self.deflater.set_dictionary(dictionary);
        self.dict_id = Some(adler);
        let keep = dictionary.len().saturating_sub(WINDOW);
        self.history = dictionary[keep..].to_vec();
        Ok(adler)
    }

    /// Quickly resets this compressor without having to reallocate anything.
    ///
    /// This is equivalent to dropping this object and then creating a new
    /// one with the same level and container.
    pub fn reset(&mut self) {
        self.deflater = Deflater::new(self.level);
        self.header_pending = self.format != Format::Raw;
        self.pending.clear();
        self.pending_pos = 0;
        self.finished = false;
        self.adler = Adler32::new();
        self.crc = Crc32::new();
        self.dict_id = None;
        self.last_flush_rank = 0;
        self.data_since_flush = false;
        self.total_in = 0;
        self.total_out = 0;
        self.history.clear();
    }

    /// Dynamically updates the compression level.
    ///
    /// Like zlib's `deflateParams`, the data compressed so far is flushed
    /// under the old level (here a byte-aligning sync flush, since the new
    /// level runs on a fresh encoder); later input is compressed with the
    /// new level and can still reference the previous 32 KiB.
    ///
    /// # Errors
    ///
    /// When the stream has already been finished.
    pub fn set_level(&mut self, level: Compression) -> Result<(), CompressError> {
        if self.finished {
            return Err(CompressError::new("stream already finished"));
        }
        let new_level = encoder_level(level);
        if new_level == self.level {
            return Ok(());
        }
        if self.total_in == 0 && self.dict_id.is_none() {
            self.level = new_level;
            self.deflater = Deflater::new(new_level);
            return Ok(());
        }
        let mut out = Vec::new();
        self.deflater
            .deflate_sync(&[], &mut out)
            .map_err(|e| CompressError::new(e.to_string()))?;
        self.pending.extend_from_slice(&out);
        let mut deflater = Deflater::new(new_level);
        if !self.history.is_empty() {
            let _ = deflater.set_dictionary(&self.history);
        }
        self.deflater = deflater;
        self.level = new_level;
        Ok(())
    }

    /// Compresses the input data into the output, consuming only as much
    /// input as needed and writing as much output as possible.
    ///
    /// # Errors
    ///
    /// Only when the underlying encoder reports an internal failure.
    pub fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushCompress,
    ) -> Result<Status, CompressError> {
        let mut pos = 0usize;
        let cap = output.len();
        self.run(input, cap, flush, &mut |bytes: &[u8]| {
            output[pos..pos + bytes.len()].copy_from_slice(bytes);
            pos += bytes.len();
        })
    }

    /// Like [`Compress::compress`] but writes into possibly-uninitialised
    /// memory. Only the prefix reported through `total_out` is initialised
    /// afterwards.
    ///
    /// # Errors
    ///
    /// See [`Compress::compress`].
    pub fn compress_uninit(
        &mut self,
        input: &[u8],
        output: &mut [MaybeUninit<u8>],
        flush: FlushCompress,
    ) -> Result<Status, CompressError> {
        let mut pos = 0usize;
        let cap = output.len();
        self.run(input, cap, flush, &mut |bytes: &[u8]| {
            for (dst, src) in output[pos..pos + bytes.len()].iter_mut().zip(bytes) {
                dst.write(*src);
            }
            pos += bytes.len();
        })
    }

    /// Compresses the input data into the extra space of the output,
    /// consuming only as much input as needed and writing as much output as
    /// possible. The vector is never reallocated.
    ///
    /// # Errors
    ///
    /// See [`Compress::compress`].
    pub fn compress_vec(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        flush: FlushCompress,
    ) -> Result<Status, CompressError> {
        let cap = output.capacity() - output.len();
        self.run(input, cap, flush, &mut |bytes: &[u8]| {
            output.extend_from_slice(bytes);
        })
    }

    /// Hand at most `room` pending bytes to `emit`; returns how many.
    fn drain(&mut self, room: usize, emit: &mut dyn FnMut(&[u8])) -> usize {
        let available = self.pending.len() - self.pending_pos;
        let n = available.min(room);
        if n > 0 {
            emit(&self.pending[self.pending_pos..self.pending_pos + n]);
            self.pending_pos += n;
        }
        if self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
        n
    }

    fn pending_is_empty(&self) -> bool {
        self.pending_pos == self.pending.len()
    }

    fn write_header(&mut self) {
        match self.format {
            Format::Raw => {}
            Format::Zlib => {
                let cmf: u8 = 0x78;
                let mut flg: u8 = zlib_flevel(self.level) << 6;
                if self.dict_id.is_some() {
                    flg |= 0x20;
                }
                let rem = ((u16::from(cmf) << 8) | u16::from(flg)) % 31;
                if rem != 0 {
                    flg += (31 - rem) as u8;
                }
                self.pending.push(cmf);
                self.pending.push(flg);
                if let Some(id) = self.dict_id {
                    self.pending.extend_from_slice(&id.to_be_bytes());
                }
            }
            Format::Gzip => {
                let xfl = match self.level {
                    9 => 2,
                    0 | 1 => 4,
                    _ => 0,
                };
                // Minimal header: no name, mtime 0, OS 255 ("unknown"), as
                // zlib's deflateInit2(windowBits + 16) writes.
                self.pending
                    .extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255]);
            }
        }
        self.header_pending = false;
    }

    fn write_trailer(&mut self) {
        match self.format {
            Format::Raw => {}
            Format::Zlib => {
                let sum = self.adler.finish();
                self.pending.extend_from_slice(&sum.to_be_bytes());
            }
            Format::Gzip => {
                let crc = self.crc.value();
                self.pending.extend_from_slice(&crc.to_le_bytes());
                self.pending
                    .extend_from_slice(&(self.total_in as u32).to_le_bytes());
            }
        }
    }

    fn remember(&mut self, input: &[u8]) {
        if input.len() >= WINDOW {
            self.history.clear();
            self.history
                .extend_from_slice(&input[input.len() - WINDOW..]);
        } else {
            self.history.extend_from_slice(input);
            if self.history.len() > WINDOW {
                let excess = self.history.len() - WINDOW;
                self.history.drain(..excess);
            }
        }
    }

    fn run(
        &mut self,
        input: &[u8],
        cap: usize,
        flush: FlushCompress,
        emit: &mut dyn FnMut(&[u8]),
    ) -> Result<Status, CompressError> {
        let mut written = 0usize;
        if self.header_pending {
            self.write_header();
        }
        written += self.drain(cap, emit);
        if !self.pending_is_empty() {
            self.total_out += written as u64;
            return Ok(if written > 0 {
                Status::Ok
            } else {
                Status::BufError
            });
        }
        if self.finished {
            self.total_out += written as u64;
            return Ok(Status::StreamEnd);
        }

        let rank = flush_rank(flush);
        let nothing_to_do = input.is_empty()
            && flush != FlushCompress::Finish
            && (flush == FlushCompress::None
                || (!self.data_since_flush && rank <= self.last_flush_rank));
        if nothing_to_do {
            self.total_out += written as u64;
            return Ok(if written > 0 {
                Status::Ok
            } else {
                Status::BufError
            });
        }

        self.adler.update(input);
        if self.format == Format::Gzip {
            self.crc.update(input);
        }
        self.remember(input);
        self.total_in += input.len() as u64;

        let mut out = std::mem::take(&mut self.pending);
        let result = match flush {
            FlushCompress::None => self.deflater.deflate(input, &mut out, false),
            FlushCompress::Partial => self.deflater.deflate_partial(input, &mut out),
            FlushCompress::Sync => self.deflater.deflate_sync(input, &mut out),
            FlushCompress::Full => self.deflater.deflate_full(input, &mut out),
            FlushCompress::Finish => self.deflater.deflate(input, &mut out, true),
        };
        self.pending = out;
        result.map_err(|e| CompressError::new(e.to_string()))?;
        if flush == FlushCompress::Finish {
            self.write_trailer();
            self.finished = true;
        }
        if flush == FlushCompress::None {
            if !input.is_empty() {
                self.data_since_flush = true;
            }
        } else {
            self.data_since_flush = false;
            self.last_flush_rank = rank;
        }

        written += self.drain(cap - written, emit);
        self.total_out += written as u64;
        if self.finished && self.pending_is_empty() {
            Ok(Status::StreamEnd)
        } else {
            Ok(Status::Ok)
        }
    }
}

/// Where a [`Decompress`] is within its container.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum DecodeState {
    /// Collecting header bytes (zlib `CMF`/`FLG`/`DICTID`, or gzip).
    Header,
    /// Waiting for [`Decompress::set_dictionary`].
    NeedDictionary(u32),
    /// Inside the DEFLATE payload.
    Body,
    /// Collecting the trailer.
    Trailer,
    /// The whole stream has been decoded and verified.
    Done,
}

/// Raw in-memory decompression stream for blocks of data.
///
/// This type is the building block for the I/O streams in the rest of this
/// crate. It requires more management than the `Read`/`Write` API but is
/// maximally flexible in terms of accepting input from any source and being
/// able to produce output to any memory location.
pub struct Decompress {
    stream: InflateStream,
    format: Format,
    state: DecodeState,
    /// Header or trailer bytes collected so far.
    scratch: Vec<u8>,
    adler: Adler32,
    crc: Crc32,
    member_out: u64,
    dictionary: Option<Vec<u8>>,
    gz_header: Option<GzHeader>,
    fault: Option<DecompressError>,
    /// Compare the zlib Adler-32 trailer (always consumed either way).
    verify_checksum: bool,
    total_in: u64,
    total_out: u64,
}

impl fmt::Debug for Decompress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decompress")
            .field("format", &self.format)
            .field("state", &self.state)
            .field("total_in", &self.total_in)
            .field("total_out", &self.total_out)
            .finish()
    }
}

impl Decompress {
    fn with_format(format: Format) -> Decompress {
        Decompress {
            stream: InflateStream::new(),
            format,
            state: if format == Format::Raw {
                DecodeState::Body
            } else {
                DecodeState::Header
            },
            scratch: Vec::new(),
            adler: Adler32::new(),
            crc: Crc32::new(),
            member_out: 0,
            dictionary: None,
            gz_header: None,
            fault: None,
            verify_checksum: true,
            total_in: 0,
            total_out: 0,
        }
    }

    /// Creates a new object ready for decompressing data that it's given.
    ///
    /// The `zlib_header` argument indicates whether the input data is
    /// expected to have a zlib header or not.
    pub fn new(zlib_header: bool) -> Decompress {
        Decompress::with_format(if zlib_header {
            Format::Zlib
        } else {
            Format::Raw
        })
    }

    /// Creates a new object ready for decompressing data that it's given.
    ///
    /// The decoder always keeps the full 32 KiB window, which decodes
    /// streams produced with any `window_bits` in `9..=15`.
    pub fn new_with_window_bits(zlib_header: bool, window_bits: u8) -> Decompress {
        let _ = window_bits;
        Decompress::new(zlib_header)
    }

    /// Creates a new object ready for decompressing data that it's given,
    /// expecting a gzip (RFC 1952) header and trailer.
    pub fn new_gzip(window_bits: u8) -> Decompress {
        let _ = window_bits;
        Decompress::with_format(Format::Gzip)
    }

    /// Returns the total number of input bytes which have been processed by
    /// this decompression object.
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Returns the total number of output bytes which have been produced by
    /// this decompression object.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Decompresses the input data into the output, consuming only as much
    /// input as needed and writing as much output as possible.
    ///
    /// # Errors
    ///
    /// Corrupt input, a checksum mismatch, or (for a zlib stream with
    /// `FDICT`) a missing dictionary — see
    /// [`DecompressError::needs_dictionary`].
    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushDecompress,
    ) -> Result<Status, DecompressError> {
        let _ = flush;
        let (consumed, produced, status) = self.run(input, output)?;
        self.total_in += consumed as u64;
        self.total_out += produced as u64;
        Ok(status)
    }

    /// Like [`Decompress::decompress`] but writes into possibly
    /// uninitialised memory.
    ///
    /// # Errors
    ///
    /// See [`Decompress::decompress`].
    pub fn decompress_uninit(
        &mut self,
        input: &[u8],
        output: &mut [MaybeUninit<u8>],
        flush: FlushDecompress,
    ) -> Result<Status, DecompressError> {
        for byte in output.iter_mut() {
            byte.write(0);
        }
        // SAFETY: every element of `output` was initialised just above, and
        // `MaybeUninit<u8>` has the same layout as `u8`.
        let init: &mut [u8] = unsafe { &mut *(output as *mut [MaybeUninit<u8>] as *mut [u8]) };
        self.decompress(input, init, flush)
    }

    /// Decompresses the input data into the extra space in the output
    /// vector. The vector is never reallocated.
    ///
    /// # Errors
    ///
    /// See [`Decompress::decompress`].
    pub fn decompress_vec(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        flush: FlushDecompress,
    ) -> Result<Status, DecompressError> {
        let len = output.len();
        output.resize(output.capacity(), 0);
        let before = self.total_out;
        let result = self.decompress(input, &mut output[len..], flush);
        let produced = (self.total_out - before) as usize;
        output.truncate(len + produced);
        result
    }

    /// Specifies the decompression dictionary to use.
    ///
    /// For a raw stream the dictionary must be set before decoding; for a
    /// zlib stream it is normally set after [`Decompress::decompress`]
    /// returned an error whose [`DecompressError::needs_dictionary`] is
    /// `Some`.
    ///
    /// # Errors
    ///
    /// When the dictionary's Adler-32 differs from the one the zlib header
    /// asked for.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<u32, DecompressError> {
        let adler = Adler32::checksum(dictionary);
        match self.state {
            DecodeState::NeedDictionary(expected) => {
                if expected != adler {
                    return Err(DecompressError::general("invalid dictionary"));
                }
                let _ = self.stream.set_dictionary(dictionary);
                self.state = DecodeState::Body;
            }
            DecodeState::Body if self.total_out == 0 => {
                let _ = self.stream.set_dictionary(dictionary);
            }
            _ => {}
        }
        self.dictionary = Some(dictionary.to_vec());
        Ok(adler)
    }

    /// oxiarc extension (not in flate2): whether the zlib Adler-32 trailer
    /// is compared. The trailer is consumed either way, so framing stays
    /// exact. Used by `oxiarc-miniz-compat` for miniz_oxide's
    /// `TINFL_FLAG_IGNORE_ADLER32` / `DataFormat::ZLibIgnoreChecksum`.
    #[doc(hidden)]
    pub fn set_checksum_verification(&mut self, verify: bool) {
        self.verify_checksum = verify;
    }

    /// Performs the equivalent of replacing this decompression state with a
    /// freshly allocated copy.
    pub fn reset(&mut self, zlib_header: bool) {
        *self = Decompress::new(zlib_header);
    }

    /// The gzip header of a stream created with [`Decompress::new_gzip`],
    /// once it has been parsed.
    pub fn gz_header(&self) -> Option<&GzHeader> {
        self.gz_header.as_ref()
    }

    fn fail(&mut self, error: DecompressError) -> DecompressError {
        self.fault = Some(error.clone());
        error
    }

    /// Returns `(consumed, produced, status)`.
    fn run(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, Status), DecompressError> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        loop {
            match self.state {
                DecodeState::Header => {
                    if !self.step_header(input, &mut in_pos)? {
                        break;
                    }
                }
                DecodeState::NeedDictionary(id) => {
                    self.total_in += in_pos as u64;
                    return Err(DecompressError(DecompressErrorInner::NeedsDictionary(id)));
                }
                DecodeState::Body => {
                    if !self.step_body(input, &mut in_pos, output, &mut out_pos)? {
                        break;
                    }
                }
                DecodeState::Trailer => {
                    if !self.step_trailer(input, &mut in_pos)? {
                        break;
                    }
                }
                DecodeState::Done => break,
            }
        }
        let status = if self.state == DecodeState::Done {
            Status::StreamEnd
        } else if in_pos == 0 && out_pos == 0 {
            Status::BufError
        } else {
            Status::Ok
        };
        Ok((in_pos, out_pos, status))
    }

    /// Advance through the header; `Ok(true)` when it is complete.
    fn step_header(&mut self, input: &[u8], in_pos: &mut usize) -> Result<bool, DecompressError> {
        match self.format {
            Format::Raw => {
                self.state = DecodeState::Body;
                Ok(true)
            }
            Format::Zlib => {
                while self.scratch.len() < 2 {
                    let Some(&byte) = input.get(*in_pos) else {
                        return Ok(false);
                    };
                    self.scratch.push(byte);
                    *in_pos += 1;
                }
                let cmf = self.scratch[0];
                let flg = self.scratch[1];
                if cmf & 0x0f != 8
                    || (cmf >> 4) > 7
                    || ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0
                {
                    return Err(self.fail(DecompressError::general("invalid zlib header")));
                }
                if flg & 0x20 != 0 {
                    while self.scratch.len() < 6 {
                        let Some(&byte) = input.get(*in_pos) else {
                            return Ok(false);
                        };
                        self.scratch.push(byte);
                        *in_pos += 1;
                    }
                    let id = u32::from_be_bytes([
                        self.scratch[2],
                        self.scratch[3],
                        self.scratch[4],
                        self.scratch[5],
                    ]);
                    self.scratch.clear();
                    let preset = self
                        .dictionary
                        .as_ref()
                        .filter(|d| Adler32::checksum(d) == id)
                        .cloned();
                    match preset {
                        Some(dictionary) => {
                            let _ = self.stream.set_dictionary(&dictionary);
                            self.state = DecodeState::Body;
                        }
                        None => self.state = DecodeState::NeedDictionary(id),
                    }
                    return Ok(true);
                }
                self.scratch.clear();
                self.state = DecodeState::Body;
                Ok(true)
            }
            Format::Gzip => loop {
                match parse_header(&self.scratch) {
                    Ok(Some((header, used))) => {
                        debug_assert_eq!(used, self.scratch.len());
                        self.gz_header = Some(header);
                        self.scratch.clear();
                        self.state = DecodeState::Body;
                        return Ok(true);
                    }
                    Ok(None) => {
                        let Some(&byte) = input.get(*in_pos) else {
                            return Ok(false);
                        };
                        self.scratch.push(byte);
                        *in_pos += 1;
                    }
                    Err(e) => return Err(self.fail(DecompressError::general(e.to_string()))),
                }
            },
        }
    }

    /// Run the DEFLATE core; `Ok(true)` when the payload has ended.
    fn step_body(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        output: &mut [u8],
        out_pos: &mut usize,
    ) -> Result<bool, DecompressError> {
        loop {
            let call_start = *in_pos;
            let progress = match self.stream.inflate(
                &input[*in_pos..],
                &mut output[*out_pos..],
                FlushMode::None,
            ) {
                Ok(progress) => progress,
                Err(e) => return Err(self.fail(DecompressError::general(e.to_string()))),
            };
            let produced = &output[*out_pos..*out_pos + progress.produced];
            if self.format == Format::Zlib {
                self.adler.update(produced);
            } else if self.format == Format::Gzip {
                self.crc.update(produced);
            }
            self.member_out += progress.produced as u64;
            *in_pos += progress.consumed;
            *out_pos += progress.produced;
            match progress.status {
                InflateStatus::StreamEnd => {
                    // Whole bytes still in the bit accumulator lie beyond
                    // the DEFLATE payload: they are the trailer, or they
                    // belong to whatever follows the stream.
                    let mut leftover = Vec::new();
                    while let Some(byte) = self.stream.take_buffered_byte() {
                        leftover.push(byte);
                    }
                    let trailer_len = match self.format {
                        Format::Raw => 0,
                        Format::Zlib => 4,
                        Format::Gzip => 8,
                    };
                    let take = leftover.len().min(trailer_len);
                    self.scratch.clear();
                    self.scratch.extend_from_slice(&leftover[..take]);
                    let give_back = (leftover.len() - take).min(*in_pos - call_start);
                    *in_pos -= give_back;
                    self.state = if trailer_len == 0 {
                        DecodeState::Done
                    } else {
                        DecodeState::Trailer
                    };
                    return Ok(true);
                }
                InflateStatus::NeedOutput => return Ok(false),
                _ => {
                    if *in_pos >= input.len() || *out_pos >= output.len() {
                        return Ok(false);
                    }
                    // A sync-flush boundary with input left: keep going.
                }
            }
        }
    }

    /// Collect and verify the trailer; `Ok(true)` once verified.
    fn step_trailer(&mut self, input: &[u8], in_pos: &mut usize) -> Result<bool, DecompressError> {
        let need = if self.format == Format::Gzip { 8 } else { 4 };
        while self.scratch.len() < need {
            let Some(&byte) = input.get(*in_pos) else {
                return Ok(false);
            };
            self.scratch.push(byte);
            *in_pos += 1;
        }
        let t = &self.scratch;
        if self.format == Format::Zlib {
            let expected = u32::from_be_bytes([t[0], t[1], t[2], t[3]]);
            if self.verify_checksum && expected != self.adler.finish() {
                return Err(self.fail(DecompressError::general("invalid adler32 checksum")));
            }
        } else {
            let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
            let isize = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
            if crc != self.crc.value() {
                return Err(self.fail(DecompressError::general("invalid gzip crc32")));
            }
            if isize != self.member_out as u32 {
                return Err(self.fail(DecompressError::general("invalid gzip size")));
            }
        }
        self.scratch.clear();
        self.state = DecodeState::Done;
        Ok(true)
    }
}
