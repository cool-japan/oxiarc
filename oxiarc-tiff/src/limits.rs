//! Allocation guards and leniency policy.
//!
//! TIFF is a tag-driven format: every allocation size in a decoder comes from
//! numbers in the file. [`Limits`] is checked *before* any allocation, so a
//! crafted file that claims a 2^31 x 2^31 image fails in microseconds and
//! allocates nothing.
//!
//! ```
//! use oxiarc_tiff::Limits;
//!
//! let limits = Limits::default();
//! assert!(limits.check_image_bytes(1024).is_ok());
//! assert!(limits.check_image_bytes(u64::from(u32::MAX) * 64).is_err());
//!
//! // The struct is `#[non_exhaustive]`, so it grows fluently.
//! let tight = Limits::default().with_max_image_bytes(4096).with_max_ifds(8);
//! assert!(tight.check_image_bytes(8192).is_err());
//! ```

use crate::error::{LimitError, Result, TiffError};

/// Guard rails applied to every size that comes out of the file.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Limits {
    /// Largest decoded image buffer, in bytes (default 256 MiB).
    pub max_image_bytes: usize,
    /// Largest single decoded strip/tile buffer, in bytes (default 256 MiB).
    pub decoding_buffer_size: usize,
    /// Largest scratch buffer (compressed chunk, unpack temp) (default 128 MiB).
    pub intermediate_buffer_size: usize,
    /// Largest single tag value, in bytes (default 1 MiB).
    pub ifd_value_size: usize,
    /// Largest number of IFDs walked per file, chain + SubIFD tree (default 1024).
    pub max_ifds: usize,
    /// Largest number of entries in one IFD (default 65535).
    pub max_ifd_entries: usize,
    /// Largest number of strips/tiles per image (default 2^24).
    pub max_chunks: u64,
    /// Largest `ImageWidth`/`ImageLength` (default 2^22).
    pub max_dimension: u32,
    /// Deepest SubIFD nesting level followed (default 8).
    pub max_ifd_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_image_bytes: 256 * 1024 * 1024,
            decoding_buffer_size: 256 * 1024 * 1024,
            intermediate_buffer_size: 128 * 1024 * 1024,
            ifd_value_size: 1024 * 1024,
            max_ifds: 1024,
            max_ifd_entries: 65535,
            max_chunks: 1 << 24,
            max_dimension: 1 << 22,
            max_ifd_depth: 8,
        }
    }
}

