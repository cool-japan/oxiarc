//! Compressed block assembly for Zstandard encoding.
//!
//! Assembles compressed Zstandard blocks from LZ77 sequences.
//!
//! A compressed block consists of:
//! 1. **Literals section** (raw, RLE, or Huffman-encoded literal bytes)
//! 2. **Sequences section** (FSE-encoded sequences describing literal lengths,
//!    match lengths, and offsets)
//!
//! The literals header format in Zstd is:
//! - Bits \[1:0\] = `Literals_Block_Type` (00=Raw, 01=RLE, 10=Compressed, 11=Treeless)
//! - Bits \[3:2\] = `Size_Format`
//! - Remaining bits = sizes (depending on `Size_Format`)
//!
//! For Raw/RLE:
//! - `Size_Format` 00 or 10: 1-byte header, 5-bit regenerated_size (max 31)
//! - `Size_Format` 01: 2-byte header, 12-bit regenerated_size (max 4095)
//! - `Size_Format` 11: 3-byte header, 20-bit regenerated_size (max ~1M)
//!
//! For Compressed/Treeless:
//! - `Size_Format` 00: single stream, 3-byte header, 10+10 bits (regen + compressed)
//! - `Size_Format` 01: 4 streams, 3-byte header, 10+10 bits
//! - `Size_Format` 10: 4 streams, 4-byte header, 14+14 bits
//! - `Size_Format` 11: 4 streams, 5-byte header, 18+18 bits

use crate::bitwriter::BackwardBitWriter;
use crate::fse_encoder;
use crate::huffman_encoder::HuffmanEncoder;
use crate::literals::LiteralsDecoder;
use crate::lz77::Lz77Sequence;
use oxiarc_core::error::{OxiArcError, Result};

/// A Zstd-format sequence with pre-computed symbol codes and extra bits.
///
/// The Zstd sequence encoding transforms raw (literal_length, match_length, offset)
/// triples into compact (code, extra_bits, extra_value) representations using the
/// standard Zstd symbol tables.
#[derive(Debug, Clone, Copy)]
struct ZstdSequence {
    /// Literal-length code (0..35).
    ll_code: u8,
    /// Number of extra bits for the literal length.
    ll_extra_bits: u8,
    /// Extra-bit value for the literal length.
    ll_extra_value: u32,
    /// Match-length code (0..52).
    ml_code: u8,
    /// Number of extra bits for the match length.
    ml_extra_bits: u8,
    /// Extra-bit value for the match length.
    ml_extra_value: u32,
    /// Offset code (highest set bit position of offset value).
    of_code: u8,
    /// Number of extra bits for the offset.
    of_extra_bits: u8,
    /// Extra-bit value for the offset.
    of_extra_value: u32,
}

/// Literal length baseline and extra-bit counts (indexed by code 0..35).
const LL_BASELINE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];

/// Number of extra bits for each literal-length code.
const LL_EXTRA: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

/// Match length baseline values (indexed by code 0..52).
const ML_BASELINE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];

/// Number of extra bits for each match-length code.
const ML_EXTRA: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Sequence compression mode for encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SequenceCompressionMode {
    /// Use the predefined FSE table from the specification.
    Predefined,
    /// RLE mode: every symbol in this category is the same value.
    Rle(u8),
    /// `FSE_Compressed_Mode`: a table built from this block's own symbol
    /// distribution, described inline in the Sequences Section header.
    ///
    /// Carries the normalized counts and the accuracy log they were normalized
    /// at, plus the already-serialized table description so it is written and
    /// costed from a single source of truth.
    Fse {
        /// Normalized counts summing to `1 << table_log`.
        norm: Vec<i16>,
        /// Accuracy log of the table.
        table_log: u8,
        /// Serialized RFC 8878 §4.1.1 table description.
        description: Vec<u8>,
    },
}

/// Encode a compressed block from LZ77 sequences.
///
/// Returns the block content (without the 3-byte block header).  The caller
/// is responsible for writing the `Last_Block` flag and block-type/size header.
///
/// # Errors
///
/// Returns an error if the input sequences are malformed or cannot be encoded.
pub fn encode_compressed_block(sequences: &[Lz77Sequence]) -> Result<Vec<u8>> {
    // 1. Collect all literals.
    let literals: Vec<u8> = sequences
        .iter()
        .flat_map(|s| s.literals.iter().copied())
        .collect();

    // 2. Encode literals section.
    let literals_section = encode_literals_section(&literals)?;

    // 3. Filter to actual back-reference sequences (match_length > 0).
    let ref_sequences: Vec<&Lz77Sequence> =
        sequences.iter().filter(|s| s.match_length > 0).collect();

    // 4. Convert to Zstd-format sequence codes.
    let zstd_sequences = convert_sequences(&ref_sequences)?;

    // 5. Encode sequences section.
    let sequences_section = encode_sequences_section(&zstd_sequences)?;

    // 6. Combine.
    let mut block = Vec::with_capacity(literals_section.len() + sequences_section.len());
    block.extend_from_slice(&literals_section);
    block.extend_from_slice(&sequences_section);

    Ok(block)
}

// ---------------------------------------------------------------------------
// Literals section encoding
// ---------------------------------------------------------------------------

/// Minimum literals-section size worth attempting Huffman compression on
/// (the table description alone costs up to ~129 bytes).
const HUFFMAN_LITERALS_MIN: usize = 64;

/// Encode the literals section.
///
/// Chooses the smallest valid representation among RLE (all bytes equal),
/// Huffman-compressed (when it wins and self-verifies), and Raw.
/// [`encode_literals_section`] for the literals decoder's own tests.
///
/// The four-stream Huffman sections this produces are the only realistic
/// input for the differential between the interleaved and checked decoders,
/// and hand-writing one would test the hand-written bits, not the format.
#[cfg(test)]
pub(crate) fn encode_literals_section_for_test(literals: &[u8]) -> Result<Vec<u8>> {
    encode_literals_section(literals)
}

fn encode_literals_section(literals: &[u8]) -> Result<Vec<u8>> {
    if literals.is_empty() {
        // Raw literals with 0 size: single header byte.
        return Ok(vec![0]);
    }

    // Check if all bytes are identical (RLE candidate).
    let first = literals[0];
    let all_same = literals.iter().all(|&b| b == first);
    if all_same {
        return encode_rle_literals(literals);
    }

    let raw = encode_raw_literals(literals)?;
    if literals.len() >= HUFFMAN_LITERALS_MIN {
        if let Some(compressed) = try_huffman_literals(literals) {
            if compressed.len() < raw.len() {
                return Ok(compressed);
            }
        }
    }
    Ok(raw)
}

