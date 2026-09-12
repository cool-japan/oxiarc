//! Output budget enforcement for the incremental decoder.
//!
//! Brotli declares no total uncompressed size anywhere in a stream, so the
//! only sound bound on the output is a cap checked while decoding. Every
//! meta-block, however, declares its own exact `MLEN` (RFC 7932 Section 9.2)
//! and produces exactly that many bytes — so checking `produced + MLEN` before
//! a meta-block is decoded is an *exact* projection with no false positives
//! and no part of a bomb's expansion ever produced.
//!
//! This mirrors [`crate::decompress()`]'s one-shot budget so the two decoders
//! reject the same streams with the same error variants.

use crate::decompress::MAX_OUTPUT_SIZE;
use crate::error::{BrotliError, BrotliResult};

/// The cap enforced while a stream is decoded.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputBudget {
    /// Maximum total output, in bytes.
    max: u64,
    /// Whether `max` came from the caller (selects the error variant so the
    /// crate's own anti-bomb guard keeps reporting
    /// [`BrotliError::OutputTooLarge`]).
    caller_budget: bool,
}

impl OutputBudget {
    /// The crate's built-in 256 MB guard, used when the caller sets no budget.
    pub(crate) const fn default_guard() -> Self {
        OutputBudget {
            max: MAX_OUTPUT_SIZE as u64,
            caller_budget: false,
        }
    }

    /// A caller-supplied memory budget.
    pub(crate) const fn caller(max: u64) -> Self {
        OutputBudget {
            max,
            caller_budget: true,
        }
    }

    /// Reject a stream that would produce `needed` total output bytes.
    pub(crate) fn check(&self, needed: u64) -> BrotliResult<()> {
        if needed > self.max {
            let requested = usize::try_from(needed).unwrap_or(usize::MAX);
            return Err(if self.caller_budget {
                BrotliError::MemoryBudgetExceeded {
                    budget: usize::try_from(self.max).unwrap_or(usize::MAX),
                    requested,
                }
            } else {
                BrotliError::OutputTooLarge(requested)
            });
        }
        Ok(())
    }
}

impl Default for OutputBudget {
    fn default() -> Self {
        Self::default_guard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_guard_reports_output_too_large() {
        let b = OutputBudget::default_guard();
        assert!(b.check(MAX_OUTPUT_SIZE as u64).is_ok());
        assert!(matches!(
            b.check(MAX_OUTPUT_SIZE as u64 + 1),
            Err(BrotliError::OutputTooLarge(_))
        ));
    }

    #[test]
    fn caller_budget_reports_memory_budget_exceeded() {
        let b = OutputBudget::caller(1024);
        assert!(b.check(1024).is_ok());
        assert!(matches!(
            b.check(1025),
            Err(BrotliError::MemoryBudgetExceeded {
                budget: 1024,
                requested: 1025
            })
        ));
    }
}
