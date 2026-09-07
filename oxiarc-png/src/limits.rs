//! Allocation and size limits.
//!
//! Two types, deliberately kept apart:
//!
//! * [`Limits`] is the `png`-crate-shaped budget: one `bytes` field that the
//!   decoder decrements as it reserves memory for its own bookkeeping. The
//!   caller's own output buffer is **not** counted against it.
//! * [`DecodeLimits`] is this crate's richer set, covering dimensions, chunk
//!   sizes, retained metadata and the per-file animation budget.

use crate::error::DecodingError;

/// The `png`-shaped memory budget.
///
/// ```
/// use oxiarc_png::Limits;
/// let mut limits = Limits { bytes: 1024 };
/// assert!(limits.reserve_bytes(512).is_ok());
/// assert!(limits.reserve_bytes(1024).is_err());
/// assert_eq!(limits.bytes, 512);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Remaining budget, in bytes.
    pub bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            bytes: 1024 * 1024 * 64,
        }
    }
}

impl Limits {
    /// Take `bytes` out of the budget, or fail without changing it.
    pub fn reserve_bytes(&mut self, bytes: usize) -> Result<(), DecodingError> {
        match self.bytes.checked_sub(bytes) {
            Some(rest) => {
                self.bytes = rest;
                Ok(())
            }
            None => Err(DecodingError::LimitsExceeded),
        }
    }

