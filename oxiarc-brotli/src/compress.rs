//! Brotli compression implementation.
//!
//! Produces RFC 7932-conformant streams that reference decoders (e.g. the
//! `brotli` CLI) accept, using:
//!
//! - LZ77 matching with backward references,
//! - one prefix code per category (literals, insert-and-copy commands,
//!   distances) per meta-block, with RFC-exact simple/complex descriptors,
//! - the real insert-and-copy command alphabet (Section 5), including
//!   implicit distance-code-0 cells for repeated distances,
//! - the Section 4 distance code space with `NPOSTFIX = 0`, `NDIRECT = 0`,
//! - uncompressed (stored) meta-blocks for incompressible chunks and for
//!   quality 0.
//!
//! Every content meta-block is emitted with `ISLAST = 0`; the stream is
//! terminated by an empty last meta-block (2 bits), which keeps the encoder
//! uniform and spec-exact.
//!
//! As a defense-in-depth guarantee against ever shipping a malformed
//! stream, the encoder decodes its own complete output and falls back to a
//! stored-only stream if the round-trip does not match (this should never
//! happen and is asserted in debug builds).

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::bit_writer::BitWriter;
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::build_and_write_prefix_code;
use crate::lz77::{Lz77Command, Lz77Params, lz77_compress_pooled};
use crate::pool::BrotliPool;
use crate::tables::{compose_command, copy_length_to_code, insert_length_to_code};

/// Brotli compression parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrotliParams {
    /// Quality level (0-11). Higher = better compression, slower.
    /// Quality 0 stores the data in uncompressed meta-blocks.
    pub quality: u32,
    /// Log2-ish window parameter WBITS (10-24). The sliding window size is
    /// `(1 << lgwin) - 16` bytes, per RFC 7932 Section 9.1. Default: 22.
    pub lgwin: u32,
    /// Log2 of the maximum input block size (16-24). Default: 0 (auto).
    pub lgblock: u32,
}

impl Default for BrotliParams {
    fn default() -> Self {
        BrotliParams {
            quality: 6,
            lgwin: 22,
            lgblock: 0,
        }
    }
}

impl BrotliParams {
    /// Validate parameters.
    pub fn validate(&self) -> BrotliResult<()> {
        if self.quality > 11 {
            return Err(BrotliError::InvalidParameter(format!(
                "quality {} out of range [0, 11]",
                self.quality
            )));
        }
        if !(10..=24).contains(&self.lgwin) {
            return Err(BrotliError::InvalidParameter(format!(
                "lgwin {} out of range [10, 24]",
                self.lgwin
            )));
        }
        if self.lgblock != 0 && !(16..=24).contains(&self.lgblock) {
            return Err(BrotliError::InvalidParameter(format!(
                "lgblock {} out of range [16, 24] (or 0 for auto)",
                self.lgblock
            )));
        }
        Ok(())
    }

    /// Get the sliding window size in bytes: `(1 << lgwin) - 16`
    /// (RFC 7932 Section 9.1).
    pub fn window_size(&self) -> usize {
        (1usize << self.lgwin) - 16
    }

    /// Get the effective meta-block input size.
    pub fn block_size(&self) -> usize {
        if self.lgblock == 0 {
            // Auto: choose based on quality.
            match self.quality {
                0..=4 => 1 << 18, // 256KB
                5..=8 => 1 << 20, // 1MB
                _ => 1 << 22,     // 4MB
            }
        } else {
            1 << self.lgblock
        }
    }
}

/// Compress data using Brotli with the given quality level.
///
/// This is the crate's primary entry point and follows the workspace-wide
/// `compress(data, level)` convention shared by the other codec crates.
/// Valid qualities are `0..=11`; use [`compress_with_params`] for full
/// control over `lgwin`/`lgblock`.
pub fn compress(data: &[u8], quality: u32) -> BrotliResult<Vec<u8>> {
    let params = BrotliParams {
        quality,
        ..Default::default()
    };
    compress_with_params(data, &params)
}

