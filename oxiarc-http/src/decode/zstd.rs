//! The `zstd` coding (RFC 8878) and its dictionary variant `dcz`
//! (RFC 9842 Compression Dictionary Transport), driven through
//! `oxiarc_zstd::ZstdStream`.
//!
//! Multi-frame concatenation is enabled: RFC 8878 §3.1.1 makes a Zstandard
//! *stream* a sequence of frames, skippable frames are ignored, and real
//! encoders (including `zstd -c a b`) emit several.
//!
//! The window ceiling stays `oxiarc-zstd`'s default of 8 MiB. That is not
//! incidental — it is the largest window an HTTP `zstd` decoder is required
//! to support, so refusing a frame that declares more is both bounded and
//! spec-defensible. A `zstd --long` body (16-128 MiB windows) is rejected
//! with a [`LimitKind::Window`] limit error rather than an allocation.
//!
//! # `dcz`
//!
//! `dcz` is ordinary Zstandard decoded against a shared dictionary the
//! client already holds (RFC 9842). It is only representable when the caller
//! supplies that dictionary — [`Decoder::with_dictionary`](crate::Decoder::with_dictionary)
//! — so a `dcz` response with no dictionary in hand fails at construction
//! with [`HttpCodingError::MissingDictionary`], not halfway through the body.

use oxiarc_core::OxiArcError;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream};

use crate::coding::ContentCoding;
use crate::decode::coding::{CodingDecoder, CodingProgress, CodingStatus, is_finish};
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::limits::DecodeLimits;

/// `oxiarc-zstd`'s own default window ceiling, restated here so the stage
/// can tell a window refusal from an output-budget refusal (both arrive as
/// `OxiArcError::MemoryBudgetExceeded`, distinguished by which configured
/// ceiling the reported `budget` equals).
const MAX_WINDOW: usize = oxiarc_zstd::MAX_WINDOW_SIZE;

/// The `zstd` / `dcz` stage.
#[derive(Debug)]
pub(crate) struct ZstdCodingDecoder {
    coding: ContentCoding,
    inner: ZstdStream,
    max_output: u64,
}

impl ZstdCodingDecoder {
    /// A `zstd` stage bounded by `limits`.
    pub(crate) fn new(limits: &DecodeLimits) -> Self {
        Self::build(ContentCoding::Zstd, limits, None)
    }

    /// A `dcz` stage: `zstd` against a caller-supplied shared dictionary.
    pub(crate) fn with_dictionary(
        coding: ContentCoding,
        limits: &DecodeLimits,
        dictionary: Vec<u8>,
    ) -> Self {
        Self::build(coding, limits, Some(dictionary))
    }

    fn build(coding: ContentCoding, limits: &DecodeLimits, dictionary: Option<Vec<u8>>) -> Self {
        let mut inner = ZstdStream::new()
            .with_multi_frame(true)
            .with_max_window(MAX_WINDOW)
            // Not merely defense in depth here: `oxiarc-zstd` checks a
            // frame's declared `Frame_Content_Size` against this budget
            // *before* decoding, so a bomb that announces itself is refused
            // without any work at all.
            .with_max_output(limits.max_output);
        if let Some(dictionary) = dictionary {
            inner = inner.with_dictionary(dictionary);
        }
        Self {
            coding,
            inner,
            max_output: limits.max_output,
        }
    }

    /// Refuse a body that carried no bytes at all.
    ///
    /// `ZstdStream` accepts an empty input as a clean stream of zero frames,
    /// which is right for a codec (concatenation is associative) and wrong
    /// for HTTP: RFC 8878 §3 makes a Zstandard *stream* one or more frames,
    /// and a response labelled `Content-Encoding: zstd` with no body is
    /// truncated, not empty. The genuinely body-less cases — `HEAD`, 204,
    /// 304, a `Range` response — must never reach a decoder at all; see the
    /// crate docs.
    fn reject_empty_body(&self) -> Result<()> {
        if self.inner.total_in() > 0 {
            return Ok(());
        }
        Err(HttpCodingError::Corrupt {
            coding: self.coding.clone(),
            source: Box::new(OxiArcError::unexpected_eof(4)),
        })
    }

