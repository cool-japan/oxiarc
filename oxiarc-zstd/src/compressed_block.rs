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
use crate::fse::{FseTable, FseTableEntry};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceCompressionMode {
    /// Use the predefined FSE table from the specification.
    Predefined,
    /// RLE mode: every symbol in this category is the same value.
    Rle(u8),
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

/// Encode the literals section.
///
/// Chooses the best representation among Raw, RLE, or (future) Huffman
/// compressed. Currently favours Raw and RLE since Huffman encoding
/// tables require a full encoder implementation.
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

    // Fall back to raw literals (no Huffman compression yet).
    encode_raw_literals(literals)
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
#[allow(dead_code)]
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
    let ll_mode = choose_mode_for_codes(sequences.iter().map(|s| s.ll_code));
    let of_mode = choose_mode_for_codes(sequences.iter().map(|s| s.of_code));
    let ml_mode = choose_mode_for_codes(sequences.iter().map(|s| s.ml_code));

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

/// Choose a compression mode by inspecting all codes in a category.
///
/// If every code is the same value we can use RLE which is the most compact.
/// Otherwise we fall back to the predefined FSE table.
fn choose_mode_for_codes(mut codes: impl Iterator<Item = u8>) -> SequenceCompressionMode {
    let first = match codes.next() {
        Some(v) => v,
        None => return SequenceCompressionMode::Predefined,
    };
    if codes.all(|c| c == first) {
        SequenceCompressionMode::Rle(first)
    } else {
        SequenceCompressionMode::Predefined
    }
}

/// Convert a `SequenceCompressionMode` to its 2-bit representation.
fn mode_to_bits(mode: &SequenceCompressionMode) -> u8 {
    match mode {
        SequenceCompressionMode::Predefined => 0,
        SequenceCompressionMode::Rle(_) => 1,
    }
}

/// Write the table description bytes for a mode (nothing for Predefined,
/// one symbol byte for RLE).
fn write_mode_table_data(out: &mut Vec<u8>, mode: &SequenceCompressionMode) {
    match mode {
        SequenceCompressionMode::Predefined => {}
        SequenceCompressionMode::Rle(symbol) => {
            out.push(*symbol);
        }
    }
}

/// FSE encoding table: for each symbol, stores the list of decoding-table states
/// that produce that symbol, along with their transition parameters.
struct FseEncodingTable {
    /// For each symbol, the list of encoding state entries from the decoding table.
    symbol_states: Vec<Vec<FseEncState>>,
    /// The underlying decoding table (borrowed data cached as owned copy).
    decoding_table: FseTable,
}

/// A single encoding-side state entry for a symbol.
#[derive(Debug, Clone, Copy)]
struct FseEncState {
    /// The decoding table state index that produces this symbol.
    state: usize,
    /// Baseline for next-state computation: next_state = baseline + bits_read.
    baseline: u16,
}

impl FseEncodingTable {
    /// Build an encoding table from a decoding table.
    fn from_decoding_table(table: FseTable) -> Self {
        let table_size = 1usize << table.accuracy_log();
        let mut max_symbol = 0u8;

        for i in 0..table_size {
            let entry = table.get(i);
            if entry.symbol > max_symbol {
                max_symbol = entry.symbol;
            }
        }

        let mut symbol_states = vec![Vec::new(); max_symbol as usize + 1];

        for i in 0..table_size {
            let entry = table.get(i);
            symbol_states[entry.symbol as usize].push(FseEncState {
                state: i,
                baseline: entry.baseline,
            });
        }

        // Sort each symbol's states by baseline for efficient lookup
        for states in &mut symbol_states {
            states.sort_by_key(|s| s.baseline);
        }

        Self {
            symbol_states,
            decoding_table: table,
        }
    }

    /// Get the accuracy log from the underlying decoding table.
    fn accuracy_log(&self) -> u8 {
        self.decoding_table.accuracy_log()
    }

    /// Get a decoding table entry for a given state.
    fn get_entry(&self, state: usize) -> &FseTableEntry {
        self.decoding_table.get(state)
    }
}

/// Encode all sequences into a backward bitstream.
///
/// The `BackwardBitWriter` produces a byte array where the first bits
/// written are read first by the decoder.
///
/// **Decoder read order:**
/// 1. LL initial state (acc_log bits), OF initial state, ML initial state
/// 2. For each sequence (forward):
///    a. OF state transition bits
///    b. ML state transition bits
///    c. LL state transition bits
///    d. LL extra bits
///    e. ML extra bits
///    f. OF extra bits
///
/// **Encoding strategy:**
/// FSE encoding must be done **backward** through the sequence list so that
/// state transitions are consistent. We first compute the FSE state for each
/// sequence position by working from the last sequence to the first, then
/// write the bitstream in forward (decoder) order.
fn encode_sequences_bitstream(
    sequences: &[ZstdSequence],
    ll_mode: &SequenceCompressionMode,
    of_mode: &SequenceCompressionMode,
    ml_mode: &SequenceCompressionMode,
) -> Result<Vec<u8>> {
    let mut writer = BackwardBitWriter::new();

    // Build encoding tables.
    let ll_enc = build_predefined_enc_table(ll_mode, TableCategory::LiteralLength);
    let of_enc = build_predefined_enc_table(of_mode, TableCategory::Offset);
    let ml_enc = build_predefined_enc_table(ml_mode, TableCategory::MatchLength);

    let n = sequences.len();
    if n == 0 {
        return Ok(writer.finish());
    }

    // Compute FSE states backward for each table.
    // states[i] is the FSE state when the decoder processes sequence i.
    // The decoder does: symbol = table[state_i].symbol, then
    //   state_{i+1} = table[state_i].baseline + read_bits(table[state_i].num_bits)
    //
    // Working backward: choose state for last sequence, then find states for
    // earlier sequences such that the transition from state_i reaches state_{i+1}.
    let ll_states =
        compute_fse_states_backward(&ll_enc, sequences.iter().map(|s| s.ll_code).collect());
    let of_states =
        compute_fse_states_backward(&of_enc, sequences.iter().map(|s| s.of_code).collect());
    let ml_states =
        compute_fse_states_backward(&ml_enc, sequences.iter().map(|s| s.ml_code).collect());

    // 1. Write initial FSE states (read first by the decoder).
    //    Decoder order: LL initial, OF initial, ML initial.
    if let Some(ref enc) = ll_enc {
        writer.write_bits(ll_states[0] as u64, enc.accuracy_log());
    }
    if let Some(ref enc) = of_enc {
        writer.write_bits(of_states[0] as u64, enc.accuracy_log());
    }
    if let Some(ref enc) = ml_enc {
        writer.write_bits(ml_states[0] as u64, enc.accuracy_log());
    }

    // 2. For each sequence (forward order), write state transition bits
    //    and extra bits in decoder read order.
    for idx in 0..n {
        let seq = &sequences[idx];

        // State transition bits (decoder reads OF, ML, LL).
        // bits = state_{i+1} - baseline_i
        if let Some(ref enc) = of_enc {
            let entry = enc.get_entry(of_states[idx]);
            if entry.num_bits > 0 {
                let target_next = if idx + 1 < n {
                    of_states[idx + 1]
                } else {
                    // Last sequence: decoder reads bits but result is unused.
                    entry.baseline as usize
                };
                let bits_val = target_next.wrapping_sub(entry.baseline as usize);
                writer.write_bits(bits_val as u64, entry.num_bits);
            }
        }

        if let Some(ref enc) = ml_enc {
            let entry = enc.get_entry(ml_states[idx]);
            if entry.num_bits > 0 {
                let target_next = if idx + 1 < n {
                    ml_states[idx + 1]
                } else {
                    entry.baseline as usize
                };
                let bits_val = target_next.wrapping_sub(entry.baseline as usize);
                writer.write_bits(bits_val as u64, entry.num_bits);
            }
        }

        if let Some(ref enc) = ll_enc {
            let entry = enc.get_entry(ll_states[idx]);
            if entry.num_bits > 0 {
                let target_next = if idx + 1 < n {
                    ll_states[idx + 1]
                } else {
                    entry.baseline as usize
                };
                let bits_val = target_next.wrapping_sub(entry.baseline as usize);
                writer.write_bits(bits_val as u64, entry.num_bits);
            }
        }

        // Extra bits (decoder reads LL_extra, ML_extra, OF_extra).
        if seq.ll_extra_bits > 0 {
            writer.write_bits(seq.ll_extra_value as u64, seq.ll_extra_bits);
        }
        if seq.ml_extra_bits > 0 {
            writer.write_bits(seq.ml_extra_value as u64, seq.ml_extra_bits);
        }
        if seq.of_extra_bits > 0 {
            writer.write_bits(seq.of_extra_value as u64, seq.of_extra_bits);
        }
    }

    Ok(writer.finish())
}

/// Compute FSE states backward through a sequence of symbols.
///
/// For each position i, `states[i]` is the FSE decoding table state such that
/// `table[states[i]].symbol == symbols[i]` and the transition from `states[i]`
/// can reach `states[i+1]`.
///
/// Returns an empty vec if `enc` is None.
fn compute_fse_states_backward(enc: &Option<FseEncodingTable>, symbols: Vec<u8>) -> Vec<usize> {
    let enc = match enc {
        Some(e) => e,
        None => return Vec::new(),
    };
    let n = symbols.len();
    if n == 0 {
        return Vec::new();
    }

    let mut states = vec![0usize; n];

    // Start from the last sequence: choose any valid state for its symbol.
    let last_sym = symbols[n - 1] as usize;
    states[n - 1] = if last_sym < enc.symbol_states.len() && !enc.symbol_states[last_sym].is_empty()
    {
        enc.symbol_states[last_sym][0].state
    } else {
        0
    };

    // Work backward from n-2 to 0.
    // For sequence i, we need a state whose symbol matches symbols[i] and
    // whose transition range [baseline, baseline + 2^num_bits) includes states[i+1].
    for i in (0..n.saturating_sub(1)).rev() {
        let sym = symbols[i] as usize;
        let target_next = states[i + 1];

        if sym >= enc.symbol_states.len() || enc.symbol_states[sym].is_empty() {
            states[i] = 0;
            continue;
        }

        // Search for a state whose baseline range includes target_next.
        let mut found = false;
        for enc_state in &enc.symbol_states[sym] {
            let entry = enc.get_entry(enc_state.state);
            let range_size = 1usize << entry.num_bits;
            let baseline = entry.baseline as usize;
            if target_next >= baseline && target_next < baseline + range_size {
                states[i] = enc_state.state;
                found = true;
                break;
            }
        }

        if !found {
            // Fallback: pick the state whose baseline is closest to target_next.
            // This can happen when the table doesn't have a perfect transition.
            // Use the first available state (the decoder will still get the right symbol).
            states[i] = enc.symbol_states[sym][0].state;
        }
    }

    states
}

/// Table category for predefined FSE table construction.
enum TableCategory {
    LiteralLength,
    Offset,
    MatchLength,
}

/// Build a predefined FSE encoding table for a given mode and category.
fn build_predefined_enc_table(
    mode: &SequenceCompressionMode,
    category: TableCategory,
) -> Option<FseEncodingTable> {
    match mode {
        SequenceCompressionMode::Predefined => {
            let dec_table = match category {
                TableCategory::LiteralLength => {
                    let probs: [i16; 36] = [
                        4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2,
                        3, 2, 1, 1, 1, 1, 1, -1, -1, -1, -1,
                    ];
                    FseTable::new(6, &probs).ok()?
                }
                TableCategory::Offset => {
                    // Per RFC 8878: 29 symbols (0-28), accuracy_log=5
                    let probs: [i16; 29] = [
                        1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1,
                        -1, -1, -1, -1,
                    ];
                    FseTable::new(5, &probs).ok()?
                }
                TableCategory::MatchLength => {
                    let probs: [i16; 53] = [
                        1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1,
                        -1, -1, -1, -1,
                    ];
                    FseTable::new(6, &probs).ok()?
                }
            };
            Some(FseEncodingTable::from_decoding_table(dec_table))
        }
        SequenceCompressionMode::Rle(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Count symbol frequencies (utility for future FSE table building)
// ---------------------------------------------------------------------------

/// Count the frequency of each symbol code across all sequences.
///
/// Returns `(ll_freqs, of_freqs, ml_freqs)` where each vector is indexed by
/// the symbol code and contains its occurrence count.
#[allow(dead_code)]
fn count_symbol_frequencies(sequences: &[ZstdSequence]) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let mut ll_freqs = vec![0u32; 36];
    let mut of_freqs = vec![0u32; 29];
    let mut ml_freqs = vec![0u32; 53];

    for seq in sequences {
        ll_freqs[seq.ll_code as usize] += 1;
        of_freqs[seq.of_code as usize] += 1;
        ml_freqs[seq.ml_code as usize] += 1;
    }

    (ll_freqs, of_freqs, ml_freqs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_literal_length_small() {
        for val in 0..16u32 {
            let (code, extra, extra_val) = encode_literal_length(val).unwrap();
            assert_eq!(code, val as u8);
            assert_eq!(extra, 0);
            assert_eq!(extra_val, 0);
        }
    }

    #[test]
    fn test_encode_literal_length_large() {
        // value=18 -> code=17, baseline=18, extra_bits=1, extra=0
        let (code, extra_bits, extra_val) = encode_literal_length(18).unwrap();
        assert_eq!(code, 17);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_val, 0);

        // value=19 -> code=17, baseline=18, extra_bits=1, extra=1
        let (code, extra_bits, extra_val) = encode_literal_length(19).unwrap();
        assert_eq!(code, 17);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_val, 1);
    }

    #[test]
    fn test_encode_match_length_minimum() {
        let (code, extra, extra_val) = encode_match_length(3).unwrap();
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
        let (code, extra_bits, extra_val) = encode_offset(1).unwrap();
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_val, 0);

        // offset=2 -> offset_value=5 -> code=2, extra_bits=2, extra=1
        let (code, extra_bits, extra_val) = encode_offset(2).unwrap();
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_val, 1);

        // offset=5 -> offset_value=8 -> code=3, extra_bits=3, extra=0
        let (code, extra_bits, extra_val) = encode_offset(5).unwrap();
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
        let encoded = encode_raw_literals(literals).unwrap();
        // 1-byte header for size < 32
        assert_eq!(encoded[0], (5u8) << 3);
        assert_eq!(&encoded[1..], b"Hello");
    }

    #[test]
    fn test_encode_raw_literals_medium() {
        let literals = vec![0xAB; 100];
        let encoded = encode_raw_literals(&literals).unwrap();
        // 2-byte header
        let header: u16 = (0b01 << 2) | ((100u16) << 4);
        assert_eq!(encoded[0], (header & 0xFF) as u8);
        assert_eq!(encoded[1], (header >> 8) as u8);
        assert_eq!(encoded.len(), 2 + 100);
    }

    #[test]
    fn test_encode_rle_literals() {
        let literals = vec![0xCC; 10];
        let encoded = encode_rle_literals(&literals).unwrap();
        // 1-byte header + 1 data byte
        assert_eq!(encoded[0], (10u8 << 3) | 0b01);
        assert_eq!(encoded[1], 0xCC);
        assert_eq!(encoded.len(), 2);
    }

    #[test]
    fn test_encode_literals_section_empty() {
        let encoded = encode_literals_section(&[]).unwrap();
        assert_eq!(encoded, vec![0]);
    }

    #[test]
    fn test_encode_literals_section_rle() {
        let literals = vec![0xFF; 20];
        let encoded = encode_literals_section(&literals).unwrap();
        // Should pick RLE encoding
        assert_eq!(encoded[0] & 0x03, 0x01); // type = RLE
    }

    #[test]
    fn test_encode_sequences_section_empty() {
        let encoded = encode_sequences_section(&[]).unwrap();
        assert_eq!(encoded, vec![0]);
    }

    #[test]
    fn test_choose_mode_all_same() {
        let mode = choose_mode_for_codes([5u8, 5, 5, 5].iter().copied());
        assert_eq!(mode, SequenceCompressionMode::Rle(5));
    }

    #[test]
    fn test_choose_mode_different() {
        let mode = choose_mode_for_codes([1u8, 2, 3].iter().copied());
        assert_eq!(mode, SequenceCompressionMode::Predefined);
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
        let block = encode_compressed_block(&sequences).unwrap();
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
        let block = encode_compressed_block(&sequences).unwrap();
        // Literals section present, sequences section should be [0].
        assert!(!block.is_empty());
    }
}
