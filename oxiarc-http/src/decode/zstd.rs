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
}