/// Compress data using Brotli with full parameter control.
pub fn compress_with_params(data: &[u8], params: &BrotliParams) -> BrotliResult<Vec<u8>> {
    compress_with_hooks(data, params, None, None)
}

/// Compress data with optional per-meta-block progress and cancellation hooks.
pub(crate) fn compress_with_hooks(
    data: &[u8],
    params: &BrotliParams,
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
) -> BrotliResult<Vec<u8>> {
    compress_with_hooks_pooled(data, params, progress, cancel, None)
}

/// Compress data with optional hooks and an optional buffer pool.
///
/// This is the internal implementation behind both [`compress_with_hooks`]
/// (no pool) and [`crate::pool::compress_with_params_pooled`] (with pool).
pub(crate) fn compress_with_hooks_pooled(
    data: &[u8],
    params: &BrotliParams,
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
    pool: Option<&BrotliPool>,
) -> BrotliResult<Vec<u8>> {
    params.validate()?;

    let output = encode_stream(data, params, progress, cancel, pool)?;

    // Defense in depth: the encoder must never emit a stream that does not
    // decode back to the input. On the (never expected) mismatch, fall back
    // to a stored-only stream, which is trivially correct.
    match crate::decompress::decompress(&output) {
        Ok(ref decoded) if decoded == data => Ok(output),
        _ => {
            debug_assert!(false, "encoder self-check failed; stored fallback used");
            encode_stored_stream(data, params)
        }
    }
}

/// Encoder state that mirrors decoder state persisting across meta-blocks.
#[derive(Clone, Copy)]
struct EncoderState {
    /// The decoder's "last distance" (distance ring head). Initialized to 4
    /// at stream start (RFC 7932 Section 4).
    last_distance: usize,
}

/// Encode the complete stream (header + meta-blocks + empty last block).
fn encode_stream(
    data: &[u8],
    params: &BrotliParams,
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
    pool: Option<&BrotliPool>,
) -> BrotliResult<Vec<u8>> {
    let mut writer = BitWriter::with_capacity(data.len() / 2 + 64);
    write_window_bits(&mut writer, params.lgwin)?;

    let mut state = EncoderState { last_distance: 4 };
    let block_size = params.block_size();

    for chunk in data.chunks(block_size.max(1)) {
        if let Some(token) = cancel {
            token.check().map_err(BrotliError::from)?;
        }

        if params.quality == 0 {
            write_stored_meta_blocks(&mut writer, chunk)?;
        } else {
            // Speculatively encode a compressed meta-block; keep it only if
            // it beats stored size.
            let saved_state = state;
            let mut tmp = BitWriter::with_capacity(chunk.len() / 2 + 64);
            encode_compressed_meta_block(&mut tmp, chunk, params, &mut state, pool)?;
            // Stored cost upper bound: payload + per-16MB-sub-block header.
            let stored_bits = chunk.len() * 8 + 48 * chunk.len().div_ceil(1 << 24).max(1);
            if tmp.bits_written() < stored_bits {
                writer.append(&tmp)?;
            } else {
                state = saved_state;
                write_stored_meta_blocks(&mut writer, chunk)?;
            }
        }

        if let Some(handle) = progress {
            handle.on_progress(writer.output().len() as u64, None);
        }
    }

    // Empty last meta-block: ISLAST = 1, ISLASTEMPTY = 1.
    writer.write_bit(true)?;
    writer.write_bit(true)?;
    Ok(writer.finish())
}

/// Encode `data` as a stored-only stream (used by the self-check fallback).
fn encode_stored_stream(data: &[u8], params: &BrotliParams) -> BrotliResult<Vec<u8>> {
    let mut writer = BitWriter::with_capacity(data.len() + 64);
    write_window_bits(&mut writer, params.lgwin)?;
    write_stored_meta_blocks(&mut writer, data)?;
    writer.write_bit(true)?;
    writer.write_bit(true)?;
    Ok(writer.finish())
}

