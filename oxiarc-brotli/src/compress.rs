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
use crate::block_split;
use crate::context::{ContextMode, distance_context_id, literal_context_id};
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::{HuffmanTree, build_and_write_prefix_code};
use crate::lz77::{Lz77Command, Lz77Params, lz77_compress_pooled};
use crate::pool::BrotliPool;
use crate::tables::{
    block_count_to_code, compose_command, copy_length_to_code, insert_length_to_code,
};

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
///
/// Carried across meta-blocks (and, for the streaming encoder, across
/// `write`/`flush`/`finish` calls) so the distance ring stays consistent with
/// the decoder.
#[derive(Clone, Copy)]
pub(crate) struct EncoderState {
    /// The decoder's "last distance" (distance ring head). Initialized to 4
    /// at stream start (RFC 7932 Section 4).
    last_distance: usize,
}

impl EncoderState {
    /// Fresh encoder state at stream start (distance ring head = 4).
    pub(crate) fn new() -> Self {
        EncoderState { last_distance: 4 }
    }
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

    let mut state = EncoderState::new();
    let block_size = params.block_size();

    for chunk in data.chunks(block_size.max(1)) {
        if let Some(token) = cancel {
            token.check().map_err(BrotliError::from)?;
        }

        encode_meta_block(&mut writer, chunk, params, &mut state, pool)?;

        if let Some(handle) = progress {
            handle.on_progress(writer.output().len() as u64, None);
        }
    }

    // Empty last meta-block: ISLAST = 1, ISLASTEMPTY = 1.
    writer.write_bit(true)?;
    writer.write_bit(true)?;
    Ok(writer.finish())
}

