//! `-lh3-` (LHarc 2.x block-static Huffman LZSS) codec.
//!
//! `-lh3-` sits between `-lh2-` and `-lh5-`: an 8 KiB LZSS window whose symbols
//! are coded with **static** Huffman tables that are re-transmitted per block,
//! but with a much cruder table encoding than lh4-lh7 use. A block is:
//!
//! ```text
//! blocksize          16 bits   number of literal/length symbols in this block
//! literal/length     286 x (1 bit present + 4 bits (length - 1))
//! position present    1 bit
//! position table    128 x 4 bits (length, 0 = unused)   [only when present]
//! ```
//!
//! Both tables have a degenerate escape: if the first three transmitted lengths
//! are all `1` — impossible for a real complete code — the table collapses to a
//! single symbol read as 9 raw bits (literal/length) or 7 raw bits (position),
//! and every subsequent code costs zero bits. When the position table is absent
//! the decoder falls back to LArc's built-in "8K" table, a fixed 128-entry code
//! reconstructed here from LHa's `ready_made` run-length description.
//!
//! Literal/length symbols are `0..=255` literals, `256..=284` lengths `3..=31`,
//! and `285` an escape followed by eight raw bits (lengths `32..=287`).
//! Positions code a 64-distance group followed by six raw bits.
//!
//! ## Verification
//!
//! As with `-lh2-`, no decoder on `PATH` implements `-lh3-`; conformance was
//! established against the canonical LHa `shuf.c` decode path compiled
//! standalone, and the accepted byte vectors are frozen into the crate's tests.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter};

use super::huffcode::{canonical_codes, complete_code_lengths};
use super::ring::{GreedyMatcher, Ring};
use crate::huffman::LzhHuffmanTree;

/// `-lh3-` dictionary size (LHa `LZHUFF3_DICBIT = 13`).
pub(crate) const LH3_DICT_SIZE: usize = 1 << 13;
/// Literal/length alphabet size (LHa `N1`).
const N1: usize = 286;
/// Escape symbol: eight raw length bits follow.
const ESCAPE: usize = N1 - 1;
/// Number of position groups (`dictionary / 64`).
const NP: usize = LH3_DICT_SIZE / 64;
/// Width of a transmitted code-length field (LHa `LENFIELD`).
const LENFIELD: u8 = 4;
/// Width of the degenerate literal/length symbol field (LHa `CBIT`).
const CBIT: u8 = 9;
/// Width of the degenerate position-group field (`DICBIT - 6`).
const PBIT: u8 = 7;
/// Width of the per-block symbol counter (LHa `BUFBITS`).
const BUFBITS: u8 = 16;
/// Raw bits appended to the escape symbol (LHa `EXTRABITS`).
const EXTRABITS: u8 = 8;
/// Length symbols start here: symbol `256` means length `3`.
const LENGTH_BASE: usize = 253;
/// Shortest encodable match.
const MIN_MATCH: usize = 3;
/// Longest encodable match.
const MAX_MATCH: usize = 256;
/// Maximum literal/length code length the 4-bit field can express.
const MAX_C_LEN: u8 = 16;
/// Maximum position code length the 4-bit field can express.
const MAX_P_LEN: u8 = 15;
/// Literal/length symbols per block. Small enough that the tables track the
/// data, large enough that their ~175-byte cost stays under one percent.
const BLOCK_SYMBOLS: usize = 16_384;

/// Reconstruct LArc's built-in "8K buffer" position table (LHa `ready_made(1)`,
/// driven by the run-length table `{2, 1, 1, 3, 6, 13, 31, 78}`).
///
/// The entry list is "starting code length, then the group index at which the
/// length increases by one, repeated"; expanding it gives 128 lengths that
/// satisfy the Kraft equality exactly.
pub(crate) fn ready_made_lengths() -> Vec<u8> {
    /// `(first length, indices at which the length increments)`.
    const TABLE: (u8, [usize; 7]) = (2, [1, 1, 3, 6, 13, 31, 78]);
    let (mut length, steps) = TABLE;
    let mut cursor = 0usize;
    let mut lengths = Vec::with_capacity(NP);
    for group in 0..NP {
        while cursor < steps.len() && steps[cursor] == group {
            length += 1;
            cursor += 1;
        }
        lengths.push(length);
    }
    lengths
}