impl Limits {
    /// Every guard set as high as the platform allows.
    ///
    /// Use only on trusted input: a malformed file can then drive an
    /// arbitrarily large allocation.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_image_bytes: usize::MAX,
            decoding_buffer_size: usize::MAX,
            intermediate_buffer_size: usize::MAX,
            ifd_value_size: usize::MAX,
            max_ifds: usize::MAX,
            max_ifd_entries: usize::MAX,
            max_chunks: u64::MAX,
            max_dimension: u32::MAX,
            max_ifd_depth: usize::MAX,
        }
    }

    /// Sets the largest decoded image buffer.
    #[must_use]
    pub fn with_max_image_bytes(mut self, bytes: usize) -> Self {
        self.max_image_bytes = bytes;
        self
    }

    /// Sets the largest single decoded strip/tile buffer.
    #[must_use]
    pub fn with_decoding_buffer_size(mut self, bytes: usize) -> Self {
        self.decoding_buffer_size = bytes;
        self
    }

    /// Sets the largest scratch buffer.
    #[must_use]
    pub fn with_intermediate_buffer_size(mut self, bytes: usize) -> Self {
        self.intermediate_buffer_size = bytes;
        self
    }

    /// Sets the largest single tag value.
    #[must_use]
    pub fn with_ifd_value_size(mut self, bytes: usize) -> Self {
        self.ifd_value_size = bytes;
        self
    }

    /// Sets the largest number of IFDs walked per file.
    #[must_use]
    pub fn with_max_ifds(mut self, count: usize) -> Self {
        self.max_ifds = count;
        self
    }

    /// Sets the largest number of entries in one IFD.
    #[must_use]
    pub fn with_max_ifd_entries(mut self, count: usize) -> Self {
        self.max_ifd_entries = count;
        self
    }

    /// Sets the largest number of strips/tiles per image.
    #[must_use]
    pub fn with_max_chunks(mut self, count: u64) -> Self {
        self.max_chunks = count;
        self
    }

    /// Sets the largest image dimension.
    #[must_use]
    pub fn with_max_dimension(mut self, value: u32) -> Self {
        self.max_dimension = value;
        self
    }

    /// Sets the deepest SubIFD nesting level followed.
    #[must_use]
    pub fn with_max_ifd_depth(mut self, depth: usize) -> Self {
        self.max_ifd_depth = depth;
        self
    }

    /// Rejects a projected decoded-image size.
    ///
    /// # Errors
    /// [`LimitError::ImageSize`] when `size` exceeds [`Self::max_image_bytes`].
    pub fn check_image_bytes(&self, size: u64) -> Result<usize> {
        if size > self.max_image_bytes as u64 {
            return Err(TiffError::Limits(LimitError::ImageSize {
                size,
                limit: self.max_image_bytes,
            }));
        }
        usize::try_from(size).map_err(|_| TiffError::IntOverflow)
    }

    /// Rejects a projected single-chunk decode buffer size.
    ///
    /// # Errors
    /// [`LimitError::DecodingBufferSize`] when the limit is exceeded.
    pub fn check_decoding_buffer(&self, size: u64) -> Result<usize> {
        if size > self.decoding_buffer_size as u64 {
            return Err(TiffError::Limits(LimitError::DecodingBufferSize {
                size,
                limit: self.decoding_buffer_size,
            }));
        }
        usize::try_from(size).map_err(|_| TiffError::IntOverflow)
    }

    /// Rejects a projected scratch buffer size.
    ///
    /// # Errors
    /// [`LimitError::IntermediateSize`] when the limit is exceeded.
    pub fn check_intermediate(&self, size: u64) -> Result<usize> {
        if size > self.intermediate_buffer_size as u64 {
            return Err(TiffError::Limits(LimitError::IntermediateSize {
                size,
                limit: self.intermediate_buffer_size,
            }));
        }
        usize::try_from(size).map_err(|_| TiffError::IntOverflow)
    }

    /// Rejects an over-large tag value.
    ///
    /// # Errors
    /// [`LimitError::IfdValueSize`] when the limit is exceeded.
    pub fn check_value_size(&self, tag: u16, size: u64) -> Result<usize> {
        if size > self.ifd_value_size as u64 {
            return Err(TiffError::Limits(LimitError::IfdValueSize {
                tag,
                size,
                limit: self.ifd_value_size,
            }));
        }
        usize::try_from(size).map_err(|_| TiffError::IntOverflow)
    }

    /// Rejects an over-large IFD entry count.
    ///
    /// # Errors
    /// [`LimitError::IfdEntries`] when the limit is exceeded.
    pub fn check_ifd_entries(&self, count: u64) -> Result<usize> {
        if count > self.max_ifd_entries as u64 {
            return Err(TiffError::Limits(LimitError::IfdEntries {
                count,
                limit: self.max_ifd_entries,
            }));
        }
        usize::try_from(count).map_err(|_| TiffError::IntOverflow)
    }

    /// Rejects an over-large strip/tile count.
    ///
    /// # Errors
    /// [`LimitError::ChunkCount`] when the limit is exceeded.
    pub fn check_chunk_count(&self, count: u64) -> Result<()> {
        if count > self.max_chunks {
            return Err(TiffError::Limits(LimitError::ChunkCount {
                count,
                limit: self.max_chunks,
            }));
        }
        Ok(())
    }

    /// Rejects an over-large image dimension.
    ///
    /// # Errors
    /// [`LimitError::Dimension`] when the limit is exceeded.
    pub fn check_dimension(&self, value: u64) -> Result<u32> {
        if value > u64::from(self.max_dimension) {
            return Err(TiffError::Limits(LimitError::Dimension {
                value,
                limit: self.max_dimension,
            }));
        }
        u32::try_from(value).map_err(|_| TiffError::IntOverflow)
    }

    /// Allocates `count` zeroed elements after checking `count * size_of::<T>()`
    /// against `budget`.
    ///
    /// A convenience for callers — including out-of-tree [`crate::Codec`]
    /// implementations — that need one allocation sized by a number from the
    /// file. The crate's own buffers are guarded by the matching `check_*`
    /// method instead (`check_intermediate` before the compressed chunk,
    /// `check_decoding_buffer` before the decoded chunk, `check_image_bytes`
    /// before the whole-image buffer, `check_value_size` before a tag value);
    /// the invariant is that *some* guard runs before every file-driven
    /// allocation, not that they all funnel through this function.
    ///
    /// # Errors
    /// [`LimitError::IntermediateSize`] when the product exceeds `budget`.
    pub fn checked_alloc<T: Copy + Default>(count: usize, budget: usize) -> Result<Vec<T>> {
        let bytes = (count as u64)
            .checked_mul(core::mem::size_of::<T>() as u64)
            .ok_or(TiffError::IntOverflow)?;
        if bytes > budget as u64 {
            return Err(TiffError::Limits(LimitError::IntermediateSize {
                size: bytes,
                limit: budget,
            }));
        }
        Ok(vec![T::default(); count])
    }
}

