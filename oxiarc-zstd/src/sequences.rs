//! Sequences section decoding for Zstandard.
//!
//! Sequences describe LZ77-style back-references using literal lengths,
//! match lengths, and offsets.

use crate::backward_bits::{BitCursor, FseBitReader};
use crate::fse::{FseTable, FseTableEntry, read_fse_table_description};
use oxiarc_core::error::{OxiArcError, Result};

/// Maximum accuracy log for literal-length FSE tables (RFC 8878).
const LL_MAX_ACCURACY_LOG: u8 = 9;
/// Maximum accuracy log for offset FSE tables (RFC 8878).
const OF_MAX_ACCURACY_LOG: u8 = 8;
/// Maximum accuracy log for match-length FSE tables (RFC 8878).
const ML_MAX_ACCURACY_LOG: u8 = 9;
/// Maximum offset code (offset extra bits count; bounded so shifts are safe).
const MAX_OFFSET_CODE: u8 = 31;

/// A decoded sequence.
#[derive(Debug, Clone, Copy)]
pub struct Sequence {
    /// Number of literal bytes to copy.
    pub literal_length: usize,
    /// Number of bytes to copy from back-reference.
    pub match_length: usize,
    /// Offset for back-reference (or repeat offset index).
    pub offset: usize,
}

/// Sequences section header.
#[derive(Debug)]
pub struct SequencesHeader {
    /// Number of sequences.
    pub num_sequences: usize,
    /// Compression mode for literal lengths.
    pub ll_mode: CompressionMode,
    /// Compression mode for offsets.
    pub of_mode: CompressionMode,
    /// Compression mode for match lengths.
    pub ml_mode: CompressionMode,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Compression mode for sequence symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Predefined FSE table.
    Predefined,
    /// RLE (single symbol).
    Rle,
    /// FSE table in compressed form.
    Fse,
    /// Repeat previous FSE table.
    Repeat,
}

impl CompressionMode {
    /// Create from 2-bit value.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => CompressionMode::Predefined,
            1 => CompressionMode::Rle,
            2 => CompressionMode::Fse,
            3 => CompressionMode::Repeat,
            _ => unreachable!(),
        }
    }
}

/// Parse sequences header.
pub fn parse_sequences_header(data: &[u8]) -> Result<SequencesHeader> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty sequences section".to_string(),
        });
    }

    let byte0 = data[0];
    let (num_sequences, header_start) = if byte0 == 0 {
        (0, 1)
    } else if byte0 < 128 {
        (byte0 as usize, 1)
    } else if byte0 < 255 {
        if data.len() < 2 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "truncated sequences header".to_string(),
            });
        }
        let count = ((byte0 as usize - 128) << 8) + data[1] as usize;
        (count, 2)
    } else {
        if data.len() < 3 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "truncated sequences header".to_string(),
            });
        }
        let count = (data[1] as usize) + ((data[2] as usize) << 8) + 0x7F00;
        (count, 3)
    };

    if num_sequences == 0 {
        return Ok(SequencesHeader {
            num_sequences: 0,
            ll_mode: CompressionMode::Predefined,
            of_mode: CompressionMode::Predefined,
            ml_mode: CompressionMode::Predefined,
            header_size: header_start,
        });
    }

    if data.len() <= header_start {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "missing compression modes".to_string(),
        });
    }

    let modes_byte = data[header_start];
    let ll_mode = CompressionMode::from_bits((modes_byte >> 6) & 0x03);
    let of_mode = CompressionMode::from_bits((modes_byte >> 4) & 0x03);
    let ml_mode = CompressionMode::from_bits((modes_byte >> 2) & 0x03);

    Ok(SequencesHeader {
        num_sequences,
        ll_mode,
        of_mode,
        ml_mode,
        header_size: header_start + 1,
    })
}

