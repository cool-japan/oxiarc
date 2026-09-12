//! Compression 32773: Apple PackBits.
//!
//! A byte-oriented run-length code. The decoder reads a signed control byte
//! `n`:
//!
//! * `0..=127` — copy the next `n + 1` bytes literally;
//! * `-127..=-1` — repeat the next byte `1 - n` times;
//! * `-128` — no operation (skip).
//!
//! libtiff flushes the encoder at every row boundary, so
//! [`encode`] takes a row length and does the same. That is what makes the
//! output byte-identical to `tiffcp -c packbits`, which the oracle suite
//! asserts.
//!
//! ```
//! use oxiarc_tiff::compression::packbits;
//!
//! let encoded = packbits::encode(&[7, 7, 7, 7, 1, 2], 6);
//! let mut out = [0u8; 6];
//! assert_eq!(packbits::decode_into(&encoded, &mut out)?, 6);
//! assert_eq!(out, [7, 7, 7, 7, 1, 2]);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use crate::error::{FormatError, Result, TiffError};

/// Largest literal or repeat run one control byte can describe.
const MAX_RUN: usize = 128;

/// Decodes a PackBits stream into `dst`, returning the bytes written.
///
/// Never writes past `dst`: a stream that would overrun is
/// [`FormatError::PackbitsOverrun`], and one that ends mid-run is
/// [`FormatError::PackbitsTruncated`].
///
/// # Errors
/// [`FormatError::PackbitsOverrun`] or [`FormatError::PackbitsTruncated`].
pub fn decode_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mut read = 0usize;
    let mut written = 0usize;
    while read < src.len() {
        let control = src.get(read).copied().unwrap_or(0) as i8;
        read += 1;
        if control == -128 {
            continue;
        }
        if control >= 0 {
            let count = control as usize + 1;
            let end = read
                .checked_add(count)
                .ok_or(TiffError::Format(FormatError::PackbitsTruncated))?;
            let Some(literal) = src.get(read..end) else {
                return Err(TiffError::Format(FormatError::PackbitsTruncated));
            };
            let out_end = written
                .checked_add(count)
                .ok_or(TiffError::Format(FormatError::PackbitsOverrun))?;
            let Some(slot) = dst.get_mut(written..out_end) else {
                return Err(TiffError::Format(FormatError::PackbitsOverrun));
            };
            slot.copy_from_slice(literal);
            read = end;
            written = out_end;
        } else {
            let count = 1 - i32::from(control);
            let count = count as usize;
            let Some(byte) = src.get(read).copied() else {
                return Err(TiffError::Format(FormatError::PackbitsTruncated));
            };
            read += 1;
            let out_end = written
                .checked_add(count)
                .ok_or(TiffError::Format(FormatError::PackbitsOverrun))?;
            let Some(slot) = dst.get_mut(written..out_end) else {
                return Err(TiffError::Format(FormatError::PackbitsOverrun));
            };
            for out in slot.iter_mut() {
                *out = byte;
            }
            written = out_end;
        }
    }
    Ok(written)
}

/// Decodes a PackBits stream into a fresh `Vec`, bounded by `max_output`.
///
/// # Errors
/// [`FormatError::PackbitsOverrun`] when the stream expands past `max_output`.
pub fn decode_bounded(src: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; max_output];
    let n = decode_into(src, &mut out)?;
    out.truncate(n);
    Ok(out)
}

/// Encodes `src` with PackBits, flushing at every `row_bytes` boundary.
///
/// A `row_bytes` of 0 encodes the whole buffer as one run of rows, which is
/// legal but is not what libtiff writes; the TIFF writer in this crate always
/// passes the real row length.
#[must_use]
pub fn encode(src: &[u8], row_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 128 + 16);
    if row_bytes == 0 || row_bytes >= src.len() {
        encode_row(src, &mut out);
        return out;
    }
    let mut start = 0usize;
    while start < src.len() {
        let end = (start + row_bytes).min(src.len());
        if let Some(row) = src.get(start..end) {
            encode_row(row, &mut out);
        }
        start = end;
    }
    out
}

/// Encodes one row, matching libtiff's run/literal decisions exactly.
fn encode_row(row: &[u8], out: &mut Vec<u8>) {
    let len = row.len();
    let mut i = 0usize;
    while i < len {
        let run = run_length(row, i);
        if run >= 3 {
            let take = run.min(MAX_RUN);
            out.push((1i32 - take as i32) as i8 as u8);
            out.push(row.get(i).copied().unwrap_or(0));
            i += take;
            continue;
        }
        // Gather literals until a run of three or more starts, or the row ends.
        let start = i;
        let mut literal = 0usize;
        while i < len && literal < MAX_RUN {
            if run_length(row, i) >= 3 {
                break;
            }
            i += 1;
            literal += 1;
        }
        if literal == 0 {
            // Only reachable when the very first sample starts a >=3 run,
            // which the branch above already handles; guard against a stall.
            i += 1;
            literal = 1;
        }
        out.push((literal - 1) as u8);
        if let Some(slice) = row.get(start..start + literal) {
            out.extend_from_slice(slice);
        }
    }
}

