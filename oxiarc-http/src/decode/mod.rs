//! [`Decoder`]: the push-style response-body decoder, and [`decode_body`],
//! the one-shot convenience over it.
//!
//! # How the bomb guard actually holds
//!
//! Every stage writes through a [`LimitedSink`], and the sink does not
//! *check* the budget after the fact — it **truncates the output slice** the
//! codec is given to whatever is still permitted. `oxiarc-deflate`'s own
//! bounded sink then enforces that same bound on every literal and every
//! match copy, so a single fixed-Huffman DEFLATE block that expands 812 KB
//! into 123 MiB stops after `max_output` bytes, mid-block, having allocated
//! exactly the budget. `tests/limits.rs` regenerates that stream from a
//! committed Rust generator and asserts it.
//!
//! Enforcing the cap between blocks — the obvious design — would materialise
//! all 123 MiB first.
//!
//! # Chained codings
//!
//! RFC 9110 §8.4 lists content codings in the order they were **applied**,
//! so a decoder applies them in reverse: `Content-Encoding: gzip, br` means
//! br was applied last and must be undone first. Each stage owns a fixed
//! 64 KiB buffer feeding the next, so a chain costs bounded memory rather
//! than one fully materialised intermediate body. The common case — exactly
//! one coding — allocates no such buffer at all.

pub(crate) mod coding;
pub(crate) mod sink;

#[cfg(any(feature = "gzip", feature = "deflate"))]
mod deflate;

#[cfg(feature = "brotli")]
mod brotli;

#[cfg(feature = "brotli")]
mod dcb;

// `pub(crate)`, not private: `encode.rs` calls `zstd::dcz_header` to write
// the same RFC 9842 preamble this module's `DczCodingDecoder` verifies.
#[cfg(feature = "zstd")]
pub(crate) mod zstd;

#[cfg(feature = "compress")]
mod compress;

use oxiarc_core::traits::FlushMode;

use crate::coding::{ContentCoding, unsupported_coding_error};
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::header::parse_content_encoding;
use crate::limits::DecodeLimits;
use coding::{CodingDecoder, CodingStatus, IdentityDecoder, is_finish};
use sink::LimitedSink;

/// Bytes an intermediate stage may hold for the next stage.
///
/// Only chained codings allocate one; a single-coding `Decoder` has none.
const STAGE_BUF: usize = 64 * 1024;

/// Growth step for the `Vec`-shaped conveniences ([`Decoder::feed_into`]).
const VEC_CHUNK: usize = 64 * 1024;

/// Why a [`Decoder::decode`] call returned.
///
/// `#[non_exhaustive]`: match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeStatus {
    /// Everything offered was absorbed and the body is not finished. Feed
    /// more wire bytes.
    NeedInput,
    /// The output slice is full and there is more decoded data waiting.
    /// Call again with room; do not treat this as end of body.
    NeedOutput,
    /// Every coding reached a clean end of stream. Call
    /// [`Decoder::close`] (or [`Decoder::finish`]) to verify trailers.
    StreamEnd,
}

/// What one [`Decoder::decode`] / [`Decoder::feed_into`] call achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Progress {
    /// Wire bytes taken from the front of `input`. Never re-offer these.
    pub consumed: usize,
    /// Decoded bytes written (or appended, for the `Vec` conveniences).
    pub produced: usize,
    /// Why the call returned.
    pub status: DecodeStatus,
}

/// What to do with bytes that follow a complete compressed stream.
///
/// The default is [`Reject`](Self::Reject): a well-formed response body
/// contains exactly one encoded stream, and bytes after it are the shape a
/// response-splitting attack takes. [`AllowZeros`](Self::AllowZeros) is the
/// pragmatic setting for peers that pad gzip with `0x00` (the `gzip` CLI and
/// CPython's `gzip` module both tolerate it, and some CDNs emit it);
/// [`Ignore`](Self::Ignore) discards anything.
///
/// # `br` is always strict
///
/// This policy is honoured for `gzip`, `deflate`, `zstd` and `identity`. It
/// has **no effect on `br`**: RFC 7932 gives a Brotli stream no length field
/// and no checksum, so its final zero-padding check is the only end-of-stream
/// signal there is, and `oxiarc-brotli` treats anything after it as corrupt
/// inside the codec — before this layer ever sees the bytes. A `br` body with
/// trailing data is therefore [`HttpCodingError::Corrupt`], not
/// [`HttpCodingError::TrailingGarbage`], whatever policy is set. That is the
/// strict direction, so no policy is weakened by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TrailingData {
    /// Any trailing byte is [`HttpCodingError::TrailingGarbage`].
    #[default]
    Reject,
    /// Trailing `0x00` padding is tolerated; anything else is an error.
    AllowZeros,
    /// Trailing bytes are discarded silently.
    Ignore,
}

/// One coding in the chain, with its budget and its buffer for the next.
struct Stage {
    decoder: Box<dyn CodingDecoder>,
    sink: LimitedSink,
    /// Decoded output feeding the *next* stage. Empty for the last stage,
    /// which writes straight into the caller's slice.
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// This stage's stream reached its end.
    finished: bool,
    /// Trailing bytes seen after that end.
    trailing: usize,
}

