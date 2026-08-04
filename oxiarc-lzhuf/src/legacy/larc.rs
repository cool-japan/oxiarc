//! LArc `-lzs-` and `-lz5-` codecs.
//!
//! LArc predates LHarc and uses plain LZSS with no entropy coding at all. Two
//! variants survive in the wild inside `.lzh` containers:
//!
//! * **`-lzs-`** — bit-oriented. A `1` bit introduces an 8-bit literal; a `0`
//!   bit introduces an 11-bit **absolute** history index plus a 4-bit length
//!   biased by 2 (so 2..=17 bytes). History is a 2048-byte ring pre-filled with
//!   spaces whose cursor starts at `2048 - 17`.
//! * **`-lz5-`** — byte-oriented. Commands come in runs of eight, introduced by
//!   a bitmap byte read **least significant bit first**; a set bit means "one
//!   literal byte follows", a clear bit means "two bytes follow: a 12-bit
//!   absolute history index and a 4-bit length biased by 3" (3..=18 bytes).
//!   History is a 4096-byte ring pre-seeded with the LArc image (a 13-byte
//!   run of every byte value, an ascending and a descending ramp, NULs and
//!   spaces) whose cursor starts at `4096 - 18`.
//!
//! `-lz4-` is a third LArc identifier and carries **stored** data; it needs no
//! codec and is handled by the stored path in [`crate::decode`].
//!
//! Both are decoded byte-at-a-time through the ring so that self-overlapping
//! copies (index equal to the cursor, the classic run-length idiom) reproduce
//! the reference behaviour exactly.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter};

use super::ring::{GreedyMatcher, Ring, lz5_initial_image};

/// `-lzs-` history size in bytes.
const LZS_RING_SIZE: usize = 2048;
/// `-lzs-` initial cursor (`RING - START_OFFSET`, lhasa `START_OFFSET = 17`).
const LZS_START_OFFSET: usize = 17;
/// `-lzs-` shortest encodable match.
const LZS_MIN_MATCH: usize = 2;
/// `-lzs-` longest encodable match (4-bit length field + `LZS_MIN_MATCH`).
const LZS_MAX_MATCH: usize = 17;
/// Width of the `-lzs-` absolute history index field.
const LZS_INDEX_BITS: u8 = 11;

/// `-lz5-` history size in bytes.
const LZ5_RING_SIZE: usize = 4096;
/// `-lz5-` initial cursor offset (lhasa `START_OFFSET = 18`).
const LZ5_START_OFFSET: usize = 18;
/// `-lz5-` shortest encodable match.
const LZ5_MIN_MATCH: usize = 3;
/// `-lz5-` longest encodable match (4-bit length field + `LZ5_MIN_MATCH`).
const LZ5_MAX_MATCH: usize = 18;

/// Fresh `-lzs-` history: 2 KiB of spaces, cursor 17 bytes before the end.
fn lzs_ring() -> Ring {
    Ring::new(LZS_RING_SIZE, b' ', LZS_RING_SIZE - LZS_START_OFFSET)
}

/// Fresh `-lz5-` history: the LArc seed image, cursor 18 bytes before the end.
fn lz5_ring() -> Ring {
    Ring::from_image(lz5_initial_image(), LZ5_RING_SIZE - LZ5_START_OFFSET)
}

/// Decode a `-lzs-` stream to exactly `uncompressed_size` bytes.
///
/// # Errors
///
/// Returns a corruption error when the stream ends before
/// `uncompressed_size` bytes have been produced. The underlying bit reader
/// zero-pads past end-of-input (the LHA convention), so the check is made on
/// the reader's fabricated-bit counter, never on output length alone — a
/// truncated stream must be an error, not a short `Ok`.
pub fn decode_lzs(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut reader = MsbBitReader::new(data);
    let mut ring = lzs_ring();
    let mut out = Vec::with_capacity(uncompressed_size.min(1 << 20) as usize);

    while (out.len() as u64) < uncompressed_size {
        let literal = reader.get_bit()?;
        if literal {
            let byte = reader.get_bits(8)? as u8;
            check_not_padded(&reader, out.len())?;
            out.push(byte);
            ring.push(byte);
        } else {
            let index = reader.get_bits(LZS_INDEX_BITS)? as usize;
            let length = reader.get_bits(4)? as usize + LZS_MIN_MATCH;
            check_not_padded(&reader, out.len())?;
            copy_from_ring(&mut ring, &mut out, index, length, uncompressed_size);
        }
    }
    Ok(out)
}