/// Sequences decoder.
#[derive(Debug)]
pub struct SequencesDecoder {
    /// Literal length FSE table.
    ll_table: Option<FseTable>,
    /// Offset FSE table.
    of_table: Option<FseTable>,
    /// Match length FSE table.
    ml_table: Option<FseTable>,
    /// `true` when `ll_table` currently holds the RFC 8878 predefined table,
    /// so consecutive `Predefined` blocks reuse it instead of rebuilding.
    ll_is_predefined: bool,
    /// As `ll_is_predefined`, for the offset table.
    of_is_predefined: bool,
    /// As `ll_is_predefined`, for the match-length table.
    ml_is_predefined: bool,
    /// Repeat offsets.
    repeat_offsets: [usize; 3],
}

impl SequencesDecoder {
    /// Create a new sequences decoder.
    pub fn new() -> Self {
        Self {
            ll_table: None,
            of_table: None,
            ml_table: None,
            ll_is_predefined: false,
            of_is_predefined: false,
            ml_is_predefined: false,
            repeat_offsets: [1, 4, 8], // Default repeat offsets
        }
    }

    /// Decode sequences section, allocating a fresh vector for the result.
    ///
    /// Equivalent to [`decode_into`](Self::decode_into) with a fresh `Vec`.
    /// Both decode paths use `decode_into` with a buffer they reuse across
    /// blocks; this stays for tests, which want an owned result.
    #[cfg(test)]
    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<Sequence>, usize)> {
        let mut out = Vec::new();
        let consumed = self.decode_into(data, &mut out)?;
        Ok((out, consumed))
    }

    /// Decode a sequences section, appending the sequences to `out`.
    ///
    /// `out` is cleared first. Returns the number of bytes of `data` the
    /// sequences section occupies (always all of it — the sequences bitstream
    /// runs to the end of the block). Reusing one buffer across blocks is what
    /// keeps the incremental decoder allocation-free in the steady state.
    pub fn decode_into(&mut self, data: &[u8], out: &mut Vec<Sequence>) -> Result<usize> {
        out.clear();
        let header = parse_sequences_header(data)?;

        if header.num_sequences == 0 {
            return Ok(header.header_size);
        }

        let mut pos = header.header_size;

        // Read/setup FSE tables
        pos += self.setup_ll_table(&data[pos..], header.ll_mode)?;
        pos += self.setup_of_table(&data[pos..], header.of_mode)?;
        pos += self.setup_ml_table(&data[pos..], header.ml_mode)?;

        // Decode sequences from bitstream
        let bitstream = &data[pos..];
        self.decode_sequences(bitstream, header.num_sequences, out)?;

        Ok(data.len())
    }

    /// Setup literal length table.
    fn setup_ll_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                // The predefined table never changes; rebuilding it per block
                // would allocate on every compressed block in the stream.
                if !self.ll_is_predefined || self.ll_table.is_none() {
                    self.ll_table = Some(predefined_ll_table()?);
                    self.ll_is_predefined = true;
                }
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for literal lengths".to_string(),
                    });
                }
                self.ll_table = Some(rle_table(data[0]));
                self.ll_is_predefined = false;
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 35, LL_MAX_ACCURACY_LOG)?;
                self.ll_table = Some(table);
                self.ll_is_predefined = false;
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.ll_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Setup offset table.
    fn setup_of_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                // The predefined table never changes; rebuilding it per block
                // would allocate on every compressed block in the stream.
                if !self.of_is_predefined || self.of_table.is_none() {
                    self.of_table = Some(predefined_of_table()?);
                    self.of_is_predefined = true;
                }
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for offsets".to_string(),
                    });
                }
                self.of_table = Some(rle_table(data[0]));
                self.of_is_predefined = false;
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 31, OF_MAX_ACCURACY_LOG)?;
                self.of_table = Some(table);
                self.of_is_predefined = false;
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.of_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Setup match length table.
    fn setup_ml_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                // The predefined table never changes; rebuilding it per block
                // would allocate on every compressed block in the stream.
                if !self.ml_is_predefined || self.ml_table.is_none() {
                    self.ml_table = Some(predefined_ml_table()?);
                    self.ml_is_predefined = true;
                }
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for match lengths".to_string(),
                    });
                }
                self.ml_table = Some(rle_table(data[0]));
                self.ml_is_predefined = false;
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 52, ML_MAX_ACCURACY_LOG)?;
                self.ml_table = Some(table);
                self.ml_is_predefined = false;
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.ml_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Decode sequences from the backward bitstream (RFC 8878 §3.1.1.4).
    ///
    /// Read order per the specification:
    /// 1. Initial FSE states: literal-length, offset, match-length.
    /// 2. Per sequence: offset extra bits, then match-length extra bits, then
    ///    literal-length extra bits (the codes come from the *current* states
    ///    without consuming bits).
    /// 3. State updates in order literal-length, match-length, offset — and
    ///    **no** update after the final sequence.
    fn decode_sequences(
        &mut self,
        data: &[u8],
        count: usize,
        out: &mut Vec<Sequence>,
    ) -> Result<()> {
        let ll_table = self
            .ll_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing literal length table"))?;
        let of_table = self
            .of_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing offset table"))?;
        let ml_table = self
            .ml_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing match length table"))?;

        // The bitstream position is held as a detached, register-resident
        // cursor: a sequence costs six to nine bit-field reads, and driving
        // them through `&mut FseBitReader` puts the 64-bit container and the
        // consumed-bit count in memory, paying a store and a dependent load
        // for each one.
        let reader = FseBitReader::new(data)?;
        let bits = reader.data();
        let mut cur = reader.detach();

        let mut ll_state = read_field(&mut cur, ll_table.accuracy_log()) as usize;
        let mut of_state = read_field(&mut cur, of_table.accuracy_log()) as usize;
        let mut ml_state = read_field(&mut cur, ml_table.accuracy_log()) as usize;
        if cur.bits_remaining() < 0 {
            return Err(OxiArcError::corrupted(
                0,
                "sequence bitstream too short for initial FSE states",
            ));
        }

        // `Number_of_Sequences` is an attacker-controlled field that reaches
        // ~98 000, i.e. a ~2.3 MB reservation from a three-byte header. Every
        // sequence consumes at least one bit of the bitstream, so this is a
        // sound upper bound on how many can actually be decoded: a valid block
        // still gets its exact reservation, a lying header gets nothing.
        out.reserve(count.min(data.len().saturating_mul(8)));

        for i in 0..count {
            // Reload point 1: the offset and match-length extra bits that
            // follow are at most 31 + 16 = 47, which fits a container reloaded
            // to at most 7 consumed bits.
            cur.refill(bits);

            let ll_entry = *ll_table.get(ll_state)?;
            let of_entry = *of_table.get(of_state)?;
            let ml_entry = *ml_table.get(ml_state)?;

            // Extra bits are read offset first, then match length, then
            // literal length.
            let offset_and_reps = decode_offset(
                of_entry.symbol,
                ll_entry.symbol,
                &mut self.repeat_offsets,
                &mut cur,
            )?;
            let ml_value = decode_ml_value(ml_entry.symbol, &mut cur)?;

            // Reload point 2: the literal-length extra bits plus the three
            // state updates that follow are at most 16 + 9 + 9 + 8 = 42.
            cur.refill(bits);
            let ll_value = decode_ll_value(ll_entry.symbol, &mut cur)?;

            out.push(Sequence {
                literal_length: ll_value,
                match_length: ml_value,
                offset: offset_and_reps,
            });

            // Update states (skipped after the last sequence).
            if i + 1 < count {
                ll_state =
                    ll_entry.baseline as usize + read_field(&mut cur, ll_entry.num_bits) as usize;
                ml_state =
                    ml_entry.baseline as usize + read_field(&mut cur, ml_entry.num_bits) as usize;
                of_state =
                    of_entry.baseline as usize + read_field(&mut cur, of_entry.num_bits) as usize;
            }

            if cur.bits_remaining() < 0 {
                return Err(OxiArcError::corrupted(
                    0,
                    "sequence bitstream exhausted early",
                ));
            }
        }

        // A well-formed stream is consumed exactly (reference checks
        // `BIT_endOfDStream` after the last sequence).
        if !cur.is_finished() {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "sequence bitstream not fully consumed ({} bits left)",
                    cur.bits_remaining()
                ),
            ));
        }

        Ok(())
    }

    /// Reset all per-frame state.
    ///
    /// Clears the repeat offsets **and** the three FSE tables. The tables must
    /// go: `CompressionMode::Repeat` in a new frame's first block is invalid,
    /// and leaving the previous frame's tables in place would make the decoder
    /// silently accept it with the wrong table instead of erroring.
    pub fn reset(&mut self) {
        self.repeat_offsets = [1, 4, 8];
        self.ll_table = None;
        self.of_table = None;
        self.ml_table = None;
        self.ll_is_predefined = false;
        self.of_is_predefined = false;
        self.ml_is_predefined = false;
    }
}