/// Attempt to Huffman-compress the literals section (RFC 8878 §3.1.1.3.1.4).
///
/// Produces a `Compressed_Literals_Block` with a direct-weight Huffman table
/// description and a 1-stream (< 1 KiB) or 4-stream (with jump table)
/// payload. Returns `None` whenever Huffman cannot be applied or does not
/// verify — the caller then falls back to Raw literals.
///
/// As a hard safety gate the finished section is decoded back with this
/// crate's (reference-exact) literals decoder and must reproduce the input
/// byte-for-byte; any imperfection downgrades to Raw rather than risking an
/// invalid frame.
fn try_huffman_literals(literals: &[u8]) -> Option<Vec<u8>> {
    let mut frequencies = [0u64; 256];
    for &byte in literals {
        frequencies[byte as usize] += 1;
    }
    let encoder = HuffmanEncoder::from_frequencies(&frequencies)?;
    // Defense in depth: RFC 8878 caps literal code lengths at 11 bits and a
    // usable table needs at least two coded symbols.
    if encoder.max_bits() > crate::huffman::MAX_CODE_LENGTH || encoder.num_symbols() < 2 {
        return None;
    }
    let table = encoder.serialize_table();

    let single_stream = literals.len() < 1024;
    let streams = if single_stream {
        // Single stream (Size_Format 00).
        encoder.encode_literals(literals)
    } else {
        // 4 streams: streams 1-3 carry ceil(n/4) literals each, stream 4 the
        // remainder; a 6-byte jump table holds the sizes of streams 1-3.
        let quarter = literals.len().div_ceil(4);
        let (part1, rest) = literals.split_at(quarter);
        let (part2, rest) = rest.split_at(quarter);
        let (part3, part4) = rest.split_at(quarter);
        let enc1 = encoder.encode_literals(part1);
        let enc2 = encoder.encode_literals(part2);
        let enc3 = encoder.encode_literals(part3);
        let enc4 = encoder.encode_literals(part4);
        if enc1.len() > u16::MAX as usize
            || enc2.len() > u16::MAX as usize
            || enc3.len() > u16::MAX as usize
        {
            return None;
        }
        let mut combined =
            Vec::with_capacity(6 + enc1.len() + enc2.len() + enc3.len() + enc4.len());
        combined.extend_from_slice(&(enc1.len() as u16).to_le_bytes());
        combined.extend_from_slice(&(enc2.len() as u16).to_le_bytes());
        combined.extend_from_slice(&(enc3.len() as u16).to_le_bytes());
        combined.extend_from_slice(&enc1);
        combined.extend_from_slice(&enc2);
        combined.extend_from_slice(&enc3);
        combined.extend_from_slice(&enc4);
        combined
    };

    // The 3-byte header (Size_Format 00) only expresses sizes below 1024 and
    // is the sole single-stream format; bail out if it cannot represent us.
    let compressed_size = table.len() + streams.len();
    if single_stream && compressed_size >= 1024 {
        return None;
    }

    let section = encode_compressed_literals(literals.len(), &table, &streams).ok()?;

    // Self-verification gate: never emit a Huffman section our own
    // reference-exact decoder cannot reproduce exactly.
    let mut check = LiteralsDecoder::new();
    match check.decode(&section) {
        Ok((decoded, consumed)) if decoded == literals && consumed == section.len() => {
            Some(section)
        }
        _ => None,
    }
}

/// Encode raw literals (uncompressed).
///
/// Header format for type=Raw (00):
/// - `Size_Format` 00/10: 1-byte header, 5-bit size (max 31)
/// - `Size_Format` 01: 2-byte header, 12-bit size (max 4095)
/// - `Size_Format` 11: 3-byte header, 20-bit size (max ~1M)
fn encode_raw_literals(literals: &[u8]) -> Result<Vec<u8>> {
    let size = literals.len();
    let mut out = Vec::with_capacity(3 + size);

    if size < 32 {
        // 1-byte header: type(2)=00 | size_format(2)=00 | regen_size(4 high of 5 bits)
        // Byte layout: [regen_size(5) | size_format(2) | type(2)]
        //              size_format = 0b00 for 1-byte / 5-bit
        out.push((size as u8) << 3); // type=0, size_format=0
    } else if size < 4096 {
        // 2-byte header: 12-bit regenerated_size
        // type=00, size_format=01
        let header: u16 = (0b01 << 2)               // type = Raw, size_format = 1
            | ((size as u16) << 4);
        out.push((header & 0xFF) as u8);
        out.push((header >> 8) as u8);
    } else {
        // 3-byte header: 20-bit regenerated_size
        // type=00, size_format=11
        let header: u32 = (0b11 << 2)               // type = Raw, size_format = 3
            | ((size as u32) << 4);
        out.push((header & 0xFF) as u8);
        out.push(((header >> 8) & 0xFF) as u8);
        out.push(((header >> 16) & 0xFF) as u8);
    }

    out.extend_from_slice(literals);
    Ok(out)
}

/// Encode RLE literals (single byte repeated).
///
/// Header format for type=RLE (01):
/// Same size encoding as Raw but `type` bits are 01.
fn encode_rle_literals(literals: &[u8]) -> Result<Vec<u8>> {
    let byte = literals[0];
    let size = literals.len();
    let mut out = Vec::with_capacity(4);

    if size < 32 {
        // 1-byte header
        out.push(((size as u8) << 3) | 0b01); // type=RLE(01), size_format=00
    } else if size < 4096 {
        // 2-byte header: 12-bit size
        let header: u16 = 0b01          // type = RLE
            | (0b01 << 2)               // size_format = 1
            | ((size as u16) << 4);
        out.push((header & 0xFF) as u8);
        out.push((header >> 8) as u8);
    } else {
        // 3-byte header: 20-bit size
        let header: u32 = 0b01          // type = RLE
            | (0b11 << 2)               // size_format = 3
            | ((size as u32) << 4);
        out.push((header & 0xFF) as u8);
        out.push(((header >> 8) & 0xFF) as u8);
        out.push(((header >> 16) & 0xFF) as u8);
    }

    out.push(byte);
    Ok(out)
}