impl Stage {
    fn new(decoder: Box<dyn CodingDecoder>, limits: &DecodeLimits, intermediate: bool) -> Self {
        let coding = decoder.coding().clone();
        Self {
            decoder,
            sink: LimitedSink::new(coding, limits),
            buf: if intermediate {
                vec![0u8; STAGE_BUF]
            } else {
                Vec::new()
            },
            start: 0,
            end: 0,
            finished: false,
            trailing: 0,
        }
    }
}

/// Apply the trailing-data policy to `bytes`.
fn check_trailing(
    policy: TrailingData,
    coding: &ContentCoding,
    count: usize,
    bytes: &[u8],
) -> Result<()> {
    match policy {
        TrailingData::Reject => Err(HttpCodingError::TrailingGarbage {
            coding: coding.clone(),
            count,
        }),
        TrailingData::AllowZeros => {
            if bytes.iter().all(|&b| b == 0) {
                Ok(())
            } else {
                Err(HttpCodingError::TrailingGarbage {
                    coding: coding.clone(),
                    count,
                })
            }
        }
        TrailingData::Ignore => Ok(()),
    }
}

/// A push-style decoder for one or more chained content codings.
///
/// Feed wire bytes as they arrive and take decoded bytes out; call
/// [`finish`](Self::finish) (or [`close`](Self::close)) at end of body to
/// verify checksums, trailers and framing. This is the right shape for
/// `reqwest::Response::chunk()`, a `hyper` body-frame loop, or any
/// callback-driven transport. For a `Read`-shaped source use
/// [`DecodedBody`](crate::DecodedBody); for `AsyncRead`, `AsyncDecodedBody`.
///
/// # A `finish` error means the response is corrupt
///
/// Skipping [`finish`](Self::finish)/[`close`](Self::close) skips gzip's
/// CRC-32 and `ISIZE`, zlib's Adler-32, zstd's XXH64 and every codec's
/// truncation check — i.e. it silently accepts a corrupted or truncated
/// body. When it returns an error, bytes already handed back are **not**
/// trustworthy: a checksum covers content that has necessarily already been
/// streamed out. A caller that must not act on unverified data has to buffer
/// until `finish` succeeds.
///
/// # Example
///
/// ```
/// use oxiarc_http::{ContentCoding, DecodeLimits, Decoder};
///
/// let gz = oxiarc_deflate::gzip_compress(b"hello, world", 6).expect("compress");
/// let mut decoder = Decoder::new(&[ContentCoding::Gzip], &DecodeLimits::default())
///     .expect("gzip is compiled in");
///
/// let mut body = Vec::new();
/// for chunk in gz.chunks(4) {
///     decoder.feed_into(chunk, &mut body).expect("decode");
/// }
/// decoder.finish_into(&mut body).expect("checksums verify");
/// assert_eq!(body, b"hello, world");
/// ```
pub struct Decoder {
    stages: Vec<Stage>,
    codings: Vec<ContentCoding>,
    trailing: TrailingData,
    limits: DecodeLimits,
    /// Kept so [`Decoder::trailing_data`] can rebuild the stages: the
    /// DEFLATE family needs its trailing policy at construction time.
    dictionary: Option<Vec<u8>>,
    closed: bool,
    poisoned: bool,
    /// Lazily allocated staging buffer for the `Vec`-shaped conveniences.
    /// Slice-API callers never allocate it.
    scratch: Vec<u8>,
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("codings", &self.codings)
            .field("trailing", &self.trailing)
            .field("limits", &self.limits)
            .field("closed", &self.closed)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl Decoder {
    /// Build a decoder for a parsed `Content-Encoding` list.
    ///
    /// `codings` is in **application order**, as
    /// [`parse_content_encoding`](crate::parse_content_encoding) returns it;
    /// the stages are applied in reverse (RFC 9110 §8.4).
    /// [`ContentCoding::Identity`] entries are dropped, and an empty (or
    /// all-identity) list yields a pass-through decoder.
    ///
    /// # Errors
    ///
    /// [`HttpCodingError::LimitExceeded`] with [`LimitKind::Codings`] when
    /// more codings are listed than [`DecodeLimits::max_codings`] allows;
    /// [`HttpCodingError::UnsupportedCoding`] for any coding this build
    /// cannot decode — never a silent pass-through, which is the trap that
    /// makes a client hand compressed bytes to a JSON parser;
    /// [`HttpCodingError::MissingDictionary`] for [`ContentCoding::Dcz`],
    /// which needs [`with_dictionary`](Self::with_dictionary).
    pub fn new(codings: &[ContentCoding], limits: &DecodeLimits) -> Result<Self> {
        Self::build(codings, limits, None)
    }

    /// As [`new`](Self::new), supplying the shared dictionary that
    /// [`ContentCoding::Dcz`] (RFC 9842 Compression Dictionary Transport)
    /// requires.
    ///
    /// The dictionary is used only by `dcz`; other codings ignore it.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_dictionary(
        codings: &[ContentCoding],
        limits: &DecodeLimits,
        dictionary: &[u8],
    ) -> Result<Self> {
        Self::build(codings, limits, Some(dictionary))
    }