/// Read `n` bits from a detached cursor with no reload.
///
/// The sequence loop reloads the 64-bit container at two *fixed* points per
/// sequence instead of testing after every field: the widths are all bounded
/// by the format (offset code <= 31, length extras <= 16, FSE state updates
/// <= 9), so the schedule is provably sufficient and the loop carries no
/// data-dependent branch for bit management. Testing after each of the six to
/// nine reads per sequence put six mispredictable branches in the loop.
#[inline(always)]
fn read_field(cur: &mut BitCursor, n: u8) -> u32 {
    let value = cur.peek(n);
    cur.advance(n);
    value
}

/// Decode offset with repeat offset handling (RFC 8878 §3.1.1.5).
///
/// The offset code and extra bits produce an `Offset_Value`:
///   `Offset_Value = (1 << code) + readBits(code)`
///
/// With `ll0 = (literal_length_code == 0)`:
/// - `Offset_Value > 3`: real offset = `Offset_Value - 3` (repeats shift down)
/// - otherwise `index = Offset_Value - 1 + ll0` selects a repeat offset:
///   index 0 = `repeat[0]`, 1 = `repeat[1]`, 2 = `repeat[2]`,
///   3 = `repeat[0] - 1`; the selected value moves to the front.
fn decode_offset(
    code: u8,
    ll_code: u8,
    repeat_offsets: &mut [usize; 3],
    cur: &mut BitCursor,
) -> Result<usize> {
    if code > MAX_OFFSET_CODE {
        return Err(OxiArcError::corrupted(
            0,
            format!("offset code {} exceeds maximum {}", code, MAX_OFFSET_CODE),
        ));
    }

    // Read extra bits (always `code` bits for offset).
    let extra = read_field(cur, code);
    let offset_value = (1usize << code) + extra as usize;

    if offset_value > 3 {
        // Regular offset: subtract 3 to get the real offset.
        let offset = offset_value - 3;
        repeat_offsets[2] = repeat_offsets[1];
        repeat_offsets[1] = repeat_offsets[0];
        repeat_offsets[0] = offset;
        return Ok(offset);
    }

    // Repeat-offset selection. A literal-length CODE of zero implies a
    // literal length of zero, which shifts the repeat index by one.
    let ll0 = usize::from(ll_code == 0);
    let index = offset_value - 1 + ll0; // 0..=3

    let offset = match index {
        0 => repeat_offsets[0], // most recent; no reordering
        1 => {
            repeat_offsets.swap(0, 1);
            repeat_offsets[0]
        }
        2 => {
            let offset = repeat_offsets[2];
            repeat_offsets[2] = repeat_offsets[1];
            repeat_offsets[1] = repeat_offsets[0];
            repeat_offsets[0] = offset;
            offset
        }
        _ => {
            // repeat[0] - 1; zero is invalid (input corrupted).
            let offset = repeat_offsets[0].wrapping_sub(1);
            if offset == 0 {
                return Err(OxiArcError::corrupted(
                    0,
                    "repeat offset underflow (offset would be zero)",
                ));
            }
            repeat_offsets[2] = repeat_offsets[1];
            repeat_offsets[1] = repeat_offsets[0];
            repeat_offsets[0] = offset;
            offset
        }
    };

    Ok(offset)
}