    /// Return `bytes` to the budget.
    pub fn free_bytes(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

/// The full set of decoding limits.
///
/// Every one of these is checked *before* the corresponding allocation
/// happens, so a hostile file is rejected rather than absorbed.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DecodeLimits {
    /// Largest accepted image width. Default `1 << 20`.
    pub max_width: u32,
    /// Largest accepted image height. Default `1 << 20`.
    pub max_height: u32,
    /// Largest accepted pixel count. Default `1 << 30`.
    pub max_pixels: u64,
    /// Largest single allocation the decoder will make for image data.
    /// Default 64 MiB.
    pub max_alloc_bytes: usize,
    /// Largest accepted chunk payload. Default `0x7FFF_FFFF`, the spec cap.
    pub max_chunk_len: u32,
    /// Largest decompressed text payload for `zTXt`/`iTXt`. Default 2 MiB,
    /// matching the `png` crate's `DECOMPRESSION_LIMIT`.
    pub max_text_bytes: usize,
    /// Largest decompressed ICC profile. Default 16 MiB.
    pub max_iccp_bytes: usize,
    /// Total bytes of unknown ancillary chunks retained in
    /// [`crate::Info::unknown_chunks`]. Default 8 MiB.
    pub max_unknown_chunk_bytes: usize,
    /// Largest accepted `acTL` frame count. Default `1 << 20`.
    pub max_frames: u32,
    /// Total raw bytes all animation frames of one file may decode to.
    ///
    /// The inflater's own cap is reset at every frame boundary, so a
    /// thousand-frame animation would otherwise get a thousand times the
    /// per-stream budget. Default 1 GiB.
    pub max_total_frame_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> DecodeLimits {
        DecodeLimits {
            max_width: 1 << 20,
            max_height: 1 << 20,
            max_pixels: 1 << 30,
            max_alloc_bytes: 1024 * 1024 * 64,
            max_chunk_len: crate::chunk::MAX_CHUNK_LEN,
            max_text_bytes: 1024 * 1024 * 2,
            max_iccp_bytes: 1024 * 1024 * 16,
            max_unknown_chunk_bytes: 1024 * 1024 * 8,
            max_frames: 1 << 20,
            max_total_frame_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl DecodeLimits {
    /// Limits with every bound set as high as it can go.
    ///
    /// Only appropriate for trusted input.
    #[must_use]
    pub fn unlimited() -> DecodeLimits {
        DecodeLimits {
            max_width: u32::MAX,
            max_height: u32::MAX,
            max_pixels: u64::MAX,
            max_alloc_bytes: usize::MAX,
            max_chunk_len: crate::chunk::MAX_CHUNK_LEN,
            max_text_bytes: usize::MAX,
            max_iccp_bytes: usize::MAX,
            max_unknown_chunk_bytes: usize::MAX,
            max_frames: u32::MAX,
            max_total_frame_bytes: u64::MAX,
        }
    }

    /// Set the largest accepted image width.
    #[must_use]
    pub fn with_max_width(mut self, value: u32) -> Self {
        self.max_width = value;
        self
    }

    /// Set the largest accepted image height.
    #[must_use]
    pub fn with_max_height(mut self, value: u32) -> Self {
        self.max_height = value;
        self
    }

    /// Set the largest accepted pixel count.
    #[must_use]
    pub fn with_max_pixels(mut self, value: u64) -> Self {
        self.max_pixels = value;
        self
    }

    /// Set the largest single image-data allocation.
    #[must_use]
    pub fn with_max_alloc_bytes(mut self, value: usize) -> Self {
        self.max_alloc_bytes = value;
        self
    }

    /// Set the largest accepted chunk payload.
    #[must_use]
    pub fn with_max_chunk_len(mut self, value: u32) -> Self {
        self.max_chunk_len = value.min(crate::chunk::MAX_CHUNK_LEN);
        self
    }

    /// Set the largest decompressed text payload.
    #[must_use]
    pub fn with_max_text_bytes(mut self, value: usize) -> Self {
        self.max_text_bytes = value;
        self
    }

    /// Set the largest decompressed ICC profile.
    #[must_use]
    pub fn with_max_iccp_bytes(mut self, value: usize) -> Self {
        self.max_iccp_bytes = value;
        self
    }

    /// Set the total bytes of unknown ancillary chunks that may be retained.
    #[must_use]
    pub fn with_max_unknown_chunk_bytes(mut self, value: usize) -> Self {
        self.max_unknown_chunk_bytes = value;
        self
    }

    /// Set the largest accepted `acTL` frame count.
    #[must_use]
    pub fn with_max_frames(mut self, value: u32) -> Self {
        self.max_frames = value;
        self
    }

    /// Set the file-level cap on decompressed animation bytes.
    #[must_use]
    pub fn with_max_total_frame_bytes(mut self, value: u64) -> Self {
        self.max_total_frame_bytes = value;
        self
    }

    /// Check image dimensions before anything is allocated for them.
    pub fn check_dimensions(&self, width: u32, height: u32) -> Result<(), DecodingError> {
        if width > self.max_width || height > self.max_height {
            return Err(DecodingError::LimitsExceeded);
        }
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        if pixels > self.max_pixels {
            return Err(DecodingError::LimitsExceeded);
        }
        Ok(())
    }

    /// Check a would-be allocation of `bytes` bytes.
    pub fn check_alloc(&self, bytes: u64) -> Result<usize, DecodingError> {
        let bytes = usize::try_from(bytes).map_err(|_| DecodingError::LimitsExceeded)?;
        if bytes > self.max_alloc_bytes {
            return Err(DecodingError::LimitsExceeded);
        }
        Ok(bytes)
    }
}

/// A running total of the raw bytes an animation has decoded so far.
///
/// The inflater's `max_output` guard bounds **one stream** and is cleared by
/// its `reset()`, which APNG performs at every frame boundary. This carries the
/// file-level budget that the codec-level guard cannot.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameBudget {
    spent: u64,
}

impl FrameBudget {
    /// Charge `bytes` to the budget.
    pub(crate) fn charge(
        &mut self,
        bytes: u64,
        limits: &DecodeLimits,
    ) -> Result<(), DecodingError> {
        self.spent = self.spent.saturating_add(bytes);
        if self.spent > limits.max_total_frame_bytes {
            return Err(DecodingError::LimitsExceeded);
        }
        Ok(())
    }

    /// Bytes charged so far.
    #[cfg(test)]
    pub(crate) fn spent(&self) -> u64 {
        self.spent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_limits_default_is_64_mib_and_decrements() {
        let mut limits = Limits::default();
        assert_eq!(limits.bytes, 64 * 1024 * 1024);
        limits.reserve_bytes(1000).expect("fits");
        assert_eq!(limits.bytes, 64 * 1024 * 1024 - 1000);
        limits.free_bytes(1000);
        assert_eq!(limits.bytes, 64 * 1024 * 1024);
        assert!(limits.reserve_bytes(usize::MAX).is_err());
        assert_eq!(
            limits.bytes,
            64 * 1024 * 1024,
            "a failed reserve is a no-op"
        );
    }

    #[test]
    fn builders_set_every_field() {
        let limits = DecodeLimits::default()
            .with_max_width(1)
            .with_max_height(2)
            .with_max_pixels(3)
            .with_max_alloc_bytes(4)
            .with_max_chunk_len(5)
            .with_max_text_bytes(6)
            .with_max_iccp_bytes(7)
            .with_max_unknown_chunk_bytes(8)
            .with_max_frames(9)
            .with_max_total_frame_bytes(10);
        assert_eq!(limits.max_width, 1);
        assert_eq!(limits.max_height, 2);
        assert_eq!(limits.max_pixels, 3);
        assert_eq!(limits.max_alloc_bytes, 4);
        assert_eq!(limits.max_chunk_len, 5);
        assert_eq!(limits.max_text_bytes, 6);
        assert_eq!(limits.max_iccp_bytes, 7);
        assert_eq!(limits.max_unknown_chunk_bytes, 8);
        assert_eq!(limits.max_frames, 9);
        assert_eq!(limits.max_total_frame_bytes, 10);
        // The chunk-length cap can never exceed the specification's.
        assert_eq!(
            DecodeLimits::default()
                .with_max_chunk_len(u32::MAX)
                .max_chunk_len,
            crate::chunk::MAX_CHUNK_LEN
        );
    }

    #[test]
    fn decode_limits_reject_before_allocating() {
        let limits = DecodeLimits::default();
        assert!(limits.check_dimensions(1024, 1024).is_ok());
        assert!(limits.check_dimensions(u32::MAX, 1).is_err());
        assert!(limits.check_dimensions(1 << 20, 1 << 20).is_err());
        assert!(limits.check_alloc(1024).is_ok());
        assert!(limits.check_alloc(u64::MAX).is_err());
        assert!(
            DecodeLimits::unlimited()
                .check_dimensions(1 << 20, 1 << 20)
                .is_ok()
        );
    }

    #[test]
    fn frame_budget_is_a_file_level_total() {
        let limits = DecodeLimits {
            max_total_frame_bytes: 100,
            ..DecodeLimits::default()
        };
        let mut budget = FrameBudget::default();
        for _ in 0..10 {
            budget.charge(10, &limits).expect("within budget");
        }
        assert_eq!(budget.spent(), 100);
        assert!(budget.charge(1, &limits).is_err());
    }
}
