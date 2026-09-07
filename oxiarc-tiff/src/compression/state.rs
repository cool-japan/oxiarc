//! Per-image codec state.
//!
//! Two things must survive from one chunk to the next:
//!
//! * **decisions** — whether this image's LZW streams follow libtiff's early
//!   code-width change or the older rule. Deciding that per strip would cost a
//!   failed decode on every strip of an old-style file;
//! * **scratch** — the inflate window. A fresh
//!   [`WrappedInflate`](oxiarc_deflate::wrapper::WrappedInflate) allocates a
//!   32 KiB history per strip, which would make "no per-chunk allocation in
//!   the steady state" false for the most common compressed TIFF there is,
//!   and the fax changing-element buffers, whose size follows the *content*
//!   of a row rather than its width.
//!
//! The state is shared (`&CodecState`) and internally synchronised, because
//! [`Codec`](super::Codec) is `Send + Sync` and
//! [`rayon_support`](crate::rayon_support) hands the same state to every
//! worker of a parallel strip decode.
//!
//! Five slots live here: the LZW dialect decision, the inflate machine and
//! its 32 KiB window, the fax changing-element buffers, the
//! [`ZstdStream`](oxiarc_zstd::ZstdStream) (window ring, literals and
//! sequence buffers) and the [`XzDecoder`](oxiarc_lzma::xz::XzDecoder) (the
//! LZMA2 dictionary). Every one of them is what a *fresh* decoder would have
//! to allocate again for the next strip of the same page.
//!
//! Four of the five are *pools* (`compression::pool`) rather than single
//! slots. A slot would have to hold its lock across the decode itself, which
//! is correct but turns a parallel decode of a Deflate, CCITT, ZSTD or LZMA
//! page into a serial one behind the codec's mutex; a pool holds the lock
//! only while a decoder is taken out and put back, so each worker gets a
//! decoder of its own and no decoder is ever touched by two threads at once.
//! A serial decode holds exactly one entry, which is the original single-slot
//! behaviour.
//!
//! What it does **not** cover: the JPEG codec allocates on its two rare paths
//! (deep precision, and a frame whose geometry disagrees with the chunk's),
//! never on the common one.

#[cfg(feature = "lzw")]
use std::sync::atomic::{AtomicU8, Ordering};

/// Which LZW dialect this image's strips follow.
#[cfg(feature = "lzw")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LzwMode {
    /// Not decided yet: the next strip decides.
    #[default]
    Undecided,
    /// libtiff's `LZWDecode`: MSB-first, the code width grows one code early.
    Standard,
    /// TIFF 6.0's own pseudo-code: MSB-first, the code width grows one code
    /// late.
    OldStyle,
    /// libtiff's `LZWDecodeCompat`: pre-1993 writers that packed codes
    /// **LSB-first** and grew the code width one code late.
    CompatLsb,
}

#[cfg(feature = "lzw")]
impl LzwMode {
    /// The wire encoding used by the atomic.
    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Standard,
            2 => Self::OldStyle,
            3 => Self::CompatLsb,
            _ => Self::Undecided,
        }
    }

    /// The wire encoding used by the atomic.
    const fn to_u8(self) -> u8 {
        match self {
            Self::Undecided => 0,
            Self::Standard => 1,
            Self::OldStyle => 2,
            Self::CompatLsb => 3,
        }
    }
}

/// Decisions and scratch buffers shared by every chunk of one image.
///
/// Held by [`crate::ImageInfo`] and handed to the codecs through
/// [`CodecContext::state`](super::CodecContext::state). Cloning an `ImageInfo`
/// shares the state rather than copying it, which is what a parallel decode of
/// the same image wants.
///
/// ```
/// use oxiarc_tiff::compression::CodecState;
///
/// let state = CodecState::new();
/// // A fresh state has taken no decisions.
/// assert!(!state.lzw_is_old_style());
/// ```
#[derive(Debug, Default)]
pub struct CodecState {
    /// [`LzwMode`], as its `u8` encoding.
    #[cfg(feature = "lzw")]
    lzw_mode: AtomicU8,
    /// The reusable inflate machine.
    #[cfg(feature = "deflate")]
    inflate: super::deflate::InflateSlot,
    /// The reusable fax changing-element buffers.
    #[cfg(feature = "ccitt")]
    fax: super::ccitt::FaxScratch,
    /// The reusable zstd decoder.
    #[cfg(feature = "zstd")]
    zstd: super::zstd::ZstdSlot,
    /// The reusable `.xz` decoder.
    #[cfg(feature = "lzma")]
    xz: super::lzma::XzSlot,
}

