//! Resumable gzip / zlib framing over [`InflateStream`].
//!
//! [`WrappedInflate`] adds RFC 1950 (zlib) and RFC 1952 (gzip) container
//! handling to the raw-DEFLATE core, with the same push API: every header
//! field, every trailer and every multi-member boundary is resumable, so a
//! one-byte-at-a-time feed decodes exactly as a whole-slice feed does.
//!
//! # Example
//!
//! ```
//! use oxiarc_core::traits::FlushMode;
//! use oxiarc_deflate::{gzip_compress, InflateStatus, InflateWrapper, WrappedInflate};
//!
//! let compressed = gzip_compress(b"framed payload", 6).expect("gzip");
//! let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
//! let mut out = [0u8; 64];
//! let progress = decoder
//!     .inflate(&compressed, &mut out, FlushMode::Finish)
//!     .expect("inflate");
//! assert_eq!(progress.status, InflateStatus::StreamEnd);
//! assert_eq!(&out[..progress.produced], b"framed payload");
//! assert_eq!(decoder.members_decoded(), 1);
//! ```
//!
//! # Trailing bytes and the first member
//!
//! Two independent knobs decide what happens around member boundaries, and
//! they are deliberately *not* merged, because real callers need different
//! answers at member 0 and after it:
//!
//! * [`TrailingPolicy`] governs bytes that follow the **last complete
//!   member**: reject them, tolerate `0x00` padding (what the `gzip` CLI and
//!   CPython do), or stop silently at the first byte that does not start
//!   another member.
//! * [`WrappedInflate::strict_first_member`] governs bytes at **offset 0**.
//!   With `true` (the default) a stream that does not begin with a valid
//!   header is an error; with `false` it decodes as an empty stream, which
//!   is what the legacy `GzipStreamDecoder` promises its callers.
//!
//! Checksums follow the same shape: [`WrappedInflate::verify_header_crc`]
//! covers the gzip `FHCRC` field and [`WrappedInflate::verify_checksum`] the
//! trailer (gzip CRC-32 + ISIZE, zlib Adler-32). Both default to `true`.
//! Turning the trailer check off still *consumes* the trailer — the framing
//! stays exact, only the comparison is skipped — which is what PNG needs,
//! since the reference `png` crate ignores IDAT Adler-32 by default.

use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::FlushMode;

use crate::stream::{InflateProgress, InflateStatus, InflateStream};
use crate::zlib::Adler32;

/// Cap on a captured gzip `FNAME` / `FCOMMENT` field, so a hostile header
/// cannot grow the decoder's memory without bound.
const MAX_HEADER_STRING: usize = 64 * 1024;

/// Which container the DEFLATE payload is wrapped in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InflateWrapper {
    /// Bare RFC 1951 DEFLATE, no header and no trailer.
    Raw,
    /// RFC 1950 zlib: a 2-byte header and a trailing Adler-32.
    #[default]
    Zlib,
    /// RFC 1952 gzip: a 10-byte header plus optional fields, and a trailing
    /// CRC-32 and ISIZE.
    Gzip,
    /// Sniff the first two bytes: gzip magic, else a valid zlib header, else
    /// raw DEFLATE.
    ///
    /// This is what browsers do for the HTTP `deflate` content-coding, which
    /// servers emit both ways. The sniff happens **only at offset 0 and only
    /// on the header bytes**: a push decoder has already handed output to
    /// its caller by the time a checksum could disagree, so there is no
    /// Adler-32 retry. A raw stream whose first two bytes happen to satisfy
    /// `CM == 8` and `(CMF*256 + FLG) % 31 == 0` is therefore read as zlib;
    /// name the wrapper explicitly when the framing is known.
    Auto,
}

/// What to do with bytes that follow the last complete member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TrailingPolicy {
    /// Any trailing byte is an error.
    #[default]
    Reject,
    /// Only `0x00` padding is tolerated, as the `gzip` CLI and CPython's
    /// `gzip` module do.
    AllowZeros,
    /// Stop silently at the first byte that does not begin another member.
    Stop,
}

/// gzip header fields of the member most recently started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GzipHeaderInfo {
    /// `FNAME`, the original file name, without its NUL terminator.
    pub name: Option<Vec<u8>>,
    /// `FCOMMENT`, without its NUL terminator.
    pub comment: Option<Vec<u8>>,
    /// `FEXTRA` subfields, verbatim.
    pub extra: Option<Vec<u8>>,
    /// Modification time as a Unix timestamp; `0` means "not set".
    pub mtime: u32,
    /// The `XFL` byte.
    pub xfl: u8,
    /// The `OS` byte.
    pub os: u8,
}

