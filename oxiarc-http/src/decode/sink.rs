//! [`LimitedSink`]: the one place decoded output bytes are counted.
//!
//! Every byte any coding stage produces passes through a `LimitedSink`, so
//! there is exactly one place where decompression-bomb protection can be
//! wrong. The design report calls this out explicitly: enforcing
//! [`DecodeLimits::max_output`] *between* DEFLATE blocks is not enough,
//! because one fixed-Huffman block of 812 KB expands to 123 MiB (see
//! `tests/limits.rs`, which regenerates that stream from a committed Rust
//! generator). The bound therefore has to reach *inside* the block decoder.
//!
//! It does, and without a callback: [`LimitedSink::allow`] truncates the
//! output slice handed to the codec to the number of bytes still permitted,
//! so `oxiarc-deflate`'s own `BoundedSink` — which checks its bound on every
//! literal and every match copy — enforces the HTTP budget as its own. A
//! stage that still has output to give when its budget is exhausted reports
//! `NeedOutput` against a zero-length slice, and that is what
//! [`LimitedSink::overflow`] turns into an error.

use crate::coding::ContentCoding;
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::limits::DecodeLimits;

/// Decoded bytes a stage may produce before the ratio guard starts to apply.
///
/// Below this figure the output:input ratio is meaningless — a gzip member's
/// 10-byte header alone reads as an infinite ratio, and every real body
/// starts with a header. 1 MiB is the value the `oxiarc-http` design report
/// picked, and the critique (§H-6) resolved the disagreement with
/// `inflate-incremental`'s 32 MiB in its favour.
pub(crate) const RATIO_GRACE_BYTES: u64 = 1 << 20;

/// Counts one stage's decoded output and enforces [`DecodeLimits`] on it.
///
/// One sink per chained coding: an intermediate stage of `gzip, gzip` can be
/// a bomb even when the final output is small, so each stage carries its own
/// budget rather than sharing a single total (which would also make a
/// legitimate two-stage body fail once the intermediate plus the final
/// exceeded the cap). [`DecodeLimits::max_codings`] is what bounds the total
/// work at `max_codings * max_output`.
#[derive(Debug, Clone)]
pub(crate) struct LimitedSink {
    coding: ContentCoding,
    limits: DecodeLimits,
    produced: u64,
    consumed: u64,
}

impl LimitedSink {
    /// A sink for `coding` bound by `limits`.
    pub(crate) fn new(coding: ContentCoding, limits: &DecodeLimits) -> Self {
        Self {
            coding,
            limits: *limits,
            produced: 0,
            consumed: 0,
        }
    }

    /// The coding whose output this sink counts.
    pub(crate) fn coding(&self) -> &ContentCoding {
        &self.coding
    }

    /// Bytes still permitted before [`DecodeLimits::max_output`] trips.
    pub(crate) fn budget(&self) -> u64 {
        self.limits.max_output.saturating_sub(self.produced)
    }

    /// How much of an output slice of `space` bytes may actually be offered
    /// to the codec.
    ///
    /// Truncating here — rather than checking afterwards — is what makes the
    /// bound hold *inside* a single DEFLATE block: the codec never has room
    /// to write the 123rd mebibyte in the first place.
    pub(crate) fn allow(&self, space: usize) -> usize {
        let budget = self.budget();
        if budget >= space as u64 {
            space
        } else {
            // `budget < space`, so it fits in a usize.
            budget as usize
        }
    }