/// Write the stream header WBITS field (RFC 7932 Section 9.1).
///
/// Encodings (bits written LSB-first):
/// - 16: `0`
/// - 18..=24: `1` + 3 bits of `wbits - 17`
/// - 17: `1 000 000`
/// - 10..=15: `1 000` + 3 bits of `wbits - 8`
fn write_window_bits(writer: &mut BitWriter, lgwin: u32) -> BrotliResult<()> {
    match lgwin {
        16 => writer.write_bit(false),
        18..=24 => {
            writer.write_bit(true)?;
            writer.write_bits(lgwin - 17, 3)
        }
        17 => {
            writer.write_bit(true)?;
            writer.write_bits(0, 3)?;
            writer.write_bits(0, 3)
        }
        10..=15 => {
            writer.write_bit(true)?;
            writer.write_bits(0, 3)?;
            writer.write_bits(lgwin - 8, 3)
        }
        _ => Err(BrotliError::InvalidWindowSize(lgwin)),
    }
}

/// Write the meta-block length: MNIBBLES code (2 bits) + `MLEN - 1`
/// (RFC 7932 Section 9.2). The nibble count is minimal, as required.
fn write_meta_block_length(writer: &mut BitWriter, mlen: usize) -> BrotliResult<()> {
    if mlen == 0 {
        return Err(BrotliError::InvalidParameter(
            "meta-block length cannot be zero".to_string(),
        ));
    }
    let value = (mlen - 1) as u32;
    let nibbles = if value < (1 << 16) {
        4
    } else if value < (1 << 20) {
        5
    } else if value < (1 << 24) {
        6
    } else {
        return Err(BrotliError::InvalidParameter(format!(
            "meta-block length {mlen} too large"
        )));
    };
    writer.write_bits(nibbles - 4, 2)?;
    writer.write_bits(value, nibbles * 4)
}

/// Write `chunk` as one or more uncompressed (stored) meta-blocks with
/// `ISLAST = 0` (an uncompressed meta-block cannot be last, Section 9.2).
fn write_stored_meta_blocks(writer: &mut BitWriter, chunk: &[u8]) -> BrotliResult<()> {
    for sub in chunk.chunks(1 << 24) {
        writer.write_bit(false)?; // ISLAST = 0
        write_meta_block_length(writer, sub.len())?;
        writer.write_bit(true)?; // ISUNCOMPRESSED = 1
        writer.flush(); // zero padding to the byte boundary
        writer.write_bytes(sub)?;
    }
    Ok(())
}

/// One insert-and-copy command prepared for emission.
struct Command {
    /// Range of literal bytes in the chunk to insert before the copy.
    literals: std::ops::Range<usize>,
    /// Insert-and-copy command symbol (0..704).
    ic_symbol: u16,
    /// Insert length extra bits.
    ins_extra: u32,
    ins_extra_bits: u8,
    /// Copy length extra bits.
    copy_extra: u32,
    copy_extra_bits: u8,
    /// Explicit distance symbol and extra bits; `None` for implicit
    /// distance-code-0 commands and for the trailing insert-only command.
    distance: Option<(u16, u32, u32)>,
}