/// Framing states shared by the gzip and zlib machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapState {
    /// Deciding the wrapper from the first two bytes (`Auto` only).
    Sniff,
    /// Feeding sniffed bytes that turned out to be raw DEFLATE back into
    /// the core before ordinary input resumes.
    RawPreamble,
    /// gzip: the two magic bytes, checked before the rest of the header so
    /// a non-gzip stream can stop cleanly under `strict_first_member(false)`.
    GzMagic,
    /// gzip: the 10 fixed header bytes.
    GzFixed,
    /// gzip: `XLEN`.
    GzExtraLen,
    /// gzip: the `FEXTRA` payload.
    GzExtra,
    /// gzip: `FNAME`, NUL-terminated.
    GzName,
    /// gzip: `FCOMMENT`, NUL-terminated.
    GzComment,
    /// gzip: the 2-byte `FHCRC`.
    GzHcrc,
    /// zlib: `CMF`/`FLG`.
    ZlHeader,
    /// zlib: the 4-byte `DICTID` (`FDICT` set).
    ZlDictId,
    /// Decoding the DEFLATE payload.
    Deflate,
    /// gzip: the 8-byte trailer.
    GzTrailer,
    /// zlib: the 4-byte trailer.
    ZlTrailer,
    /// Between members: start another, or apply the trailing policy.
    Between,
    /// Every member is done and the stream is closed.
    Done,
}

/// A resumable gzip / zlib / raw DEFLATE decoder.
///
/// See the [module documentation](self) for the worked example and the
/// trailing-byte semantics.
#[derive(Debug)]
pub struct WrappedInflate {
    core: InflateStream,
    wrapper: InflateWrapper,
    /// The wrapper actually in force; `Auto` resolves into this at offset 0.
    active: InflateWrapper,
    state: WrapState,
    multi_member: bool,
    trailing: TrailingPolicy,
    verify_header_crc: bool,
    verify_checksum: bool,
    strict_first_member: bool,

    // per-member accumulators
    crc: Crc32,
    adler: Adler32,
    member_out: u64,
    members_done: u32,
    total_out: u64,
    total_in: u64,

    // header/trailer scratch
    field: Vec<u8>,
    flg: u8,
    extra_remaining: usize,
    header_crc: Crc32,
    header: Option<GzipHeaderInfo>,
    dictionary: Option<Vec<u8>>,
    /// Latched error, replayed by every later call.
    fault: Option<String>,
}

impl WrappedInflate {
    /// A decoder for `wrapper`, with `multi_member` off,
    /// [`TrailingPolicy::Reject`], both checksum checks on and
    /// `strict_first_member` on.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::{InflateWrapper, WrappedInflate};
    ///
    /// let decoder = WrappedInflate::new(InflateWrapper::Zlib);
    /// assert_eq!(decoder.members_decoded(), 0);
    /// assert!(!decoder.is_finished());
    /// ```
    pub fn new(wrapper: InflateWrapper) -> Self {
        let state = initial_state(wrapper);
        Self {
            core: InflateStream::new(),
            wrapper,
            active: wrapper,
            state,
            multi_member: false,
            trailing: TrailingPolicy::Reject,
            verify_header_crc: true,
            verify_checksum: true,
            strict_first_member: true,
            crc: Crc32::new(),
            adler: Adler32::new(),
            member_out: 0,
            members_done: 0,
            total_out: 0,
            total_in: 0,
            field: Vec::new(),
            flg: 0,
            extra_remaining: 0,
            header_crc: Crc32::new(),
            header: None,
            dictionary: None,
            fault: None,
        }
    }

    /// Decode every concatenated member rather than stopping after the
    /// first (RFC 1952 §2.2 for gzip; the same shape for zlib).
    #[must_use]
    pub fn multi_member(mut self, yes: bool) -> Self {
        self.multi_member = yes;
        self
    }

    /// What to do with bytes after the last complete member.
    #[must_use]
    pub fn trailing_policy(mut self, policy: TrailingPolicy) -> Self {
        self.trailing = policy;
        self
    }

