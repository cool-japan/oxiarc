//! The private [`CodingDecoder`] seam every content coding is driven
//! through, plus the identity pass-through and the shared error mapping.
//!
//! Deliberately **not** public and deliberately **not** a shared trait in
//! `oxiarc-core`: the Phase 8 owner decisions (#10) place cross-codec
//! unification here and nowhere else, because a blanket `Decompressor` impl
//! in the core would collide with `Inflater`'s hand-written one and would
//! make `reset`/`is_finished` ambiguous at every call site.
//!
//! The shape mirrors `oxiarc_deflate::InflateStream::inflate`,
//! `oxiarc_brotli::BrotliStream::decode` and
//! `oxiarc_zstd::ZstdStream::decode` field for field, so no implementation
//! of this trait has to buffer a whole body to satisfy it.

use oxiarc_core::traits::FlushMode;

use crate::coding::ContentCoding;
use crate::error::Result;
// Only `map_core_error` needs these, and it is gated on the DEFLATE-family
// features; `br` and `zstd` map their own error types in their own modules.
#[cfg(any(feature = "gzip", feature = "deflate"))]
use crate::error::{HttpCodingError, LimitKind};

/// Why a [`CodingDecoder::decode`] call returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodingStatus {
    /// All of `input` was absorbed and the stream is not finished.
    NeedInput,
    /// `output` is full and the stream is not finished.
    NeedOutput,
    /// The stream reached a clean end.
    StreamEnd,
}

/// What one [`CodingDecoder::decode`] call achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodingProgress {
    /// Bytes taken from the front of `input`.
    pub(crate) consumed: usize,
    /// Bytes written to the front of `output`.
    pub(crate) produced: usize,
    /// Why the call returned.
    pub(crate) status: CodingStatus,
}

/// One content coding, driven as a resumable push decoder.
///
/// `Send` is required so a [`Decoder`](crate::Decoder) can be moved between
/// threads (a `reqwest` body loop routinely is); it is not `Sync`, and no
/// implementation holds interior mutability that would need it.
pub(crate) trait CodingDecoder: Send {
    /// Decode as much as `input` and `output` allow.
    ///
    /// `flush` is [`FlushMode::Finish`] exactly when `input` is the last
    /// input this stage will ever receive. Every other mode means "more may
    /// follow"; implementations must map them to "not finished" explicitly
    /// rather than by a catch-all, because [`FlushMode`] is
    /// `#[non_exhaustive]`.
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress>;

    /// Assert the stream really ended: checksums, trailers, framing.
    ///
    /// Called only after the stage has been pumped dry, so an implementation
    /// may assume no decoded output is pending.
    fn finish(&mut self) -> Result<()>;

    /// The coding this stage decodes, for diagnostics.
    fn coding(&self) -> &ContentCoding;

    /// Input bytes the codec took in but that are not part of its stream.
    ///
    /// Only `zstd` can end up holding any (up to the four bytes of a magic
    /// number it had to read to discover the stream was over); everything
    /// else leaves trailing bytes in the caller's slice, where the chain
    /// sees them directly.
    fn unused_input(&self) -> &[u8] {
        &[]
    }
}

/// Whether a flush mode means "this is the last input".
///
/// [`FlushMode`] is `#[non_exhaustive]`, so this is written as an explicit
/// list plus a wildcard rather than `flush != FlushMode::Finish`: a future
/// variant must default to "more may follow", which is the safe direction —
/// it can only turn a would-be truncation error into a `NeedInput` that the
/// caller resolves, never the reverse.
pub(crate) fn is_finish(flush: FlushMode) -> bool {
    match flush {
        FlushMode::Finish => true,
        FlushMode::None | FlushMode::Sync | FlushMode::Full | FlushMode::Partial => false,
        _ => false,
    }
}

