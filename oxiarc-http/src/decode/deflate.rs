//! The DEFLATE-family codings: `gzip`/`x-gzip` (RFC 1952) and `deflate`
//! (RFC 9110 §8.4.1.2), both driven through `oxiarc_deflate::WrappedInflate`.
//!
//! # `gzip`
//!
//! `WrappedInflate` in [`InflateWrapper::Gzip`] mode with
//! `multi_member(true)` — RFC 1952 §2.2 makes a gzip stream a *series* of
//! members and `gzip -c a b`, `pigz` and `oxiarc`'s own
//! `compress_gzip_parallel` all emit several — and
//! `strict_first_member(true)`: unlike the legacy `GzipStreamDecoder` (which
//! must keep returning an empty successful read on non-gzip leading bytes,
//! Phase 8 owner decision #2), an HTTP response that claims `gzip` and is
//! not gzip is a protocol error, never an empty body. `FHCRC` is verified
//! and so are the trailing CRC-32 and `ISIZE`.
//!
//! # `deflate`
//!
//! [`InflateWrapper::Auto`], which sniffs the first two bytes exactly once,
//! at offset 0: gzip magic, else a valid zlib header (`CM == 8`,
//! `CINFO <= 7`, `(CMF*256 + FLG) % 31 == 0`), else raw RFC 1951. That
//! single wrapper covers all three real-world spellings of
//! `Content-Encoding: deflate`:
//!
//! * zlib-wrapped, which is what RFC 9110 §8.4.1.2 actually defines;
//! * raw, which §8.4.1.2 explicitly sanctions accepting ("some
//!   non-conformant implementations send the 'deflate' compressed data
//!   without the zlib wrapper" — historically IIS);
//! * a whole gzip stream mislabelled `deflate`, which browsers accept and
//!   which costs two bytes of lookahead.
//!
//! The sniff has a known, documented blind spot: a *raw* stream whose first
//! two bytes happen to satisfy the zlib header test is read as zlib. It
//! cannot arise from a real encoder — a non-final stored block starts
//! `0x00`, whose low nibble is 0, not the 8 a zlib `CM` needs — so it takes
//! a hand-crafted stream to hit, and `tests/framing.rs`
//! (`a_raw_stream_crafted_to_look_like_a_zlib_header_is_read_as_zlib`)
//! contains one, pinning the behaviour rather than papering over it. Name the
//! coding `gzip` when the framing is known.

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateWrapper, TrailingPolicy, WrappedInflate};

use crate::coding::ContentCoding;
use crate::decode::TrailingData;
use crate::decode::coding::{
    CodingDecoder, CodingProgress, CodingStatus, is_finish, map_core_error,
};
use crate::error::{HttpCodingError, Result};
use crate::limits::DecodeLimits;

/// A `gzip` or `deflate` stage over one `WrappedInflate`.
#[derive(Debug)]
pub(crate) struct DeflateFamilyDecoder {
    coding: ContentCoding,
    inner: WrappedInflate,
}

impl DeflateFamilyDecoder {
    /// A `gzip` / `x-gzip` stage: multi-member, strict at member 0, header
    /// CRC and trailer verified.
    #[cfg(feature = "gzip")]
    pub(crate) fn gzip(limits: &DecodeLimits, policy: TrailingData) -> Self {
        Self::build(ContentCoding::Gzip, InflateWrapper::Gzip, limits, policy)
    }

    /// A `deflate` stage: zlib / raw / mislabelled-gzip, decided at offset 0.
    #[cfg(feature = "deflate")]
    pub(crate) fn deflate(limits: &DecodeLimits, policy: TrailingData) -> Self {
        Self::build(ContentCoding::Deflate, InflateWrapper::Auto, limits, policy)
    }

    fn build(
        coding: ContentCoding,
        wrapper: InflateWrapper,
        limits: &DecodeLimits,
        policy: TrailingData,
    ) -> Self {
        // The trailing-data policy is pushed **into** the container decoder
        // rather than applied by the chain afterwards. It has to be: deciding
        // whether a second member starts costs a two-byte lookahead, and
        // `TrailingPolicy::Stop` discards those two bytes, so a chain-level
        // check would never see a one- or two-byte tail at all. Handing the
        // real policy down makes the wrapper — which does see them — enforce
        // it, and `map_error` translates its verdict back into this crate's
        // `TrailingGarbage`.
        let trailing = match policy {
            TrailingData::Reject => TrailingPolicy::Reject,
            TrailingData::AllowZeros => TrailingPolicy::AllowZeros,
            TrailingData::Ignore => TrailingPolicy::Stop,
        };
        let inner = WrappedInflate::new(wrapper)
            .multi_member(true)
            .strict_first_member(true)
            .verify_header_crc(true)
            .verify_checksum(true)
            .trailing_policy(trailing)
            // Defense in depth only: `LimitedSink` already truncates the
            // output slice to the remaining budget, so this can never be the
            // first guard to fire. It exists so a future refactor that loses
            // the truncation still cannot run away.
            .with_max_output(limits.max_output);
        Self { coding, inner }
    }