/// Encode one compressed meta-block (ISLAST=0) for `chunk`.
fn encode_compressed_meta_block(
    writer: &mut BitWriter,
    chunk: &[u8],
    params: &BrotliParams,
    state: &mut EncoderState,
    pool: Option<&BrotliPool>,
) -> BrotliResult<()> {
    // ── LZ77 ─────────────────────────────────────────────────────────────
    let lz77_params = Lz77Params {
        quality: params.quality,
        window_size: params.window_size(),
        min_match_len: 4,
        max_match_len: 16 * 1024,
    };
    let lz_commands = lz77_compress_pooled(chunk, &lz77_params, pool);

    // ── Command construction + histograms ────────────────────────────────
    // Frequency scratch: literals (256) + insert-and-copy (704) + distance
    // (64, NPOSTFIX=0/NDIRECT=0) = exactly the pool's 1024-u32 buffer.
    let mut scratch_guard = pool.map(|p| p.get_huffman_scratch());
    let (mut lit_freqs, mut ic_freqs, mut dist_freqs) = if let Some(ref mut g) = scratch_guard {
        let (lit, rest) = g.buf.split_at(256);
        let (ic, dist) = rest.split_at(704);
        (lit.to_vec(), ic.to_vec(), dist[..64].to_vec())
    } else {
        (vec![0u32; 256], vec![0u32; 704], vec![0u32; 64])
    };

    let mut commands: Vec<Command> = Vec::new();
    let mut lit_start = 0usize; // start of the pending literal run
    let mut pos = 0usize; // current position in chunk

    for cmd in &lz_commands {
        match cmd {
            Lz77Command::Literal(_) => {
                pos += 1;
            }
            Lz77Command::Reference { length, distance } => {
                let insert_len = pos - lit_start;
                let copy_len = *length;
                let (ins_code, ins_extra_bits, ins_base) = insert_length_to_code(insert_len as u32);
                let (copy_code, copy_extra_bits, copy_base) = copy_length_to_code(copy_len as u32);

                let implicit = *distance == state.last_distance && ins_code < 8 && copy_code < 16;
                let ic_symbol = compose_command(ins_code, copy_code, implicit);

                let distance_field = if implicit {
                    None
                } else {
                    let (dsym, dextra, dbits) = distance_symbol(*distance)?;
                    dist_freqs[dsym as usize] += 1;
                    state.last_distance = *distance;
                    Some((dsym, dextra, dbits))
                };

                ic_freqs[ic_symbol as usize] += 1;
                for &b in &chunk[lit_start..pos] {
                    lit_freqs[b as usize] += 1;
                }
                commands.push(Command {
                    literals: lit_start..pos,
                    ic_symbol,
                    ins_extra: insert_len as u32 - ins_base,
                    ins_extra_bits,
                    copy_extra: copy_len as u32 - copy_base,
                    copy_extra_bits,
                    distance: distance_field,
                });
                pos += copy_len;
                lit_start = pos;
            }
        }
    }

    // Trailing literals: an insert-only command. Its copy length is ignored
    // by the decoder because the insert completes MLEN (Section 9.3); no
    // distance is emitted.
    if lit_start < pos || commands.is_empty() {
        let insert_len = pos - lit_start;
        let (ins_code, ins_extra_bits, ins_base) = insert_length_to_code(insert_len as u32);
        let ic_symbol = compose_command(ins_code, 0, false);
        ic_freqs[ic_symbol as usize] += 1;
        for &b in &chunk[lit_start..pos] {
            lit_freqs[b as usize] += 1;
        }
        commands.push(Command {
            literals: lit_start..pos,
            ic_symbol,
            ins_extra: insert_len as u32 - ins_base,
            ins_extra_bits,
            copy_extra: 0,
            copy_extra_bits: 0,
            distance: None,
        });
    }

    // ── Meta-block header (Section 9.2) ──────────────────────────────────
    writer.write_bit(false)?; // ISLAST = 0
    write_meta_block_length(writer, chunk.len())?;
    writer.write_bit(false)?; // ISUNCOMPRESSED = 0
    writer.write_bit(false)?; // NBLTYPESL = 1
    writer.write_bit(false)?; // NBLTYPESI = 1
    writer.write_bit(false)?; // NBLTYPESD = 1
    writer.write_bits(0, 2)?; // NPOSTFIX = 0
    writer.write_bits(0, 4)?; // NDIRECT = 0
    writer.write_bits(0, 2)?; // context mode for literal block type 0: LSB6
    writer.write_bit(false)?; // NTREESL = 1 (trivial literal context map)
    writer.write_bit(false)?; // NTREESD = 1 (trivial distance context map)

    let lit_tree = build_and_write_prefix_code(writer, &lit_freqs, 256)?;
    let ic_tree = build_and_write_prefix_code(writer, &ic_freqs, 704)?;
    let dist_tree = build_and_write_prefix_code(writer, &dist_freqs, 64)?;

    // ── Meta-block data (Section 9.3) ────────────────────────────────────
    for cmd in &commands {
        ic_tree.encode_symbol(writer, cmd.ic_symbol)?;
        if cmd.ins_extra_bits > 0 {
            writer.write_bits(cmd.ins_extra, cmd.ins_extra_bits as u32)?;
        }
        if cmd.copy_extra_bits > 0 {
            writer.write_bits(cmd.copy_extra, cmd.copy_extra_bits as u32)?;
        }
        for &b in &chunk[cmd.literals.clone()] {
            lit_tree.encode_symbol(writer, b as u16)?;
        }
        if let Some((dsym, dextra, dbits)) = cmd.distance {
            dist_tree.encode_symbol(writer, dsym)?;
            if dbits > 0 {
                writer.write_bits(dextra, dbits)?;
            }
        }
    }

    drop(scratch_guard);
    Ok(())
}