/// Encode a single content meta-block (`ISLAST = 0`) for `chunk`, choosing the
/// smaller of a compressed and a stored representation.
///
/// This is the shared per-chunk core of both the one-shot [`encode_stream`]
/// and the incremental streaming compressor. `chunk` must be at most
/// `1 << 24` bytes; the `block_size()` chunking in every caller keeps it well
/// within that limit. `state.last_distance` is threaded across calls so the
/// distance ring stays consistent with the decoder.
///
/// Progress and cancellation are deliberately *not* handled here: the one-shot
/// path reports absolute `writer.output().len()` per chunk, whereas the
/// streaming path drains that buffer between chunks and reports a cumulative
/// counter — so each caller owns those hooks.
pub(crate) fn encode_meta_block(
    writer: &mut BitWriter,
    chunk: &[u8],
    params: &BrotliParams,
    state: &mut EncoderState,
    pool: Option<&BrotliPool>,
) -> BrotliResult<()> {
    if params.quality == 0 {
        write_stored_meta_blocks(writer, chunk)?;
        return Ok(());
    }

    // Speculatively encode a compressed meta-block into a *fresh* writer; keep
    // it only if it beats the stored size. The fresh `tmp` is essential for
    // the streaming caller: comparing against a persistent writer's
    // `bits_written()` (which grows without bound) would break the choice.
    let saved_state = *state;
    let mut tmp = BitWriter::with_capacity(chunk.len() / 2 + 64);
    encode_compressed_meta_block(&mut tmp, chunk, params, state, pool)?;
    // Stored cost upper bound: payload + per-16MB-sub-block header.
    let stored_bits = chunk.len() * 8 + 48 * chunk.len().div_ceil(1 << 24).max(1);
    if tmp.bits_written() < stored_bits {
        writer.append(&tmp)?;
    } else {
        *state = saved_state;
        write_stored_meta_blocks(writer, chunk)?;
    }
    Ok(())
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
pub(crate) fn write_window_bits(writer: &mut BitWriter, lgwin: u32) -> BrotliResult<()> {
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
    /// Decoded copy length. Not written directly (the insert-and-copy symbol
    /// plus extra bits carry it) but needed to derive the RFC 7932 Section 7.2
    /// distance context ID.
    copy_length: usize,
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
                    copy_length: copy_len,
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
            copy_length: 0,
            distance: None,
        });
    }

    let freqs = MetaBlockFreqs {
        literals: &lit_freqs,
        insert_and_copy: &ic_freqs,
        distances: &dist_freqs,
    };

    // ── Choose block splits and context models (quality 10-11 only) ──────
    //
    // Below quality 10 the encoder emits exactly the bits it always has: the
    // plan stays `MetaBlockPlan::BASELINE`, `write_meta_block_body` takes the
    // single-block-type / trivial-context-map path, and the output is
    // byte-identical to the pre-split encoder. That is what keeps the
    // reference-verified lower-quality output frozen.
    if params.quality < 10 {
        write_meta_block_body(writer, chunk, &commands, &freqs, &MetaBlockPlan::BASELINE)?;
        drop(scratch_guard);
        return Ok(());
    }

    let streams = SymbolStreams::collect(chunk, &commands);

    // Search the three categories independently by coordinate ascent: start
    // from the baseline and, one category at a time, try that category's
    // candidate settings against the *current best* plan for the other two,
    // keeping a change only when the fully-written meta-block gets smaller.
    //
    // Independent search matters. A single joint on/off switch would couple the
    // categories: data whose literal statistics are uniform but whose command
    // statistics change halfway would have to buy literal splitting (which
    // costs bits and buys nothing) in order to get insert-and-copy splitting,
    // and the measured comparison would then correctly reject the whole
    // bundle — leaving both features permanently unused on exactly the inputs
    // one of them was built for.
    //
    // Measuring rather than estimating is what makes this safe: every accepted
    // step is a real, written-out size reduction, so the result is never larger
    // than the baseline no matter how badly the heuristics misjudge.
    let mut plan = MetaBlockPlan::BASELINE;
    let mut best_bits = measure_meta_block(chunk, &commands, &freqs, &plan)?;

    // Literals: block types, context modeling, or both.
    for &(split, context_model) in &[(true, false), (false, true), (true, true)] {
        let candidate = block_split::plan_literals(
            &streams.literals,
            &streams.literal_contexts,
            split,
            context_model,
        );
        if candidate.is_none() {
            continue;
        }
        let previous = std::mem::replace(&mut plan.literals, candidate);
        let bits = measure_meta_block(chunk, &commands, &freqs, &plan)?;
        if bits < best_bits {
            best_bits = bits;
        } else {
            plan.literals = previous;
        }
    }

    // Insert-and-copy: block types only (this category has no context map).
    if let Some(candidate) = block_split::split_insert_and_copy(&streams.ic_symbols) {
        plan.insert_and_copy = Some(candidate);
        let bits = measure_meta_block(chunk, &commands, &freqs, &plan)?;
        if bits < best_bits {
            best_bits = bits;
        } else {
            plan.insert_and_copy = None;
        }
    }

    // Distances: block types, context modeling, or both.
    for &(split, context_model) in &[(true, false), (false, true), (true, true)] {
        let candidate = block_split::plan_distances(
            &streams.distance_symbols,
            &streams.distance_contexts,
            split,
            context_model,
        );
        if candidate.is_none() {
            continue;
        }
        let previous = std::mem::replace(&mut plan.distances, candidate);
        let bits = measure_meta_block(chunk, &commands, &freqs, &plan)?;
        if bits < best_bits {
            best_bits = bits;
        } else {
            plan.distances = previous;
        }
    }

    write_meta_block_body(writer, chunk, &commands, &freqs, &plan)?;
    drop(scratch_guard);
    Ok(())
}

/// Encoded size, in bits, of the meta-block `plan` would produce.
///
/// Measured by actually writing it, so the number includes every header field,
/// prefix-code descriptor and context map — the estimate and the artifact can
/// never disagree.
fn measure_meta_block(
    chunk: &[u8],
    commands: &[Command],
    freqs: &MetaBlockFreqs<'_>,
    plan: &MetaBlockPlan,
) -> BrotliResult<usize> {
    let mut probe = BitWriter::with_capacity(chunk.len() / 2 + 64);
    write_meta_block_body(&mut probe, chunk, commands, freqs, plan)?;
    Ok(probe.bits_written())
}

/// Symbol frequencies for one meta-block, shared by every encoding attempt.
struct MetaBlockFreqs<'a> {
    literals: &'a [u32],
    insert_and_copy: &'a [u32],
    distances: &'a [u32],
}

/// The three symbol streams a meta-block codes, flattened in decoder order
/// together with their RFC 7932 Section 7 context IDs.
///
/// Building these once and reusing them across candidate plans keeps the
/// measure-everything search from re-walking the command list per candidate.
struct SymbolStreams {
    /// Every inserted literal byte, in order.
    literals: Vec<u8>,
    /// Section 7.1 context ID of each literal under the declared context mode.
    literal_contexts: Vec<u8>,
    /// Every insert-and-copy command symbol, in order.
    ic_symbols: Vec<u16>,
    /// Distance symbols of the *distance-emitting* commands only.
    distance_symbols: Vec<u16>,
    /// Section 7.2 context ID (from copy length) of each of those commands.
    distance_contexts: Vec<u8>,
}