    /// Parse a `Content-Encoding` header value and build a decoder for it.
    ///
    /// This takes **one** header value. A message that repeats
    /// `Content-Encoding` across several lines is equivalent to the single
    /// comma-joined value (RFC 9110 §5.2), so collect the lines with
    /// [`parse_content_encoding_all`](crate::parse_content_encoding_all) and
    /// pass the result to [`new`](Self::new) instead of calling this once
    /// per line — calling it per line would build one decoder per line and
    /// lose the ordering between them.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), plus any header syntax error.
    ///
    /// # Example
    ///
    /// ```
    /// use oxiarc_http::{DecodeLimits, Decoder};
    ///
    /// let decoder = Decoder::from_header("gzip", &DecodeLimits::default())
    ///     .expect("gzip is compiled in");
    /// assert_eq!(decoder.codings().len(), 1);
    ///
    /// // An absent header is `identity`; so is an explicit one.
    /// let passthrough = Decoder::from_header("identity", &DecodeLimits::default())
    ///     .expect("identity always works");
    /// assert!(passthrough.codings().is_empty());
    /// ```
    pub fn from_header(value: &str, limits: &DecodeLimits) -> Result<Self> {
        let codings = parse_content_encoding(value)?;
        Self::new(&codings, limits)
    }

    /// A decoder that copies input to output unchanged.
    ///
    /// What to use when a response carries no `Content-Encoding` header at
    /// all, so that call sites do not need two code paths.
    ///
    /// Deliberately **unlimited**: a byte-for-byte copy cannot be a
    /// decompression bomb, and capping it at the 64 MiB default would make a
    /// plain 100 MB download fail for no reason. Build with
    /// `Decoder::new(&[], &limits)` instead when a pass-through body should
    /// still be bounded.
    pub fn identity() -> Self {
        let limits = DecodeLimits::unlimited();
        Self {
            stages: vec![Stage::new(Box::new(IdentityDecoder::new()), &limits, false)],
            codings: Vec::new(),
            trailing: TrailingData::default(),
            limits,
            dictionary: None,
            closed: false,
            poisoned: false,
            scratch: Vec::new(),
        }
    }

    /// Build a decoder over one caller-supplied stage.
    ///
    /// Test-only: the point is to drive the adapters against a stage that
    /// misbehaves in ways no real codec does (reporting `NeedOutput` without
    /// ever writing a byte, say), so that the guards which turn such a stage
    /// into a reported error instead of a spin are actually exercised.
    #[cfg(test)]
    pub(crate) fn from_stage(decoder: Box<dyn CodingDecoder>, limits: &DecodeLimits) -> Self {
        Self {
            stages: vec![Stage::new(decoder, limits, false)],
            codings: Vec::new(),
            trailing: TrailingData::default(),
            limits: *limits,
            dictionary: None,
            closed: false,
            poisoned: false,
            scratch: Vec::new(),
        }
    }

    fn build(
        codings: &[ContentCoding],
        limits: &DecodeLimits,
        dictionary: Option<&[u8]>,
    ) -> Result<Self> {
        Self::build_with(codings, limits, dictionary, TrailingData::default())
    }

    fn build_with(
        codings: &[ContentCoding],
        limits: &DecodeLimits,
        dictionary: Option<&[u8]>,
        policy: TrailingData,
    ) -> Result<Self> {
        if codings.len() > limits.max_codings {
            return Err(HttpCodingError::LimitExceeded {
                limit: limits.max_codings as f64,
                kind: LimitKind::Codings {
                    count: codings.len(),
                },
            });
        }
        let kept: Vec<ContentCoding> = codings
            .iter()
            .filter(|c| **c != ContentCoding::Identity)
            .cloned()
            .collect();

        // Decode order is the reverse of application order (RFC 9110 §8.4).
        let mut stages = Vec::with_capacity(kept.len().max(1));
        let count = kept.len();
        for (index, coding) in kept.iter().rev().enumerate() {
            let decoder = build_stage(coding, limits, dictionary, policy)?;
            let intermediate = index + 1 < count;
            stages.push(Stage::new(decoder, limits, intermediate));
        }
        if stages.is_empty() {
            stages.push(Stage::new(Box::new(IdentityDecoder::new()), limits, false));
        }
        Ok(Self {
            stages,
            codings: kept,
            trailing: policy,
            limits: *limits,
            dictionary: dictionary.map(<[u8]>::to_vec),
            closed: false,
            poisoned: false,
            scratch: Vec::new(),
        })
    }