/// Encode Huffman-compressed literals.
///
/// This produces a Compressed-type literals header followed by the Huffman
/// table description and the compressed bitstream.
fn encode_compressed_literals(regen_size: usize, table: &[u8], streams: &[u8]) -> Result<Vec<u8>> {
    let compressed_size = table.len() + streams.len();
    let mut out = Vec::with_capacity(5 + compressed_size);

    if regen_size < 1024 && compressed_size < 1024 {
        // 3-byte header: single stream, 10+10 bits
        // type=10 (Compressed), size_format=00 (single stream)
        let header: u32 = 0b10                     // type = Compressed, size_format = 0 (single stream)
            | ((regen_size as u32) << 4)
            | ((compressed_size as u32) << 14);
        out.push((header & 0xFF) as u8);
        out.push(((header >> 8) & 0xFF) as u8);
        out.push(((header >> 16) & 0xFF) as u8);
    } else if regen_size < 16384 && compressed_size < 16384 {
        // 4-byte header: 4 streams, 14+14 bits
        let header: u32 = 0b10                     // type = Compressed
            | (0b10 << 2)                          // size_format = 2
            | ((regen_size as u32) << 4)
            | ((compressed_size as u32) << 18);
        out.push((header & 0xFF) as u8);
        out.push(((header >> 8) & 0xFF) as u8);
        out.push(((header >> 16) & 0xFF) as u8);
        out.push(((header >> 24) & 0xFF) as u8);
    } else {
        // 5-byte header: 4 streams, 18+18 bits
        let header: u64 = 0b10                     // type = Compressed
            | (0b11 << 2)                          // size_format = 3
            | ((regen_size as u64) << 4)
            | ((compressed_size as u64) << 22);
        out.push((header & 0xFF) as u8);
        out.push(((header >> 8) & 0xFF) as u8);
        out.push(((header >> 16) & 0xFF) as u8);
        out.push(((header >> 24) & 0xFF) as u8);
        out.push(((header >> 32) & 0xFF) as u8);
    }

    out.extend_from_slice(table);
    out.extend_from_slice(streams);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sequence conversion (raw values -> Zstd codes)
// ---------------------------------------------------------------------------

/// Convert raw LZ77 sequences into Zstd-coded sequences.
fn convert_sequences(sequences: &[&Lz77Sequence]) -> Result<Vec<ZstdSequence>> {
    let mut out = Vec::with_capacity(sequences.len());

    for seq in sequences {
        let ll = seq.literals.len() as u32;
        let ml = seq.match_length as u32;
        let offset = seq.offset as u32;

        let (ll_code, ll_extra_bits, ll_extra_value) = encode_literal_length(ll)?;
        let (ml_code, ml_extra_bits, ml_extra_value) = encode_match_length(ml)?;
        let (of_code, of_extra_bits, of_extra_value) = encode_offset(offset)?;

        out.push(ZstdSequence {
            ll_code,
            ll_extra_bits,
            ll_extra_value,
            ml_code,
            ml_extra_bits,
            ml_extra_value,
            of_code,
            of_extra_bits,
            of_extra_value,
        });
    }

    Ok(out)
}

/// Encode a literal length value into (code, extra_bits, extra_value).
fn encode_literal_length(value: u32) -> Result<(u8, u8, u32)> {
    for (code, (&baseline, &extra)) in LL_BASELINE.iter().zip(LL_EXTRA.iter()).enumerate().rev() {
        if value >= baseline {
            let extra_value = value - baseline;
            return Ok((code as u8, extra, extra_value));
        }
    }
    // Should be unreachable since LL_BASELINE[0] == 0.
    Ok((0, 0, value))
}

/// Encode a match length value into (code, extra_bits, extra_value).
fn encode_match_length(value: u32) -> Result<(u8, u8, u32)> {
    if value < 3 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("match length {} is less than minimum 3", value),
        });
    }
    for (code, (&baseline, &extra)) in ML_BASELINE.iter().zip(ML_EXTRA.iter()).enumerate().rev() {
        if value >= baseline {
            let extra_value = value - baseline;
            return Ok((code as u8, extra, extra_value));
        }
    }
    // Should be unreachable since ML_BASELINE[0] == 3 and we check >= 3 above.
    Err(OxiArcError::CorruptedData {
        offset: 0,
        message: format!("could not encode match length {}", value),
    })
}

/// Encode an offset value into (code, extra_bits, extra_value).
///
/// In Zstandard, the raw offset `d` is first converted to an `Offset_Value`
/// by adding 3 (to skip the repeat offset codes 1, 2, 3).
///
/// `Offset_Value = d + 3`
///
/// The offset code is the highest set bit position of `Offset_Value`.
/// Extra bits are the remaining lower bits.
fn encode_offset(offset: u32) -> Result<(u8, u8, u32)> {
    if offset == 0 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "offset must be >= 1".to_string(),
        });
    }
    // Convert raw offset to Offset_Value (skip repeat offset codes).
    let offset_value = offset + 3;
    let code = 31 - offset_value.leading_zeros(); // highest bit position
    let extra_bits = code as u8;
    let extra_value = offset_value - (1u32 << code);
    Ok((code as u8, extra_bits, extra_value))
}

// ---------------------------------------------------------------------------
// Sequences section encoding
// ---------------------------------------------------------------------------

/// Encode the sequences section.
///
/// Produces:
/// 1. Sequence count (variable length 1-3 bytes).
/// 2. Compression-modes byte.
/// 3. Per-mode table descriptions (for RLE modes, a single symbol byte).
/// 4. A backward bitstream containing the FSE-encoded symbols and extra bits.
fn encode_sequences_section(sequences: &[ZstdSequence]) -> Result<Vec<u8>> {
    if sequences.is_empty() {
        return Ok(vec![0]); // 0 sequences
    }

    let mut out = Vec::new();

    // Write number of sequences (variable-length encoding).
    let count = sequences.len();
    if count < 128 {
        out.push(count as u8);
    } else if count < 0x7F00 {
        out.push(((count >> 8) as u8) + 128);
        out.push((count & 0xFF) as u8);
    } else {
        out.push(255);
        let adjusted = count - 0x7F00;
        out.push((adjusted & 0xFF) as u8);
        out.push(((adjusted >> 8) & 0xFF) as u8);
    }

    // Determine compression mode for each symbol type.
    let (ll_freqs, of_freqs, ml_freqs) = count_symbol_frequencies(sequences);
    let ll_mode = choose_mode(&ll_freqs, TableCategory::LiteralLength);
    let of_mode = choose_mode(&of_freqs, TableCategory::Offset);
    let ml_mode = choose_mode(&ml_freqs, TableCategory::MatchLength);

    // Write compression-modes byte.
    // Bits: [LL(2)][OF(2)][ML(2)][reserved(2)]
    let modes_byte = (mode_to_bits(&ll_mode) << 6)
        | (mode_to_bits(&of_mode) << 4)
        | (mode_to_bits(&ml_mode) << 2);
    out.push(modes_byte);

    // Write per-mode table data.
    write_mode_table_data(&mut out, &ll_mode);
    write_mode_table_data(&mut out, &of_mode);
    write_mode_table_data(&mut out, &ml_mode);

    // Encode the backward bitstream containing FSE states + extra bits.
    let bitstream = encode_sequences_bitstream(sequences, &ll_mode, &of_mode, &ml_mode)?;
    out.extend_from_slice(&bitstream);

    Ok(out)
}