impl SymbolStreams {
    /// Walk the command list exactly as the decoder will, recording each
    /// stream and the context each symbol is coded under.
    fn collect(chunk: &[u8], commands: &[Command]) -> Self {
        let mut literals = Vec::new();
        let mut literal_contexts = Vec::new();
        let mut ic_symbols = Vec::with_capacity(commands.len());
        let mut distance_symbols = Vec::new();
        let mut distance_contexts = Vec::new();

        // The context of a literal depends on the two bytes that precede it in
        // the *output*, which for a literal inside a command is either an
        // earlier literal of the same run or the tail of the previous copy.
        // Tracking `position` in the chunk gives both cases for free.
        for cmd in commands {
            ic_symbols.push(cmd.ic_symbol);
            for index in cmd.literals.clone() {
                let p1 = if index >= 1 { chunk[index - 1] } else { 0 };
                let p2 = if index >= 2 { chunk[index - 2] } else { 0 };
                literals.push(chunk[index]);
                literal_contexts.push(literal_context_id(ENCODER_CONTEXT_MODE, p1, p2) as u8);
            }
            if let Some((dsym, _, _)) = cmd.distance {
                distance_symbols.push(dsym);
                distance_contexts.push(distance_context_id(cmd.copy_length) as u8);
            }
        }

        SymbolStreams {
            literals,
            literal_contexts,
            ic_symbols,
            distance_symbols,
            distance_contexts,
        }
    }
}

/// One candidate encoding of a meta-block's header.
///
/// `BASELINE` — all three fields `None` — is the pre-split form:
/// `NBLTYPESL = NBLTYPESI = NBLTYPESD = 1` with trivial context maps.
struct MetaBlockPlan {
    /// Literal block types and context map.
    literals: Option<block_split::LiteralPlan>,
    /// Insert-and-copy block types (this category has no context map).
    insert_and_copy: Option<block_split::SymbolSplit>,
    /// Distance block types and context map.
    distances: Option<block_split::DistancePlan>,
}

impl MetaBlockPlan {
    /// The single-block-type, trivial-context-map encoding.
    const BASELINE: MetaBlockPlan = MetaBlockPlan {
        literals: None,
        insert_and_copy: None,
        distances: None,
    };
}

/// Context mode this encoder declares for every literal block type.
///
/// `LSB6` keys on the low six bits of the preceding byte, which separates the
/// ASCII sub-alphabets (letters vs digits vs punctuation) that dominate text
/// without the UTF8 mode's sensitivity to the second-previous byte.
const ENCODER_CONTEXT_MODE: ContextMode = ContextMode::Lsb6;

/// Emits block-switch commands for one category, mirroring the decoder's
/// `BlockCategory::tick` exactly.
///
/// Constructed from a plan's runs; `tick` must be called once per symbol of
/// that category, before the symbol itself is written, and returns the block
/// type the symbol is coded under.
struct BlockSwitcher<'a> {
    runs: &'a [(u8, u32)],
    btype_tree: HuffmanTree,
    blen_tree: HuffmanTree,
    run_index: usize,
    remaining: u32,
    current_type: usize,
}

impl<'a> BlockSwitcher<'a> {
    /// Write the category's block-type/count prefix codes and first block
    /// count, returning the switcher that emits the remaining switches.
    ///
    /// Every switch after the first run names its type explicitly with symbol
    /// `2 + type`; symbols 0 and 1 (previous type / next type) are never used,
    /// which keeps the encoder's choice trivially valid.
    fn write_header(
        writer: &mut BitWriter,
        runs: &'a [(u8, u32)],
        num_types: usize,
    ) -> BrotliResult<Self> {
        block_split::write_block_type_count(writer, num_types)?;
        let mut btype_freqs = vec![0u32; num_types + 2];
        let mut blen_freqs = vec![0u32; 26];
        for (index, &(block_type, length)) in runs.iter().enumerate() {
            if index > 0 {
                btype_freqs[usize::from(block_type) + 2] += 1;
            }
            let (code, _, _) = block_count_to_code(length);
            blen_freqs[usize::from(code)] += 1;
        }
        let btype_tree = build_and_write_prefix_code(writer, &btype_freqs, num_types as u32 + 2)?;
        let blen_tree = build_and_write_prefix_code(writer, &blen_freqs, 26)?;

        // The first run's type is implicit (0); only its length is written.
        let first_length = runs[0].1;
        let (code, extra_bits, base) = block_count_to_code(first_length);
        blen_tree.encode_symbol(writer, code)?;
        if extra_bits > 0 {
            writer.write_bits(first_length - base, u32::from(extra_bits))?;
        }

        Ok(BlockSwitcher {
            runs,
            btype_tree,
            blen_tree,
            run_index: 0,
            remaining: first_length,
            current_type: 0,
        })
    }