    /// Set the policy for bytes that follow the end of the encoded stream.
    ///
    /// Default [`TrailingData::Reject`].
    ///
    /// # Call this before feeding any data
    ///
    /// The DEFLATE family needs its policy at construction time (see
    /// `decode/deflate.rs`), so this **rebuilds the stages** — there is no
    /// way to carry a half-decoded DEFLATE state across the rebuild.
    ///
    /// On a decoder that has already consumed input, been
    /// [`close`](Self::close)d, or reported an error, the rebuild is
    /// therefore skipped and the decoder is returned unchanged: a policy
    /// that quietly failed to apply is a far smaller failure than a decoder
    /// that silently forgot the first half of a body, or one that
    /// un-poisoned itself after reporting corruption. Debug builds assert
    /// the intended usage rather than letting the no-op pass unnoticed.
    ///
    /// # Example
    ///
    /// ```
    /// use oxiarc_http::{DecodeLimits, Decoder, TrailingData};
    ///
    /// let decoder = Decoder::from_header("identity", &DecodeLimits::default())
    ///     .expect("identity")
    ///     .trailing_data(TrailingData::Ignore);
    /// assert_eq!(decoder.input_len(), 0);
    /// ```
    #[must_use]
    pub fn trailing_data(self, policy: TrailingData) -> Self {
        debug_assert!(
            self.input_len() == 0 && !self.poisoned && !self.closed,
            "Decoder::trailing_data must be called before any data is fed"
        );
        if self.input_len() != 0 || self.poisoned || self.closed {
            return self;
        }
        match Self::build_with(
            &self.codings,
            &self.limits,
            self.dictionary.as_deref(),
            policy,
        ) {
            Ok(rebuilt) => rebuilt,
            // Unreachable: the same inputs already built once, and nothing
            // about the policy can make them fail. Keeping the decoder the
            // caller already has is the right fallback either way.
            Err(_) => self,
        }
    }

    /// The codings this decoder undoes, in application order.
    ///
    /// Empty for a pass-through decoder; `identity` never appears.
    pub fn codings(&self) -> &[ContentCoding] {
        &self.codings
    }

    /// The limits this decoder enforces.
    pub fn limits(&self) -> &DecodeLimits {
        &self.limits
    }

    /// Decoded bytes handed back so far.
    pub fn output_len(&self) -> u64 {
        self.stages.last().map_or(0, |s| s.sink.produced())
    }

    /// Wire bytes consumed so far.
    pub fn input_len(&self) -> u64 {
        self.stages.first().map_or(0, |s| s.sink.consumed())
    }

    /// Whether every coding has reached a clean end of stream.
    ///
    /// Says nothing about trailers: [`close`](Self::close) is what verifies
    /// those.
    pub fn is_finished(&self) -> bool {
        self.stages.last().is_some_and(|s| s.finished)
    }