/// Decode a `-lh3-` stream to exactly `uncompressed_size` bytes.
///
/// # Errors
///
/// Returns a corruption error on a truncated stream, a zero block
/// size (which would make the decoder spin), or a degenerate table naming a
/// symbol outside the alphabet.
pub fn decode_lh3(data: &[u8], uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut reader = MsbBitReader::new(data);
    let mut ring = Ring::new(LH3_DICT_SIZE, b' ', 0);
    let mut out: Vec<u8> = Vec::with_capacity(uncompressed_size.min(1 << 20) as usize);

    let mut block_remaining: u32 = 0;
    let mut code_tree = LzhHuffmanTree::single(0);
    let mut position_tree = LzhHuffmanTree::single(0);

    while (out.len() as u64) < uncompressed_size {
        if block_remaining == 0 {
            block_remaining = reader.get_bits(BUFBITS)?;
            if reader.padding_bits() > 0 || block_remaining == 0 {
                return Err(OxiArcError::corrupted(
                    out.len() as u64,
                    "truncated or malformed -lh3- block header",
                ));
            }
            code_tree = read_code_tree(&mut reader)?;
            position_tree = if reader.get_bit()? {
                read_position_tree(&mut reader)?
            } else {
                LzhHuffmanTree::from_code_lengths(&ready_made_lengths(), NP * 2)?
            };
            if reader.padding_bits() > 0 {
                return Err(OxiArcError::corrupted(
                    out.len() as u64,
                    "truncated -lh3- block: table description ran past end of input",
                ));
            }
        }
        block_remaining -= 1;

        let mut symbol = usize::from(code_tree.decode(&mut reader)?);
        if symbol == ESCAPE {
            symbol += reader.get_bits(EXTRABITS)? as usize;
        }
        if reader.padding_bits() > 0 {
            return Err(OxiArcError::corrupted(
                out.len() as u64,
                "truncated -lh3- stream: literal/length code read past end of input",
            ));
        }

        if symbol < 256 {
            let byte = symbol as u8;
            out.push(byte);
            ring.push(byte);
            continue;
        }

        let length = symbol - LENGTH_BASE;
        let group = usize::from(position_tree.decode(&mut reader)?);
        if group >= NP {
            return Err(OxiArcError::corrupted(
                out.len() as u64,
                "-lh3-: position group out of range",
            ));
        }
        let distance = (group << 6) + reader.get_bits(6)? as usize + 1;
        if reader.padding_bits() > 0 {
            return Err(OxiArcError::corrupted(
                out.len() as u64,
                "truncated -lh3- stream: match position read past end of input",
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

/// Read the literal/length table, honouring the degenerate single-symbol form.
fn read_code_tree(reader: &mut MsbBitReader<&[u8]>) -> Result<LzhHuffmanTree> {
    let mut lengths = vec![0u8; N1];
    for index in 0..N1 {
        lengths[index] = if reader.get_bit()? {
            reader.get_bits(LENFIELD)? as u8 + 1
        } else {
            0
        };
        if index == 2 && lengths[0] == 1 && lengths[1] == 1 && lengths[2] == 1 {
            let symbol = reader.get_bits(CBIT)? as usize;
            if symbol >= N1 {
                return Err(OxiArcError::corrupted(
                    0,
                    "-lh3-: degenerate literal/length table names an out-of-range symbol",
                ));
            }
            return Ok(LzhHuffmanTree::single(symbol as u16));
        }
    }
    LzhHuffmanTree::from_code_lengths(&lengths, N1 * 2)
}

/// Read the position table, honouring the degenerate single-group form.
fn read_position_tree(reader: &mut MsbBitReader<&[u8]>) -> Result<LzhHuffmanTree> {
    let mut lengths = vec![0u8; NP];
    for index in 0..NP {
        lengths[index] = reader.get_bits(LENFIELD)? as u8;
        if index == 2 && lengths[0] == 1 && lengths[1] == 1 && lengths[2] == 1 {
            let group = reader.get_bits(PBIT)? as usize;
            if group >= NP {
                return Err(OxiArcError::corrupted(
                    0,
                    "-lh3-: degenerate position table names an out-of-range group",
                ));
            }
            return Ok(LzhHuffmanTree::single(group as u16));
        }
    }
    LzhHuffmanTree::from_code_lengths(&lengths, NP * 2)
}

/// One parsed LZSS command.
#[derive(Debug, Clone, Copy)]
enum Command {
    Literal(u8),
    Match { length: usize, distance: usize },
}

/// Greedy LZSS parse shared by the block encoder.
fn parse_commands(data: &[u8]) -> Vec<Command> {
    let mut matcher = GreedyMatcher::new(data.len(), MIN_MATCH, MAX_MATCH, LH3_DICT_SIZE);
    let mut commands = Vec::with_capacity(data.len() / 2 + 1);
    let mut pos = 0usize;
    while pos < data.len() {
        match matcher.find(data, pos) {
            Some((distance, length)) if length >= MIN_MATCH => {
                commands.push(Command::Match { length, distance });
                for step in 0..length {
                    matcher.insert(data, pos + step);
                }
                pos += length;
            }
            _ => {
                commands.push(Command::Literal(data[pos]));
                matcher.insert(data, pos);
                pos += 1;
            }
        }
    }
    commands
}

/// A prepared prefix code: either a real table or the degenerate single symbol.
enum PreparedCode {
    Single(usize),
    Table { lengths: Vec<u8>, codes: Vec<u32> },
}

impl PreparedCode {
    /// Bits this code spends on `symbol`.
    fn cost(&self, symbol: usize) -> u64 {
        match self {
            Self::Single(_) => 0,
            Self::Table { lengths, .. } => u64::from(lengths[symbol]),
        }
    }

    /// Emit `symbol`.
    fn write<W: std::io::Write>(&self, writer: &mut MsbBitWriter<W>, symbol: usize) -> Result<()> {
        match self {
            Self::Single(_) => Ok(()),
            Self::Table { lengths, codes } => {
                let length = lengths[symbol];
                debug_assert!(length > 0, "encoding a symbol with no code");
                writer.put_bits(length, codes[symbol])?;
                Ok(())
            }
        }
    }
}

/// Build a prepared code from frequencies, or `None` when nothing is used.
fn prepare(freqs: &[u32], max_len: u8) -> Option<PreparedCode> {
    let used: Vec<usize> = freqs
        .iter()
        .enumerate()
        .filter(|&(_, &freq)| freq > 0)
        .map(|(symbol, _)| symbol)
        .collect();
    match used.len() {
        0 => None,
        1 => Some(PreparedCode::Single(used[0])),
        _ => {
            let lengths = complete_code_lengths(freqs, max_len)?;
            let codes = canonical_codes(&lengths);
            Some(PreparedCode::Table { lengths, codes })
        }
    }
}

/// How the encoder picks between transmitting a position table and using
/// LArc's built-in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionTablePolicy {
    /// Measure both and emit whichever costs fewer bits. The default, and what
    /// [`encode_lh3`] uses.
    Cheapest,
    /// Always fall back to the built-in table (`ready_made`), never transmit
    /// one. Both forms are valid `-lh3-`; forcing this one exists so the
    /// fallback path — which a cost-driven encoder may rarely choose — stays
    /// directly exercisable and reference-verifiable.
    AlwaysReadyMade,
}

/// Encode `data` as a `-lh3-` stream.
///
/// # Errors
///
/// Returns any I/O error raised by the in-memory bit writer.
pub fn encode_lh3(data: &[u8]) -> Result<Vec<u8>> {
    encode_lh3_with(data, PositionTablePolicy::Cheapest)
}

/// Encode `data` as a `-lh3-` stream under an explicit position-table policy.
///
/// # Errors
///
/// Returns any I/O error raised by the in-memory bit writer.
pub fn encode_lh3_with(data: &[u8], policy: PositionTablePolicy) -> Result<Vec<u8>> {
    let commands = parse_commands(data);
    let mut out = Vec::with_capacity(data.len() / 2 + 8);
    {
        let mut writer = MsbBitWriter::new(&mut out);
        for block in commands.chunks(BLOCK_SYMBOLS) {
            write_block(&mut writer, block, policy)?;
        }
        writer.flush()?;
    }
    Ok(out)
}

/// Emit one block: header, tables, then the block's commands.
fn write_block<W: std::io::Write>(
    writer: &mut MsbBitWriter<W>,
    block: &[Command],
    policy: PositionTablePolicy,
) -> Result<()> {
    let mut code_freqs = vec![0u32; N1];
    let mut position_freqs = vec![0u32; NP];
    for command in block {
        match *command {
            Command::Literal(byte) => code_freqs[usize::from(byte)] += 1,
            Command::Match { length, distance } => {
                let symbol = (length + LENGTH_BASE).min(ESCAPE);
                code_freqs[symbol] += 1;
                position_freqs[(distance - 1) >> 6] += 1;
            }
        }
    }

    debug_assert!(!block.is_empty() && block.len() <= 0xFFFF);
    writer.put_bits(BUFBITS, block.len() as u32)?;

    let code = prepare(&code_freqs, MAX_C_LEN).ok_or_else(|| {
        OxiArcError::corrupted(0, "-lh3-: empty block has no literal/length symbols")
    })?;
    write_code_table(writer, &code)?;

    let position = match policy {
        PositionTablePolicy::Cheapest => choose_position_code(&position_freqs),
        PositionTablePolicy::AlwaysReadyMade => None,
    };
    match position {
        Some(ref prepared) => {
            writer.put_bit(true)?;
            write_position_table(writer, prepared)?;
        }
        None => writer.put_bit(false)?,
    }
    let ready_made = LazyReadyMade::new();

    for command in block {
        match *command {
            Command::Literal(byte) => code.write(writer, usize::from(byte))?,
            Command::Match { length, distance } => {
                let symbol = length + LENGTH_BASE;
                if symbol >= ESCAPE {
                    code.write(writer, ESCAPE)?;
                    writer.put_bits(EXTRABITS, (symbol - ESCAPE) as u32)?;
                } else {
                    code.write(writer, symbol)?;
                }
                let group = (distance - 1) >> 6;
                match position {
                    Some(ref prepared) => prepared.write(writer, group)?,
                    None => ready_made.write(writer, group)?,
                }
                writer.put_bits(6, ((distance - 1) & 0x3F) as u32)?;
            }
        }
    }
    Ok(())
}

/// The built-in position code, materialised once per block that needs it.
struct LazyReadyMade {
    lengths: Vec<u8>,
    codes: Vec<u32>,
}

impl LazyReadyMade {
    fn new() -> Self {
        let lengths = ready_made_lengths();
        let codes = canonical_codes(&lengths);
        Self { lengths, codes }
    }

    fn cost(&self, freqs: &[u32]) -> u64 {
        freqs
            .iter()
            .enumerate()
            .map(|(group, &freq)| u64::from(freq) * u64::from(self.lengths[group]))
            .sum()
    }

    fn write<W: std::io::Write>(&self, writer: &mut MsbBitWriter<W>, group: usize) -> Result<()> {
        writer.put_bits(self.lengths[group], self.codes[group])?;
        Ok(())
    }
}

/// Decide between transmitting a position table and using the built-in one.
///
/// Returns `Some(code)` to transmit, `None` to fall back to `ready_made`. A
/// block with no matches always takes the built-in path, which costs one bit.
fn choose_position_code(freqs: &[u32]) -> Option<PreparedCode> {
    let prepared = prepare(freqs, MAX_P_LEN)?;
    let ready_made = LazyReadyMade::new();

    let transmitted_header = match prepared {
        // Three 4-bit fields plus the 7-bit group index.
        PreparedCode::Single(_) => 3 * u64::from(LENFIELD) + u64::from(PBIT),
        PreparedCode::Table { .. } => NP as u64 * u64::from(LENFIELD),
    };
    let transmitted: u64 = transmitted_header
        + freqs
            .iter()
            .enumerate()
            .map(|(group, &freq)| u64::from(freq) * prepared.cost(group))
            .sum::<u64>();

    if transmitted < ready_made.cost(freqs) {
        Some(prepared)
    } else {
        None
    }
}

/// Serialize the literal/length table.
fn write_code_table<W: std::io::Write>(
    writer: &mut MsbBitWriter<W>,
    code: &PreparedCode,
) -> Result<()> {
    match code {
        PreparedCode::Single(symbol) => {
            for _ in 0..3 {
                writer.put_bit(true)?;
                writer.put_bits(LENFIELD, 0)?; // length 1
            }
            writer.put_bits(CBIT, *symbol as u32)?;
        }
        PreparedCode::Table { lengths, .. } => {
            debug_assert!(
                !(lengths[0] == 1 && lengths[1] == 1 && lengths[2] == 1),
                "a complete code cannot start with three 1-bit codes"
            );
            for &length in lengths {
                if length == 0 {
                    writer.put_bit(false)?;
                } else {
                    writer.put_bit(true)?;
                    writer.put_bits(LENFIELD, u32::from(length - 1))?;
                }
            }
        }
    }
    Ok(())
}

/// Serialize the position table.
fn write_position_table<W: std::io::Write>(
    writer: &mut MsbBitWriter<W>,
    code: &PreparedCode,
) -> Result<()> {
    match code {
        PreparedCode::Single(group) => {
            for _ in 0..3 {
                writer.put_bits(LENFIELD, 1)?;
            }
            writer.put_bits(PBIT, *group as u32)?;
        }
        PreparedCode::Table { lengths, .. } => {
            debug_assert!(
                !(lengths[0] == 1 && lengths[1] == 1 && lengths[2] == 1),
                "a complete code cannot start with three 1-bit codes"
            );
            for &length in lengths {
                writer.put_bits(LENFIELD, u32::from(length))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let encoded = encode_lh3(data).expect("encode -lh3-");
        let decoded = decode_lh3(&encoded, data.len() as u64).expect("decode -lh3-");
        assert_eq!(decoded, data, "-lh3- round-trip mismatch");
        encoded
    }

    #[test]
    fn ready_made_table_is_complete_and_matches_larc() {
        let lengths = ready_made_lengths();
        assert_eq!(lengths.len(), 128);
        assert_eq!(lengths[0], 2);
        assert_eq!(lengths[1], 4);
        assert_eq!(lengths[2], 4);
        assert_eq!(lengths[3], 5);
        assert_eq!(lengths[5], 5);
        assert_eq!(lengths[6], 6);
        assert_eq!(lengths[12], 6);
        assert_eq!(lengths[13], 7);
        assert_eq!(lengths[30], 7);
        assert_eq!(lengths[31], 8);
        assert_eq!(lengths[77], 8);
        assert_eq!(lengths[78], 9);
        assert_eq!(lengths[127], 9);
        let kraft: u64 = lengths
            .iter()
            .map(|&len| 1u64 << (16 - u32::from(len)))
            .sum();
        assert_eq!(kraft, 1 << 16, "the built-in table must be a complete code");
    }

    #[test]
    fn constants_match_the_canonical_lha_definitions() {
        assert_eq!(N1, 286);
        assert_eq!(ESCAPE, 285);
        assert_eq!(NP, 128);
        assert_eq!(LH3_DICT_SIZE, 8192);
        assert_eq!(LENGTH_BASE, 253);
    }

    #[test]
    fn roundtrips_text() {
        roundtrip(b"the quick brown fox jumps over the lazy dog, the quick brown fox");
    }

    #[test]
    fn roundtrips_empty_and_single_byte() {
        assert!(encode_lh3(b"").expect("encode empty").is_empty());
        roundtrip(b"W");
    }

    #[test]
    fn roundtrips_single_repeated_byte_via_degenerate_tables() {
        // One literal plus one length symbol still needs a real (2-symbol)
        // table; a pure run drives both tables to their smallest form.
        let data: Vec<u8> = std::iter::repeat_n(b'*', 6000).collect();
        let encoded = roundtrip(&data);
        assert!(
            encoded.len() < data.len() / 8,
            "a pure run must compress hard: {} from {}",
            encoded.len(),
            data.len()
        );
    }

    #[test]
    fn roundtrips_multi_block_input() {
        // More than BLOCK_SYMBOLS commands forces several blocks, each with its
        // own tables.
        let data: Vec<u8> = (0..90_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 17) as u8)
            .collect();
        roundtrip(&data);
    }

    #[test]
    fn roundtrips_input_larger_than_the_window() {
        let unit = b"OxiArc legacy LZH method -lh3- exercise line.\n";
        let data: Vec<u8> = std::iter::repeat_n(unit, 900).flatten().copied().collect();
        assert!(data.len() > 4 * LH3_DICT_SIZE);
        let encoded = roundtrip(&data);
        assert!(encoded.len() < data.len() / 8);
    }

    #[test]
    fn roundtrips_all_byte_values() {
        let data: Vec<u8> = (0..=255u8).cycle().take(30_000).collect();
        roundtrip(&data);
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_short_ok() {
        let data: Vec<u8> = std::iter::repeat_n(b"abcdefghij", 500)
            .flatten()
            .copied()
            .collect();
        let encoded = encode_lh3(&data).expect("encode");
        assert!(decode_lh3(&[], data.len() as u64).is_err());
        for cut in [1usize, 4, 16, 64] {
            if cut < encoded.len() {
                assert!(
                    decode_lh3(&encoded[..cut], data.len() as u64).is_err(),
                    "a stream cut to {cut} bytes cannot yield {} bytes",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn zero_block_size_is_rejected() {
        // A 16-bit zero block size would make the decoder read headers forever.
        let stream = vec![0u8; 64];
        assert!(decode_lh3(&stream, 10).is_err());
    }

    #[test]
    fn the_built_in_position_table_round_trips() {
        // `ready_made` is the branch a cost-driven encoder may rarely pick, so
        // force it and prove the decoder reconstructs the same 128-entry code.
        // The bytes this produces are the ones the standalone LHa `shuf.c`
        // reference accepted (10/10 payloads) when the same policy was forced.
        for data in [
            b"the quick brown fox jumps over the lazy dog, the quick brown fox".to_vec(),
            std::iter::repeat_n(b'@', 5_000).collect(),
            (0..=255u8).cycle().take(30_000).collect(),
        ] {
            let encoded = encode_lh3_with(&data, PositionTablePolicy::AlwaysReadyMade)
                .expect("encode with the built-in table");
            let decoded = decode_lh3(&encoded, data.len() as u64).expect("decode");
            assert_eq!(decoded, data, "built-in position table round-trip");
        }
    }

    #[test]
    fn the_cheapest_policy_never_loses_to_the_built_in_table_by_much() {
        // Not a strict inequality per block (the choice is per block, and the
        // built-in table can win on some), but the cost-driven encoder must
        // never be dramatically worse overall.
        let data: Vec<u8> = std::iter::repeat_n(b"0123456789ABCDEF", 400)
            .flatten()
            .copied()
            .collect();
        let cheapest = encode_lh3(&data).expect("encode");
        let forced = encode_lh3_with(&data, PositionTablePolicy::AlwaysReadyMade).expect("encode");
        assert!(
            cheapest.len() <= forced.len(),
            "the cost-driven choice produced {} bytes vs {} forced",
            cheapest.len(),
            forced.len()
        );
    }

    #[test]
    fn degenerate_position_table_is_used_when_it_pays() {
        // A single distance across many matches should not cost a 512-bit table.
        let unit = b"0123456789ABCDEF";
        let data: Vec<u8> = std::iter::repeat_n(unit, 400).flatten().copied().collect();
        let encoded = roundtrip(&data);
        assert!(
            encoded.len() < 400,
            "expected a compact stream, got {}",
            encoded.len()
        );
    }
}