/// Choose the cheapest RFC 8878 compression mode for one symbol category.
///
/// The decision is made on measured cost, not on a heuristic guess:
///
/// * If every sequence carries the same code, `RLE` wins trivially — one byte
///   of header and not a single bit in the bitstream.
/// * Otherwise the block's own distribution is normalized and serialized, and
///   the resulting `FSE_Compressed_Mode` cost (entropy under the custom table
///   **plus** the bytes its description occupies) is compared against the cost
///   of the RFC's predefined table. The cheaper one is used.
///
/// Comparing total cost — rather than always taking the custom table — is what
/// keeps small blocks from regressing: a 20-sequence block would spend more on
/// the table description than the sharper distribution ever recovers.
///
/// Any failure to build a custom table (an alphabet wider than the table, a
/// distribution the normalizer cannot represent) simply falls through to the
/// predefined table, which is always valid.
fn choose_mode(frequencies: &[u32], category: TableCategory) -> SequenceCompressionMode {
    let total: u32 = frequencies.iter().sum();
    if total == 0 {
        return SequenceCompressionMode::Predefined;
    }

    // Single distinct symbol -> RLE.
    let mut used = frequencies.iter().enumerate().filter(|&(_, &f)| f > 0);
    let first = used.next();
    // NOTE: deliberately NOT an `if let ... && ...` let-chain: those need
    // rustc 1.88 and this workspace's MSRV is 1.85 (see `rust-version` in the
    // root Cargo.toml and `msrv` in clippy.toml).
    if let Some((symbol, &count)) = first {
        if count == total {
            return SequenceCompressionMode::Rle(symbol as u8);
        }
    }

    let predefined = predefined_distribution(category);
    let predefined_cost =
        fse_encoder::estimate_encoded_bits(frequencies, predefined.norm, predefined.table_log);

    match build_custom_mode(frequencies, total, category) {
        Some((mode, custom_cost)) => match predefined_cost {
            // The predefined table cannot represent this block's alphabet at
            // all (an offset code beyond 28, for instance): the custom table is
            // not just cheaper, it is the only legal choice.
            None => mode,
            Some(base) if custom_cost < base => mode,
            Some(_) => SequenceCompressionMode::Predefined,
        },
        None => SequenceCompressionMode::Predefined,
    }
}

/// Build an `FSE_Compressed_Mode` candidate and its total cost in bits.
///
/// Returns `None` when no valid custom table exists for this distribution.
fn build_custom_mode(
    frequencies: &[u32],
    total: u32,
    category: TableCategory,
) -> Option<(SequenceCompressionMode, f64)> {
    let max_symbol = frequencies.iter().rposition(|&f| f > 0)? as u8;
    let table_log =
        fse_encoder::optimal_table_log(max_table_log(category), total as usize, max_symbol);
    // The reference switches to the "less than one" marker only for blocks with
    // many sequences, where its extra precision outweighs the wider table it
    // implies (`ZSTD_useLowProbCount`).
    let low_prob_count = if total >= 2048 { -1i16 } else { 1i16 };

    let alphabet = &frequencies[..=max_symbol as usize];
    let norm = fse_encoder::normalize_counts(alphabet, total, table_log, low_prob_count).ok()?;
    let description = fse_encoder::write_ncount(&norm, table_log).ok()?;
    // Reject anything the decoder side would not accept, before it reaches an
    // archive: build the very table the decoder will build.
    FseCTable::from_normalized(table_log, &norm).ok()?;

    let bits = fse_encoder::estimate_encoded_bits(alphabet, &norm, table_log)?;
    let cost = bits + (description.len() as f64) * 8.0;
    Some((
        SequenceCompressionMode::Fse {
            norm,
            table_log,
            description,
        },
        cost,
    ))
}

/// Convert a `SequenceCompressionMode` to its 2-bit representation.
fn mode_to_bits(mode: &SequenceCompressionMode) -> u8 {
    match mode {
        SequenceCompressionMode::Predefined => 0,
        SequenceCompressionMode::Rle(_) => 1,
        SequenceCompressionMode::Fse { .. } => 2,
    }
}

/// Write the table description bytes for a mode (nothing for Predefined,
/// one symbol byte for RLE, the serialized normalized counts for FSE).
fn write_mode_table_data(out: &mut Vec<u8>, mode: &SequenceCompressionMode) {
    match mode {
        SequenceCompressionMode::Predefined => {}
        SequenceCompressionMode::Rle(symbol) => {
            out.push(*symbol);
        }
        SequenceCompressionMode::Fse { description, .. } => {
            out.extend_from_slice(description);
        }
    }
}

/// FSE compression table, mirroring the reference `FSE_buildCTable`.
///
/// `state_table` maps a "find state" index to the next encoder state value
/// (which lives in `[table_size, 2*table_size)`); `symbol_tt` carries the
/// per-symbol `(delta_nb_bits, delta_find_state)` transformation exactly as
/// in the reference implementation.
struct FseCTable {
    /// Accuracy log (table size = 1 << table_log).
    table_log: u8,
    /// Next-state lookup, indexed by `(state >> nb_bits) + delta_find_state`.
    state_table: Vec<u16>,
    /// Per-symbol transformation entries.
    symbol_tt: Vec<SymbolTransform>,
}

/// Per-symbol encoding transform (see reference `FSE_symbolCompressionTransform`).
#[derive(Debug, Clone, Copy, Default)]
struct SymbolTransform {
    /// Encodes both the bit count threshold and the state cutoff.
    delta_nb_bits: u32,
    /// Offset into `state_table` for this symbol's states.
    delta_find_state: i32,
}

/// A running FSE encoder state.
struct FseCState {
    value: usize,
}

