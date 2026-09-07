//! Resource limits applied while decoding.
//!
//! Every allocation the decoder performs is derived from numbers that a
//! hostile file controls, so each one is checked against a [`DecodeLimits`]
//! entry *before* the allocation happens. The defaults are permissive enough
//! for real-world imagery (1 Gpx, 4 GiB of coefficients) and
//! [`DecodeLimits::strict`] tightens them for untrusted input.

use crate::error::{JpegError, LimitKind, Result};

/// Caps applied to a single decode.
///
/// # Examples
///
/// ```
/// use oxiarc_jpeg::DecodeLimits;
///
/// let strict = DecodeLimits::strict();
/// assert!(strict.max_pixels < DecodeLimits::default().max_pixels);
/// assert!(DecodeLimits::UNLIMITED.max_pixels > strict.max_pixels);
/// ```
///
/// The struct is deliberately *not* `#[non_exhaustive]`, so a caller can write
/// `DecodeLimits { max_pixels: N, ..DecodeLimits::default() }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Largest accepted frame width in pixels. Default 65 535 (JPEG's own cap).
    pub max_width: u32,
    /// Largest accepted frame height in pixels. Default 65 535.
    pub max_height: u32,
    /// Largest accepted `width * height`. Default 1 Gpx.
    pub max_pixels: u64,
    /// Largest accepted `Nf`. Default 4; T.81 permits up to 255.
    pub max_components: u8,
    /// Largest accepted number of scans, guarding progressive scan bombs.
    /// Default 512.
    pub max_scans: u32,
    /// Largest progressive coefficient buffer, in bytes. Default 4 GiB.
    pub max_coefficient_bytes: u64,
    /// Largest decoded output, in bytes. Default 4 GiB.
    pub max_output_bytes: u64,
    /// Largest compressed stream read from a [`std::io::Read`] source, in
    /// bytes. Default 1 GiB.
    ///
    /// [`crate::Decoder`] buffers its source before parsing (see the crate
    /// documentation), so this bounds that buffer. Slice entry points such as
    /// [`crate::decode_abbreviated_into`] do not allocate an input buffer and
    /// ignore this field.
    pub max_input_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_width: 65_535,
            max_height: 65_535,
            max_pixels: 1 << 30,
            max_components: 4,
            max_scans: 512,
            max_coefficient_bytes: 1 << 32,
            max_output_bytes: 1 << 32,
            max_input_bytes: 1 << 30,
        }
    }
}

impl DecodeLimits {
    /// Limits that accept anything representable by the format.
    pub const UNLIMITED: Self = Self {
        max_width: 65_535,
        max_height: 65_535,
        max_pixels: u64::MAX,
        max_components: 255,
        max_scans: u32::MAX,
        max_coefficient_bytes: u64::MAX,
        max_output_bytes: u64::MAX,
        max_input_bytes: u64::MAX,
    };

    /// Conservative limits for untrusted input: 64 Mpx, 256 MiB of
    /// coefficients, 256 MiB of output, 64 scans, 64 MiB of input.
    pub fn strict() -> Self {
        Self {
            max_width: 32_768,
            max_height: 32_768,
            max_pixels: 64 << 20,
            max_components: 4,
            max_scans: 64,
            max_coefficient_bytes: 256 << 20,
            max_output_bytes: 256 << 20,
            max_input_bytes: 64 << 20,
        }
    }

    /// Set [`DecodeLimits::max_pixels`].
    #[must_use]
    pub fn with_max_pixels(mut self, pixels: u64) -> Self {
        self.max_pixels = pixels;
        self
    }

    /// Set [`DecodeLimits::max_output_bytes`].
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Set [`DecodeLimits::max_input_bytes`].
    #[must_use]
    pub fn with_max_input_bytes(mut self, bytes: u64) -> Self {
        self.max_input_bytes = bytes;
        self
    }

    /// Check a frame's geometry against the width/height/pixel caps.
    pub(crate) fn check_dimensions(&self, width: u32, height: u32) -> Result<()> {
        if width > self.max_width {
            return Err(JpegError::LimitExceeded(LimitKind::Width));
        }
        if height > self.max_height {
            return Err(JpegError::LimitExceeded(LimitKind::Height));
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.max_pixels {
            return Err(JpegError::LimitExceeded(LimitKind::Pixels));
        }
        Ok(())
    }

    /// Check a component count against [`DecodeLimits::max_components`].
    pub(crate) fn check_components(&self, count: usize) -> Result<()> {
        if count > usize::from(self.max_components) {
            return Err(JpegError::LimitExceeded(LimitKind::Components));
        }
        Ok(())
    }

    /// Check an output size in bytes.
    pub(crate) fn check_output_bytes(&self, bytes: u64) -> Result<()> {
        if bytes > self.max_output_bytes {
            return Err(JpegError::LimitExceeded(LimitKind::OutputBytes));
        }
        Ok(())
    }

    /// Check a coefficient buffer size in bytes.
    pub(crate) fn check_coefficient_bytes(&self, bytes: u64) -> Result<()> {
        if bytes > self.max_coefficient_bytes {
            return Err(JpegError::LimitExceeded(LimitKind::CoefficientMemory));
        }
        Ok(())
    }
}

/// Multiply `a * b * c` in `u64`, mapping overflow onto `kind`.
pub(crate) fn checked_product3(a: u64, b: u64, c: u64, kind: LimitKind) -> Result<u64> {
    a.checked_mul(b)
        .and_then(|v| v.checked_mul(c))
        .ok_or(JpegError::LimitExceeded(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_accepts_a_normal_photo() {
        let limits = DecodeLimits::default();
        assert!(limits.check_dimensions(6000, 4000).is_ok());
        assert!(limits.check_components(3).is_ok());
    }

    #[test]
    fn strict_rejects_a_gigapixel() {
        let limits = DecodeLimits::strict();
        assert!(matches!(
            limits.check_dimensions(40_000, 40_000),
            Err(JpegError::LimitExceeded(LimitKind::Width))
        ));
        assert!(matches!(
            limits.check_dimensions(20_000, 20_000),
            Err(JpegError::LimitExceeded(LimitKind::Pixels))
        ));
    }

    #[test]
    fn height_limit_is_reported_separately() {
        let limits = DecodeLimits {
            max_height: 100,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            limits.check_dimensions(10, 200),
            Err(JpegError::LimitExceeded(LimitKind::Height))
        ));
    }

    #[test]
    fn component_cap_is_enforced() {
        let limits = DecodeLimits::default();
        assert!(limits.check_components(4).is_ok());
        assert!(limits.check_components(5).is_err());
    }

    #[test]
    fn product_overflow_is_a_limit_error() {
        assert!(matches!(
            checked_product3(u64::MAX, 2, 2, LimitKind::Pixels),
            Err(JpegError::LimitExceeded(LimitKind::Pixels))
        ));
        assert_eq!(
            checked_product3(2, 3, 4, LimitKind::Pixels).expect("no overflow"),
            24
        );
    }

    #[test]
    fn budget_checks_are_inclusive() {
        let limits = DecodeLimits {
            max_output_bytes: 100,
            max_coefficient_bytes: 100,
            ..DecodeLimits::default()
        };
        assert!(limits.check_output_bytes(100).is_ok());
        assert!(limits.check_output_bytes(101).is_err());
        assert!(limits.check_coefficient_bytes(100).is_ok());
        assert!(limits.check_coefficient_bytes(101).is_err());
    }
}
