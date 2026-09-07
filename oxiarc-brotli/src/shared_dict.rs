//! Shared (custom) LZ77 dictionaries.
//!
//! A *shared dictionary* is a block of bytes both peers already have. Attaching
//! one lets a stream's backward references reach into it, so a small response
//! can be coded almost entirely as references into content the client already
//! holds. This is the mechanism behind `Content-Encoding: dcb`
//! (dictionary-compressed Brotli, RFC 9842) and behind the reference
//! `brotli --dictionary=FILE` / `-D FILE` command-line option.
//!
//! It is **not** the RFC 7932 Appendix A static dictionary, which is built into
//! every Brotli implementation and stays available whether or not a shared
//! dictionary is attached (see [`crate::dictionary`]). The two live in adjacent
//! ranges of the same distance space, and [`classify_distance`] is the single
//! place that splits them.
//!
//! # Distance space
//!
//! With `max_backward = min(window_size, bytes_produced)` and a shared
//! dictionary of `dict_len` bytes, a decoded distance `d` means:
//!
//! | range | meaning |
//! |---|---|
//! | `1 ..= max_backward` | ordinary backward reference into the produced output |
//! | `max_backward + 1 ..= max_backward + dict_len` | shared dictionary, at offset `dict_len - (d - max_backward)` |
//! | `> max_backward + dict_len` | Appendix A static dictionary, word id `d - max_backward - dict_len - 1` |
//!
//! Two consequences are worth stating because they are easy to get wrong:
//!
//! * the shared dictionary is addressed **relative to `max_backward`**, not to
//!   a fixed virtual position, so the same dictionary byte has a different
//!   distance code at different output positions until `max_backward`
//!   saturates at the window size;
//! * because `max_backward` is capped by the window but the dictionary range
//!   sits *beyond* it, a shared dictionary stays reachable no matter how much
//!   output has been produced, and its distances legitimately exceed the
//!   declared window.
//!
//! Both were established by measurement against `brotli 1.1.0`, not assumed: a
//! 1.1 MB dictionary attached to a stream declaring `lgwin = 10` (a 1008-byte
//! window) codes a 4 KiB copy from the *start* of that dictionary in 18 bytes,
//! and the distance the reference encoder emits for it is
//! `max_backward + dict_len - offset` (observed 1,100,986 for
//! `max_backward = 1008`, `dict_len = 1,100,000`, `offset = 22`), not
//! `produced + dict_len - offset`. The same stream still reaches the dictionary
//! after 200,000 bytes of output.
//!
//! The literal context bytes `p1`/`p2` are **not** seeded from the dictionary:
//! at output position 0 they are zero even with a dictionary attached. That too
//! is measured — the reference encoder emits byte-identical output for 37
//! dictionaries differing only in their last two bytes.

use crate::error::{BrotliError, BrotliResult};

/// Largest shared dictionary this crate will attach: 16 MiB.
///
/// The ceiling exists so that `max_backward + dict_len` always stays inside the
/// distance range RFC 7932 can encode (just over 50 MB), for every legal window
/// size up to `WBITS = 24`. Real transport dictionaries are orders of magnitude
/// smaller.
pub const MAX_SHARED_DICTIONARY: usize = 1 << 24;

/// What a decoded backward distance refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DistanceSource {
    /// An ordinary backward reference into the produced output.
    Output,
    /// A copy out of the attached shared dictionary, starting at `offset`.
    ///
    /// `available` is `dict_len - offset`: a copy longer than that continues
    /// into the produced output, which the caller handles.
    Shared {
        /// Byte offset of the first source byte inside the dictionary.
        offset: usize,
        /// Bytes from `offset` to the end of the dictionary.
        available: usize,
    },
    /// An RFC 7932 Appendix A static dictionary word.
    Static {
        /// Combined word index and transform id, as in Section 8.
        word_id: u64,
    },
}

/// Split a decoded distance into its three possible meanings.
///
/// `max_backward` is `min(window_size, bytes_produced_so_far)` and `dict_len`
/// is the attached shared dictionary's length (0 when none is attached, in
/// which case this reduces exactly to the plain RFC 7932 rule).
#[inline]
pub(crate) fn classify_distance(
    distance: usize,
    max_backward: usize,
    dict_len: usize,
) -> DistanceSource {
    if distance <= max_backward {
        return DistanceSource::Output;
    }
    let past = distance - max_backward;
    if past <= dict_len {
        let offset = dict_len - past;
        return DistanceSource::Shared {
            offset,
            available: past,
        };
    }
    DistanceSource::Static {
        word_id: (past - dict_len - 1) as u64,
    }
}

/// Reject a shared dictionary that is too large to address.
///
/// # Errors
///
/// [`BrotliError::DictionaryError`] when `len` exceeds
/// [`MAX_SHARED_DICTIONARY`].
pub(crate) fn check_dictionary_len(len: usize) -> BrotliResult<()> {
    if len > MAX_SHARED_DICTIONARY {
        return Err(BrotliError::DictionaryError(format!(
            "shared dictionary of {len} bytes exceeds the {MAX_SHARED_DICTIONARY}-byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_dictionary_reduces_to_the_plain_rule() {
        assert_eq!(classify_distance(1, 10, 0), DistanceSource::Output);
        assert_eq!(classify_distance(10, 10, 0), DistanceSource::Output);
        assert_eq!(
            classify_distance(11, 10, 0),
            DistanceSource::Static { word_id: 0 }
        );
    }

    #[test]
    fn the_three_ranges_are_contiguous_and_exhaustive() {
        let (max_backward, dict_len) = (100usize, 50usize);
        assert_eq!(
            classify_distance(max_backward, max_backward, dict_len),
            DistanceSource::Output
        );
        // The first byte past the window is the *last* byte of the dictionary.
        assert_eq!(
            classify_distance(max_backward + 1, max_backward, dict_len),
            DistanceSource::Shared {
                offset: dict_len - 1,
                available: 1
            }
        );
        // The last dictionary distance is its first byte.
        assert_eq!(
            classify_distance(max_backward + dict_len, max_backward, dict_len),
            DistanceSource::Shared {
                offset: 0,
                available: dict_len
            }
        );
        // One past that is static word 0.
        assert_eq!(
            classify_distance(max_backward + dict_len + 1, max_backward, dict_len),
            DistanceSource::Static { word_id: 0 }
        );
        assert_eq!(
            classify_distance(max_backward + dict_len + 7, max_backward, dict_len),
            DistanceSource::Static { word_id: 6 }
        );
    }

    #[test]
    fn oversized_dictionaries_are_refused() {
        assert!(check_dictionary_len(MAX_SHARED_DICTIONARY).is_ok());
        assert!(check_dictionary_len(MAX_SHARED_DICTIONARY + 1).is_err());
    }
}