impl FseCTable {
    /// Build a compression table from a normalized distribution
    /// (probabilities summing to `1 << table_log`, `-1` = "less than one").
    fn from_normalized(table_log: u8, norm: &[i16]) -> Result<Self> {
        let table_size = 1usize << table_log;
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let num_symbols = norm.len();

        // Cumulative symbol start positions ("cumul" in the reference).
        let mut cumul = vec![0i32; num_symbols + 1];
        let mut table_symbol = vec![0u8; table_size];
        let mut high_threshold = table_size - 1;

        for s in 0..num_symbols {
            if norm[s] == -1 {
                cumul[s + 1] = cumul[s] + 1;
                table_symbol[high_threshold] = s as u8;
                high_threshold = high_threshold.wrapping_sub(1);
            } else {
                if norm[s] < 0 {
                    return Err(OxiArcError::corrupted(0, "invalid FSE normalized count"));
                }
                cumul[s + 1] = cumul[s] + norm[s] as i32;
            }
        }
        if cumul[num_symbols] != table_size as i32 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE normalized counts do not sum to table size",
            ));
        }

        // Spread symbols across the table.
        let mut position = 0usize;
        for (s, &count) in norm.iter().enumerate() {
            for _ in 0..count.max(0) {
                table_symbol[position] = s as u8;
                loop {
                    position = (position + step) & table_mask;
                    if position <= high_threshold {
                        break;
                    }
                }
            }
        }
        if position != 0 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE symbol spread did not terminate at position 0",
            ));
        }

        // Build the next-state table, grouped by symbol.
        let mut state_table = vec![0u16; table_size];
        let mut cumul_run = cumul.clone();
        for (u, &sym) in table_symbol.iter().enumerate() {
            let slot = cumul_run[sym as usize];
            cumul_run[sym as usize] += 1;
            if slot < 0 || slot as usize >= table_size {
                return Err(OxiArcError::corrupted(0, "FSE state table overflow"));
            }
            state_table[slot as usize] = (table_size + u) as u16;
        }

        // Build the per-symbol transforms.
        let mut symbol_tt = vec![SymbolTransform::default(); num_symbols];
        let mut total = 0i32;
        for (s, &count) in norm.iter().enumerate() {
            match count {
                0 => {
                    // Symbol never used; poison the bit count so accidental
                    // use is caught by the assertion in `encode_symbol`.
                    symbol_tt[s].delta_nb_bits =
                        (((table_log as u32) + 1) << 16) - (1u32 << table_log);
                }
                -1 | 1 => {
                    symbol_tt[s].delta_nb_bits = ((table_log as u32) << 16) - (1u32 << table_log);
                    symbol_tt[s].delta_find_state = total - 1;
                    total += 1;
                }
                _ => {
                    let count_u = count as u32;
                    let max_bits_out = table_log as u32 - (31 - (count_u - 1).leading_zeros());
                    let min_state_plus = count_u << max_bits_out;
                    symbol_tt[s].delta_nb_bits = (max_bits_out << 16).wrapping_sub(min_state_plus);
                    symbol_tt[s].delta_find_state = total - count as i32;
                    total += count as i32;
                }
            }
        }

        Ok(Self {
            table_log,
            state_table,
            symbol_tt,
        })
    }

    /// Look up a symbol's transform, erroring on out-of-range symbols.
    fn transform(&self, symbol: u8) -> Result<SymbolTransform> {
        self.symbol_tt.get(symbol as usize).copied().ok_or_else(|| {
            OxiArcError::corrupted(0, format!("symbol {} outside FSE table", symbol))
        })
    }

    /// Initialize an encoder state for the *last* symbol of the stream
    /// (reference `FSE_initCState2`); emits no bits.
    fn init_state(&self, symbol: u8) -> Result<FseCState> {
        let tt = self.transform(symbol)?;
        let nb_bits_out = (tt.delta_nb_bits.wrapping_add(1 << 15)) >> 16;
        let value = ((nb_bits_out << 16).wrapping_sub(tt.delta_nb_bits)) as u64;
        let index = (value >> nb_bits_out) as i64 + tt.delta_find_state as i64;
        let next = self.lookup_state(index, symbol)?;
        Ok(FseCState {
            value: next as usize,
        })
    }

    /// Encode one symbol (reference `FSE_encodeSymbol`): emits the current
    /// state's low bits and advances to the next state.
    fn encode_symbol(
        &self,
        writer: &mut BackwardBitWriter,
        state: &mut FseCState,
        symbol: u8,
    ) -> Result<()> {
        let tt = self.transform(symbol)?;
        let nb_bits = ((state.value as u32).wrapping_add(tt.delta_nb_bits)) >> 16;
        if nb_bits > self.table_log as u32 + 1 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE encoder produced an invalid bit count (unused symbol?)",
            ));
        }
        writer.write_bits(state.value as u64, nb_bits as u8);
        let index = (state.value >> nb_bits) as i64 + tt.delta_find_state as i64;
        state.value = self.lookup_state(index, symbol)? as usize;
        Ok(())
    }

    /// Flush the final encoder state (reference `FSE_flushCState`).
    fn flush_state(&self, writer: &mut BackwardBitWriter, state: &FseCState) {
        writer.write_bits(state.value as u64, self.table_log);
    }

    /// Bounds-checked `state_table` lookup.
    fn lookup_state(&self, index: i64, symbol: u8) -> Result<u16> {
        if index < 0 || index as usize >= self.state_table.len() {
            return Err(OxiArcError::corrupted(
                0,
                format!("FSE encoder state out of range for symbol {}", symbol),
            ));
        }
        Ok(self.state_table[index as usize])
    }
}

/// Predefined literal-length distribution (RFC 8878 §3.1.1.3.2.2, log 6).
const LL_PREDEFINED_DIST: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

/// Predefined offset-code distribution (RFC 8878, log 5, symbols 0-28).
const OF_PREDEFINED_DIST: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// Predefined match-length distribution (RFC 8878, log 6, symbols 0-52).
const ML_PREDEFINED_DIST: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

/// Table category for predefined FSE table construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableCategory {
    LiteralLength,
    Offset,
    MatchLength,
}

/// A predefined distribution together with the accuracy log it is defined at.
struct PredefinedTable {
    norm: &'static [i16],
    table_log: u8,
}

/// The RFC 8878 predefined distribution for a symbol category.
fn predefined_distribution(category: TableCategory) -> PredefinedTable {
    match category {
        TableCategory::LiteralLength => PredefinedTable {
            norm: &LL_PREDEFINED_DIST,
            table_log: 6,
        },
        TableCategory::Offset => PredefinedTable {
            norm: &OF_PREDEFINED_DIST,
            table_log: 5,
        },
        TableCategory::MatchLength => PredefinedTable {
            norm: &ML_PREDEFINED_DIST,
            table_log: 6,
        },
    }
}

/// Largest `Accuracy_Log` RFC 8878 §3.1.1.3.2.2.1 permits for a category.
///
/// Offsets are capped one bit lower than the length tables; exceeding the cap
/// makes the description undecodable rather than merely inefficient.
fn max_table_log(category: TableCategory) -> u8 {
    match category {
        TableCategory::LiteralLength | TableCategory::MatchLength => 9,
        TableCategory::Offset => 8,
    }
}

/// Build the FSE compression table for a mode (None for RLE — an RLE
/// category emits no state or transition bits at all).
fn build_ctable_for_mode(
    mode: &SequenceCompressionMode,
    category: TableCategory,
) -> Result<Option<FseCTable>> {
    match mode {
        SequenceCompressionMode::Predefined => {
            let predefined = predefined_distribution(category);
            let table = FseCTable::from_normalized(predefined.table_log, predefined.norm)?;
            Ok(Some(table))
        }
        SequenceCompressionMode::Rle(_) => Ok(None),
        SequenceCompressionMode::Fse {
            norm, table_log, ..
        } => Ok(Some(FseCTable::from_normalized(*table_log, norm)?)),
    }
}