/// Length of the run of equal bytes starting at `i`.
fn run_length(row: &[u8], i: usize) -> usize {
    let Some(first) = row.get(i).copied() else {
        return 0;
    };
    let mut n = 1usize;
    while let Some(byte) = row.get(i + n) {
        if *byte != first {
            break;
        }
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8], row_bytes: usize) {
        let encoded = encode(data, row_bytes);
        let mut out = vec![0u8; data.len()];
        let n = decode_into(&encoded, &mut out).expect("decode");
        assert_eq!(n, data.len());
        assert_eq!(out, data);
    }

    #[test]
    fn the_apple_reference_example_decodes() {
        // The example from Apple's TN1023 / TIFF 6.0 appendix.
        let encoded = [
            0xFEu8, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        // 3 x AA, 3 literals, 4 x AA, 4 literals, 10 x AA.
        let expected = [
            0xAAu8, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        let mut out = vec![0u8; expected.len()];
        assert_eq!(decode_into(&encoded, &mut out).expect("decode"), 24);
        assert_eq!(out, expected);
    }

    #[test]
    fn the_no_op_control_byte_is_skipped() {
        let mut out = [0u8; 2];
        assert_eq!(decode_into(&[0x80, 0x01, 1, 2], &mut out).expect("skip"), 2);
        assert_eq!(out, [1, 2]);
    }

    #[test]
    fn runs_and_literals_round_trip() {
        round_trip(&[], 0);
        round_trip(&[1], 1);
        round_trip(&[7, 7, 7, 7], 4);
        round_trip(&[1, 2, 3, 4, 5], 5);
        round_trip(&[1, 1, 2, 2, 3, 3], 6);
        round_trip(&[0; 300], 300);
        round_trip(&[0xFF; 129], 129);
        let mixed: Vec<u8> = (0..500u32)
            .map(|i| if i % 7 == 0 { 0 } else { i as u8 })
            .collect();
        round_trip(&mixed, 500);
    }

    #[test]
    fn rows_are_flushed_independently() {
        // Two rows of four identical bytes must produce two runs, not one.
        let data = [5u8, 5, 5, 5, 5, 5, 5, 5];
        let encoded = encode(&data, 4);
        assert_eq!(encoded, vec![0xFD, 5, 0xFD, 5]);
        round_trip(&data, 4);
    }

    #[test]
    fn long_runs_are_split_at_128() {
        let data = vec![3u8; 300];
        let encoded = encode(&data, 300);
        // 128 + 128 + 44
        assert_eq!(encoded.len(), 6);
        assert_eq!(encoded[0], (1i32 - 128) as i8 as u8);
        round_trip(&data, 300);
    }

    #[test]
    fn long_literals_are_split_at_128() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 7 % 251) as u8).collect();
        let encoded = encode(&data, 200);
        assert_eq!(encoded.first().copied(), Some(127));
        round_trip(&data, 200);
    }

    #[test]
    fn a_stream_that_would_overrun_is_rejected() {
        let mut out = [0u8; 2];
        let err = decode_into(&[3, 1, 2, 3, 4], &mut out).expect_err("overrun");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::PackbitsOverrun)
        ));
        let err = decode_into(&[0xFD, 7], &mut out).expect_err("run overrun");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::PackbitsOverrun)
        ));
    }

    #[test]
    fn a_truncated_stream_is_rejected() {
        let mut out = [0u8; 8];
        let err = decode_into(&[3, 1, 2], &mut out).expect_err("truncated literal");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::PackbitsTruncated)
        ));
        let err = decode_into(&[0xFE], &mut out).expect_err("truncated run");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::PackbitsTruncated)
        ));
    }

    #[test]
    fn bounded_decode_truncates_to_the_real_length() {
        let encoded = encode(&[1, 2, 3], 3);
        let out = decode_bounded(&encoded, 16).expect("bounded");
        assert_eq!(out, vec![1, 2, 3]);
        assert!(decode_bounded(&encoded, 2).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut out = vec![0u8; 64];
        for seed in 0u32..2000 {
            let len = (seed % 17) as usize;
            let src: Vec<u8> = (0..len)
                .map(|i| ((seed.wrapping_mul(2654435761).wrapping_add(i as u32)) >> 13) as u8)
                .collect();
            let _ = decode_into(&src, &mut out);
        }
    }
}