/// Decode a `-lz5-` stream to exactly `uncompressed_size` bytes.
///
/// # Errors
///
/// Returns an unexpected-end-of-input error when a command's bytes are
/// missing.
pub fn decode_lz5(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut ring = lz5_ring();
    let mut out = Vec::with_capacity(uncompressed_size.min(1 << 20) as usize);
    let mut cursor = 0usize;
    // `bitmap == 1` means "the eight command flags are exhausted"; the sentinel
    // bit is shifted in with the fresh byte (lhasa's `bitmap | 0x0100`).
    let mut bitmap: u16 = 1;

    while (out.len() as u64) < uncompressed_size {
        if bitmap == 1 {
            let byte = *data
                .get(cursor)
                .ok_or_else(|| OxiArcError::unexpected_eof(1))?;
            cursor += 1;
            bitmap = u16::from(byte) | 0x0100;
        }
        if bitmap & 1 == 1 {
            let byte = *data
                .get(cursor)
                .ok_or_else(|| OxiArcError::unexpected_eof(1))?;
            cursor += 1;
            out.push(byte);
            ring.push(byte);
        } else {
            let low = *data
                .get(cursor)
                .ok_or_else(|| OxiArcError::unexpected_eof(2))?;
            let high = *data
                .get(cursor + 1)
                .ok_or_else(|| OxiArcError::unexpected_eof(2))?;
            cursor += 2;
            let index = ((usize::from(high) & 0xF0) << 4) | usize::from(low);
            let length = (usize::from(high) & 0x0F) + LZ5_MIN_MATCH;
            copy_from_ring(&mut ring, &mut out, index, length, uncompressed_size);
        }
        bitmap >>= 1;
    }
    Ok(out)
}

/// Copy `length` bytes starting at absolute ring index `index`, one byte at a
/// time so that a self-overlapping copy repeats what it just wrote.
fn copy_from_ring(ring: &mut Ring, out: &mut Vec<u8>, index: usize, length: usize, limit: u64) {
    for step in 0..length {
        if (out.len() as u64) >= limit {
            break;
        }
        let byte = ring.byte_at(index + step);
        out.push(byte);
        ring.push(byte);
    }
}

/// Reject a command whose bits were fabricated by end-of-input zero padding.
fn check_not_padded(reader: &MsbBitReader<&[u8]>, produced: usize) -> Result<()> {
    if reader.padding_bits() > 0 {
        return Err(OxiArcError::corrupted(
            produced as u64,
            "truncated LArc stream: command read past end of input",
        ));
    }
    Ok(())
}

/// Encode `data` as a `-lzs-` stream.
///
/// # Errors
///
/// Returns any I/O error raised by the internal bit writer (which writes into
/// an in-memory buffer, so this cannot fail in practice).
pub fn encode_lzs(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 2 + 8);
    {
        let mut writer = MsbBitWriter::new(&mut out);
        let mut ring = lzs_ring();
        let mut matcher =
            GreedyMatcher::new(data.len(), LZS_MIN_MATCH, LZS_MAX_MATCH, LZS_RING_SIZE - 1);
        let mut pos = 0usize;
        while pos < data.len() {
            match matcher.find(data, pos) {
                Some((distance, length)) if length >= LZS_MIN_MATCH => {
                    let index = ring.index_for_distance(distance);
                    writer.put_bit(false)?;
                    writer.put_bits(LZS_INDEX_BITS, index as u32)?;
                    writer.put_bits(4, (length - LZS_MIN_MATCH) as u32)?;
                    for step in 0..length {
                        ring.push(data[pos + step]);
                        matcher.insert(data, pos + step);
                    }
                    pos += length;
                }
                _ => {
                    writer.put_bit(true)?;
                    writer.put_bits(8, u32::from(data[pos]))?;
                    ring.push(data[pos]);
                    matcher.insert(data, pos);
                    pos += 1;
                }
            }
        }
        writer.flush()?;
    }
    Ok(out)
}