    /// Cap the total decoded output across every member of this stream.
    ///
    /// Enforced inside blocks, exactly as
    /// [`InflateStream::with_max_output`] describes.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.core.set_max_output(limit);
        self
    }

    /// Reject the stream once it expands by more than `ratio`, checked only
    /// after `min_output` bytes.
    #[must_use]
    pub fn with_ratio_guard(mut self, ratio: f64, min_output: u64) -> Self {
        self.core.set_ratio_guard(ratio, min_output);
        self
    }

    /// Supply the preset dictionary a zlib `FDICT` stream requires.
    ///
    /// The dictionary's Adler-32 is checked against the stream's `DICTID`.
    /// For `Raw` framing the dictionary is installed immediately, which is
    /// what a MSZIP/CAB folder continuation needs.
    #[must_use]
    pub fn with_dictionary(mut self, dictionary: &[u8]) -> Self {
        self.dictionary = Some(dictionary.to_vec());
        if self.active == InflateWrapper::Raw {
            self.core.set_dictionary(dictionary);
        }
        self
    }

    /// Verify the gzip `FHCRC` header checksum (default `true`).
    ///
    /// Legacy one-shot decoders in this crate read the field and discard it;
    /// this decoder checks it, because no reference encoder emits a wrong
    /// one.
    #[must_use]
    pub fn verify_header_crc(mut self, yes: bool) -> Self {
        self.verify_header_crc = yes;
        self
    }

    /// Verify the trailer checksum — gzip CRC-32 and ISIZE, zlib Adler-32
    /// (default `true`).
    ///
    /// With `false` the trailer is still consumed, so framing and
    /// [`WrappedInflate::total_in`] are unchanged; only the comparison is
    /// skipped. PNG sets this to `false` to match the reference `png`
    /// crate's `ignore_adler32` default.
    #[must_use]
    pub fn verify_checksum(mut self, yes: bool) -> Self {
        self.verify_checksum = yes;
        self
    }

    /// Whether a bad header at offset 0 is an error (default `true`).
    ///
    /// With `false` a stream that does not start with a valid header
    /// decodes as an empty stream — the behaviour the legacy
    /// `GzipStreamDecoder` guarantees. Later members always follow
    /// [`TrailingPolicy`] instead.
    #[must_use]
    pub fn strict_first_member(mut self, yes: bool) -> Self {
        self.strict_first_member = yes;
        self
    }

    /// Whether every member has been decoded and the stream is closed.
    pub fn is_finished(&self) -> bool {
        self.state == WrapState::Done
    }

    /// Total bytes taken from callers' `input` slices.
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Total bytes decoded across every member.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Number of members fully decoded (header, payload and trailer).
    pub fn members_decoded(&self) -> u32 {
        self.members_done
    }

    /// gzip header fields of the member most recently started.
    ///
    /// `None` for non-gzip framing and before the first member's header has
    /// been parsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::traits::FlushMode;
    /// use oxiarc_deflate::{InflateWrapper, WrappedInflate};
    ///
    /// // A gzip member carrying FNAME = "notes.txt".
    /// let mut raw = vec![0x1f, 0x8b, 0x08, 0x08, 0, 0, 0, 0, 0x00, 0xff];
    /// raw.extend_from_slice(b"notes.txt\0");
    /// raw.extend_from_slice(&oxiarc_deflate::deflate(b"hi", 6).expect("deflate"));
    /// raw.extend_from_slice(&oxiarc_core::Crc32::compute(b"hi").to_le_bytes());
    /// raw.extend_from_slice(&2u32.to_le_bytes());
    ///
    /// let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    /// let mut out = [0u8; 16];
    /// decoder.inflate(&raw, &mut out, FlushMode::Finish).expect("inflate");
    /// let header = decoder.gzip_header().expect("header");
    /// assert_eq!(header.name.as_deref(), Some(&b"notes.txt"[..]));
    /// ```
    pub fn gzip_header(&self) -> Option<&GzipHeaderInfo> {
        self.header.as_ref()
    }

    /// The wrapper actually in force.
    ///
    /// Equal to the one passed to [`WrappedInflate::new`] except for
    /// [`InflateWrapper::Auto`], which reports what the sniff resolved to
    /// once the first two bytes have been seen.
    pub fn active_wrapper(&self) -> InflateWrapper {
        self.active
    }

    /// Return to the initial state, dropping all decoding state, counters
    /// and any latched error. Builder settings and the configured
    /// dictionary are preserved.
    pub fn reset(&mut self) {
        self.core.reset();
        self.active = self.wrapper;
        self.state = initial_state(self.wrapper);
        self.crc = Crc32::new();
        self.adler = Adler32::new();
        self.member_out = 0;
        self.members_done = 0;
        self.total_out = 0;
        self.total_in = 0;
        self.field.clear();
        self.flg = 0;
        self.extra_remaining = 0;
        self.header_crc = Crc32::new();
        self.header = None;
        self.fault = None;
        if self.active == InflateWrapper::Raw {
            if let Some(dictionary) = &self.dictionary {
                self.core.set_dictionary(dictionary);
            }
        }
    }

    /// Decode a prefix of `input` into a prefix of `output`.
    ///
    /// The contract is [`InflateStream::inflate`]'s, extended over the
    /// container: header fields, trailers and member boundaries are all
    /// resumable at any byte.
    ///
    /// # Errors
    ///
    /// An invalid header or trailer, a checksum mismatch (when verification
    /// is on), a violated [`TrailingPolicy`], anything the DEFLATE core
    /// rejects, or — under [`FlushMode::Finish`] — a stream that ends in the
    /// middle of a member. Errors are latched and replayed until
    /// [`WrappedInflate::reset`].
    pub fn inflate(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<InflateProgress> {
        if let Some(message) = &self.fault {
            return Err(OxiArcError::corrupted(0, message.clone()));
        }
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        let status = match self.drive(input, &mut in_pos, output, &mut out_pos, flush) {
            Ok(status) => status,
            Err(error) => {
                self.fault = Some(error.to_string());
                return Err(error);
            }
        };
        self.total_in += in_pos as u64;
        Ok(InflateProgress {
            consumed: in_pos,
            produced: out_pos,
            status,
        })
    }

    /// Latch `message` and build the error it stands for.
    fn fail(&mut self, error: OxiArcError) -> OxiArcError {
        self.fault = Some(error.to_string());
        self.state = WrapState::Done;
        error
    }

    /// Pull the next framing byte: the DEFLATE core's accumulator first
    /// (those bytes were already reported as consumed and are available
    /// nowhere else), then `input`.
    fn next_byte(&mut self, input: &[u8], in_pos: &mut usize) -> Option<u8> {
        if let Some(byte) = self.core.take_buffered_byte() {
            return Some(byte);
        }
        let byte = input.get(*in_pos).copied();
        if byte.is_some() {
            *in_pos += 1;
        }
        byte
    }

    /// Accumulate exactly `count` bytes into `self.field`.
    ///
    /// `Ok(false)` means "not enough input yet"; whatever arrived stays in
    /// `field`, so the state is safe to re-enter.
    fn take_exact(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        count: usize,
        flush: FlushMode,
    ) -> Result<bool> {
        while self.field.len() < count {
            let Some(byte) = self.next_byte(input, in_pos) else {
                if is_finish(flush) {
                    let missing = count - self.field.len();
                    return Err(self.fail(OxiArcError::unexpected_eof(missing)));
                }
                return Ok(false);
            };
            self.field.push(byte);
        }
        Ok(true)
    }

    /// Drive the framing state machine until it must return.
    fn drive(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        output: &mut [u8],
        out_pos: &mut usize,
        flush: FlushMode,
    ) -> Result<InflateStatus> {
        loop {
            match self.state {
                WrapState::Sniff => {
                    if !self.take_exact(input, in_pos, 2, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let first = self.field.first().copied().unwrap_or(0);
                    let second = self.field.get(1).copied().unwrap_or(0);
                    // gzip magic wins, then a structurally valid zlib
                    // header, then raw. The two bytes stay in `field`, so
                    // whichever state runs next re-reads them from there.
                    self.active = if first == 0x1F && second == 0x8B {
                        InflateWrapper::Gzip
                    } else if zlib_header_is_plausible(first, second) {
                        InflateWrapper::Zlib
                    } else {
                        InflateWrapper::Raw
                    };
                    self.state = match self.active {
                        InflateWrapper::Gzip => WrapState::GzMagic,
                        InflateWrapper::Zlib => WrapState::ZlHeader,
                        _ => {
                            if let Some(dictionary) = self.dictionary.clone() {
                                self.core.set_dictionary(&dictionary);
                            }
                            WrapState::RawPreamble
                        }
                    };
                }

                WrapState::RawPreamble => {
                    // The sniffed bytes are DEFLATE data after all. Feed
                    // them through the core before touching `input` again;
                    // whatever the core cannot absorb this call stays in
                    // `field` for the next one, so no byte is ever sniffed
                    // or decoded twice.
                    let pending = std::mem::take(&mut self.field);
                    let mut taken = 0usize;
                    let status =
                        self.run_deflate(&pending, &mut taken, output, out_pos, FlushMode::None)?;
                    if taken < pending.len() {
                        self.field = pending.get(taken..).unwrap_or_default().to_vec();
                        return Ok(status);
                    }
                    self.state = if self.core.is_finished() {
                        self.members_done += 1;
                        WrapState::Between
                    } else {
                        WrapState::Deflate
                    };
                }

                WrapState::GzMagic => {
                    // Checked separately from the rest of the header so a
                    // non-gzip stream can stop cleanly at member 0 under
                    // `strict_first_member(false)`, exactly as the legacy
                    // `GzipStreamDecoder` does.
                    let lenient = self.members_done == 0 && !self.strict_first_member;
                    if !self.take_exact(
                        input,
                        in_pos,
                        2,
                        if lenient { FlushMode::None } else { flush },
                    )? {
                        if lenient && is_finish(flush) {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Ok(InflateStatus::NeedInput);
                    }
                    let magic0 = self.field.first().copied().unwrap_or(0);
                    let magic1 = self.field.get(1).copied().unwrap_or(0);
                    if magic0 != 0x1F || magic1 != 0x8B {
                        if lenient {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Err(self.fail(OxiArcError::invalid_magic(
                            vec![0x1F, 0x8B],
                            vec![magic0, magic1],
                        )));
                    }
                    self.state = WrapState::GzFixed;
                }

                WrapState::GzFixed => {
                    if !self.take_exact(input, in_pos, 10, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let header = std::mem::take(&mut self.field);
                    let cm = header.get(2).copied().unwrap_or(0);
                    if cm != 8 {
                        return Err(self.fail(OxiArcError::unsupported_method(format!(
                            "gzip compression method {}",
                            cm
                        ))));
                    }
                    self.flg = header.get(3).copied().unwrap_or(0);
                    if self.flg & 0xE0 != 0 {
                        return Err(
                            self.fail(OxiArcError::invalid_header("gzip reserved flags set"))
                        );
                    }
                    let mtime = u32::from_le_bytes([
                        header.get(4).copied().unwrap_or(0),
                        header.get(5).copied().unwrap_or(0),
                        header.get(6).copied().unwrap_or(0),
                        header.get(7).copied().unwrap_or(0),
                    ]);
                    self.header = Some(GzipHeaderInfo {
                        name: None,
                        comment: None,
                        extra: None,
                        mtime,
                        xfl: header.get(8).copied().unwrap_or(0),
                        os: header.get(9).copied().unwrap_or(0),
                    });
                    // FHCRC covers every header byte, magic included.
                    self.header_crc = Crc32::new();
                    self.header_crc.update(&header);
                    self.state = if self.flg & 0x04 != 0 {
                        WrapState::GzExtraLen
                    } else {
                        self.after_extra()
                    };
                }

                WrapState::GzExtraLen => {
                    if !self.take_exact(input, in_pos, 2, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    self.header_crc.update(&bytes);
                    self.extra_remaining = u16::from_le_bytes([
                        bytes.first().copied().unwrap_or(0),
                        bytes.get(1).copied().unwrap_or(0),
                    ]) as usize;
                    self.state = WrapState::GzExtra;
                }

                WrapState::GzExtra => {
                    while self.extra_remaining > 0 {
                        let Some(byte) = self.next_byte(input, in_pos) else {
                            if is_finish(flush) {
                                let missing = self.extra_remaining;
                                return Err(self.fail(OxiArcError::unexpected_eof(missing)));
                            }
                            return Ok(InflateStatus::NeedInput);
                        };
                        self.header_crc.update(&[byte]);
                        if self.field.len() < MAX_HEADER_STRING {
                            self.field.push(byte);
                        }
                        self.extra_remaining -= 1;
                    }
                    let extra = std::mem::take(&mut self.field);
                    if let Some(header) = self.header.as_mut() {
                        header.extra = Some(extra);
                    }
                    self.state = self.after_extra();
                }

                WrapState::GzName | WrapState::GzComment => {
                    let is_name = self.state == WrapState::GzName;
                    loop {
                        let Some(byte) = self.next_byte(input, in_pos) else {
                            if is_finish(flush) {
                                return Err(self.fail(OxiArcError::unexpected_eof(1)));
                            }
                            return Ok(InflateStatus::NeedInput);
                        };
                        self.header_crc.update(&[byte]);
                        if byte == 0 {
                            break;
                        }
                        if self.field.len() >= MAX_HEADER_STRING {
                            return Err(self.fail(OxiArcError::invalid_header(
                                "gzip header field exceeds 64 KiB",
                            )));
                        }
                        self.field.push(byte);
                    }
                    let value = std::mem::take(&mut self.field);
                    if let Some(header) = self.header.as_mut() {
                        if is_name {
                            header.name = Some(value);
                        } else {
                            header.comment = Some(value);
                        }
                    }
                    self.state = if is_name {
                        self.after_name()
                    } else {
                        self.after_comment()
                    };
                }

                WrapState::GzHcrc => {
                    // The stored value covers every header byte before it.
                    let expected = self.header_crc.value() & 0xFFFF;
                    if !self.take_exact(input, in_pos, 2, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    let stored = u16::from_le_bytes([
                        bytes.first().copied().unwrap_or(0),
                        bytes.get(1).copied().unwrap_or(0),
                    ]) as u32;
                    if self.verify_header_crc && stored != expected {
                        return Err(self.fail(OxiArcError::crc_mismatch(expected, stored)));
                    }
                    self.state = WrapState::Deflate;
                }

                WrapState::ZlHeader => {
                    if !self.take_exact(input, in_pos, 2, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    let cmf = bytes.first().copied().unwrap_or(0);
                    let flg = bytes.get(1).copied().unwrap_or(0);
                    if cmf & 0x0F != 8 {
                        if self.members_done == 0 && !self.strict_first_member {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Err(self.fail(OxiArcError::invalid_header(
                            "unsupported compression method",
                        )));
                    }
                    if cmf >> 4 > 7 {
                        if self.members_done == 0 && !self.strict_first_member {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Err(self.fail(OxiArcError::invalid_header("invalid window size")));
                    }
                    if ((cmf as u16) * 256 + (flg as u16)) % 31 != 0 {
                        if self.members_done == 0 && !self.strict_first_member {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Err(
                            self.fail(OxiArcError::invalid_header("zlib header check failed"))
                        );
                    }
                    self.flg = flg;
                    self.state = if flg & 0x20 != 0 {
                        WrapState::ZlDictId
                    } else {
                        WrapState::Deflate
                    };
                }

                WrapState::ZlDictId => {
                    if !self.take_exact(input, in_pos, 4, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    let wanted = u32::from_be_bytes([
                        bytes.first().copied().unwrap_or(0),
                        bytes.get(1).copied().unwrap_or(0),
                        bytes.get(2).copied().unwrap_or(0),
                        bytes.get(3).copied().unwrap_or(0),
                    ]);
                    let Some(dictionary) = self.dictionary.clone() else {
                        return Err(self.fail(OxiArcError::unsupported_method(
                            "zlib preset dictionary (FDICT) required but none supplied",
                        )));
                    };
                    let have = self.core.set_dictionary(&dictionary);
                    if have != wanted {
                        return Err(self.fail(OxiArcError::crc_mismatch(wanted, have)));
                    }
                    self.state = WrapState::Deflate;
                }

                WrapState::Deflate => {
                    let status = self.run_deflate(input, in_pos, output, out_pos, flush)?;
                    if !self.core.is_finished() {
                        return Ok(status);
                    }
                    self.state = match self.active {
                        InflateWrapper::Gzip => WrapState::GzTrailer,
                        InflateWrapper::Zlib => WrapState::ZlTrailer,
                        _ => WrapState::Between,
                    };
                    if self.state == WrapState::Between {
                        self.members_done += 1;
                    }
                }

                WrapState::GzTrailer => {
                    if !self.take_exact(input, in_pos, 8, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    let stored_crc = u32::from_le_bytes([
                        bytes.first().copied().unwrap_or(0),
                        bytes.get(1).copied().unwrap_or(0),
                        bytes.get(2).copied().unwrap_or(0),
                        bytes.get(3).copied().unwrap_or(0),
                    ]);
                    let stored_size = u32::from_le_bytes([
                        bytes.get(4).copied().unwrap_or(0),
                        bytes.get(5).copied().unwrap_or(0),
                        bytes.get(6).copied().unwrap_or(0),
                        bytes.get(7).copied().unwrap_or(0),
                    ]);
                    if self.verify_checksum {
                        let computed = self.crc.value();
                        if computed != stored_crc {
                            return Err(self.fail(OxiArcError::crc_mismatch(stored_crc, computed)));
                        }
                        let expected_size = (self.member_out & 0xFFFF_FFFF) as u32;
                        if expected_size != stored_size {
                            return Err(self.fail(OxiArcError::corrupted(
                                self.total_out,
                                format!(
                                    "gzip ISIZE mismatch: stored {}, decoded {}",
                                    stored_size, expected_size
                                ),
                            )));
                        }
                    }
                    self.members_done += 1;
                    self.state = WrapState::Between;
                }

                WrapState::ZlTrailer => {
                    if !self.take_exact(input, in_pos, 4, flush)? {
                        return Ok(InflateStatus::NeedInput);
                    }
                    let bytes = std::mem::take(&mut self.field);
                    let stored = u32::from_be_bytes([
                        bytes.first().copied().unwrap_or(0),
                        bytes.get(1).copied().unwrap_or(0),
                        bytes.get(2).copied().unwrap_or(0),
                        bytes.get(3).copied().unwrap_or(0),
                    ]);
                    if self.verify_checksum {
                        let computed = self.adler.finish();
                        if computed != stored {
                            return Err(self.fail(OxiArcError::crc_mismatch(stored, computed)));
                        }
                    }
                    self.members_done += 1;
                    self.state = WrapState::Between;
                }

                WrapState::Between => {
                    if !self.multi_member {
                        return self.close(input, in_pos, flush);
                    }
                    // Deciding whether another member starts needs **two**
                    // bytes, not one: `CM == 8` alone matches 1 byte in 16,
                    // so a single-byte test turns ordinary trailing garbage
                    // into a header error and defeats `TrailingPolicy`.
                    //
                    // Those bytes may come out of the DEFLATE core's
                    // accumulator — a 4-byte zlib trailer drains a <=7-byte
                    // residue, leaving 1-3 bytes of the next member behind —
                    // so they are buffered in `field`, which the header
                    // states then read before touching `input` again.
                    while self.field.len() < 2 {
                        let Some(byte) = self.next_byte(input, in_pos) else {
                            if is_finish(flush) {
                                // Nothing more is coming; whatever was held
                                // back is trailing data.
                                return self.close(input, in_pos, flush);
                            }
                            return Ok(InflateStatus::NeedInput);
                        };
                        self.field.push(byte);
                    }
                    let first = self.field.first().copied().unwrap_or(0);
                    let second = self.field.get(1).copied().unwrap_or(0);
                    let starts_member = match self.active {
                        InflateWrapper::Gzip => first == 0x1F && second == 0x8B,
                        InflateWrapper::Zlib => zlib_header_is_plausible(first, second),
                        _ => false,
                    };
                    if !starts_member {
                        return self.close(input, in_pos, flush);
                    }
                    self.start_next_member();
                }

                WrapState::Done => return Ok(InflateStatus::StreamEnd),
            }
        }
    }

    /// Run the DEFLATE core for one member, updating the running checksums.
    fn run_deflate(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        output: &mut [u8],
        out_pos: &mut usize,
        flush: FlushMode,
    ) -> Result<InflateStatus> {
        let Some(dst) = output.get_mut(*out_pos..) else {
            return Ok(InflateStatus::NeedOutput);
        };
        let src = input.get(*in_pos..).unwrap_or_default();
        let progress = self.core.inflate(src, dst, flush)?;
        *in_pos += progress.consumed;
        if let Some(fresh) = dst.get(..progress.produced) {
            match self.active {
                InflateWrapper::Gzip => self.crc.update(fresh),
                InflateWrapper::Zlib => self.adler.update(fresh),
                _ => {}
            }
        }
        *out_pos += progress.produced;
        self.member_out += progress.produced as u64;
        self.total_out += progress.produced as u64;
        Ok(progress.status)
    }

    /// The state after `FEXTRA`.
    fn after_extra(&self) -> WrapState {
        if self.flg & 0x08 != 0 {
            WrapState::GzName
        } else {
            self.after_name()
        }
    }

    /// The state after `FNAME`.
    fn after_name(&self) -> WrapState {
        if self.flg & 0x10 != 0 {
            WrapState::GzComment
        } else {
            self.after_comment()
        }
    }

    /// The state after `FCOMMENT`.
    fn after_comment(&self) -> WrapState {
        if self.flg & 0x02 != 0 {
            WrapState::GzHcrc
        } else {
            WrapState::Deflate
        }
    }

    /// Prepare for the next member of a concatenated stream.
    fn start_next_member(&mut self) {
        // MUST preserve the accumulator: a zlib trailer drain can leave 1-3
        // bytes of the next member's header sitting in it.
        self.core.reset_for_next_member();
        self.crc = Crc32::new();
        self.adler = Adler32::new();
        self.header_crc = Crc32::new();
        self.member_out = 0;
        self.state = match self.active {
            InflateWrapper::Gzip => WrapState::GzMagic,
            InflateWrapper::Zlib => WrapState::ZlHeader,
            _ => WrapState::Deflate,
        };
        if self.active == InflateWrapper::Raw {
            if let Some(dictionary) = &self.dictionary {
                self.core.set_dictionary(dictionary);
            }
        }
    }

    /// Apply [`TrailingPolicy`] to whatever follows the last member.
    fn close(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        flush: FlushMode,
    ) -> Result<InflateStatus> {
        match self.trailing {
            TrailingPolicy::Stop => {
                self.field.clear();
                self.state = WrapState::Done;
                Ok(InflateStatus::StreamEnd)
            }
            TrailingPolicy::AllowZeros => {
                // Drain any residue held back at the member boundary first.
                let held = std::mem::take(&mut self.field);
                for byte in held {
                    if byte != 0 {
                        return Err(
                            self.fail(OxiArcError::invalid_magic(vec![0x1F, 0x8B], vec![byte]))
                        );
                    }
                }
                loop {
                    let Some(byte) = self.next_byte(input, in_pos) else {
                        if is_finish(flush) {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        return Ok(InflateStatus::NeedInput);
                    };
                    if byte != 0 {
                        return Err(
                            self.fail(OxiArcError::invalid_magic(vec![0x1F, 0x8B], vec![byte]))
                        );
                    }
                }
            }
            TrailingPolicy::Reject => {
                let held = std::mem::take(&mut self.field);
                if let Some(&byte) = held.first() {
                    return Err(self.fail(OxiArcError::corrupted(
                        self.total_in,
                        format!("trailing byte {:#04x} after the last member", byte),
                    )));
                }
                // Nothing was held back; check the input directly.
                match self.next_byte(input, in_pos) {
                    Some(byte) => Err(self.fail(OxiArcError::corrupted(
                        self.total_in,
                        format!("trailing byte {:#04x} after the last member", byte),
                    ))),
                    None => {
                        if is_finish(flush) {
                            self.state = WrapState::Done;
                            return Ok(InflateStatus::StreamEnd);
                        }
                        Ok(InflateStatus::NeedInput)
                    }
                }
            }
        }
    }
}

/// The starting state for a wrapper.
fn initial_state(wrapper: InflateWrapper) -> WrapState {
    match wrapper {
        InflateWrapper::Raw => WrapState::Deflate,
        InflateWrapper::Zlib => WrapState::ZlHeader,
        InflateWrapper::Gzip => WrapState::GzMagic,
        // `InflateWrapper` is `#[non_exhaustive]`, but it is defined in
        // this crate, so the match is exhaustive here.
        InflateWrapper::Auto => WrapState::Sniff,
    }
}

/// Whether two bytes form a structurally valid RFC 1950 header.
fn zlib_header_is_plausible(cmf: u8, flg: u8) -> bool {
    cmf & 0x0F == 8 && cmf >> 4 <= 7 && ((cmf as u16) * 256 + (flg as u16)) % 31 == 0
}

/// `Full`/`Partial`/anything added later behave as `None`.
#[inline]
fn is_finish(flush: FlushMode) -> bool {
    match flush {
        FlushMode::Finish => true,
        FlushMode::None | FlushMode::Sync | FlushMode::Full | FlushMode::Partial => false,
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{deflate, gzip_compress, zlib_compress};

    fn decode(decoder: &mut WrappedInflate, input: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 97];
        let mut fed = 0usize;
        loop {
            let progress = decoder.inflate(
                input.get(fed..).unwrap_or_default(),
                &mut scratch,
                FlushMode::Finish,
            )?;
            fed += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.status == InflateStatus::StreamEnd {
                return Ok(out);
            }
        }
    }

    #[test]
    fn gzip_and_zlib_round_trip() {
        let payload = b"wrapper round trip".repeat(50);
        let gz = gzip_compress(&payload, 6).expect("gzip");
        let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
        assert_eq!(decode(&mut decoder, &gz).expect("gzip decode"), payload);
        assert_eq!(decoder.members_decoded(), 1);

        let zl = zlib_compress(&payload, 6).expect("zlib");
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib);
        assert_eq!(decode(&mut decoder, &zl).expect("zlib decode"), payload);
    }

    #[test]
    fn auto_sniffs_all_three_framings() {
        let payload = b"sniff me".repeat(30);
        for (name, bytes, expected) in [
            (
                "gzip",
                gzip_compress(&payload, 6).expect("gzip"),
                InflateWrapper::Gzip,
            ),
            (
                "zlib",
                zlib_compress(&payload, 6).expect("zlib"),
                InflateWrapper::Zlib,
            ),
            (
                "raw",
                deflate(&payload, 6).expect("deflate"),
                InflateWrapper::Raw,
            ),
        ] {
            let mut decoder = WrappedInflate::new(InflateWrapper::Auto);
            assert_eq!(decode(&mut decoder, &bytes).expect(name), payload, "{name}");
            assert_eq!(decoder.active_wrapper(), expected, "{name}");
        }
    }

    #[test]
    fn checksum_mismatch_is_reported_and_latched() {
        let mut zl = zlib_compress(b"checksum", 6).expect("zlib");
        let last = zl.len() - 1;
        zl[last] ^= 0xFF;
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib);
        let err = decode(&mut decoder, &zl).expect_err("bad adler");
        assert!(matches!(err, OxiArcError::CrcMismatch { .. }));
        // Latched.
        let again = decoder
            .inflate(&[], &mut [0u8; 8], FlushMode::None)
            .expect_err("latched");
        assert!(again.to_string().contains("CRC"));
    }

    #[test]
    fn verify_checksum_off_still_consumes_the_trailer() {
        let mut zl = zlib_compress(b"ignore adler", 6).expect("zlib");
        let last = zl.len() - 1;
        zl[last] ^= 0xFF;
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib).verify_checksum(false);
        assert_eq!(decode(&mut decoder, &zl).expect("decode"), b"ignore adler");
        assert_eq!(decoder.total_in(), zl.len() as u64);
    }
}
