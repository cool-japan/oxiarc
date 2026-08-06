//! `-lh2-` (LHarc 2.x adaptive-Huffman LZSS) codec.
//!
//! `-lh2-` pairs an 8 KiB LZSS window with the *dynamic* (adaptive) Huffman
//! tree of LHarc 2.x: there is no per-block code table on the wire at all, both
//! sides simply keep identical adaptive trees. Two symbol spaces are coded:
//!
//! * a 286-symbol literal/length tree — `0..=255` are literals, `256..=284`
//!   are match lengths `3..=31`, and symbol `285` is an escape meaning "eight
//!   raw bits follow", covering lengths `32..=287`;
//! * a match-position tree over 64-distance *groups*, which starts with a
//!   single group (so the first 64 distances cost zero tree bits) and grows by
//!   one group every time the output passes another 64-byte boundary. Each
//!   coded group is followed by six raw bits selecting the distance inside it.
//!
//! ## Verification
//!
//! Neither Lhasa nor `delharc` implements `-lh2-`, so there is no decoder on
//! `PATH` to act as an oracle. Conformance was instead established against the
//! canonical LHa `dhuf.c` decode path, compiled standalone and driven with the
//! streams this encoder produces; the byte vectors it accepted are frozen into
//! `tests/lzh_legacy_vectors.rs` so the in-repo gate stays hermetic.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter};

use super::dynhuff::{DynTree, THRESHOLD};
use super::ring::{GreedyMatcher, Ring};

/// `-lh2-` dictionary size (LHa `LZHUFF2_DICBIT = 13`).
pub(crate) const LH2_DICT_SIZE: usize = 1 << 13;
/// Shortest encodable match.
const MIN_MATCH: usize = THRESHOLD;
/// Longest encodable match (LHa `MAXMATCH`).
const MAX_MATCH: usize = 256;
/// Length symbols start here: symbol `256` means length `3`.
const LENGTH_BASE: usize = 256 - THRESHOLD;

/// Decode a `-lh2-` stream to exactly `uncompressed_size` bytes.
///
/// # Errors
///
/// Returns a corruption error when the stream is truncated (any bit
/// consumed by a command came from end-of-input zero padding) or carries a
/// symbol outside the format's range.
pub fn decode_lh2(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut reader = MsbBitReader::new(data);
    let mut tree = DynTree::new_lh2();
    let mut ring = Ring::new(LH2_DICT_SIZE, b' ', 0);
    let mut out: Vec<u8> = Vec::with_capacity(uncompressed_size.min(1 << 20) as usize);

    while (out.len() as u64) < uncompressed_size {
        let mut symbol = tree.decode_c(|| Ok(reader.get_bit()?))?;
        if symbol == tree.escape_symbol() {
            symbol += reader.get_bits(8)? as usize;
        }
        if reader.padding_bits() > 0 {
            return Err(OxiArcError::corrupted(
                out.len() as u64,
                "truncated -lh2- stream: literal/length code read past end of input",
            ));
        }

        if symbol < 256 {
            let byte = symbol as u8;
            out.push(byte);
            ring.push(byte);
            continue;
        }

        let length = symbol - LENGTH_BASE;
        tree.grow_positions(out.len() as u64);
        let group = tree.decode_p(|| Ok(reader.get_bit()?))?;
        let distance = (group << 6) + reader.get_bits(6)? as usize + 1;
        if reader.padding_bits() > 0 {
            return Err(OxiArcError::corrupted(
                out.len() as u64,
                "truncated -lh2- stream: match position read past end of input",
            ));
        }

        let index = ring.index_for_distance(distance);
        for step in 0..length {
            if (out.len() as u64) >= uncompressed_size {
                break;
            }
            let byte = ring.byte_at(index + step);
            out.push(byte);
            ring.push(byte);
        }
    }
    Ok(out)
}

/// Encode `data` as a `-lh2-` stream.
///
/// The encoder mirrors the decoder's tree mutations one for one — the code
/// path, the escape, the position-group growth check and the two tree updates
/// all happen at exactly the bit-stream position the reference decoder performs
/// them, which is what makes the output readable by an independent
/// implementation rather than merely by this crate.
///
/// # Errors
///
/// Returns any I/O error raised by the in-memory bit writer.
pub fn encode_lh2(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 2 + 8);
    {
        let mut writer = MsbBitWriter::new(&mut out);
        let mut tree = DynTree::new_lh2();
        let mut matcher = GreedyMatcher::new(data.len(), MIN_MATCH, MAX_MATCH, LH2_DICT_SIZE);
        let escape = tree.escape_symbol();

        let mut pos = 0usize;
        while pos < data.len() {
            let candidate = matcher.find(data, pos);
            match candidate {
                Some((distance, length)) if length >= MIN_MATCH => {
                    let symbol = length + LENGTH_BASE;
                    write_symbol(&mut writer, &mut tree, symbol, escape)?;
                    tree.grow_positions(pos as u64);
                    let group = (distance - 1) >> 6;
                    for bit in tree.encode_p(group) {
                        writer.put_bit(bit)?;
                    }
                    writer.put_bits(6, ((distance - 1) & 0x3F) as u32)?;
                    for step in 0..length {
                        matcher.insert(data, pos + step);
                    }
                    pos += length;
                }
                _ => {
                    write_symbol(&mut writer, &mut tree, usize::from(data[pos]), escape)?;
                    matcher.insert(data, pos);
                    pos += 1;
                }
            }
        }
        writer.flush()?;
    }
    Ok(out)
}