/// Wrap a codec-level error as [`HttpCodingError::Corrupt`], except for a
/// budget overrun, which is a limit, not corruption.
///
/// Only the DEFLATE family reports `oxiarc_core::OxiArcError`; `br` and
/// `zstd` map their own error types, so this is gated on those features.
#[cfg(any(feature = "gzip", feature = "deflate"))]
pub(crate) fn map_core_error(
    coding: &ContentCoding,
    error: oxiarc_core::OxiArcError,
) -> HttpCodingError {
    if let oxiarc_core::OxiArcError::MemoryBudgetExceeded { budget, requested } = error {
        return HttpCodingError::LimitExceeded {
            limit: budget as f64,
            kind: LimitKind::Output {
                produced: requested as u64,
            },
        };
    }
    HttpCodingError::Corrupt {
        coding: coding.clone(),
        source: Box::new(error),
    }
}

/// The `identity` coding: a byte-for-byte copy.
///
/// Present as a real stage rather than a special case so that
/// `Content-Encoding: identity, gzip` (legal, if pointless) and a
/// pass-through [`Decoder`](crate::Decoder) share exactly one code path with
/// every other coding.
#[derive(Debug, Default)]
pub(crate) struct IdentityDecoder {
    finished: bool,
}

impl IdentityDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl CodingDecoder for IdentityDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress> {
        let n = input.len().min(output.len());
        output[..n].copy_from_slice(&input[..n]);
        let status = if n < input.len() {
            CodingStatus::NeedOutput
        } else if is_finish(flush) {
            self.finished = true;
            CodingStatus::StreamEnd
        } else {
            CodingStatus::NeedInput
        };
        Ok(CodingProgress {
            consumed: n,
            produced: n,
            status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        Ok(())
    }

    fn coding(&self) -> &ContentCoding {
        // A `static` rather than `&ContentCoding::Identity`: `ContentCoding`
        // owns a `String` in one variant, so the enum has a destructor and a
        // borrow of a temporary would not be const-promoted to `'static`.
        static IDENTITY: ContentCoding = ContentCoding::Identity;
        &IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_copies_and_reports_stream_end_only_on_finish() {
        let mut d = IdentityDecoder::new();
        let mut out = [0u8; 8];
        let p = d
            .decode(b"abc", &mut out, FlushMode::None)
            .expect("identity never fails");
        assert_eq!(p.consumed, 3);
        assert_eq!(p.produced, 3);
        assert_eq!(p.status, CodingStatus::NeedInput);
        assert_eq!(&out[..3], b"abc");

        let p = d
            .decode(b"de", &mut out, FlushMode::Finish)
            .expect("identity never fails");
        assert_eq!(p.status, CodingStatus::StreamEnd);
        assert_eq!(&out[..2], b"de");
        d.finish().expect("identity finish never fails");
        assert_eq!(d.coding(), &ContentCoding::Identity);
        assert!(d.unused_input().is_empty());
    }

    #[test]
    fn identity_reports_need_output_when_short() {
        let mut d = IdentityDecoder::new();
        let mut out = [0u8; 2];
        let p = d
            .decode(b"abcd", &mut out, FlushMode::Finish)
            .expect("identity never fails");
        assert_eq!(p.consumed, 2);
        assert_eq!(p.status, CodingStatus::NeedOutput);
    }

    #[test]
    fn only_finish_counts_as_end_of_input() {
        assert!(is_finish(FlushMode::Finish));
        for mode in [
            FlushMode::None,
            FlushMode::Sync,
            FlushMode::Full,
            FlushMode::Partial,
        ] {
            assert!(!is_finish(mode), "{mode:?} must not mean end of input");
        }
    }

    #[cfg(any(feature = "gzip", feature = "deflate"))]
    #[test]
    fn budget_overrun_from_a_codec_is_a_limit_not_corruption() {
        let err = map_core_error(
            &ContentCoding::Gzip,
            oxiarc_core::OxiArcError::MemoryBudgetExceeded {
                budget: 16,
                requested: 4096,
            },
        );
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Output { produced: 4096 },
                ..
            }
        ));
    }

    #[cfg(any(feature = "gzip", feature = "deflate"))]
    #[test]
    fn other_codec_errors_are_corruption() {
        let err = map_core_error(
            &ContentCoding::Gzip,
            oxiarc_core::OxiArcError::corrupted(3, "boom"),
        );
        assert!(matches!(err, HttpCodingError::Corrupt { .. }));
    }
}