impl CodecState {
    /// Fresh state: no decisions taken, no scratch allocated.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` once a strip of this image has proved to use TIFF 6.0's
    /// late code-width change with MSB-first packing.
    ///
    /// It stays `false` for libtiff's `LZWDecodeCompat` dialect, which is a
    /// *different* deviation (LSB-first packing, also with the late change);
    /// [`CodecState::lzw_is_compat_lsb`] reports that one.
    ///
    /// Always `false` without the `lzw` feature.
    #[must_use]
    pub fn lzw_is_old_style(&self) -> bool {
        #[cfg(feature = "lzw")]
        {
            self.lzw_mode() == LzwMode::OldStyle
        }
        #[cfg(not(feature = "lzw"))]
        {
            false
        }
    }

    /// `true` once a strip of this image has proved to be libtiff's
    /// `LZWDecodeCompat` dialect: LSB-first packing, late code-width change.
    ///
    /// Always `false` without the `lzw` feature.
    ///
    /// ```
    /// use oxiarc_tiff::compression::CodecState;
    ///
    /// let state = CodecState::new();
    /// assert!(!state.lzw_is_compat_lsb());
    /// ```
    #[must_use]
    pub fn lzw_is_compat_lsb(&self) -> bool {
        #[cfg(feature = "lzw")]
        {
            self.lzw_mode() == LzwMode::CompatLsb
        }
        #[cfg(not(feature = "lzw"))]
        {
            false
        }
    }

    /// The code-width rule decided for this image so far.
    #[cfg(feature = "lzw")]
    pub(crate) fn lzw_mode(&self) -> LzwMode {
        LzwMode::from_u8(self.lzw_mode.load(Ordering::Relaxed))
    }

    /// Records the code-width rule a strip proved.
    #[cfg(feature = "lzw")]
    pub(crate) fn set_lzw_mode(&self, mode: LzwMode) {
        self.lzw_mode.store(mode.to_u8(), Ordering::Relaxed);
    }

    /// The reusable inflate machine.
    #[cfg(feature = "deflate")]
    pub(crate) fn inflate(&self) -> &super::deflate::InflateSlot {
        &self.inflate
    }

    /// The reusable fax changing-element buffers.
    #[cfg(feature = "ccitt")]
    pub(crate) fn fax(&self) -> &super::ccitt::FaxScratch {
        &self.fax
    }

    /// The reusable zstd decoder.
    #[cfg(feature = "zstd")]
    pub(crate) fn zstd(&self) -> &super::zstd::ZstdSlot {
        &self.zstd
    }

    /// The reusable `.xz` decoder.
    #[cfg(feature = "lzma")]
    pub(crate) fn xz(&self) -> &super::lzma::XzSlot {
        &self.xz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lzw")]
    #[test]
    fn the_lzw_decision_survives_between_chunks() {
        let state = CodecState::new();
        assert_eq!(state.lzw_mode(), LzwMode::Undecided);
        state.set_lzw_mode(LzwMode::OldStyle);
        assert_eq!(state.lzw_mode(), LzwMode::OldStyle);
        assert!(state.lzw_is_old_style());
        state.set_lzw_mode(LzwMode::Standard);
        assert!(!state.lzw_is_old_style());
    }

    #[cfg(feature = "lzw")]
    #[test]
    fn the_wire_encoding_round_trips() {
        for mode in [
            LzwMode::Undecided,
            LzwMode::Standard,
            LzwMode::OldStyle,
            LzwMode::CompatLsb,
        ] {
            assert_eq!(LzwMode::from_u8(mode.to_u8()), mode);
        }
        assert_eq!(LzwMode::from_u8(200), LzwMode::Undecided);
    }

    #[test]
    fn the_state_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CodecState>();
    }
}