    /// Translate a codec error, recovering this crate's own
    /// [`HttpCodingError::TrailingGarbage`] from the container's verdict.
    ///
    /// Only two error shapes can mean "what follows the last complete member
    /// is not an acceptable continuation", and only once at least one member
    /// has decoded: a rejected trailing byte, and a failed member magic.
    /// Truncation (`UnexpectedEof`) and a bad checksum (`CrcMismatch`) are
    /// distinct variants, so neither is mistaken for trailing data.
    fn map_error(&self, error: oxiarc_core::OxiArcError) -> HttpCodingError {
        if self.inner.members_decoded() >= 1 {
            let is_trailing = match &error {
                oxiarc_core::OxiArcError::InvalidMagic { .. } => true,
                oxiarc_core::OxiArcError::CorruptedData { message, .. } => {
                    message.starts_with("trailing byte")
                }
                _ => false,
            };
            if is_trailing {
                return HttpCodingError::TrailingGarbage {
                    coding: self.coding.clone(),
                    // At least this many; the container reports the first
                    // offending byte, not a total it never counted.
                    count: 1,
                };
            }
        }
        map_core_error(&self.coding, error)
    }
}

impl CodingDecoder for DeflateFamilyDecoder {
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
        let progress = self
            .inner
            .inflate(input, output, flush)
            .map_err(|e| self.map_error(e))?;
        let status = match progress.status {
            InflateStatus::NeedInput => CodingStatus::NeedInput,
            InflateStatus::NeedOutput => CodingStatus::NeedOutput,
            InflateStatus::StreamEnd => CodingStatus::StreamEnd,
            // `InflateStatus` is `#[non_exhaustive]`: an unknown future
            // status must mean "not done yet", which the chain resolves by
            // offering more input rather than by declaring success.
            _ => CodingStatus::NeedInput,
        };
        Ok(CodingProgress {
            consumed: progress.consumed,
            produced: progress.produced,
            status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        if self.inner.is_finished() {
            return Ok(());
        }
        // The chain pumps every stage dry before calling this, so a
        // zero-length output slice is enough to reach the end of the
        // container. A stream that stops mid-member reports
        // `UnexpectedEof` here, which is exactly the "truncated response"
        // signal HTTP callers need.
        let progress = self
            .inner
            .inflate(&[], &mut [], FlushMode::Finish)
            .map_err(|e| self.map_error(e))?;
        if progress.status == InflateStatus::StreamEnd || self.inner.is_finished() {
            return Ok(());
        }
        Err(HttpCodingError::Corrupt {
            coding: self.coding.clone(),
            source: Box::new(oxiarc_core::OxiArcError::corrupted(
                self.inner.total_in(),
                format!("{} stream ended before its last member", self.coding),
            )),
        })
    }

    fn coding(&self) -> &ContentCoding {
        &self.coding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(decoder: &mut DeflateFamilyDecoder, body: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 256];
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

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_stage_round_trips() {
        let body = b"the quick brown fox jumps over the lazy dog";
        let gz = oxiarc_deflate::gzip_compress(body, 6).expect("compress");
        let mut d = DeflateFamilyDecoder::gzip(&DecodeLimits::default(), TrailingData::Reject);
        assert_eq!(drive(&mut d, &gz).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Gzip);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_stage_rejects_a_non_gzip_first_member() {
        let zlib = oxiarc_deflate::zlib_compress(b"not gzip", 6).expect("compress");
        let mut d = DeflateFamilyDecoder::gzip(&DecodeLimits::default(), TrailingData::Reject);
        drive(&mut d, &zlib).expect_err("strict_first_member(true) must reject");
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_stage_rejects_an_empty_body() {
        let mut d = DeflateFamilyDecoder::gzip(&DecodeLimits::default(), TrailingData::Reject);
        drive(&mut d, b"").expect_err("an empty body is not a valid gzip stream");
    }

    #[cfg(feature = "deflate")]
    #[test]
    fn deflate_stage_accepts_zlib_raw_and_gzip() {
        let body = b"servers spell `deflate` three different ways";
        let zlib = oxiarc_deflate::zlib_compress(body, 6).expect("compress");
        let raw = oxiarc_deflate::deflate(body, 6).expect("compress");
        let gz = oxiarc_deflate::gzip_compress(body, 6).expect("compress");
        for stream in [zlib, raw, gz] {
            let mut d =
                DeflateFamilyDecoder::deflate(&DecodeLimits::default(), TrailingData::Reject);
            assert_eq!(drive(&mut d, &stream).expect("decode"), body);
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn truncation_is_an_error_not_a_short_read() {
        let body = vec![b'x'; 4096];
        let gz = oxiarc_deflate::gzip_compress(&body, 6).expect("compress");
        let mut d = DeflateFamilyDecoder::gzip(&DecodeLimits::default(), TrailingData::Reject);
        drive(&mut d, &gz[..gz.len() - 4]).expect_err("a truncated member must be an error");
    }
}
