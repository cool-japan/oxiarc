//! Run-Length Encoding for BZip2.
//!
//! BZip2 uses two types of RLE:
//! 1. Initial RLE (rle1): Encodes runs of 4+ identical bytes
//! 2. Final RLE (rle2): Encodes runs of zeros after MTF

use oxiarc_core::Result;
use oxiarc_core::error::OxiArcError;

/// Encode data with initial RLE (rle1).
/// Runs of 4 or more identical bytes are encoded as:
/// - First 4 bytes as-is
/// - Then a count byte (0-251) for additional repeats
pub fn rle1_encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        let byte = data[i];
        let mut run_len = 1;

        // Count consecutive identical bytes
        while i + run_len < data.len() && data[i + run_len] == byte && run_len < 255 {
            run_len += 1;
        }

        if run_len >= 4 {
            // Encode as 4 bytes + count
            result.extend_from_slice(&[byte, byte, byte, byte]);
            let extra = (run_len - 4).min(251) as u8;
            result.push(extra);
            i += 4 + extra as usize;
        } else {
            // Output bytes as-is
            for _ in 0..run_len {
                result.push(byte);
            }
            i += run_len;
        }
    }

    result
}

/// Decode RLE1-encoded data.
///
/// Any 4 consecutive identical bytes in the encoded stream must be followed
/// by a count byte; a missing count byte is a corruption error.
pub fn rle1_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(data.len() * 2);
    let mut i = 0usize;

    while i < data.len() {
        let byte = data[i];
        let mut run = 1usize;
        while run < 4 && i + run < data.len() && data[i + run] == byte {
            run += 1;
        }
        if run == 4 {
            let extra = *data.get(i + 4).ok_or_else(|| {
                OxiArcError::corrupted(i as u64, "BZip2 RLE run length byte missing")
            })? as usize;
            result.resize(result.len() + 4 + extra, byte);
            i += 5;
        } else {
            result.resize(result.len() + run, byte);
            i += run;
        }
    }

    Ok(result)
}

/// Encode zeros using RUNA/RUNB encoding.
/// This is the zero-run length encoding used after MTF.
/// - RUNA (0) and RUNB (1) encode the run length in bijective base-2.
/// - Non-zero MTF values are output directly (shifted by +1 for RUNA/RUNB)
///
/// The input values are MTF indices over the block's used-symbol list, so a
/// non-zero value `v` maps to the bzip2 alphabet symbol `v + 1` (symbols 0
/// and 1 are RUNA/RUNB; the end-of-block symbol is appended by the caller).
pub fn encode_zero_runs(data: &[u8]) -> Vec<u16> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        if data[i] == 0 {
            // Count zeros
            let mut count = 0usize;
            while i < data.len() && data[i] == 0 {
                count += 1;
                i += 1;
            }

            // Encode using RUNA/RUNB (bijective numeration)
            // 1 zero  -> RUNA (adds 1)
            // 2 zeros -> RUNB (adds 2)
            // 3 zeros -> RUNA RUNA (adds 1 + 2 = 3)
            // 4 zeros -> RUNB RUNA (adds 2 + 2 = 4)
            // 5 zeros -> RUNA RUNB (adds 1 + 4 = 5)
            // etc.
            let mut n = count;
            while n > 0 {
                if n & 1 == 1 {
                    result.push(0); // RUNA
                } else {
                    result.push(1); // RUNB
                }
                n = (n - 1) >> 1;
            }
        } else {
            // Non-zero MTF value: output as symbol value + 1
            // (since symbols 0 and 1 are reserved for RUNA/RUNB)
            // Symbol 2 = MTF value 1, Symbol 3 = MTF value 2, etc.
            result.push(data[i] as u16 + 1);
            i += 1;
        }
    }

    result
}

/// Decode zero-run encoded data (utility counterpart of `encode_zero_runs`).
#[allow(dead_code)]
pub fn decode_zero_runs(data: &[u16], num_symbols: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        let sym = data[i];

        if sym == 0 || sym == 1 {
            // RUNA or RUNB - decode run of zeros
            let mut power = 1usize;
            let mut count = 0usize;

            while i < data.len() && (data[i] == 0 || data[i] == 1) {
                if data[i] == 0 {
                    // RUNA
                    count += power;
                } else {
                    // RUNB
                    count += 2 * power;
                }
                power *= 2;
                i += 1;
            }

            result.resize(result.len() + count, 0);
        } else if (sym as usize) <= num_symbols {
            // Regular symbol (offset by 1)
            result.push((sym - 1) as u8);
            i += 1;
        } else {
            // End of block symbol
            break;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rle1_no_runs() {
        let data = b"abcdef";
        let encoded = rle1_encode(data);
        assert_eq!(encoded, data.as_slice());

        let decoded = rle1_decode(&encoded).expect("rle1 decode no runs");
        assert_eq!(decoded, data.as_slice());
    }

    #[test]
    fn test_rle1_short_runs() {
        let data = b"aabbbcccc";
        let encoded = rle1_encode(data);
        // "aa" stays as-is, "bbb" stays as-is, "cccc" becomes "cccc\0"
        assert_eq!(&encoded[..5], b"aabbb");
        // "cccc" followed by count 0
        assert_eq!(&encoded[5..], &[b'c', b'c', b'c', b'c', 0]);
    }

    #[test]
    fn test_rle1_roundtrip() {
        let data = b"aaaaaabbbbbbbbccccccccccc";
        let encoded = rle1_encode(data);
        let decoded = rle1_decode(&encoded).expect("rle1 decode roundtrip");
        assert_eq!(decoded, data.as_slice());
    }

    #[test]
    fn test_zero_run_encoding() {
        // Single zero -> RUNA (1 = 1)
        let data = vec![0];
        let encoded = encode_zero_runs(&data);
        assert_eq!(encoded, vec![0]); // RUNA

        // Two zeros -> RUNB (2 = 2)
        let data = vec![0, 0];
        let encoded = encode_zero_runs(&data);
        assert_eq!(encoded, vec![1]); // RUNB

        // Three zeros -> RUNA RUNA (1 + 2 = 3)
        let data = vec![0, 0, 0];
        let encoded = encode_zero_runs(&data);
        // n=3: 3&1=1 -> RUNA, (3-1)>>1=1, 1&1=1 -> RUNA
        assert_eq!(encoded, vec![0, 0]); // RUNA RUNA
    }

    #[test]
    fn test_zero_run_roundtrip() {
        let data = vec![0, 0, 0, 1, 0, 0, 2, 0, 0, 0, 0, 0];
        let encoded = encode_zero_runs(&data);
        let decoded = decode_zero_runs(&encoded, 256);
        assert_eq!(decoded, data);
    }
}