    /// Consume one symbol of this category, emitting a block switch first when
    /// the current run is exhausted. Returns the block type to code under.
    fn tick(&mut self, writer: &mut BitWriter) -> BrotliResult<usize> {
        if self.remaining == 0 {
            self.run_index += 1;
            let (block_type, length) = self.runs[self.run_index];
            self.btype_tree
                .encode_symbol(writer, u16::from(block_type) + 2)?;
            let (code, extra_bits, base) = block_count_to_code(length);
            self.blen_tree.encode_symbol(writer, code)?;
            if extra_bits > 0 {
                writer.write_bits(length - base, u32::from(extra_bits))?;
            }
            self.current_type = usize::from(block_type);
            self.remaining = length;
        }
        self.remaining -= 1;
        Ok(self.current_type)
    }
}

/// Write the meta-block header and data for an already-built command list
/// under one candidate `plan`.
fn write_meta_block_body(
    writer: &mut BitWriter,
    chunk: &[u8],
    commands: &[Command],
    freqs: &MetaBlockFreqs<'_>,
    plan: &MetaBlockPlan,
) -> BrotliResult<()> {
    // ── Meta-block header (Section 9.2) ──────────────────────────────────
    writer.write_bit(false)?; // ISLAST = 0
    write_meta_block_length(writer, chunk.len())?;
    writer.write_bit(false)?; // ISUNCOMPRESSED = 0

    // Block type counts, in the fixed order L, I, D. When a category splits,
    // its block-type and block-count prefix codes and first block count follow
    // immediately (Section 9.2).
    let mut literal_switcher =
        match plan.literals {
            Some(ref literal_plan) if literal_plan.num_types >= 2 => Some(
                BlockSwitcher::write_header(writer, &literal_plan.runs, literal_plan.num_types)?,
            ),
            _ => {
                writer.write_bit(false)?; // NBLTYPESL = 1
                None
            }
        };
    let mut ic_switcher = match plan.insert_and_copy {
        Some(ref split) if split.num_types >= 2 => Some(BlockSwitcher::write_header(
            writer,
            &split.runs,
            split.num_types,
        )?),
        _ => {
            writer.write_bit(false)?; // NBLTYPESI = 1
            None
        }
    };
    let mut distance_switcher = match plan.distances {
        Some(ref distance_plan) if distance_plan.num_types >= 2 => Some(
            BlockSwitcher::write_header(writer, &distance_plan.runs, distance_plan.num_types)?,
        ),
        _ => {
            writer.write_bit(false)?; // NBLTYPESD = 1
            None
        }
    };

    writer.write_bits(0, 2)?; // NPOSTFIX = 0
    writer.write_bits(0, 4)?; // NDIRECT = 0

    // Context mode per literal block type.
    let num_literal_types = plan
        .literals
        .as_ref()
        .map_or(1, |literal_plan| literal_plan.num_types);
    for _ in 0..num_literal_types {
        writer.write_bits(ENCODER_CONTEXT_MODE as u32, 2)?;
    }

    // Literal context map (NTREESL + CMAPL). This is what binds prefix codes
    // to (block type, context) pairs; with a trivial map every pair would
    // share tree 0 and neither the split nor the context model would save
    // anything.
    //
    // Both context maps must be written *before* any prefix code: the header
    // order is NTREESL, CMAPL, NTREESD, CMAPD, then the codes themselves
    // (Section 9.2).
    match plan.literals {
        None => writer.write_bit(false)?, // NTREESL = 1 (trivial context map)
        Some(ref literal_plan) => {
            block_split::write_block_type_count(writer, literal_plan.num_trees)?;
            if literal_plan.num_trees >= 2 {
                block_split::write_context_map(
                    writer,
                    &literal_plan.context_map,
                    literal_plan.num_trees,
                )?;
            }
        }
    }

    // Distance context map (NTREESD + CMAPD).
    match plan.distances {
        None => writer.write_bit(false)?, // NTREESD = 1 (trivial context map)
        Some(ref distance_plan) => {
            block_split::write_block_type_count(writer, distance_plan.num_trees)?;
            if distance_plan.num_trees >= 2 {
                block_split::write_context_map(
                    writer,
                    &distance_plan.context_map,
                    distance_plan.num_trees,
                )?;
            }
        }
    }

    // Prefix codes: NTREESL literal codes, NBLTYPESI insert-and-copy codes,
    // NTREESD distance codes — in that order (Section 9.2).
    let lit_trees = match plan.literals {
        None => vec![build_and_write_prefix_code(writer, freqs.literals, 256)?],
        Some(ref literal_plan) => {
            let mut trees = Vec::with_capacity(literal_plan.num_trees);
            for histogram in &literal_plan.histograms {
                trees.push(build_and_write_prefix_code(writer, histogram, 256)?);
            }
            trees
        }
    };

    let ic_trees = match plan.insert_and_copy {
        None => vec![build_and_write_prefix_code(
            writer,
            freqs.insert_and_copy,
            704,
        )?],
        Some(ref split) => {
            let mut per_type = vec![vec![0u32; 704]; split.num_types];
            let mut position = 0usize;
            for &(block_type, length) in &split.runs {
                for command in &commands[position..position + length as usize] {
                    per_type[usize::from(block_type)][usize::from(command.ic_symbol)] += 1;
                }
                position += length as usize;
            }
            let mut trees = Vec::with_capacity(split.num_types);
            for histogram in &per_type {
                trees.push(build_and_write_prefix_code(writer, histogram, 704)?);
            }
            trees
        }
    };

    let dist_trees = match plan.distances {
        None => vec![build_and_write_prefix_code(writer, freqs.distances, 64)?],
        Some(ref distance_plan) => {
            let mut trees = Vec::with_capacity(distance_plan.num_trees);
            for histogram in &distance_plan.histograms {
                trees.push(build_and_write_prefix_code(writer, histogram, 64)?);
            }
            trees
        }
    };

    // ── Meta-block data (Section 9.3) ────────────────────────────────────
    //
    // The decoder consumes one "tick" of each category's block counter per
    // symbol of that category, switching block type when the current run is
    // exhausted; mirror that exactly, in the decoder's order.
    for cmd in commands {
        let ic_type = match ic_switcher {
            Some(ref mut switcher) => switcher.tick(writer)?,
            None => 0,
        };
        ic_trees[ic_type].encode_symbol(writer, cmd.ic_symbol)?;
        if cmd.ins_extra_bits > 0 {
            writer.write_bits(cmd.ins_extra, cmd.ins_extra_bits as u32)?;
        }
        if cmd.copy_extra_bits > 0 {
            writer.write_bits(cmd.copy_extra, cmd.copy_extra_bits as u32)?;
        }

        for index in cmd.literals.clone() {
            let literal_type = match literal_switcher {
                Some(ref mut switcher) => switcher.tick(writer)?,
                None => 0,
            };
            let tree_index = match plan.literals {
                None => 0,
                Some(ref literal_plan) => {
                    let p1 = if index >= 1 { chunk[index - 1] } else { 0 };
                    let p2 = if index >= 2 { chunk[index - 2] } else { 0 };
                    let context = literal_context_id(ENCODER_CONTEXT_MODE, p1, p2);
                    usize::from(
                        literal_plan.context_map
                            [literal_type * block_split::NUM_LITERAL_CONTEXTS + context],
                    )
                }
            };
            lit_trees[tree_index].encode_symbol(writer, u16::from(chunk[index]))?;
        }

        if let Some((dsym, dextra, dbits)) = cmd.distance {
            let distance_type = match distance_switcher {
                Some(ref mut switcher) => switcher.tick(writer)?,
                None => 0,
            };
            let tree_index = match plan.distances {
                None => 0,
                Some(ref distance_plan) => {
                    let context = distance_context_id(cmd.copy_length);
                    usize::from(
                        distance_plan.context_map
                            [distance_type * block_split::NUM_DISTANCE_CONTEXTS + context],
                    )
                }
            };
            dist_trees[tree_index].encode_symbol(writer, dsym)?;
            if dbits > 0 {
                writer.write_bits(dextra, dbits)?;
            }
        }
    }

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