impl Default for SequencesDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode literal length value from code and extra bits.
fn decode_ll_value(code: u8, cur: &mut BitCursor) -> Result<usize> {
    // Literal length baseline and extra bits table
    const LL_BASELINE: [u32; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    const LL_EXTRA: [u8; 36] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ];

    let idx = code as usize;
    if idx >= LL_BASELINE.len() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("invalid literal length code: {}", code),
        });
    }

    let extra = read_field(cur, LL_EXTRA[idx]);
    Ok(LL_BASELINE[idx] as usize + extra as usize)
}

/// Decode match length value from code and extra bits.
fn decode_ml_value(code: u8, cur: &mut BitCursor) -> Result<usize> {
    // Match length baseline and extra bits table
    const ML_BASELINE: [u32; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ];
    const ML_EXTRA: [u8; 53] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ];

    let idx = code as usize;
    if idx >= ML_BASELINE.len() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("invalid match length code: {}", code),
        });
    }

    let extra = read_field(cur, ML_EXTRA[idx]);
    Ok(ML_BASELINE[idx] as usize + extra as usize)
}

/// Create RLE FSE table (single symbol).
fn rle_table(symbol: u8) -> FseTable {
    let entries = vec![FseTableEntry {
        symbol,
        num_bits: 0,
        baseline: 0,
    }];
    FseTable::from_entries(0, entries)
}

