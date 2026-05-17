//! Bounded-memory LZMA streaming compressor and decompressor.
//!
//! [`LzmaCompressor`] and [`LzmaDecompressor`] provide a one-shot
//! `compress` / `decompress` interface with configurable memory budgets.
//! Both types enforce that the dictionary size plus the input chunk size
//! plus a conservative scratch overhead stay within the configured budget
//! before performing any allocation-heavy operation.
//!
//! ## Memory budget
//!
//! The budget controls the peak in-process memory for a single
//! compress/decompress call:
//!
//! ```
//! # use oxiarc_lzma::{LzmaCompressor, LzmaDecompressor};
//! let compressor = LzmaCompressor::new().with_memory_budget(64 * 1024 * 1024);
//! let decompressor = LzmaDecompressor::new().with_memory_budget(64 * 1024 * 1024);
//! ```
//!
//! ## COOLJAPAN policies
//!
//! - Zero `unwrap()` in production code — all fallible paths propagate errors.
//! - Zero compiler warnings.
//! - `snake_case` throughout.

use crate::{LzmaLevel, decode_lzma2_chunked, encode_lzma2_chunked};
use oxiarc_core::error::{OxiArcError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Default memory budget constants
// ─────────────────────────────────────────────────────────────────────────────

/// Default memory budget for the LZMA compressor (64 MiB).
///
/// LZMA dictionaries are much larger than LZ4 blocks, so the default is
/// correspondingly larger (vs the 16 MiB LZ4 default).
pub const LZMA_COMPRESSOR_DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

/// Default memory budget for the LZMA decompressor (64 MiB).
pub const LZMA_DECOMPRESSOR_DEFAULT_BUDGET: usize = 64 * 1024 * 1024;

/// Conservative scratch overhead estimate per encode/decode operation (8 MiB).
///
/// Accounts for internal range-coder state, probability model tables, and
/// match-finder working buffers.
const LZMA_SCRATCH_OVERHEAD: usize = 8 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────────────
// LzmaCompressor
// ─────────────────────────────────────────────────────────────────────────────

/// LZMA compressor with a configurable memory budget.
///
/// Provides a simple one-shot [`compress`][LzmaCompressor::compress] method
/// that uses the LZMA2 chunked encoder under the hood.  Before encoding, the
/// memory footprint is estimated as
/// `dict_size + input.len() + SCRATCH_OVERHEAD`; if that exceeds the
/// configured budget an [`OxiArcError::MemoryBudgetExceeded`] error is
/// returned immediately, without allocating any large buffers.
///
/// # Default budget
///
/// `64 MiB` — set via [`with_memory_budget`][LzmaCompressor::with_memory_budget].
#[derive(Debug, Clone)]
pub struct LzmaCompressor {
    /// Compression level.
    level: LzmaLevel,
    /// Maximum allowed peak memory (bytes).
    memory_budget: usize,
}

impl LzmaCompressor {
    /// Create a new LZMA compressor with default options and a 64 MiB budget.
    pub fn new() -> Self {
        Self {
            level: LzmaLevel::DEFAULT,
            memory_budget: LZMA_COMPRESSOR_DEFAULT_BUDGET,
        }
    }

    /// Create a new LZMA compressor with the given compression level.
    pub fn with_level(level: LzmaLevel) -> Self {
        Self {
            level,
            memory_budget: LZMA_COMPRESSOR_DEFAULT_BUDGET,
        }
    }

    /// Set the maximum peak memory budget for a single compress call.
    ///
    /// If `dict_size + input.len() + scratch_overhead` exceeds `budget`,
    /// [`compress`][Self::compress] returns
    /// [`OxiArcError::MemoryBudgetExceeded`] without performing any work.
    ///
    /// Defaults to 64 MiB.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    /// Compress `input` and return the LZMA2 stream.
    ///
    /// Returns `Err(`[`OxiArcError::MemoryBudgetExceeded`]`)` when the estimated
    /// peak memory exceeds the configured budget.
    pub fn compress(&self, input: &[u8]) -> Result<Vec<u8>> {
        let dict_size = self.level.dict_size() as usize;
        let needed = dict_size
            .saturating_add(input.len())
            .saturating_add(LZMA_SCRATCH_OVERHEAD);

        if needed > self.memory_budget {
            return Err(OxiArcError::memory_budget_exceeded(
                self.memory_budget,
                needed,
            ));
        }

        encode_lzma2_chunked(input, self.level)
    }
}

impl Default for LzmaCompressor {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LzmaDecompressor
// ─────────────────────────────────────────────────────────────────────────────

/// LZMA decompressor with a configurable memory budget.
///
/// Provides a simple one-shot [`decompress`][LzmaDecompressor::decompress]
/// method that drives the LZMA2 decoder.  Before decoding, the memory
/// footprint is estimated as `dict_size + compressed.len() + SCRATCH_OVERHEAD`;
/// if that exceeds the configured budget an
/// [`OxiArcError::MemoryBudgetExceeded`] error is returned immediately.
///
/// The dict size used for the budget check and for decoding is derived from
/// the compression level (default [`LzmaLevel::DEFAULT`]).  When the compressed
/// data was produced at a different level, supply the matching level via
/// [`with_level`][LzmaDecompressor::with_level].
///
/// # Default budget
///
/// `64 MiB` — set via [`with_memory_budget`][LzmaDecompressor::with_memory_budget].
#[derive(Debug, Clone)]
pub struct LzmaDecompressor {
    /// LZMA level — determines the dict size used for the budget check.
    level: LzmaLevel,
    /// Maximum allowed peak memory (bytes).
    memory_budget: usize,
}

impl LzmaDecompressor {
    /// Create a new LZMA decompressor with default options and a 64 MiB budget.
    pub fn new() -> Self {
        Self {
            level: LzmaLevel::DEFAULT,
            memory_budget: LZMA_DECOMPRESSOR_DEFAULT_BUDGET,
        }
    }

    /// Create a new LZMA decompressor for data compressed at the given level.
    pub fn with_level(level: LzmaLevel) -> Self {
        Self {
            level,
            memory_budget: LZMA_DECOMPRESSOR_DEFAULT_BUDGET,
        }
    }

    /// Set the maximum peak memory budget for a single decompress call.
    ///
    /// If `dict_size + compressed.len() + scratch_overhead` exceeds `budget`,
    /// [`decompress`][Self::decompress] returns
    /// [`OxiArcError::MemoryBudgetExceeded`] without performing any work.
    ///
    /// Defaults to 64 MiB.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = budget;
        self
    }

    /// Decompress an LZMA2 stream produced by [`LzmaCompressor`] or
    /// [`lzma2_compress`][crate::lzma2_compress].
    ///
    /// Returns `Err(`[`OxiArcError::MemoryBudgetExceeded`]`)` when the estimated
    /// peak memory exceeds the configured budget.
    pub fn decompress(&self, compressed: &[u8]) -> Result<Vec<u8>> {
        let dict_size = self.level.dict_size() as usize;
        let needed = dict_size
            .saturating_add(compressed.len())
            .saturating_add(LZMA_SCRATCH_OVERHEAD);

        if needed > self.memory_budget {
            return Err(OxiArcError::memory_budget_exceeded(
                self.memory_budget,
                needed,
            ));
        }

        decode_lzma2_chunked(compressed, self.level.dict_size())
    }
}

impl Default for LzmaDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compressor_default_budget_ok() {
        let data = vec![0xAAu8; 4 * 1024];
        let result = LzmaCompressor::new().compress(&data);
        assert!(
            result.is_ok(),
            "default budget should accommodate small input"
        );
    }

    #[test]
    fn test_decompressor_default_budget_ok() {
        let data = vec![0xBBu8; 4 * 1024];
        let compressed = LzmaCompressor::new().compress(&data).expect("compress");
        let result = LzmaDecompressor::new().decompress(&compressed);
        assert!(result.is_ok());
        assert_eq!(result.expect("decompress"), data);
    }

    #[test]
    fn test_budget_builder_roundtrip() {
        let data: Vec<u8> = (0u8..=255).cycle().take(16 * 1024).collect();
        let budget = 64 * 1024 * 1024;
        let compressed = LzmaCompressor::new()
            .with_memory_budget(budget)
            .compress(&data)
            .expect("compress");
        let decompressed = LzmaDecompressor::new()
            .with_memory_budget(budget)
            .decompress(&compressed)
            .expect("decompress");
        assert_eq!(decompressed, data);
    }
}