/// Encode all sequences into the RFC 8878 backward bitstream.
///
/// Mirrors the reference `ZSTD_encodeSequences`: because the decoder reads
/// the stream back-to-front, the encoder emits data for the **last** sequence
/// first and finishes with the initial FSE states.
///
/// Write order (decoder reads the reverse):
/// 1. Init encoder states from the *last* sequence's codes (no bits).
/// 2. Last sequence's extra bits: LL, ML, OF.
/// 3. For each earlier sequence, back to front: OF/ML/LL state-transition
///    bits, then LL/ML/OF extra bits.
/// 4. Final states: ML, OF, LL (each `accuracy_log` bits).
///
/// The decoder therefore reads: LL/OF/ML initial states; per sequence the
/// OF, ML, LL extra bits; and LL/ML/OF state updates after every sequence
/// except the last — exactly RFC 8878 §3.1.1.4.
fn encode_sequences_bitstream(
    sequences: &[ZstdSequence],
    ll_mode: &SequenceCompressionMode,
    of_mode: &SequenceCompressionMode,
    ml_mode: &SequenceCompressionMode,
) -> Result<Vec<u8>> {
    let mut writer = BackwardBitWriter::new();

    let n = sequences.len();
    if n == 0 {
        return Ok(writer.finish());
    }

    let ll_ctable = build_ctable_for_mode(ll_mode, TableCategory::LiteralLength)?;
    let of_ctable = build_ctable_for_mode(of_mode, TableCategory::Offset)?;
    let ml_ctable = build_ctable_for_mode(ml_mode, TableCategory::MatchLength)?;

    // 1. Initialize states from the last sequence (emits no bits).
    let last = &sequences[n - 1];
    let mut ml_state = match &ml_ctable {
        Some(t) => Some(t.init_state(last.ml_code)?),
        None => None,
    };
    let mut of_state = match &of_ctable {
        Some(t) => Some(t.init_state(last.of_code)?),
        None => None,
    };
    let mut ll_state = match &ll_ctable {
        Some(t) => Some(t.init_state(last.ll_code)?),
        None => None,
    };

    // 2. Last sequence's extra bits (LL, ML, OF).
    writer.write_bits(last.ll_extra_value as u64, last.ll_extra_bits);
    writer.write_bits(last.ml_extra_value as u64, last.ml_extra_bits);
    writer.write_bits(last.of_extra_value as u64, last.of_extra_bits);

    // 3. Remaining sequences, back to front.
    for seq in sequences[..n - 1].iter().rev() {
        if let (Some(table), Some(state)) = (&of_ctable, of_state.as_mut()) {
            table.encode_symbol(&mut writer, state, seq.of_code)?;
        }
        if let (Some(table), Some(state)) = (&ml_ctable, ml_state.as_mut()) {
            table.encode_symbol(&mut writer, state, seq.ml_code)?;
        }
        if let (Some(table), Some(state)) = (&ll_ctable, ll_state.as_mut()) {
            table.encode_symbol(&mut writer, state, seq.ll_code)?;
        }
        writer.write_bits(seq.ll_extra_value as u64, seq.ll_extra_bits);
        writer.write_bits(seq.ml_extra_value as u64, seq.ml_extra_bits);
        writer.write_bits(seq.of_extra_value as u64, seq.of_extra_bits);
    }

    // 4. Flush final states (ML, OF, LL) — the decoder reads these first,
    //    in reverse, as the LL/OF/ML initial states.
    if let (Some(table), Some(state)) = (&ml_ctable, ml_state.as_ref()) {
        table.flush_state(&mut writer, state);
    }
    if let (Some(table), Some(state)) = (&of_ctable, of_state.as_ref()) {
        table.flush_state(&mut writer, state);
    }
    if let (Some(table), Some(state)) = (&ll_ctable, ll_state.as_ref()) {
        table.flush_state(&mut writer, state);
    }

    Ok(writer.finish())
}

// ---------------------------------------------------------------------------
// Count symbol frequencies (input to FSE table building)
// ---------------------------------------------------------------------------

/// Largest offset code an `FSE_Compressed_Mode` offset table can describe.
///
/// RFC 8878 caps `Offset_Code` at 31; the predefined table only covers 0..=28,
/// which is one reason a custom table is sometimes mandatory rather than merely
/// cheaper.
const MAX_OFFSET_CODE_SYMBOLS: usize = 32;