/// Emit one literal/length symbol, using the escape form when it is at or
/// above the escape code.
fn write_symbol<W: std::io::Write>(
    writer: &mut MsbBitWriter<W>,
    tree: &mut DynTree,
    symbol: usize,
    escape: usize,
) -> Result<()> {
    let (coded, extra) = if symbol >= escape {
        (escape, Some(symbol - escape))
    } else {
        (symbol, None)
    };
    for bit in tree.encode_c(coded) {
        writer.put_bit(bit)?;
    }
    if let Some(extra) = extra {
        debug_assert!(extra <= 0xFF, "escape payload must fit in eight bits");
        writer.put_bits(8, extra as u32)?;
    }
    Ok(())
}

/// Position-symbol offset inside the shared symbol space, re-exported for the
/// reference-vector test to assert the constant it was validated against.
#[cfg(test)]
pub(crate) const POSITION_SYMBOL_BASE: usize = super::dynhuff::N_CHAR;

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let encoded = encode_lh2(data).expect("encode -lh2-");
        let decoded = decode_lh2(&encoded, data.len() as u64).expect("decode -lh2-");
        assert_eq!(decoded, data, "-lh2- round-trip mismatch");
        encoded
    }

    #[test]
    fn constants_match_the_canonical_lha_definitions() {
        assert_eq!(LH2_DICT_SIZE, 8192);
        assert_eq!(MIN_MATCH, 3);
        assert_eq!(MAX_MATCH, 256);
        assert_eq!(LENGTH_BASE, 253);
        assert_eq!(POSITION_SYMBOL_BASE, 314);
    }

    #[test]
    fn roundtrips_text() {
        roundtrip(b"the quick brown fox jumps over the lazy dog, the quick brown fox");
    }

    #[test]
    fn roundtrips_empty_and_single_byte() {
        roundtrip(b"");
        roundtrip(b"Q");
    }

    #[test]
    fn roundtrips_long_runs_using_the_escape_symbol() {
        // A 5000-byte run forces matches far longer than 31, i.e. the escape
        // code plus eight raw bits.
        let data: Vec<u8> = std::iter::repeat_n(b'#', 5000).collect();
        let encoded = roundtrip(&data);
        assert!(
            encoded.len() < data.len() / 10,
            "a pure run must compress hard: {} from {}",
            encoded.len(),
            data.len()
        );
    }

    #[test]
    fn roundtrips_across_the_position_group_boundaries() {
        // Distances that straddle 64, 128, ... exercise `grow_positions`.
        let mut data = Vec::new();
        for block in 0..64u32 {
            data.extend((0..97u32).map(|i| (i.wrapping_mul(block + 1)) as u8));
        }
        data.extend_from_slice(&data.clone());
        roundtrip(&data);
    }

    #[test]
    fn roundtrips_binary_and_high_entropy() {
        let data: Vec<u8> = (0..20_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        roundtrip(&data);
    }

    #[test]
    fn roundtrips_input_larger_than_the_window() {
        let unit = b"OxiArc legacy LZH method -lh2- exercise line.\n";
        let data: Vec<u8> = std::iter::repeat_n(unit, 900).flatten().copied().collect();
        assert!(data.len() > 4 * LH2_DICT_SIZE);
        let encoded = roundtrip(&data);
        assert!(encoded.len() < data.len() / 8);
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_short_ok() {
        let data: Vec<u8> = std::iter::repeat_n(b"abcdefghij", 400)
            .flatten()
            .copied()
            .collect();
        let encoded = encode_lh2(&data).expect("encode");
        assert!(decode_lh2(&encoded[..1], data.len() as u64).is_err());
        assert!(decode_lh2(&[], data.len() as u64).is_err());
        for cut in [2usize, 8, 32, 64] {
            if cut < encoded.len() {
                let result = decode_lh2(&encoded[..cut], data.len() as u64);
                assert!(
                    result.is_err(),
                    "a stream cut to {cut} bytes cannot yield {} bytes",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn decoding_stops_exactly_at_the_declared_size() {
        let data: Vec<u8> = std::iter::repeat_n(b'z', 1000).collect();
        let encoded = encode_lh2(&data).expect("encode");
        for size in [1u64, 2, 17, 255, 999] {
            let decoded = decode_lh2(&encoded, size).expect("decode prefix");
            assert_eq!(decoded.len(), size as usize);
            assert!(decoded.iter().all(|&b| b == b'z'));
        }
    }
}