    /// Translate a codec error, keeping resource refusals out of `Corrupt`.
    fn map_error(&self, error: OxiArcError) -> HttpCodingError {
        if let OxiArcError::MemoryBudgetExceeded { budget, requested } = error {
            // `budget` echoes whichever ceiling was crossed. When the two
            // ceilings coincide either label is correct, so prefer the
            // window reading only when it is unambiguous.
            let kind = if budget == MAX_WINDOW && budget as u64 != self.max_output {
                LimitKind::Window {
                    declared: requested as u64,
                }
            } else {
                LimitKind::Output {
                    produced: requested as u64,
                }
            };
            return HttpCodingError::LimitExceeded {
                limit: budget as f64,
                kind,
            };
        }
        HttpCodingError::Corrupt {
            coding: self.coding.clone(),
            source: Box::new(error),
        }
    }
}

impl CodingDecoder for ZstdCodingDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress> {
        let flush = if is_finish(flush) {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = match self.inner.decode(input, output, flush) {
            Ok(progress) => progress,
            Err(error) => return Err(self.map_error(error)),
        };
        let status = match progress.status {
            ZstdStatus::NeedInput => CodingStatus::NeedInput,
            ZstdStatus::NeedOutput => CodingStatus::NeedOutput,
            ZstdStatus::StreamEnd => {
                self.reject_empty_body()?;
                CodingStatus::StreamEnd
            }
            // `ZstdStatus` is `#[non_exhaustive]`; an unknown status must
            // mean "not done yet".
            _ => CodingStatus::NeedInput,
        };
        Ok(CodingProgress {
            consumed: progress.consumed,
            produced: progress.produced,
            status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        match self.inner.finish() {
            Ok(()) => self.reject_empty_body(),
            Err(error) => Err(self.map_error(error)),
        }
    }

    fn coding(&self) -> &ContentCoding {
        &self.coding
    }

    fn unused_input(&self) -> &[u8] {
        self.inner.unused_input()
    }
}

// ── the `dcz` preamble (RFC 9842 §2, zstd variant) ─────────────────────────
//
// A `dcz` body opens with an RFC 8878 §3.1.2 *skippable* frame whose 32-byte
// payload is the SHA-256 of the dictionary — the magic `5E 2A 4D 18`
// (`0x184D2A5E` little-endian; nibble `0xE` of the skippable range) is RFC
// 9842's own reserved value for this, not merely "a" skippable frame. Real
// Zstandard framing carries its own 4-byte `Dictionary_ID` (checked inside
// `oxiarc_zstd::ZstdStream` itself — see `fuzz/fuzz_targets/fuzz_zstd_stream.rs`'s
// finding 1), which is a weaker, non-cryptographic identifier; this preamble
// is the RFC 9842 SHA-256 binding on top of it, and this stage is what
// actually checks it — `ZstdStream`, given `multi_frame(true)`, would
// otherwise skip this frame in silence, the same as any other skippable one.
//
// `dcb` (`decode/dcb.rs`) is the same idea for Brotli, which has no skippable
// frame of its own to reuse, hence a hand-rolled 36-byte preamble there
// against this one's 40 (Zstandard's skippable-frame header itself costs 8:
// 4-byte magic + 4-byte little-endian size).

use oxiarc_core::sha256::{Sha256, hex32};

/// The skippable-frame magic nibble RFC 9842 reserves for `dcz`:
/// `oxiarc_zstd::SKIPPABLE_MAGIC_LOW | 0xE == 0x184D2A5E`, i.e. `5E 2A 4D 18`
/// on the wire.
const DCZ_MAGIC_NIBBLE: u8 = 0x0E;

/// One SHA-256 digest.
const DCZ_HASH_LEN: usize = 32;

/// The full `dcz` preamble: an 8-byte skippable-frame header (4-byte magic,
/// 4-byte little-endian size) plus the 32-byte digest it declares.
const DCZ_HEADER_LEN: usize = 8 + DCZ_HASH_LEN;

fn dcz_corrupt(message: String) -> HttpCodingError {
    HttpCodingError::Corrupt {
        coding: ContentCoding::Dcz,
        source: Box::new(OxiArcError::corrupted(0, message)),
    }
}

/// Build the 40-byte `dcz` preamble naming `dictionary`.
///
/// `pub(crate)`: `encode.rs` calls this so an encoded `dcz` body actually
/// carries what this module's [`DczCodingDecoder`] requires on the way back
/// in.
pub(crate) fn dcz_header(dictionary: &[u8]) -> Vec<u8> {
    oxiarc_zstd::write_skippable_frame(&Sha256::compute(dictionary), DCZ_MAGIC_NIBBLE)
}

/// Verify a complete, exactly-[`DCZ_HEADER_LEN`]-byte `dcz` preamble against
/// `dictionary`.
fn verify_dcz_header(header: &[u8], dictionary: &[u8]) -> Result<()> {
    debug_assert_eq!(header.len(), DCZ_HEADER_LEN);
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let expected_magic = oxiarc_zstd::SKIPPABLE_MAGIC_LOW | u32::from(DCZ_MAGIC_NIBBLE);
    if magic != expected_magic {
        return Err(dcz_corrupt(format!(
            "dcz preamble does not open with the RFC 9842 skippable magic \
             {expected_magic:#010x} (found {magic:#010x})"
        )));
    }
    let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if size as usize != DCZ_HASH_LEN {
        return Err(dcz_corrupt(format!(
            "dcz preamble declares a {size}-byte payload, not the {DCZ_HASH_LEN} \
             a SHA-256 digest needs"
        )));
    }
    let mut named = [0u8; DCZ_HASH_LEN];
    named.copy_from_slice(&header[8..DCZ_HEADER_LEN]);
    let expected = Sha256::compute(dictionary);
    if named != expected {
        return Err(dcz_corrupt(format!(
            "dcz body names dictionary {} but {} was supplied",
            hex32(&named),
            hex32(&expected)
        )));
    }
    Ok(())
}

/// Where a [`DczCodingDecoder`] is in its two phases.
#[derive(Debug)]
enum DczState {
    /// Accumulating the 40-byte preamble; always `< DCZ_HEADER_LEN` bytes —
    /// the instant it reaches that, the state becomes `Stream`.
    Header(Vec<u8>),
    /// Preamble verified; everything from here on is ordinary `zstd`, via
    /// the same [`ZstdCodingDecoder`] the plain `zstd` coding uses. Boxed:
    /// `ZstdStream` is hundreds of bytes, next to a 24-byte `Vec` in the
    /// other arm (`clippy::large_enum_variant`).
    Stream(Box<ZstdCodingDecoder>),
}

/// The `dcz` stage (RFC 9842, Zstandard variant): verify the 40-byte
/// preamble against the caller's dictionary, then decode the rest as `zstd`
/// against that same dictionary.
///
/// Only buildable with a dictionary in hand — see
/// [`Decoder::with_dictionary`](crate::Decoder::with_dictionary) — so this
/// never sits in a "waiting for its dictionary" state; the dictionary is
/// fixed at construction and the preamble is checked against exactly that
/// dictionary the moment it arrives, before a single `zstd` frame byte is
/// decoded.
#[derive(Debug)]
pub(crate) struct DczCodingDecoder {
    /// Moved into the inner [`ZstdCodingDecoder`] once the preamble
    /// verifies, so `None` for the rest of the stream's life.
    dictionary: Option<Vec<u8>>,
    limits: DecodeLimits,
    state: DczState,
}

impl DczCodingDecoder {
    /// A `dcz` stage bounded by `limits`, verified against `dictionary`.
    pub(crate) fn new(limits: &DecodeLimits, dictionary: Vec<u8>) -> Self {
        Self {
            dictionary: Some(dictionary),
            limits: *limits,
            state: DczState::Header(Vec::with_capacity(DCZ_HEADER_LEN)),
        }
    }

    fn short_body_error(len: usize) -> HttpCodingError {
        dcz_corrupt(format!(
            "dcz body of {len} bytes is shorter than its {DCZ_HEADER_LEN}-byte preamble"
        ))
    }
}

impl CodingDecoder for DczCodingDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress> {
        let mut consumed = 0usize;
        if let DczState::Header(buf) = &mut self.state {
            let need = DCZ_HEADER_LEN - buf.len();
            let take = need.min(input.len());
            buf.extend_from_slice(&input[..take]);
            consumed += take;
            if buf.len() < DCZ_HEADER_LEN {
                return if is_finish(flush) {
                    Err(Self::short_body_error(buf.len()))
                } else {
                    Ok(CodingProgress {
                        consumed,
                        produced: 0,
                        status: CodingStatus::NeedInput,
                    })
                };
            }
            // `self.dictionary` is `Some` for exactly as long as `self.state`
            // is `Header` — see the field doc.
            let dictionary = self.dictionary.take().unwrap_or_default();
            verify_dcz_header(buf, &dictionary)?;
            self.state = DczState::Stream(Box::new(ZstdCodingDecoder::with_dictionary(
                ContentCoding::Dcz,
                &self.limits,
                dictionary,
            )));
        }

        // The `Header` arm above always either returns or transitions to
        // `Stream`, so this is never the stale `Header` case; the fallback
        // is defensive, not a real path.
        let DczState::Stream(inner) = &mut self.state else {
            return Ok(CodingProgress {
                consumed,
                produced: 0,
                status: CodingStatus::NeedInput,
            });
        };
        let progress = inner.decode(&input[consumed..], output, flush)?;
        Ok(CodingProgress {
            consumed: consumed + progress.consumed,
            produced: progress.produced,
            status: progress.status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        match &mut self.state {
            DczState::Stream(inner) => inner.finish(),
            DczState::Header(buf) => Err(Self::short_body_error(buf.len())),
        }
    }

    fn coding(&self) -> &ContentCoding {
        static DCZ: ContentCoding = ContentCoding::Dcz;
        &DCZ
    }

    fn unused_input(&self) -> &[u8] {
        match &self.state {
            DczState::Stream(inner) => inner.unused_input(),
            DczState::Header(_) => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(decoder: &mut ZstdCodingDecoder, body: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 64];
        let mut pos = 0usize;
        loop {
            let progress = decoder.decode(&body[pos..], &mut scratch, FlushMode::Finish)?;
            pos += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            match progress.status {
                CodingStatus::StreamEnd => break,
                CodingStatus::NeedOutput => continue,
                CodingStatus::NeedInput => {
                    if progress.consumed == 0 && progress.produced == 0 {
                        break;
                    }
                }
            }
        }
        decoder.finish()?;
        Ok(out)
    }

    #[test]
    fn zstd_stage_round_trips() {
        let body = b"zstandard over http, frame by frame";
        let z = oxiarc_zstd::compress(body).expect("compress");
        let mut d = ZstdCodingDecoder::new(&DecodeLimits::default());
        assert_eq!(drive(&mut d, &z).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Zstd);
    }

    #[test]
    fn an_empty_body_is_not_a_valid_stream() {
        let mut d = ZstdCodingDecoder::new(&DecodeLimits::default());
        drive(&mut d, b"").expect_err("an empty zstd body is truncated, not empty");
    }

    #[test]
    fn multi_frame_bodies_concatenate() {
        let a = oxiarc_zstd::compress(b"first ").expect("compress");
        let b = oxiarc_zstd::compress(b"second").expect("compress");
        let mut joined = a;
        joined.extend_from_slice(&b);
        let mut d = ZstdCodingDecoder::new(&DecodeLimits::default());
        assert_eq!(drive(&mut d, &joined).expect("decode"), b"first second");
    }

    #[test]
    fn truncation_is_an_error() {
        let body = vec![b'z'; 8192];
        let z = oxiarc_zstd::compress(&body).expect("compress");
        let mut d = ZstdCodingDecoder::new(&DecodeLimits::default());
        drive(&mut d, &z[..z.len() - 3]).expect_err("a truncated zstd body must be an error");
    }

    #[test]
    fn a_dictionary_frame_needs_its_dictionary() {
        let dictionary = b"the quick brown fox jumps over the lazy dog".to_vec();
        let body = b"the quick brown fox";
        let mut encoder = oxiarc_zstd::ZstdEncoder::new();
        encoder.set_dictionary(&dictionary);
        let z = encoder.compress(body).expect("compress");

        let mut with = ZstdCodingDecoder::with_dictionary(
            ContentCoding::Dcz,
            &DecodeLimits::default(),
            dictionary,
        );
        assert_eq!(drive(&mut with, &z).expect("decode"), body);
        assert_eq!(with.coding(), &ContentCoding::Dcz);
    }

    // ── DczCodingDecoder: the RFC 9842 preamble on top of the above ────────

    fn drive_dcz(decoder: &mut DczCodingDecoder, body: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 64];
        let mut pos = 0usize;
        loop {
            let progress = decoder.decode(&body[pos..], &mut scratch, FlushMode::Finish)?;
            pos += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            match progress.status {
                CodingStatus::StreamEnd => break,
                CodingStatus::NeedOutput => continue,
                CodingStatus::NeedInput => {
                    if progress.consumed == 0 && progress.produced == 0 {
                        break;
                    }
                }
            }
        }
        decoder.finish()?;
        Ok(out)
    }

    /// Build a real `dcz` wire body: the 40-byte preamble, then an ordinary
    /// dictionary-referencing `zstd` frame.
    fn make_dcz(dictionary: &[u8], body: &[u8]) -> Vec<u8> {
        let mut encoder = oxiarc_zstd::ZstdEncoder::new();
        encoder.set_dictionary(dictionary);
        let mut wire = dcz_header(dictionary);
        wire.extend_from_slice(&encoder.compress(body).expect("compress"));
        wire
    }

    #[test]
    fn header_len_matches_the_skippable_frame_shape() {
        // 8-byte skippable-frame header + 32-byte digest, independent of
        // `oxiarc_zstd::write_skippable_frame`'s own internals.
        assert_eq!(dcz_header(b"x").len(), DCZ_HEADER_LEN);
    }

    #[test]
    fn dcz_stage_round_trips() {
        let dictionary = b"a shared dictionary both ends already have".repeat(4);
        let body = b"dcz over http, decoded incrementally, against a shared dictionary";
        let wire = make_dcz(&dictionary, body);

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        assert_eq!(drive_dcz(&mut d, &wire).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Dcz);
    }

    #[test]
    fn byte_at_a_time_feeding_matches_the_whole_body() {
        let dictionary = b"byte at a time over the preamble boundary too".repeat(4);
        let body = b"a body long enough that the preamble and the frame both split \
                     awkwardly across a one-byte-at-a-time feed";
        let wire = make_dcz(&dictionary, body);

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        let mut out = Vec::new();
        let mut scratch = [0u8; 4096];
        for (index, byte) in wire.iter().enumerate() {
            let flush = if index + 1 == wire.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            loop {
                let progress = d
                    .decode(std::slice::from_ref(byte), &mut scratch, flush)
                    .expect("decode");
                out.extend_from_slice(&scratch[..progress.produced]);
                if progress.consumed > 0 || progress.status != CodingStatus::NeedInput {
                    break;
                }
            }
        }
        d.finish().expect("finish");
        assert_eq!(out, body);
    }

    #[test]
    fn wrong_dictionary_is_rejected_before_any_frame_byte_is_decoded() {
        let dictionary = b"the real dictionary".repeat(4);
        let wrong = b"a completely different dictionary".repeat(4);
        let body = b"secret payload";
        let wire = make_dcz(&dictionary, body);

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), wrong);
        let err = drive_dcz(&mut d, &wire).expect_err("a wrong dictionary must be refused");
        assert!(matches!(err, HttpCodingError::Corrupt { .. }));
    }

    #[test]
    fn a_bare_zstd_frame_with_no_preamble_is_rejected() {
        // Exactly the shape `a_dictionary_frame_needs_its_dictionary` builds
        // above, fed to the *wrapper* instead of the bare `ZstdCodingDecoder`:
        // `ZstdStream` would happily decode it (it is a perfectly ordinary
        // dictionary-referencing frame), which is precisely why the preamble
        // must be checked *before* any of it reaches that decoder.
        let dictionary = b"the quick brown fox jumps over the lazy dog".to_vec();
        let mut encoder = oxiarc_zstd::ZstdEncoder::new();
        encoder.set_dictionary(&dictionary);
        let bare = encoder.compress(b"the quick brown fox").expect("compress");

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive_dcz(&mut d, &bare).expect_err("a dcz body must open with the RFC 9842 preamble");
    }

    #[test]
    fn a_body_shorter_than_the_preamble_is_an_error() {
        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), b"dictionary".to_vec());
        drive_dcz(&mut d, &[0u8; 10]).expect_err("10 bytes is short of the 40-byte preamble");
    }

    #[test]
    fn a_body_that_is_exactly_the_preamble_with_no_frame_is_an_error() {
        // The exact boundary `a_body_shorter_than_the_preamble_is_an_error`
        // (< 40 bytes) and `truncation_inside_the_frame_is_an_error`
        // (len - 2, i.e. inside a real frame) leave unpinned: precisely 40
        // bytes, a complete and valid preamble, with nothing after it at
        // all. `ZstdStream` treats zero frames as a clean empty stream (see
        // `ZstdCodingDecoder::reject_empty_body`'s own doc comment), so
        // without that guard this would silently succeed with an empty
        // body instead of reporting the truncation it actually is.
        let dictionary = b"a shared dictionary, repeated enough to matter".repeat(4);
        let wire = dcz_header(&dictionary);
        assert_eq!(wire.len(), DCZ_HEADER_LEN);

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive_dcz(&mut d, &wire).expect_err(
            "a dcz body that is only the preamble, with no frame after it, must be an error",
        );
    }

    #[test]
    fn an_empty_body_is_an_error() {
        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), b"dictionary".to_vec());
        drive_dcz(&mut d, b"").expect_err("an empty body is not a valid dcz stream");
    }

    #[test]
    fn a_bad_magic_nibble_is_rejected() {
        let dictionary = b"dictionary".repeat(4);
        // A skippable frame with the *wrong* nibble and an otherwise
        // well-formed 32-byte digest payload: some other, unrelated use of
        // the skippable range must not be mistaken for a `dcz` preamble.
        let mut wire = oxiarc_zstd::write_skippable_frame(&Sha256::compute(&dictionary), 0x3);
        wire.extend_from_slice(&make_dcz(&dictionary, b"body")[DCZ_HEADER_LEN..]);
        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive_dcz(&mut d, &wire).expect_err("the wrong skippable nibble must be rejected");
    }

    #[test]
    fn truncation_inside_the_frame_is_an_error() {
        let dictionary = b"a shared dictionary, repeated enough to matter".repeat(4);
        let body = vec![b'q'; 8192];
        let wire = make_dcz(&dictionary, &body);

        let mut d = DczCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive_dcz(&mut d, &wire[..wire.len() - 2])
            .expect_err("a truncated dcz frame must be an error");
    }
}