/// Map a distance to its `(symbol, extra, extra_bits)` for the distance
/// code space with `NPOSTFIX = 0`, `NDIRECT = 0` (RFC 7932 Section 4).
fn distance_symbol(distance: usize) -> BrotliResult<(u16, u32, u32)> {
    if distance == 0 {
        return Err(BrotliError::InvalidParameter(
            "distance cannot be zero".to_string(),
        ));
    }
    let d = distance as u64;
    for hcode in 0u64..48 {
        let ndistbits = 1 + (hcode >> 1);
        let offset = ((2 + (hcode & 1)) << ndistbits) - 4;
        let low = offset + 1;
        let high = offset + (1 << ndistbits);
        if d >= low && d <= high {
            return Ok(((16 + hcode) as u16, (d - low) as u32, ndistbits as u32));
        }
    }
    Err(BrotliError::InvalidParameter(format!(
        "distance {distance} exceeds the encodable range"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress::decompress;

    #[test]
    fn test_params_default() {
        let params = BrotliParams::default();
        assert_eq!(params.quality, 6);
        assert_eq!(params.lgwin, 22);
        assert_eq!(params.lgblock, 0);
        assert!(params.validate().is_ok());
    }

    #[test]
    fn test_params_validation() {
        let mut params = BrotliParams {
            quality: 12,
            ..Default::default()
        };
        assert!(params.validate().is_err());

        params.quality = 6;
        params.lgwin = 25;
        assert!(params.validate().is_err());
        params.lgwin = 9;
        assert!(params.validate().is_err());
        // The full RFC WBITS range 10..=24 is accepted.
        for lgwin in 10..=24 {
            params.lgwin = lgwin;
            assert!(params.validate().is_ok(), "lgwin {lgwin}");
        }

        params.lgwin = 22;
        params.lgblock = 15;
        assert!(params.validate().is_err());
    }

    #[test]
    fn test_window_size_rfc_semantics() {
        let params = BrotliParams {
            lgwin: 16,
            ..Default::default()
        };
        assert_eq!(params.window_size(), 65520);
        let params = BrotliParams {
            lgwin: 22,
            ..Default::default()
        };
        assert_eq!(params.window_size(), (1 << 22) - 16);
    }

    #[test]
    fn test_compress_empty() {
        let result = compress(b"", 6).expect("should compress empty");
        assert!(!result.is_empty());
        assert_eq!(decompress(&result).ok(), Some(Vec::new()));
    }

    #[test]
    fn test_roundtrip_small() {
        for quality in 0..=11 {
            let data = b"Hello, Brotli! Hello, Brotli! Hello, Brotli!";
            let compressed = compress(data, quality).expect("compress");
            let decompressed = decompress(&compressed).expect("decompress");
            assert_eq!(decompressed, data, "quality {quality}");
        }
    }

    #[test]
    fn test_roundtrip_all_window_sizes() {
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        for lgwin in 10..=24 {
            let params = BrotliParams {
                quality: 5,
                lgwin,
                lgblock: 0,
            };
            let compressed = compress_with_params(&data, &params).expect("compress");
            let decompressed = decompress(&compressed).expect("decompress");
            assert_eq!(decompressed, data, "lgwin {lgwin}");
        }
    }

    #[test]
    fn test_repeated_distance_uses_implicit_code() {
        // Periodic data produces repeated distances; the stream must still
        // round-trip through the implicit distance-code-0 path.
        let data: Vec<u8> = b"abcdefgh".repeat(500);
        let compressed = compress(&data, 6).expect("compress");
        let decompressed = decompress(&compressed).expect("decompress");
        assert_eq!(decompressed, data);
        assert!(compressed.len() < data.len() / 4, "should compress well");
    }

    #[test]
    fn test_incompressible_falls_back_to_stored() {
        // Pseudo-random bytes cannot be compressed; the output must stay
        // close to the input size (stored) and round-trip exactly.
        let mut state = 0x0123_4567_89AB_CDEFu64;
        let data: Vec<u8> = (0..100_000)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        let compressed = compress(&data, 6).expect("compress");
        assert!(compressed.len() < data.len() + 256, "stored fallback size");
        let decompressed = decompress(&compressed).expect("decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_distance_symbol_formula() {
        // dcode 16: distances 1..2; dcode 17: 3..4 (RFC Section 4 with
        // NPOSTFIX=0, NDIRECT=0).
        assert_eq!(distance_symbol(1).ok(), Some((16, 0, 1)));
        assert_eq!(distance_symbol(2).ok(), Some((16, 1, 1)));
        assert_eq!(distance_symbol(3).ok(), Some((17, 0, 1)));
        assert_eq!(distance_symbol(4).ok(), Some((17, 1, 1)));
        assert!(distance_symbol(0).is_err());
        // Verify the full decode formula inverse over a range.
        for dist in [1usize, 2, 5, 17, 100, 1000, 65535, 1 << 20, (1 << 24) - 16] {
            let (sym, extra, bits) = distance_symbol(dist).expect("symbol");
            let hcode = (sym - 16) as u64;
            let ndistbits = 1 + (hcode >> 1);
            assert_eq!(ndistbits as u32, bits);
            let offset = ((2 + (hcode & 1)) << ndistbits) - 4;
            assert_eq!((offset + extra as u64 + 1) as usize, dist);
        }
    }

    #[test]
    fn test_meta_block_length_minimal_nibbles() {
        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, 1).expect("mlen 1");
        assert_eq!(writer.bits_written(), 2 + 16);
        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, 1 << 16).expect("mlen 2^16");
        assert_eq!(writer.bits_written(), 2 + 16); // value 2^16 - 1 fits 4 nibbles
        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, (1 << 16) + 1).expect("mlen 2^16+1");
        assert_eq!(writer.bits_written(), 2 + 20);
        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, 1 << 24).expect("mlen 2^24");
        assert_eq!(writer.bits_written(), 2 + 24);
        assert!(write_meta_block_length(&mut BitWriter::new(), (1 << 24) + 1).is_err());
    }

    #[test]
    fn test_window_bits_all_values_roundtrip() {
        use crate::bit_reader::BitReader;
        for lgwin in 10..=24u32 {
            let mut writer = BitWriter::new();
            write_window_bits(&mut writer, lgwin).expect("write");
            let data = writer.finish();
            let mut reader = BitReader::new(&data);
            // Reuse the decoder's reader through a tiny local mirror of
            // decompress::read_window_bits semantics.
            let got = {
                if !reader.read_bit().expect("bit") {
                    16
                } else {
                    let n = reader.read_bits(3).expect("bits");
                    if n != 0 {
                        17 + n
                    } else {
                        let m = reader.read_bits(3).expect("bits");
                        if m == 0 { 17 } else { 8 + m }
                    }
                }
            };
            assert_eq!(got, lgwin, "lgwin {lgwin}");
        }
        assert!(write_window_bits(&mut BitWriter::new(), 9).is_err());
        assert!(write_window_bits(&mut BitWriter::new(), 25).is_err());
    }
}