    /// Record `produced` decoded bytes and `consumed` input bytes.
    ///
    /// # Errors
    ///
    /// [`HttpCodingError::LimitExceeded`] with [`LimitKind::Ratio`] when the
    /// output:input ratio exceeds [`DecodeLimits::max_ratio`] after at least
    /// [`RATIO_GRACE_BYTES`] of output. The output cap itself cannot be
    /// exceeded here — [`allow`](Self::allow) makes that unrepresentable —
    /// so this never reports [`LimitKind::Output`].
    pub(crate) fn commit(&mut self, produced: usize, consumed: usize) -> Result<()> {
        self.produced = self.produced.saturating_add(produced as u64);
        self.consumed = self.consumed.saturating_add(consumed as u64);
        let Some(max_ratio) = self.limits.max_ratio else {
            return Ok(());
        };
        if self.produced < RATIO_GRACE_BYTES || self.consumed == 0 {
            return Ok(());
        }
        let ratio = self.produced as f64 / self.consumed as f64;
        if ratio > max_ratio {
            return Err(HttpCodingError::LimitExceeded {
                limit: max_ratio,
                kind: LimitKind::Ratio {
                    input: self.consumed,
                    output: self.produced,
                },
            });
        }
        Ok(())
    }

    /// The error for a stage that still had output to give after its budget
    /// was spent.
    pub(crate) fn overflow(&self) -> HttpCodingError {
        HttpCodingError::LimitExceeded {
            limit: self.limits.max_output as f64,
            kind: LimitKind::Output {
                produced: self.produced.saturating_add(1),
            },
        }
    }

    /// Decoded bytes this stage has produced so far.
    pub(crate) fn produced(&self) -> u64 {
        self.produced
    }

    /// Input bytes this stage has consumed so far.
    pub(crate) fn consumed(&self) -> u64 {
        self.consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(limits: DecodeLimits) -> LimitedSink {
        LimitedSink::new(ContentCoding::Gzip, &limits)
    }

    #[test]
    fn allow_truncates_to_the_remaining_budget() {
        let mut s = sink(DecodeLimits::default().with_max_output(10));
        assert_eq!(s.allow(64), 10);
        s.commit(7, 1).expect("under the ratio grace");
        assert_eq!(s.allow(64), 3);
        assert_eq!(s.allow(2), 2);
        s.commit(3, 1).expect("under the ratio grace");
        assert_eq!(s.allow(64), 0);
        assert_eq!(s.budget(), 0);
    }

    #[test]
    fn allow_never_exceeds_the_offered_space() {
        let s = sink(DecodeLimits::unlimited());
        assert_eq!(s.allow(0), 0);
        assert_eq!(s.allow(4096), 4096);
    }

    #[test]
    fn ratio_guard_is_silent_below_the_grace() {
        let mut s = sink(DecodeLimits::default().with_max_ratio(Some(2.0)));
        // 1 byte in, a whole grace window out: still accepted.
        s.commit((RATIO_GRACE_BYTES - 1) as usize, 1)
            .expect("below the grace window");
    }

    #[test]
    fn ratio_guard_trips_above_the_grace() {
        let mut s = sink(DecodeLimits::default().with_max_ratio(Some(2.0)));
        let err = s
            .commit(RATIO_GRACE_BYTES as usize, 1)
            .expect_err("ratio 1048576:1 must trip a 2.0 guard");
        match err {
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Ratio { input, output },
                limit,
            } => {
                assert_eq!(input, 1);
                assert_eq!(output, RATIO_GRACE_BYTES);
                assert!((limit - 2.0).abs() < f64::EPSILON);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn ratio_guard_can_be_disabled() {
        let mut s = sink(DecodeLimits::default().with_max_ratio(None));
        s.commit(RATIO_GRACE_BYTES as usize * 8, 1)
            .expect("guard disabled");
    }

    #[test]
    fn overflow_reports_one_past_the_budget() {
        let mut s = sink(DecodeLimits::default().with_max_output(4));
        s.commit(4, 4).expect("exactly at the cap is fine");
        match s.overflow() {
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Output { produced },
                limit,
            } => {
                assert_eq!(produced, 5);
                assert!((limit - 4.0).abs() < f64::EPSILON);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn counters_and_coding_are_reported() {
        let mut s = sink(DecodeLimits::default().with_max_output(8));
        s.commit(8, 4).expect("at the cap");
        assert_eq!(s.budget(), 0);
        assert_eq!(s.produced(), 8);
        assert_eq!(s.consumed(), 4);
        assert_eq!(s.coding(), &ContentCoding::Gzip);
    }
}