/// Create predefined literal length FSE table.
///
/// # Errors
///
/// The RFC 8878 predefined distribution below is a compile-time constant that
/// `FseTable::new` always accepts; this returns `Result` (rather than
/// `.expect`-ing internally) purely so that guarantee is enforced by the
/// caller's own error handling instead of an internal panic if it were ever
/// violated by a future edit to the table.
fn predefined_ll_table() -> Result<FseTable> {
    // Predefined distribution for literal lengths (accuracy log 6)
    let probs = [
        4i16, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
        1, 1, 1, -1, -1, -1, -1,
    ];
    FseTable::new(6, &probs)
}

/// Create predefined offset FSE table.
///
/// # Errors
///
/// See [`predefined_ll_table`]; the same "always valid, but not `.expect`-ed"
/// reasoning applies.
fn predefined_of_table() -> Result<FseTable> {
    // Predefined distribution for offsets (accuracy log 5, 29 symbols 0-28)
    // Per RFC 8878 Section 3.1.1.3.2.2.1
    let probs = [
        1i16, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1,
        -1,
    ];
    FseTable::new(5, &probs)
}

/// Create predefined match length FSE table.
///
/// # Errors
///
/// See [`predefined_ll_table`]; the same "always valid, but not `.expect`-ed"
/// reasoning applies.
fn predefined_ml_table() -> Result<FseTable> {
    // Predefined distribution for match lengths (accuracy log 6)
    let probs = [
        1i16, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
    ];
    FseTable::new(6, &probs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_mode_from_bits() {
        assert_eq!(CompressionMode::from_bits(0), CompressionMode::Predefined);
        assert_eq!(CompressionMode::from_bits(1), CompressionMode::Rle);
        assert_eq!(CompressionMode::from_bits(2), CompressionMode::Fse);
        assert_eq!(CompressionMode::from_bits(3), CompressionMode::Repeat);
    }

    #[test]
    fn test_parse_zero_sequences() {
        let data = [0];
        let header = parse_sequences_header(&data).expect("valid sequences header");
        assert_eq!(header.num_sequences, 0);
    }

    #[test]
    fn test_parse_small_sequence_count() {
        let data = [10, 0b00000000]; // 10 sequences, all predefined
        let header = parse_sequences_header(&data).expect("valid sequences header");
        assert_eq!(header.num_sequences, 10);
        assert_eq!(header.ll_mode, CompressionMode::Predefined);
        assert_eq!(header.of_mode, CompressionMode::Predefined);
        assert_eq!(header.ml_mode, CompressionMode::Predefined);
    }

    #[test]
    fn test_predefined_tables() {
        let ll = predefined_ll_table().expect("predefined LL table is always valid");
        let of = predefined_of_table().expect("predefined OF table is always valid");
        let ml = predefined_ml_table().expect("predefined ML table is always valid");

        assert_eq!(ll.accuracy_log(), 6);
        assert_eq!(of.accuracy_log(), 5);
        assert_eq!(ml.accuracy_log(), 6);
    }

    #[test]
    fn test_rle_table() {
        let table = rle_table(42);
        let entry = table.get(0).expect("state 0 must exist");
        assert_eq!(entry.symbol, 42);
        assert_eq!(entry.num_bits, 0);
    }
}