/// Encode `data` as a `-lz5-` stream.
///
/// # Errors
///
/// Never fails for well-formed input; the signature is `Result` for symmetry
/// with the other codecs and so future validation has somewhere to report.
pub fn encode_lz5(data: &[u8]) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len() / 2 + 8);
    let mut ring = lz5_ring();
    let mut matcher =
        GreedyMatcher::new(data.len(), LZ5_MIN_MATCH, LZ5_MAX_MATCH, LZ5_RING_SIZE - 1);

    // Commands are buffered eight at a time because the bitmap byte that
    // describes them precedes them in the stream.
    let mut bitmap: u8 = 0;
    let mut filled = 0u8;
    let mut pending: Vec<u8> = Vec::with_capacity(16);
    let mut pos = 0usize;

    while pos < data.len() {
        match matcher.find(data, pos) {
            Some((distance, length)) if length >= LZ5_MIN_MATCH => {
                let index = ring.index_for_distance(distance);
                pending.push((index & 0xFF) as u8);
                pending.push((((index >> 4) & 0xF0) | (length - LZ5_MIN_MATCH)) as u8);
                for step in 0..length {
                    ring.push(data[pos + step]);
                    matcher.insert(data, pos + step);
                }
                pos += length;
            }
            _ => {
                bitmap |= 1 << filled;
                pending.push(data[pos]);
                ring.push(data[pos]);
                matcher.insert(data, pos);
                pos += 1;
            }
        }
        filled += 1;
        if filled == 8 {
            out.push(bitmap);
            out.append(&mut pending);
            bitmap = 0;
            filled = 0;
        }
    }
    if filled > 0 {
        out.push(bitmap);
        out.append(&mut pending);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_lzs(data: &[u8]) {
        let encoded = encode_lzs(data).expect("encode -lzs-");
        let decoded = decode_lzs(&encoded, data.len() as u64).expect("decode -lzs-");
        assert_eq!(decoded, data, "-lzs- round-trip mismatch");
    }

    fn roundtrip_lz5(data: &[u8]) {
        let encoded = encode_lz5(data).expect("encode -lz5-");
        let decoded = decode_lz5(&encoded, data.len() as u64).expect("decode -lz5-");
        assert_eq!(decoded, data, "-lz5- round-trip mismatch");
    }

    #[test]
    fn lzs_roundtrips_text() {
        roundtrip_lzs(b"the quick brown fox jumps over the quick brown fox");
    }

    #[test]
    fn lzs_roundtrips_runs() {
        let data: Vec<u8> = std::iter::repeat_n(b'=', 4000).collect();
        roundtrip_lzs(&data);
    }

    #[test]
    fn lzs_roundtrips_empty_and_single() {
        roundtrip_lzs(b"");
        roundtrip_lzs(b"x");
    }

    #[test]
    fn lzs_roundtrips_binary() {
        let data: Vec<u8> = (0..8192u32)
            .map(|i| (i.wrapping_mul(37) >> 3) as u8)
            .collect();
        roundtrip_lzs(&data);
    }

    #[test]
    fn lz5_roundtrips_text() {
        roundtrip_lz5(b"the quick brown fox jumps over the quick brown fox");
    }

    #[test]
    fn lz5_roundtrips_runs() {
        let data: Vec<u8> = std::iter::repeat_n(b'-', 9000).collect();
        roundtrip_lz5(&data);
    }

    #[test]
    fn lz5_roundtrips_empty_and_single() {
        roundtrip_lz5(b"");
        roundtrip_lz5(b"x");
    }

    #[test]
    fn lz5_roundtrips_binary() {
        let data: Vec<u8> = (0..16384u32).map(|i| (i ^ (i >> 5)) as u8).collect();
        roundtrip_lz5(&data);
    }

    #[test]
    fn lzs_truncated_stream_is_an_error_not_a_short_ok() {
        let data: Vec<u8> = std::iter::repeat_n(b'a', 2000).collect();
        let encoded = encode_lzs(&data).expect("encode");
        for cut in 1..encoded.len() {
            let result = decode_lzs(&encoded[..cut], data.len() as u64);
            if let Ok(ref produced) = result {
                assert_eq!(
                    produced.len(),
                    data.len(),
                    "a successful decode must be complete"
                );
            }
        }
        // A hard truncation to a single byte cannot possibly carry 2000 bytes.
        assert!(decode_lzs(&encoded[..1], data.len() as u64).is_err());
    }

    #[test]
    fn lz5_truncated_stream_is_an_error_not_a_short_ok() {
        let data: Vec<u8> = std::iter::repeat_n(b'b', 3000).collect();
        let encoded = encode_lz5(&data).expect("encode");
        assert!(decode_lz5(&encoded[..1], data.len() as u64).is_err());
        assert!(decode_lz5(&[], data.len() as u64).is_err());
    }

    #[test]
    fn lzs_decodes_a_match_against_the_preinitialised_space_history() {
        // A single copy command reading the fresh (all-space) history proves
        // the ring is seeded with 0x20 and addressed absolutely: index 0,
        // length 2 + 5 = 7 spaces.
        let mut encoded = Vec::new();
        {
            let mut writer = MsbBitWriter::new(&mut encoded);
            writer.put_bit(false).expect("flag");
            writer.put_bits(LZS_INDEX_BITS, 0).expect("index");
            writer.put_bits(4, 5).expect("length");
            writer.flush().expect("flush");
        }
        let decoded = decode_lzs(&encoded, 7).expect("decode");
        assert_eq!(decoded, b"       ");
    }

    #[test]
    fn lz5_decodes_a_match_against_the_seeded_history() {
        // Index 3328 is the start of the ascending 0..255 ramp in the LArc
        // seed image; a length-3 copy there must yield 0x00 0x01 0x02.
        let index = 3328usize;
        let encoded = vec![
            0b0000_0000, // bitmap: first command is a copy
            (index & 0xFF) as u8,
            ((index >> 4) & 0xF0) as u8, // length nibble 0 => 3 bytes
        ];
        let decoded = decode_lz5(&encoded, 3).expect("decode");
        assert_eq!(decoded, vec![0u8, 1, 2]);
    }

    #[test]
    fn lz5_bitmap_is_read_least_significant_bit_first() {
        // bitmap 0b0000_0001: command 0 is a literal, command 1 is a copy.
        let index = 3328usize;
        let encoded = vec![
            0b0000_0001,
            b'Z',
            (index & 0xFF) as u8,
            (((index >> 4) & 0xF0) | 1) as u8,
        ];
        let decoded = decode_lz5(&encoded, 5).expect("decode");
        assert_eq!(decoded, vec![b'Z', 0, 1, 2, 3]);
    }

    #[test]
    fn lzs_compresses_repetitive_input() {
        let data: Vec<u8> = std::iter::repeat_n(b"abcdefgh", 512)
            .flatten()
            .copied()
            .collect();
        let encoded = encode_lzs(&data).expect("encode");
        assert!(
            encoded.len() < data.len() / 2,
            "expected real compression, got {} from {}",
            encoded.len(),
            data.len()
        );
        roundtrip_lzs(&data);
    }

    #[test]
    fn lz5_compresses_repetitive_input() {
        let data: Vec<u8> = std::iter::repeat_n(b"abcdefgh", 512)
            .flatten()
            .copied()
            .collect();
        let encoded = encode_lz5(&data).expect("encode");
        assert!(
            encoded.len() < data.len() / 3,
            "expected real compression, got {} from {}",
            encoded.len(),
            data.len()
        );
        roundtrip_lz5(&data);
    }
}
