//! Call-free copies for the short runs a Zstandard block is made of.
//!
//! # Why this module exists
//!
//! `copy_from_slice`, `copy_within`, `extend_from_slice` and `extend_from_within`
//! all lower to a `memcpy`/`memmove` **call** when the length is not a
//! compile-time constant. A Zstandard block is a stream of *short* runs: on a
//! 50 MB text corpus at level 3 the average literal run is a handful of bytes
//! and the average match is around ten, and profiling
//! [`crate::ZstdStream`] on exactly that input put **51 % of the whole decode
//! in `_platform_memmove`** — almost all of it call overhead and length
//! dispatch rather than moving bytes.
//!
//! The helpers here copy runs up to [`SHORT_COPY`] bytes with a plain
//! element-wise loop, which the compiler unrolls and vectorises in place with
//! no call and no bounds check, and fall back to the library routine for the
//! longer runs where it wins. Nothing here changes which bytes are produced;
//! [`copy_run_within`] documents the one case where the source and destination
//! genuinely overlap and a real `memmove` is required.

/// Longest run copied element-wise instead of through `memcpy`/`memmove`.
///
/// Two 16-byte vector operations, which is well inside the fixed cost of a
/// call into the platform's copy routine.
pub(crate) const SHORT_COPY: usize = 32;

/// Copy a fixed `N`-byte word from `src[at..]` to `dst[at..]`.
///
/// The width is a constant, which is the whole point: `copy_from_slice` on a
/// `[u8; N]` lowers to one load and one store, where the same call with a
/// runtime length lowers to a call into `memcpy`.
macro_rules! copy_word {
    ($dst:expr, $src:expr, $at:expr, $n:literal) => {{
        let at = $at;
        let mut word = [0u8; $n];
        word.copy_from_slice(&$src[at..at + $n]);
        $dst[at..at + $n].copy_from_slice(&word);
    }};
}

/// Copy `src` over `dst` (equal lengths), inline for short runs.
///
/// Short runs go through a fixed-width ladder rather than a byte loop: a run
/// of 9 bytes is two 8-byte word copies (the second overlapping the first by
/// seven bytes), a run of 7 is two 4-byte copies, and so on down. The
/// overlapping tail writes the same values twice and never writes outside the
/// run, which matters because in the sliding window the bytes just past a run
/// are still-live history — the usual "round the length up to 16 and copy
/// wide" trick is not available there.
///
/// # Panics
///
/// Panics if the lengths differ, exactly as
/// [`slice::copy_from_slice`](slice::copy_from_slice) does. Every caller in
/// this crate slices both sides to the same run length.
#[inline(always)]
pub(crate) fn copy_short(dst: &mut [u8], src: &[u8]) {
    let len = src.len();
    if len > SHORT_COPY || dst.len() != len {
        // Long enough to deserve the call — and a length mismatch is routed
        // here so the panic is the library's own.
        dst.copy_from_slice(src);
        return;
    }
    if len >= 8 {
        let mut i = 0;
        while i + 8 <= len {
            copy_word!(dst, src, i, 8);
            i += 8;
        }
        if i < len {
            copy_word!(dst, src, len - 8, 8);
        }
    } else if len >= 4 {
        copy_word!(dst, src, 0, 4);
        copy_word!(dst, src, len - 4, 4);
    } else if len >= 2 {
        copy_word!(dst, src, 0, 2);
        copy_word!(dst, src, len - 2, 2);
    } else if len == 1 {
        dst[0] = src[0];
    }
}

/// Set `dst` to `byte`, inline for short runs.
#[inline(always)]
pub(crate) fn fill_short(dst: &mut [u8], byte: u8) {
    let len = dst.len();
    if len > SHORT_COPY {
        dst.fill(byte);
        return;
    }
    let word = [byte; 8];
    if len >= 8 {
        let mut i = 0;
        while i + 8 <= len {
            dst[i..i + 8].copy_from_slice(&word);
            i += 8;
        }
        if i < len {
            dst[len - 8..len].copy_from_slice(&word);
        }
    } else {
        for d in dst.iter_mut() {
            *d = byte;
        }
    }
}

/// Copy `run` bytes inside `buf`, from `src` to `dst`.
///
/// Splits the buffer whenever the two ranges are disjoint — which is the case
/// for every non-wrapping LZ77 run, since a back-reference reads strictly
/// behind the write cursor — so the copy goes through [`copy_short`] and stays
/// a call only when it is long enough to deserve one.
///
/// The ranges *can* genuinely overlap in a ring buffer: when the source has
/// wrapped past the write cursor, `dst < src < dst + run` is reachable, and
/// there the move semantics of [`slice::copy_within`] (copy as if through a
/// temporary) are exactly what is wanted and what the caller relies on.
///
/// # Panics
///
/// Panics if either range falls outside `buf`, as `copy_within` does.
#[inline(always)]
pub(crate) fn copy_run_within(buf: &mut [u8], src: usize, dst: usize, run: usize) {
    if src + run <= dst {
        let (lo, hi) = buf.split_at_mut(dst);
        copy_short(&mut hi[..run], &lo[src..src + run]);
    } else if dst + run <= src {
        let (lo, hi) = buf.split_at_mut(src);
        copy_short(&mut lo[dst..dst + run], &hi[..run]);
    } else {
        buf.copy_within(src..src + run, dst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_short_matches_copy_from_slice_at_every_length() {
        for len in 0..=(SHORT_COPY + 8) {
            let src: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31) | 1).collect();
            let mut a = vec![0u8; len];
            let mut b = vec![0u8; len];
            copy_short(&mut a, &src);
            b.copy_from_slice(&src);
            assert_eq!(a, b, "len {len}");
        }
    }

    #[test]
    fn fill_short_matches_fill_at_every_length() {
        for len in 0..=(SHORT_COPY + 8) {
            let mut a = vec![0u8; len];
            let mut b = vec![0u8; len];
            fill_short(&mut a, 0xA7);
            b.fill(0xA7);
            assert_eq!(a, b, "len {len}");
        }
    }

    #[test]
    fn copy_run_within_matches_copy_within_including_overlap() {
        let base: Vec<u8> = (0..128u8).collect();
        for src in 0..40usize {
            for dst in 0..40usize {
                for run in 0..=40usize {
                    if src + run > base.len() || dst + run > base.len() {
                        continue;
                    }
                    let mut a = base.clone();
                    let mut b = base.clone();
                    copy_run_within(&mut a, src, dst, run);
                    b.copy_within(src..src + run, dst);
                    assert_eq!(a, b, "src {src} dst {dst} run {run}");
                }
            }
        }
    }
}