/// Count the frequency of each symbol code across all sequences.
///
/// Returns `(ll_freqs, of_freqs, ml_freqs)` where each vector is indexed by
/// the symbol code and contains its occurrence count. Codes are produced by
/// `encode_literal_length` / `encode_match_length` / `encode_offset`, which
/// bound them to the array sizes below.
fn count_symbol_frequencies(sequences: &[ZstdSequence]) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let mut ll_freqs = vec![0u32; 36];
    let mut of_freqs = vec![0u32; MAX_OFFSET_CODE_SYMBOLS];
    let mut ml_freqs = vec![0u32; 53];

    for seq in sequences {
        if let Some(slot) = ll_freqs.get_mut(seq.ll_code as usize) {
            *slot += 1;
        }
        if let Some(slot) = of_freqs.get_mut(seq.of_code as usize) {
            *slot += 1;
        }
        if let Some(slot) = ml_freqs.get_mut(seq.ml_code as usize) {
            *slot += 1;
        }
    }

    (ll_freqs, of_freqs, ml_freqs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_literal_length_small() {
        for val in 0..16u32 {
            let (code, extra, extra_val) =
                encode_literal_length(val).expect("valid encode operation");
            assert_eq!(code, val as u8);
            assert_eq!(extra, 0);
            assert_eq!(extra_val, 0);
        }
    }

    #[test]
    fn test_encode_literal_length_large() {
        // value=18 -> code=17, baseline=18, extra_bits=1, extra=0
        let (code, extra_bits, extra_val) =
            encode_literal_length(18).expect("valid encode operation");
        assert_eq!(code, 17);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_val, 0);

        // value=19 -> code=17, baseline=18, extra_bits=1, extra=1
        let (code, extra_bits, extra_val) =
            encode_literal_length(19).expect("valid encode operation");
        assert_eq!(code, 17);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_val, 1);
    }

    #[test]
    fn test_encode_match_length_minimum() {
        let (code, extra, extra_val) = encode_match_length(3).expect("valid encode operation");
        assert_eq!(code, 0);
        assert_eq!(extra, 0);
        assert_eq!(extra_val, 0);
    }

    #[test]
    fn test_encode_match_length_too_small() {
        assert!(encode_match_length(2).is_err());
        assert!(encode_match_length(0).is_err());
    }

    #[test]
    fn test_encode_offset() {
        // offset=1 -> offset_value=4 -> code=2, extra_bits=2, extra=0
        let (code, extra_bits, extra_val) = encode_offset(1).expect("valid encode operation");
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_val, 0);

        // offset=2 -> offset_value=5 -> code=2, extra_bits=2, extra=1
        let (code, extra_bits, extra_val) = encode_offset(2).expect("valid encode operation");
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_val, 1);

        // offset=5 -> offset_value=8 -> code=3, extra_bits=3, extra=0
        let (code, extra_bits, extra_val) = encode_offset(5).expect("valid encode operation");
        assert_eq!(code, 3);
        assert_eq!(extra_bits, 3);
        assert_eq!(extra_val, 0);
    }

    #[test]
    fn test_encode_offset_zero_fails() {
        assert!(encode_offset(0).is_err());
    }

    #[test]
    fn test_encode_raw_literals_small() {
        let literals = b"Hello";
        let encoded = encode_raw_literals(literals).expect("valid encode operation");
        // 1-byte header for size < 32
        assert_eq!(encoded[0], (5u8) << 3);
        assert_eq!(&encoded[1..], b"Hello");
    }

    #[test]
    fn test_encode_raw_literals_medium() {
        let literals = vec![0xAB; 100];
        let encoded = encode_raw_literals(&literals).expect("valid encode operation");
        // 2-byte header
        let header: u16 = (0b01 << 2) | ((100u16) << 4);
        assert_eq!(encoded[0], (header & 0xFF) as u8);
        assert_eq!(encoded[1], (header >> 8) as u8);
        assert_eq!(encoded.len(), 2 + 100);
    }

    #[test]
    fn test_encode_rle_literals() {
        let literals = vec![0xCC; 10];
        let encoded = encode_rle_literals(&literals).expect("valid encode operation");
        // 1-byte header + 1 data byte
        assert_eq!(encoded[0], (10u8 << 3) | 0b01);
        assert_eq!(encoded[1], 0xCC);
        assert_eq!(encoded.len(), 2);
    }

    #[test]
    fn test_encode_literals_section_empty() {
        let encoded = encode_literals_section(&[]).expect("valid encode operation");
        assert_eq!(encoded, vec![0]);
    }

    #[test]
    fn test_encode_literals_section_rle() {
        let literals = vec![0xFF; 20];
        let encoded = encode_literals_section(&literals).expect("valid encode operation");
        // Should pick RLE encoding
        assert_eq!(encoded[0] & 0x03, 0x01); // type = RLE
    }

    #[test]
    fn test_encode_sequences_section_empty() {
        let encoded = encode_sequences_section(&[]).expect("valid encode operation");
        assert_eq!(encoded, vec![0]);
    }

    /// Build a frequency vector from a list of symbol codes.
    fn freqs_of(codes: &[u8], alphabet: usize) -> Vec<u32> {
        let mut freqs = vec![0u32; alphabet];
        for &code in codes {
            freqs[code as usize] += 1;
        }
        freqs
    }

    #[test]
    fn test_choose_mode_all_same() {
        let freqs = freqs_of(&[5, 5, 5, 5], 36);
        let mode = choose_mode(&freqs, TableCategory::LiteralLength);
        assert_eq!(mode, SequenceCompressionMode::Rle(5));
    }

    /// A handful of sequences cannot pay for a custom table description, so the
    /// cost comparison must keep choosing the predefined table.
    #[test]
    fn test_choose_mode_tiny_block_stays_predefined() {
        let freqs = freqs_of(&[1, 2, 3], 36);
        let mode = choose_mode(&freqs, TableCategory::LiteralLength);
        assert_eq!(mode, SequenceCompressionMode::Predefined);
    }

    /// A large, sharply skewed block is exactly the case a custom table exists
    /// for: the predefined distribution spreads probability across 36 symbols
    /// while this block only ever uses three.
    #[test]
    fn test_choose_mode_skewed_block_uses_custom_table() {
        let mut codes = vec![4u8; 4000];
        codes.extend(std::iter::repeat_n(9u8, 400));
        codes.extend(std::iter::repeat_n(20u8, 40));
        let freqs = freqs_of(&codes, 36);
        let mode = choose_mode(&freqs, TableCategory::LiteralLength);
        match &mode {
            SequenceCompressionMode::Fse {
                norm,
                table_log,
                description,
            } => {
                assert!((5..=9).contains(table_log), "table log {table_log}");
                assert!(!description.is_empty());
                let sum: i32 = norm
                    .iter()
                    .map(|&p| if p == -1 { 1 } else { i32::from(p) })
                    .sum();
                assert_eq!(sum, 1i32 << table_log);
                assert_eq!(mode_to_bits(&mode), 2);
            }
            other => panic!("expected FSE_Compressed_Mode, got {other:?}"),
        }
    }

    /// Build a synthetic sequence list whose symbol distributions are skewed
    /// enough that a custom FSE table beats the predefined one.
    ///
    /// Returns the encoder-side sequences together with the (literal length,
    /// match length, offset) triples they were built from, so a decode can be
    /// checked against the intended values rather than against itself.
    #[allow(clippy::type_complexity)]
    fn skewed_sequences(count: usize) -> (Vec<ZstdSequence>, Vec<(usize, usize, usize)>) {
        let mut sequences = Vec::with_capacity(count);
        let mut expected = Vec::with_capacity(count);
        for i in 0..count {
            // Literal lengths cluster hard on 3 and 4, match lengths on 4..6,
            // offsets on three magnitudes — a realistic profile for structured
            // data, and one the RFC's flat predefined tables model poorly.
            // The offsets are spread across distinct offset *codes* (the
            // magnitude, not the value, is what the FSE table sees), and every
            // `Offset_Value` stays above 3 so no sequence takes the decoder's
            // repeat-offset path, which would rewrite the value.
            let literal_len = if i % 17 == 0 { 11 } else { 3 + (i % 2) };
            let match_len = 4 + (i % 3);
            let offset = match i % 20 {
                0 => 61,
                1..=3 => 13,
                _ => 5,
            };
            let (ll_code, ll_extra_bits, ll_extra_value) =
                encode_literal_length(literal_len as u32).expect("valid literal length");
            let (ml_code, ml_extra_bits, ml_extra_value) =
                encode_match_length(match_len as u32).expect("valid match length");
            let (of_code, of_extra_bits, of_extra_value) =
                encode_offset(offset as u32).expect("valid offset");
            sequences.push(ZstdSequence {
                ll_code,
                ll_extra_bits,
                ll_extra_value,
                ml_code,
                ml_extra_bits,
                ml_extra_value,
                of_code,
                of_extra_bits,
                of_extra_value,
            });
            expected.push((literal_len, match_len, offset));
        }
        (sequences, expected)
    }

    /// End-to-end proof for `FSE_Compressed_Mode`: a sequences section that
    /// carries custom tables must decode back to the exact same sequences
    /// through the crate's own (reference-verified) `SequencesDecoder`.
    ///
    /// This is the check that catches a table description written with the
    /// wrong threshold or a normalized distribution the decoder reads
    /// differently — the encoder and decoder here are independent code paths,
    /// and the decoder is the one validated against real `zstd` output.
    #[test]
    fn test_fse_compressed_sequences_round_trip_through_decoder() {
        let (sequences, expected) = skewed_sequences(3000);
        let (ll_freqs, of_freqs, ml_freqs) = count_symbol_frequencies(&sequences);
        assert!(
            matches!(
                choose_mode(&ll_freqs, TableCategory::LiteralLength),
                SequenceCompressionMode::Fse { .. }
            ),
            "skewed literal lengths should select a custom table"
        );
        assert!(
            matches!(
                choose_mode(&of_freqs, TableCategory::Offset),
                SequenceCompressionMode::Fse { .. }
            ),
            "skewed offsets should select a custom table"
        );
        assert!(
            matches!(
                choose_mode(&ml_freqs, TableCategory::MatchLength),
                SequenceCompressionMode::Fse { .. }
            ),
            "skewed match lengths should select a custom table"
        );

        let encoded = encode_sequences_section(&sequences).expect("sequences section encodes");

        // The modes byte must actually advertise FSE_Compressed_Mode (2) for
        // all three categories, or this test would pass vacuously.
        let count_bytes = if sequences.len() < 128 {
            1
        } else if sequences.len() < 0x7F00 {
            2
        } else {
            3
        };
        let modes_byte = encoded[count_bytes];
        assert_eq!((modes_byte >> 6) & 3, 2, "LL mode is not FSE_Compressed");
        assert_eq!((modes_byte >> 4) & 3, 2, "OF mode is not FSE_Compressed");
        assert_eq!((modes_byte >> 2) & 3, 2, "ML mode is not FSE_Compressed");

        let mut decoder = crate::sequences::SequencesDecoder::new();
        let (decoded, _) = decoder.decode(&encoded).expect("custom tables decode");
        assert_eq!(decoded.len(), expected.len());
        for (index, (got, &(literal_len, match_len, offset))) in
            decoded.iter().zip(expected.iter()).enumerate()
        {
            // `Sequence` stores the format's `u32` bounds; the expectations
            // here are `usize`, so widen the decoded side rather than
            // narrowing (and possibly truncating) the expectation.
            assert_eq!(
                got.literal_length as usize, literal_len,
                "literal length differs at sequence {index}"
            );
            assert_eq!(
                got.match_length as usize, match_len,
                "match length differs at sequence {index}"
            );
            assert_eq!(
                got.offset as usize, offset,
                "offset differs at sequence {index}"
            );
        }
    }

    /// The custom table has to actually pay for itself: encoding the same
    /// skewed sequences with `FSE_Compressed_Mode` must be smaller than the
    /// predefined-table encoding, header bytes included.
    #[test]
    fn test_fse_compressed_sequences_beat_predefined() {
        let (sequences, _) = skewed_sequences(3000);
        let custom = encode_sequences_section(&sequences).expect("sequences section encodes");

        // Same sequences, forced through the predefined tables.
        let predefined = {
            let mut out = Vec::new();
            let count = sequences.len();
            assert!((128..0x7F00).contains(&count));
            out.push(((count >> 8) as u8) + 128);
            out.push((count & 0xFF) as u8);
            out.push(0); // all three modes = Predefined
            let bitstream = encode_sequences_bitstream(
                &sequences,
                &SequenceCompressionMode::Predefined,
                &SequenceCompressionMode::Predefined,
                &SequenceCompressionMode::Predefined,
            )
            .expect("predefined encoding works");
            out.extend_from_slice(&bitstream);
            out
        };

        assert!(
            custom.len() < predefined.len(),
            "custom FSE tables ({} bytes) did not beat predefined ({} bytes)",
            custom.len(),
            predefined.len()
        );
    }

    /// The offset table's accuracy log is capped at 8, one below the length
    /// tables; a custom offset table that exceeded it would be undecodable.
    #[test]
    fn test_custom_offset_table_respects_accuracy_cap() {
        let mut codes: Vec<u8> = Vec::new();
        for code in 1u8..=20 {
            codes.extend(std::iter::repeat_n(code, 200 - usize::from(code) * 5));
        }
        let freqs = freqs_of(&codes, MAX_OFFSET_CODE_SYMBOLS);
        if let SequenceCompressionMode::Fse { table_log, .. } =
            choose_mode(&freqs, TableCategory::Offset)
        {
            assert!(table_log <= 8, "offset accuracy log {table_log} exceeds 8");
        }
    }

    #[test]
    fn test_count_symbol_frequencies() {
        let seqs = vec![
            ZstdSequence {
                ll_code: 0,
                ll_extra_bits: 0,
                ll_extra_value: 0,
                ml_code: 0,
                ml_extra_bits: 0,
                ml_extra_value: 0,
                of_code: 1,
                of_extra_bits: 1,
                of_extra_value: 0,
            },
            ZstdSequence {
                ll_code: 0,
                ll_extra_bits: 0,
                ll_extra_value: 0,
                ml_code: 1,
                ml_extra_bits: 0,
                ml_extra_value: 0,
                of_code: 1,
                of_extra_bits: 1,
                of_extra_value: 0,
            },
        ];
        let (ll, of, ml) = count_symbol_frequencies(&seqs);
        assert_eq!(ll[0], 2);
        assert_eq!(of[1], 2);
        assert_eq!(ml[0], 1);
        assert_eq!(ml[1], 1);
    }

    #[test]
    fn test_encode_compressed_block_simple() {
        let sequences = vec![Lz77Sequence {
            literals: b"Hello".to_vec(),
            match_length: 3,
            offset: 1,
        }];
        let block = encode_compressed_block(&sequences).expect("valid encode operation");
        // Should produce a non-empty block.
        assert!(!block.is_empty());
    }

    #[test]
    fn test_encode_compressed_block_literals_only() {
        let sequences = vec![Lz77Sequence {
            literals: b"Trailing literals".to_vec(),
            match_length: 0,
            offset: 0,
        }];
        let block = encode_compressed_block(&sequences).expect("valid encode operation");
        // Literals section present, sequences section should be [0].
        assert!(!block.is_empty());
    }
}
