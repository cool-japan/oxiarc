//! Lenient-mode recovery primitives for archive readers.
//!
//! When a reader is configured in "lenient" mode (via a format-specific
//! builder such as `ZipReader::lenient`), integrity failures that would
//! normally abort extraction are instead recorded as [`LenientWarning`]s
//! and the reader does its best to continue.
//!
//! This is intended for **best-effort recovery** of slightly-corrupt
//! archives — e.g. files with a few flipped bits, TAR archives with a
//! mangled header block mid-stream, or LZH archives whose entry data CRCs
//! do not match but whose surrounding structure is intact. Lenient mode
//! does NOT guarantee that the returned data is valid; callers MUST
//! inspect [`Reader::warnings`](crate::zip::ZipReader::warnings) and
//! decide how to handle non-empty warning lists.
//!
//! The format strings used in [`LenientWarning::format`] are currently
//! `"ZIP"`, `"TAR"`, and `"LZH"`.

/// A non-fatal integrity issue encountered during lenient-mode reading.
///
/// Warnings are accumulated on the reader and can be retrieved via the
/// per-format `.warnings()` accessor after an extraction or enumeration
/// operation. Each warning carries the archive format, an optional entry
/// name (when the failure is attributable to a specific entry), a
/// machine-readable [`LenientWarningKind`] discriminator, and a
/// human-readable message describing the condition.
#[derive(Debug, Clone)]
pub struct LenientWarning {
    /// Short archive-format identifier. Currently one of `"ZIP"`, `"TAR"`,
    /// or `"LZH"`.
    pub format: &'static str,

    /// Name of the entry the warning is attributable to, when available.
    ///
    /// For archive-level warnings (e.g. scanning forward past a corrupt
    /// block) this is `None`.
    pub entry_name: Option<String>,

    /// Machine-readable classification of the warning.
    pub kind: LenientWarningKind,

    /// Human-readable description of the warning. Safe to surface in
    /// log output or end-user messages.
    pub message: String,
}

/// Classification of a [`LenientWarning`].
///
/// Additional variants may be added in future versions; callers matching
/// on this enum SHOULD include a catch-all arm to remain
/// forward-compatible.
#[derive(Debug, Clone)]
pub enum LenientWarningKind {
    /// The entry's stored CRC-32 (ZIP) or CRC-16 (LZH) did not match the
    /// CRC computed from the decompressed payload.
    ///
    /// Both values are 32-bit so that CRC-16 warnings reuse the same
    /// variant — LZH populates `expected` and `computed` with the 16-bit
    /// values widened to `u32`.
    CrcMismatch {
        /// The CRC value recorded in the archive metadata.
        expected: u32,
        /// The CRC value computed from the decompressed/unpacked data.
        computed: u32,
    },

    /// A TAR 512-byte header block's POSIX checksum field did not match
    /// the sum-of-bytes computation over the block.
    HeaderChecksumMismatch,

    /// A header-level CRC (currently used by LZH extension header 0x00
    /// and similar) did not match the computed value.
    BadHeaderCrc,

    /// The reader skipped `bytes` bytes while searching for the next
    /// valid header after a corrupt region.
    ScannedForward {
        /// Number of bytes skipped before a valid header was located.
        bytes: u64,
    },

    /// An entry was skipped entirely because its header or data could
    /// not be interpreted.
    SkippedEntry,
}
