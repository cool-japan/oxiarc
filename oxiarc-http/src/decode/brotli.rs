//! The `br` coding (RFC 7932), driven through `oxiarc_brotli::BrotliStream`.
//!
//! Brotli carries no checksum and no declared output length, so
//! [`CodingDecoder::finish`] can only assert that the final meta-block and
//! its zero padding were seen — which is exactly what
//! `BrotliStream::finish` does, and is enough to turn a truncated response
//! into an error instead of a short body.
//!
//! The window ceiling is `oxiarc-brotli`'s own default (16 MiB, the largest
//! an RFC 7932 `WBITS` field can name), checked against the stream header
//! *before* a single window byte is allocated. A `br` body is therefore
//! bounded in memory by that window plus this crate's staging buffers, no
//! matter what the peer declares.

use oxiarc_brotli::{BrotliError, BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

use crate::coding::ContentCoding;
use crate::decode::coding::{CodingDecoder, CodingProgress, CodingStatus, is_finish};
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::limits::DecodeLimits;

/// Translate a codec error for the plain `br` stage.
fn map_error(error: BrotliError) -> HttpCodingError {
    map_error_for(&ContentCoding::Brotli, error)
}

/// Translate a codec error, keeping resource refusals out of `Corrupt`.
///
/// `pub(crate)` and coding-parameterised, not private and hard-wired to
/// `br`: [`super::dcb::DcbCodingDecoder`] shares this —
/// `verify_dcb_header`/`BrotliStream::decode` report the same [`BrotliError`],
/// so a bad dictionary digest ([`BrotliError::DictionaryError`]) and a bad
/// meta-block both fall through to the same [`HttpCodingError::Corrupt`] arm
/// below, exactly as a plain `br` body's corruption does — but the
/// [`Corrupt::coding`](HttpCodingError::Corrupt) it names has to be the
/// coding the *response* declared, `dcb` or `br`, since that field is a
/// public part of the error a caller matches on to say which coding in the
/// chain failed.
pub(crate) fn map_error_for(coding: &ContentCoding, error: BrotliError) -> HttpCodingError {
    match error {
        BrotliError::WindowTooLarge { declared, max } => HttpCodingError::LimitExceeded {
            limit: max as f64,
            kind: LimitKind::Window {
                declared: declared as u64,
            },
        },
        BrotliError::MemoryBudgetExceeded { budget, requested } => HttpCodingError::LimitExceeded {
            limit: budget as f64,
            kind: LimitKind::Output {
                produced: requested as u64,
            },
        },
        BrotliError::OutputTooLarge(produced) => HttpCodingError::LimitExceeded {
            limit: produced as f64,
            kind: LimitKind::Output {
                produced: produced as u64,
            },
        },
        other => HttpCodingError::Corrupt {
            coding: coding.clone(),
            source: Box::new(other),
        },
    }
}

/// The `br` stage.
#[derive(Debug)]
pub(crate) struct BrotliCodingDecoder {
    inner: BrotliStream,
}

impl BrotliCodingDecoder {
    /// A `br` stage bounded by `limits`.
    pub(crate) fn new(limits: &DecodeLimits) -> Self {
        Self {
            // Defense in depth, as for the DEFLATE family: `LimitedSink`
            // truncates the output slice, so this cap cannot be the first
            // guard to fire.
            inner: BrotliStream::new().with_max_output(limits.max_output),
        }
    }
}

impl CodingDecoder for BrotliCodingDecoder {
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
        let progress = self.inner.decode(input, output, flush).map_err(map_error)?;
        let status = match progress.status {
            BrotliStatus::NeedInput => CodingStatus::NeedInput,
            BrotliStatus::NeedOutput => CodingStatus::NeedOutput,
            BrotliStatus::StreamEnd => CodingStatus::StreamEnd,
        };
        Ok(CodingProgress {
            consumed: progress.consumed,
            produced: progress.produced,
            status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.inner.finish().map_err(map_error)
    }

    fn coding(&self) -> &ContentCoding {
        static BROTLI: ContentCoding = ContentCoding::Brotli;
        &BROTLI
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(decoder: &mut BrotliCodingDecoder, body: &[u8]) -> Result<Vec<u8>> {
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
    fn brotli_stage_round_trips() {
        let body = b"brotli over http, decoded incrementally, bounded by a real window";
        let br = oxiarc_brotli::compress(body, 4).expect("compress");
        let mut d = BrotliCodingDecoder::new(&DecodeLimits::default());
        assert_eq!(drive(&mut d, &br).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Brotli);
    }

    #[test]
    fn truncation_is_an_error() {
        let body = vec![b'q'; 8192];
        let br = oxiarc_brotli::compress(&body, 4).expect("compress");
        let mut d = BrotliCodingDecoder::new(&DecodeLimits::default());
        drive(&mut d, &br[..br.len() - 2]).expect_err("a truncated br body must be an error");
    }

    #[test]
    fn empty_body_is_an_error() {
        let mut d = BrotliCodingDecoder::new(&DecodeLimits::default());
        drive(&mut d, b"").expect_err("an empty body is not a valid br stream");
    }

    #[test]
    fn window_refusal_is_a_limit_not_corruption() {
        let err = map_error(BrotliError::WindowTooLarge {
            declared: 1 << 24,
            max: 1 << 20,
        });
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Window { .. },
                ..
            }
        ));
    }
}