    /// Decode from `input` into `output`.
    ///
    /// The allocation-free primary API: nothing here grows a buffer, so
    /// memory is whatever the caller's two slices plus each codec's own
    /// window cost.
    ///
    /// Pass [`FlushMode::Finish`] exactly when `input` is the last wire data
    /// that will ever arrive; running short then becomes an error instead of
    /// [`DecodeStatus::NeedInput`], which is what turns a truncated response
    /// into a reported failure rather than a short body.
    ///
    /// # Errors
    ///
    /// [`HttpCodingError::LimitExceeded`], [`HttpCodingError::Corrupt`],
    /// [`HttpCodingError::TrailingGarbage`]. The first error poisons the
    /// decoder: later calls report it again rather than resuming.
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<Progress> {
        self.guard()?;
        match self.pump(input, output, flush) {
            Ok(progress) => Ok(progress),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// Feed wire bytes, appending decoded bytes to `out`.
    ///
    /// The `Vec` convenience over [`decode`](Self::decode): it keeps calling
    /// the slice API until the decoder stops asking for output room, so it
    /// never returns [`DecodeStatus::NeedOutput`]. After the first call it
    /// allocates only when `out` itself has to grow.
    ///
    /// # Errors
    ///
    /// As [`decode`](Self::decode).
    pub fn feed_into(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Progress> {
        self.drive_vec(input, out, FlushMode::None)
    }

    /// As [`feed_into`](Self::feed_into), returning a fresh `Vec`.
    ///
    /// Allocates once per call; prefer `feed_into` in a body loop.
    ///
    /// # Errors
    ///
    /// As [`decode`](Self::decode).
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.feed_into(input, &mut out)?;
        Ok(out)
    }

    /// Signal end of body: flush, then verify checksums, trailers and
    /// framing. Appends any final decoded bytes to `out`.
    ///
    /// **Must be called.** See the type docs.
    ///
    /// # Errors
    ///
    /// [`HttpCodingError::Corrupt`] for a truncated stream or a failed
    /// checksum, [`HttpCodingError::TrailingGarbage`], or a limit error.
    pub fn finish_into(&mut self, out: &mut Vec<u8>) -> Result<usize> {
        let before = out.len();
        self.drive_vec(&[], out, FlushMode::Finish)?;
        self.close()?;
        Ok(out.len() - before)
    }

    /// As [`finish_into`](Self::finish_into), returning the final bytes.
    ///
    /// # Errors
    ///
    /// As [`finish_into`](Self::finish_into).
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.finish_into(&mut out)?;
        Ok(out)
    }

    /// Verify end of stream without producing output.
    ///
    /// The slice-API counterpart of [`finish`](Self::finish), which calls
    /// this after draining. Idempotent: a second call on a decoder that
    /// closed cleanly is `Ok`.
    ///
    /// # Errors
    ///
    /// [`HttpCodingError::Corrupt`] when any stage's stream did not end
    /// cleanly (truncation, bad CRC-32/Adler-32/XXH64, a bad trailer), or
    /// [`HttpCodingError::TrailingGarbage`].
    pub fn close(&mut self) -> Result<()> {
        self.guard()?;
        if self.closed {
            return Ok(());
        }
        // Undrained decoded output is a caller error, not a corrupt body:
        // say so, rather than letting a stage report the truncation it
        // cannot distinguish this from.
        let drained = match self.pump(&[], &mut [], FlushMode::Finish) {
            Ok(progress) => progress,
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        };
        if drained.status == DecodeStatus::NeedOutput {
            self.poisoned = true;
            let coding = self
                .stages
                .last()
                .map_or(ContentCoding::Identity, |s| s.sink.coding().clone());
            return Err(HttpCodingError::Corrupt {
                coding,
                source: Box::new(oxiarc_core::OxiArcError::corrupted(
                    self.output_len(),
                    "decoded output was never drained; call `decode` until it stops \
                     reporting `NeedOutput` before closing",
                )),
            });
        }
        for stage in &mut self.stages {
            if let Err(error) = stage.decoder.finish() {
                self.poisoned = true;
                return Err(error);
            }
        }
        self.closed = true;
        Ok(())
    }

    /// Refuse to keep going after a reported error.
    fn guard(&self) -> Result<()> {
        if !self.poisoned {
            return Ok(());
        }
        let coding = self
            .stages
            .first()
            .map_or(ContentCoding::Identity, |s| s.sink.coding().clone());
        Err(HttpCodingError::Corrupt {
            coding,
            source: Box::new(oxiarc_core::OxiArcError::corrupted(
                self.input_len(),
                "the decoder already reported an error; the response body is not usable",
            )),
        })
    }

    /// Drive [`decode`](Self::decode) until it stops asking for output room,
    /// appending everything through a reused staging buffer.
    fn drive_vec(&mut self, input: &[u8], out: &mut Vec<u8>, flush: FlushMode) -> Result<Progress> {
        if self.scratch.is_empty() {
            self.scratch = vec![0u8; VEC_CHUNK];
        }
        // Borrow the staging buffer out of `self` so the decoder can be
        // driven mutably at the same time; it always goes back.
        let mut scratch = std::mem::take(&mut self.scratch);
        let mut consumed = 0usize;
        let mut produced = 0usize;
        let result = loop {
            match self.decode(&input[consumed..], &mut scratch, flush) {
                Ok(progress) => {
                    consumed += progress.consumed;
                    produced += progress.produced;
                    out.extend_from_slice(&scratch[..progress.produced]);
                    if progress.status != DecodeStatus::NeedOutput {
                        break Ok(Progress {
                            consumed,
                            produced,
                            status: progress.status,
                        });
                    }
                    if progress.produced == 0 && progress.consumed == 0 {
                        // A 64 KiB staging buffer was offered and neither
                        // filled nor consumed: looping again would spin.
                        break Err(HttpCodingError::Corrupt {
                            coding: ContentCoding::Identity,
                            source: Box::new(oxiarc_core::OxiArcError::corrupted(
                                0,
                                "decoder reported `NeedOutput` without using the output buffer",
                            )),
                        });
                    }
                }
                Err(error) => break Err(error),
            }
        };
        self.scratch = scratch;
        result
    }

    /// The chain pump. See the module docs for the invariants.
    fn pump(&mut self, input: &[u8], output: &mut [u8], flush: FlushMode) -> Result<Progress> {
        let policy = self.trailing;
        let stage_count = self.stages.len();
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        // Assigned at the top of every pass; the loop body always runs at
        // least once before any `break`.
        let mut need_output;

        loop {
            let mut progressed = false;
            need_output = false;

            for index in 0..stage_count {
                let (left, right) = self.stages.split_at_mut(index);
                let Stage {
                    decoder,
                    sink,
                    buf,
                    start,
                    end,
                    finished,
                    trailing,
                } = &mut right[0];
                let is_last = index + 1 == stage_count;

                // Source view: the caller's slice for stage 0, the previous
                // stage's buffer otherwise.
                let (src, src_eof): (&[u8], bool) = match left.last() {
                    None => (&input[in_pos..], is_finish(flush)),
                    Some(prev) => (
                        &prev.buf[prev.start..prev.end],
                        prev.finished && prev.start == prev.end,
                    ),
                };

                if *finished {
                    if !src.is_empty() {
                        let count = src.len();
                        *trailing += count;
                        check_trailing(policy, sink.coding(), *trailing, src)?;
                        if index == 0 {
                            in_pos += count;
                        } else if let Some(prev) = left.last_mut() {
                            prev.start += count;
                            if prev.start == prev.end {
                                prev.start = 0;
                                prev.end = 0;
                            }
                        }
                        progressed = true;
                    }
                    continue;
                }

                let stage_flush = if src_eof {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };

                // Compact an intermediate buffer whose free space has all
                // migrated to the front. Nothing in the pump *needs* this —
                // the next stage resets `start`/`end` whenever it drains a
                // buffer completely — but without it a stage that is
                // repeatedly drained only partially could sit at
                // `end == buf.len()` with room going unused.
                if !is_last && *end == buf.len() && *start > 0 {
                    buf.copy_within(*start..*end, 0);
                    *end -= *start;
                    *start = 0;
                }

                let (space_all, progress) = if is_last {
                    let space_all = output.len() - out_pos;
                    let space = sink.allow(space_all);
                    let progress =
                        decoder.decode(src, &mut output[out_pos..out_pos + space], stage_flush)?;
                    (space_all, progress)
                } else {
                    let space_all = buf.len() - *end;
                    let space = sink.allow(space_all);
                    let progress =
                        decoder.decode(src, &mut buf[*end..*end + space], stage_flush)?;
                    (space_all, progress)
                };

                sink.commit(progress.produced, progress.consumed)?;
                if is_last {
                    out_pos += progress.produced;
                } else {
                    *end += progress.produced;
                }
                if index == 0 {
                    in_pos += progress.consumed;
                } else if let Some(prev) = left.last_mut() {
                    prev.start += progress.consumed;
                    if prev.start == prev.end {
                        prev.start = 0;
                        prev.end = 0;
                    }
                }
                if progress.consumed > 0 || progress.produced > 0 {
                    progressed = true;
                }

                match progress.status {
                    CodingStatus::StreamEnd => {
                        *finished = true;
                        progressed = true;
                        let unused = decoder.unused_input();
                        if !unused.is_empty() {
                            *trailing += unused.len();
                            check_trailing(policy, sink.coding(), *trailing, unused)?;
                        }
                    }
                    CodingStatus::NeedOutput => {
                        // The budget is the binding constraint only when the
                        // sink allowed nothing *and* real space was offered:
                        // then the codec answered "more output pending"
                        // against a zero-length slice, which is unambiguous.
                        if sink.budget() == 0 && space_all > 0 {
                            return Err(sink.overflow());
                        }
                        if is_last {
                            need_output = true;
                        }
                    }
                    CodingStatus::NeedInput => {}
                }
            }

            if !progressed {
                break;
            }
        }

        let finished = self.is_finished();
        let status = if finished {
            DecodeStatus::StreamEnd
        } else if need_output {
            DecodeStatus::NeedOutput
        } else {
            DecodeStatus::NeedInput
        };

        if !finished && !need_output && in_pos < input.len() {
            // Nothing consumed it, nothing is waiting on room: the chain is
            // stuck. An `Err` here, rather than a `debug_assert`, is what
            // stops a caller's read loop from spinning forever on a crafted
            // body.
            let coding = self
                .stages
                .first()
                .map_or(ContentCoding::Identity, |s| s.sink.coding().clone());
            return Err(HttpCodingError::Corrupt {
                coding,
                source: Box::new(oxiarc_core::OxiArcError::corrupted(
                    self.input_len(),
                    "content-coding decoder made no progress on available input",
                )),
            });
        }

        Ok(Progress {
            consumed: in_pos,
            produced: out_pos,
            status,
        })
    }
}

/// Build the stage for one coding, or explain why this build cannot.
fn build_stage(
    coding: &ContentCoding,
    limits: &DecodeLimits,
    dictionary: Option<&[u8]>,
    policy: TrailingData,
) -> Result<Box<dyn CodingDecoder>> {
    match coding {
        ContentCoding::Identity => Ok(Box::new(IdentityDecoder::new())),

        #[cfg(feature = "gzip")]
        ContentCoding::Gzip => Ok(Box::new(deflate::DeflateFamilyDecoder::gzip(
            limits, policy,
        ))),

        #[cfg(feature = "deflate")]
        ContentCoding::Deflate => Ok(Box::new(deflate::DeflateFamilyDecoder::deflate(
            limits, policy,
        ))),

        #[cfg(feature = "brotli")]
        ContentCoding::Brotli => Ok(Box::new(brotli::BrotliCodingDecoder::new(limits))),

        #[cfg(feature = "zstd")]
        ContentCoding::Zstd => Ok(Box::new(zstd::ZstdCodingDecoder::new(limits))),

        // `compress` / `x-compress`: legacy UNIX `.Z`, bridged from
        // `oxiarc_lzw::z::ZReader`'s pull shape onto this push seam; see
        // `decode/compress.rs`.
        #[cfg(feature = "compress")]
        ContentCoding::Compress => Ok(Box::new(compress::CompressCodingDecoder::new(limits))),

        // `dcz` verifies the RFC 9842 preamble (a zstd skippable frame
        // carrying the dictionary's SHA-256) before a single frame byte
        // reaches `ZstdStream`; see `decode/zstd.rs`'s `DczCodingDecoder`.
        #[cfg(feature = "zstd")]
        ContentCoding::Dcz => match dictionary {
            Some(dictionary) => Ok(Box::new(zstd::DczCodingDecoder::new(
                limits,
                dictionary.to_vec(),
            ))),
            None => Err(HttpCodingError::MissingDictionary {
                coding: ContentCoding::Dcz,
            }),
        },

        // `dcb` verifies `oxiarc_brotli::dcb`'s 36-byte preamble (magic +
        // the dictionary's SHA-256) before a single Brotli byte is decoded;
        // see `decode/dcb.rs`.
        #[cfg(feature = "brotli")]
        ContentCoding::Dcb => match dictionary {
            Some(dictionary) => Ok(Box::new(dcb::DcbCodingDecoder::new(
                limits,
                dictionary.to_vec(),
            ))),
            None => Err(HttpCodingError::MissingDictionary {
                coding: ContentCoding::Dcb,
            }),
        },

        other => {
            let _ = (limits, dictionary, policy);
            Err(unsupported_coding_error(other))
        }
    }
}

/// Decode a complete in-memory body.
///
/// `codings` is in application order, as
/// [`parse_content_encoding`](crate::parse_content_encoding) returns it.
///
/// # Errors
///
/// As [`Decoder::new`](Decoder::new) and [`Decoder::decode`], plus
/// [`HttpCodingError::Corrupt`] for a truncated or corrupt body — a
/// complete body that fails its checksum is an error here, never a silent
/// short result.
///
/// # Example
///
/// ```
/// use oxiarc_http::{ContentCoding, DecodeLimits, decode_body};
///
/// let gz = oxiarc_deflate::gzip_compress(b"hello, world", 6).expect("compress");
/// let body = decode_body(&[ContentCoding::Gzip], &gz, &DecodeLimits::default())
///     .expect("decode");
/// assert_eq!(body, b"hello, world");
/// ```
pub fn decode_body(
    codings: &[ContentCoding],
    body: &[u8],
    limits: &DecodeLimits,
) -> Result<Vec<u8>> {
    let mut decoder = Decoder::new(codings, limits)?;
    let mut out = Vec::new();
    decoder.feed_into(body, &mut out)?;
    decoder.finish_into(&mut out)?;
    Ok(out)
}

/// Decode a complete in-memory body, parsing the `Content-Encoding` header
/// value directly.
///
/// The shape most call sites want: a response's header value is a `&str`,
/// and an absent header is `"identity"`.
///
/// # Errors
///
/// As [`decode_body`], plus any header syntax error.
///
/// # Example
///
/// ```
/// use oxiarc_http::{DecodeLimits, decode_body_from_header};
///
/// let gz = oxiarc_deflate::gzip_compress(b"hello, world", 6).expect("compress");
/// let body = decode_body_from_header("gzip", &gz, &DecodeLimits::default())
///     .expect("decode");
/// assert_eq!(body, b"hello, world");
///
/// // No `Content-Encoding` header at all:
/// let plain = decode_body_from_header("identity", b"raw", &DecodeLimits::default())
///     .expect("identity");
/// assert_eq!(plain, b"raw");
/// ```
pub fn decode_body_from_header(
    content_encoding: &str,
    body: &[u8],
    limits: &DecodeLimits,
) -> Result<Vec<u8>> {
    let codings = parse_content_encoding(content_encoding)?;
    decode_body(&codings, body, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_decoder_passes_bytes_through() {
        let mut d = Decoder::identity();
        let mut out = Vec::new();
        d.feed_into(b"hello ", &mut out).expect("feed");
        d.feed_into(b"world", &mut out).expect("feed");
        d.finish_into(&mut out).expect("finish");
        assert_eq!(out, b"hello world");
        assert!(d.is_finished());
        assert!(d.codings().is_empty());
        assert_eq!(d.output_len(), 11);
    }

    #[test]
    fn an_empty_coding_list_is_a_pass_through() {
        let d = Decoder::new(&[], &DecodeLimits::default()).expect("empty list");
        assert!(d.codings().is_empty());
        let out = decode_body(&[], b"plain", &DecodeLimits::default()).expect("decode");
        assert_eq!(out, b"plain");
    }

    #[test]
    fn identity_entries_are_dropped() {
        let d = Decoder::new(&[ContentCoding::Identity], &DecodeLimits::default())
            .expect("identity always works");
        assert!(d.codings().is_empty());
    }

    #[test]
    fn too_many_codings_is_a_limit_error() {
        let codings = vec![ContentCoding::Gzip; 5];
        let limits = DecodeLimits::default().with_max_codings(4);
        let err = Decoder::new(&codings, &limits).expect_err("5 > 4");
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Codings { count: 5 },
                ..
            }
        ));
    }

    #[test]
    fn an_undecodable_coding_never_passes_through_silently() {
        let err = Decoder::new(
            &[ContentCoding::Unknown("shrink-o-matic".into())],
            &DecodeLimits::default(),
        )
        .expect_err("unknown codings must be refused");
        assert!(matches!(err, HttpCodingError::UnsupportedCoding { .. }));
    }

    // `compress` and `dcb` used to be permanently unsupported; both are now
    // real, feature-gated codings — see `tests/dictionary.rs` for `dcb`'s
    // "with/without a dictionary, with/without the feature" coverage
    // (mirroring `dcz`'s own pattern there) and `decode::compress::tests`
    // for `compress`'s. This crate's own DoD runs `--features compress`
    // alone, so both branches below are exercised by CI either way.
    #[cfg(not(feature = "compress"))]
    #[test]
    fn compress_is_unsupported_without_its_feature() {
        let err = Decoder::new(&[ContentCoding::Compress], &DecodeLimits::default())
            .expect_err("compress needs the `compress` feature");
        assert!(matches!(err, HttpCodingError::UnsupportedCoding { .. }));
    }

    #[cfg(feature = "compress")]
    #[test]
    fn compress_decodes_a_real_dot_z_body() {
        let plain = b"compress, wired all the way through Decoder::new".repeat(8);
        let wire = oxiarc_lzw::z::compress(&plain, 16).expect("compress");
        let out = decode_body(&[ContentCoding::Compress], &wire, &DecodeLimits::default())
            .expect("compress now decodes for real");
        assert_eq!(out, plain);
    }

    #[test]
    fn a_pass_through_decoder_is_unlimited_but_a_built_one_is_not() {
        assert_eq!(Decoder::identity().limits().max_output, u64::MAX);
        let bounded =
            Decoder::new(&[], &DecodeLimits::default().with_max_output(4)).expect("empty list");
        assert_eq!(bounded.limits().max_output, 4);
    }

    #[test]
    fn a_capped_pass_through_still_refuses_an_over_long_body() {
        let mut d =
            Decoder::new(&[], &DecodeLimits::default().with_max_output(4)).expect("empty list");
        let err = d
            .feed(b"more than four bytes")
            .expect_err("the cap applies to identity too when asked for");
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Output { .. },
                ..
            }
        ));
    }

    #[test]
    fn an_error_poisons_the_decoder() {
        let mut d =
            Decoder::new(&[], &DecodeLimits::default().with_max_output(2)).expect("empty list");
        d.feed(b"abcdef").expect_err("over the cap");
        let err = d
            .feed(b"more")
            .expect_err("a poisoned decoder must stay poisoned");
        assert!(matches!(err, HttpCodingError::Corrupt { .. }));
        assert!(d.close().is_err());
    }

    #[test]
    fn progress_reports_consumed_produced_and_status() {
        let mut d = Decoder::identity();
        let mut out = [0u8; 3];
        let p = d
            .decode(b"abcdef", &mut out, FlushMode::None)
            .expect("identity");
        assert_eq!(p.consumed, 3);
        assert_eq!(p.produced, 3);
        assert_eq!(p.status, DecodeStatus::NeedOutput);

        let p = d
            .decode(b"def", &mut out, FlushMode::Finish)
            .expect("identity");
        assert_eq!(p.status, DecodeStatus::StreamEnd);
        assert_eq!(d.input_len(), 6);
        assert_eq!(d.output_len(), 6);
    }

    #[test]
    fn a_decoder_can_move_between_threads() {
        // A `reqwest` body loop routinely moves the decoder onto a task, so
        // `CodingDecoder: Send` is load-bearing, not decorative.
        fn assert_send<T: Send>() {}
        assert_send::<Decoder>();

        let mut decoder = Decoder::identity();
        let handle = std::thread::spawn(move || {
            let mut out = decoder.feed(b"moved").expect("identity");
            out.extend(decoder.finish().expect("finish"));
            out
        });
        assert_eq!(handle.join().expect("join"), b"moved");
    }

    #[test]
    fn trailing_data_policy_is_configurable() {
        for policy in [
            TrailingData::Reject,
            TrailingData::AllowZeros,
            TrailingData::Ignore,
        ] {
            let d = Decoder::identity().trailing_data(policy);
            assert!(d.codings().is_empty());
            assert_eq!(d.input_len(), 0);
        }
        assert_eq!(TrailingData::default(), TrailingData::Reject);
    }

    /// `trailing_data` rebuilds the stages, so calling it mid-body would
    /// throw away everything decoded so far. Debug builds shout about it.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "before any data is fed")]
    fn trailing_data_after_feeding_is_caught_in_debug_builds() {
        let mut decoder = Decoder::identity();
        let _ = decoder.feed(b"already started").expect("identity");
        let _ = decoder.trailing_data(TrailingData::Ignore);
    }

    /// And with assertions compiled out it is a no-op, never a silent reset:
    /// losing the policy change beats losing the first half of the body.
    #[cfg(not(debug_assertions))]
    #[test]
    fn trailing_data_after_feeding_is_a_no_op_in_release_builds() {
        let mut decoder = Decoder::identity();
        let _ = decoder.feed(b"already started").expect("identity");
        let consumed = decoder.input_len();
        assert_ne!(consumed, 0);
        let decoder = decoder.trailing_data(TrailingData::Ignore);
        assert_eq!(
            decoder.input_len(),
            consumed,
            "decode state was silently discarded"
        );
    }
}