/// A running budget for containers that reset the codec per strip/tile.
///
/// Per-stream caps inside a codec are cleared whenever the codec is reset,
/// so a 1000-strip file would otherwise get 1000x the per-stream cap. Every
/// decode path in this crate charges its output against one `OutputBudget`
/// that lives as long as the image.
#[derive(Clone, Debug)]
pub struct OutputBudget {
    limit: u64,
    used: u64,
}

impl OutputBudget {
    /// A budget of `limit` bytes.
    #[must_use]
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    /// Charges `bytes` against the budget.
    ///
    /// # Errors
    /// [`LimitError::OutputBudget`] once the running total exceeds the limit.
    pub fn charge(&mut self, bytes: u64) -> Result<()> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.limit {
            return Err(TiffError::Limits(LimitError::OutputBudget {
                size: self.used,
                limit: self.limit,
            }));
        }
        Ok(())
    }

    /// Bytes charged so far.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// The configured budget.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Resets the running total (used when moving to a new image).
    pub fn reset(&mut self) {
        self.used = 0;
    }
}

/// How strictly spec violations are treated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Leniency {
    /// Any spec violation is an error.
    Strict,
    /// libtiff-compatible tolerances; violations are recorded as warnings.
    #[default]
    Normal,
    /// Recover as much as possible; never panic, never silently succeed
    /// without recording a warning.
    Lenient,
}

impl Leniency {
    /// `true` for [`Leniency::Strict`].
    #[must_use]
    pub fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }

    /// `true` for [`Leniency::Lenient`].
    #[must_use]
    pub fn is_lenient(self) -> bool {
        matches!(self, Self::Lenient)
    }
}

/// A recoverable spec violation encountered while reading.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Warning {
    /// A tag was ignored because it does not apply here.
    IgnoredTag {
        /// The raw tag number.
        tag: u16,
        /// Why it was ignored.
        reason: &'static str,
    },
    /// A tag was absent and a default was assumed.
    AssumedDefault {
        /// The raw tag number.
        tag: u16,
        /// What was assumed.
        reason: &'static str,
    },
    /// A structural rule was violated but decoding continued.
    SpecViolation {
        /// A human-readable description.
        message: String,
    },
    /// The same tag appeared twice in one IFD; the first was kept.
    DuplicateTag {
        /// The raw tag number.
        tag: u16,
    },
    /// A chunk was clipped to the end of the file.
    ChunkTruncated {
        /// Index of the chunk.
        index: u64,
        /// How many bytes were dropped.
        dropped: u64,
    },
}

/// Warnings collected while reading one file.
#[derive(Clone, Debug, Default)]
pub struct Warnings(Vec<Warning>);

impl Warnings {
    /// An empty warning list.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Records one warning.
    pub fn push(&mut self, warning: Warning) {
        self.0.push(warning);
    }

    /// All warnings recorded so far.
    #[must_use]
    pub fn as_slice(&self) -> &[Warning] {
        &self.0
    }

    /// Number of recorded warnings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` when nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drops every recorded warning.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Moves every warning out of `other` and appends them here, in order,
    /// leaving `other` empty. For merging per-worker warning lists back into
    /// a shared one after a parallel decode.
    pub fn append(&mut self, other: &mut Warnings) {
        self.0.append(&mut other.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_reject_a_bomb() {
        let limits = Limits::default();
        let err = limits
            .check_image_bytes(1 << 40)
            .expect_err("1 TiB must be rejected");
        assert!(err.is_limits());
    }

    #[test]
    fn every_builder_sets_its_field() {
        let limits = Limits::default()
            .with_max_image_bytes(1)
            .with_decoding_buffer_size(2)
            .with_intermediate_buffer_size(3)
            .with_ifd_value_size(4)
            .with_max_ifds(5)
            .with_max_ifd_entries(6)
            .with_max_chunks(7)
            .with_max_dimension(8)
            .with_max_ifd_depth(9);
        assert_eq!(limits.max_image_bytes, 1);
        assert_eq!(limits.decoding_buffer_size, 2);
        assert_eq!(limits.intermediate_buffer_size, 3);
        assert_eq!(limits.ifd_value_size, 4);
        assert_eq!(limits.max_ifds, 5);
        assert_eq!(limits.max_ifd_entries, 6);
        assert_eq!(limits.max_chunks, 7);
        assert_eq!(limits.max_dimension, 8);
        assert_eq!(limits.max_ifd_depth, 9);
    }

    #[test]
    fn unlimited_accepts_everything_that_fits_in_usize() {
        let limits = Limits::unlimited();
        assert!(limits.check_chunk_count(u64::MAX).is_ok());
        assert!(limits.check_dimension(u64::from(u32::MAX)).is_ok());
    }

    #[test]
    fn checked_alloc_refuses_overflowing_products() {
        let err = Limits::checked_alloc::<u64>(usize::MAX, 1024)
            .expect_err("usize::MAX u64s must not be allocated");
        assert!(matches!(
            err,
            TiffError::IntOverflow | TiffError::Limits(LimitError::IntermediateSize { .. })
        ));
    }

    #[test]
    fn checked_alloc_allocates_when_in_budget() {
        let buf = Limits::checked_alloc::<u16>(8, 1024).expect("in budget");
        assert_eq!(buf.len(), 8);
        assert!(buf.iter().all(|v| *v == 0));
    }

    #[test]
    fn output_budget_accumulates_across_chunks() {
        let mut budget = OutputBudget::new(100);
        assert!(budget.charge(60).is_ok());
        let err = budget.charge(60).expect_err("cumulative overflow");
        assert!(err.is_limits());
        assert_eq!(budget.limit(), 100);
        budget.reset();
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn leniency_default_is_normal() {
        assert_eq!(Leniency::default(), Leniency::Normal);
        assert!(Leniency::Strict.is_strict());
        assert!(Leniency::Lenient.is_lenient());
    }

    #[test]
    fn warnings_collects() {
        let mut w = Warnings::new();
        assert!(w.is_empty());
        w.push(Warning::DuplicateTag { tag: 256 });
        assert_eq!(w.len(), 1);
        assert_eq!(w.as_slice()[0], Warning::DuplicateTag { tag: 256 });
        w.clear();
        assert!(w.is_empty());
    }
}
