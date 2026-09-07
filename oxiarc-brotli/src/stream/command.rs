//! The resumable command loop of the incremental decoder (tier 2).
//!
//! RFC 7932 Section 9.3's command loop is where a Brotli meta-block actually
//! produces bytes: an insert-and-copy symbol, then `insert_length` literals,
//! then a backward reference of `copy_length` bytes (or a static-dictionary
//! word). Three things can interrupt it:
//!
//! 1. **input runs out mid-symbol** — the bit cursor and the block-switch
//!    state are rewound to the start of the step and
//!    [`CommandStatus::NeedInput`] is returned, so no bit is ever consumed
//!    twice and no half-decoded symbol is observable;
//! 2. **the caller's output slice fills up mid-insert, mid-copy or
//!    mid-dictionary-word** — the remaining count is kept in [`CmdState`] and
//!    [`CommandStatus::NeedOutput`] is returned;
//! 3. **the meta-block completes** — [`CommandStatus::Finished`].
//!
//! Rewinding is only ever done *before* a byte is produced. Once a byte has
//! been handed to the caller it is final, which is why the loop is split into
//! a bit-consuming step (rewindable) and a byte-producing step (resumable by
//! count, consuming no bits).

use crate::bit_reader::BitReader;
use crate::context::{distance_context_id, literal_context_id};
use crate::decompress::{DecoderState, MetaBlockHeader, decode_distance};
use crate::dictionary;
use crate::error::{BrotliError, BrotliResult};
use crate::tables::{COPY_LENGTH_CODES, INSERT_LENGTH_CODES, decompose_command};

use super::window::BrotliWindow;

/// Where the command loop is inside one meta-block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmdState {
    /// At a command boundary: the next insert-and-copy symbol comes next.
    Begin,
    /// Emitting the literals of the current command.
    Insert {
        /// Literals still to decode and emit.
        remaining: usize,
        /// The copy length that follows the insert.
        copy_length: usize,
        /// Whether the command implies distance code 0 (reuse last distance).
        implicit_zero: bool,
    },
    /// Literals done; the distance of the current command comes next.
    Distance {
        /// The copy length decoded with the command symbol.
        copy_length: usize,
        /// Whether the command implies distance code 0.
        implicit_zero: bool,
    },
    /// Emitting a backward reference.
    Copy {
        /// Validated backward distance.
        distance: usize,
        /// Bytes of the match still to emit.
        remaining: usize,
    },
    /// Emitting a transformed static-dictionary word held in the stream's
    /// scratch buffer; `pos` bytes of it have already been emitted.
    DictWord {
        /// Bytes of the word already emitted.
        pos: usize,
    },
}

/// Why the command loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandStatus {
    /// The meta-block produced all `MLEN` bytes.
    Finished,
    /// Input ran out; the decoder was rewound to a resumable point.
    NeedInput,
    /// The caller's output slice is full.
    NeedOutput,
}

/// Per-meta-block state that must survive a `NeedInput` or `NeedOutput`
/// return: the prefix codes, context maps and block-switch counters parsed
/// from the prelude, plus how far through the meta-block the loop has got.
pub(crate) struct MetaBlockState {
    /// Prefix codes, context maps and block-switch state.
    pub(crate) header: Box<MetaBlockHeader>,
    /// `MLEN`: the exact number of bytes this meta-block produces.
    pub(crate) mlen: usize,
    /// Bytes of this meta-block produced so far.
    pub(crate) produced: usize,
    /// Whether this meta-block carried `ISLAST = 1`.
    pub(crate) is_last: bool,
    /// Position inside the command loop.
    pub(crate) cmd: CmdState,
}

/// The stream-wide state the command loop mutates, borrowed field by field so
/// the driver keeps ownership of all of it.
pub(crate) struct CommandCtx<'a> {
    /// The sliding window; every produced byte is appended to it.
    pub(crate) window: &'a mut BrotliWindow,
    /// Distance ring buffer and window size (persist across meta-blocks).
    pub(crate) dist: &'a mut DecoderState,
    /// Total bytes the stream has produced.
    pub(crate) total_out: &'a mut u64,
    /// Most recently produced byte (literal context `p1`).
    pub(crate) p1: &'a mut u8,
    /// Second most recently produced byte (literal context `p2`).
    pub(crate) p2: &'a mut u8,
    /// Scratch holding the transformed dictionary word being emitted.
    pub(crate) dict_buf: &'a mut Vec<u8>,
}

impl CommandCtx<'_> {
    /// Record that `bytes` (the most recent output) were produced.
    fn note_output(&mut self, bytes: &[u8]) {
        match bytes.len() {
            0 => {}
            1 => {
                *self.p2 = *self.p1;
                *self.p1 = bytes[0];
            }
            n => {
                *self.p2 = bytes[n - 2];
                *self.p1 = bytes[n - 1];
            }
        }
        *self.total_out += bytes.len() as u64;
    }
}

/// Bits that no single resumable step can exceed.
///
/// Worst cases, from RFC 7932: a block switch costs a block-type symbol
/// (<= 15 bits), a block-count symbol (<= 15) and its extra bits (<= 24) = 54;
/// on top of that an insert-and-copy command adds its own symbol (<= 15) plus
/// insert and copy extra bits (<= 24 each) = 117; a literal adds <= 15 = 69;
/// a distance adds its symbol (<= 15) plus <= 24 extra bits = 93. 128 clears
/// all three with room to spare.
///
/// While more than this many bits remain, a step cannot end in
/// [`BrotliError::UnexpectedEof`], so the loop skips saving a rollback point —
/// which is the difference between a checkpoint per literal and none at all on
/// the hot path.
const MAX_STEP_BITS: usize = 128;

/// Bits one literal can consume at most: a block switch (54, see
/// [`MAX_STEP_BITS`]) plus the literal symbol itself (<= 15).
///
/// Dividing the remaining bits by this gives a *lower bound* on how many
/// literals are safe to decode without another end-of-input check — a very
/// conservative bound (real literals cost 2-15 bits), but one that hoists the
/// check out of the hot loop entirely and is re-derived every batch.
const MAX_LITERAL_BITS: usize = 69;

/// A rewindable snapshot of one block-switch category.
#[derive(Debug, Clone, Copy)]
struct CatSave {
    btype: usize,
    prev_btype: usize,
    blen: u32,
}

impl CatSave {
    fn of(cat: &crate::decompress::BlockCategory) -> Self {
        CatSave {
            btype: cat.btype,
            prev_btype: cat.prev_btype,
            blen: cat.blen,
        }
    }

    fn apply(self, cat: &mut crate::decompress::BlockCategory) {
        cat.btype = self.btype;
        cat.prev_btype = self.prev_btype;
        cat.blen = self.blen;
    }
}

/// Run the command loop until the meta-block finishes, input runs out, or the
/// caller's `out` slice fills up.
///
/// Returns the number of bytes written into `out` and why it stopped.
pub(crate) fn run_commands(
    reader: &mut BitReader<'_>,
    meta: &mut MetaBlockState,
    ctx: &mut CommandCtx<'_>,
    out: &mut [u8],
) -> BrotliResult<(usize, CommandStatus)> {
    let mut written = 0usize;
    loop {
        if meta.produced == meta.mlen {
            return Ok((written, CommandStatus::Finished));
        }
        match meta.cmd {
            CmdState::Begin => {
                if reader.bits_available() >= MAX_STEP_BITS {
                    meta.cmd = begin_command(reader, meta)?;
                    continue;
                }
                let cursor = reader.save();
                let saved = CatSave::of(&meta.header.cat_i);
                match begin_command(reader, meta) {
                    Ok(next) => meta.cmd = next,
                    Err(BrotliError::UnexpectedEof) => {
                        reader.restore(cursor);
                        saved.apply(&mut meta.header.cat_i);
                        return Ok((written, CommandStatus::NeedInput));
                    }
                    Err(e) => return Err(e),
                }
            }
            CmdState::Insert {
                remaining,
                copy_length,
                implicit_zero,
            } => {
                if remaining == 0 {
                    meta.cmd = CmdState::Distance {
                        copy_length,
                        implicit_zero,
                    };
                    continue;
                }
                // Hot path: decode a run of literals straight into the ring's
                // linear region — one write per byte, no per-literal
                // checkpoint, no mask — then hand the caller a single bulk
                // copy. This is what keeps a push decoder's per-literal cost
                // level with the one-shot decoder, which gets to use its output
                // `Vec` as the window.
                let want = remaining.min(out.len() - written);
                if want > 0 && reader.bits_available() >= MAX_STEP_BITS {
                    // Literals go straight into the ring's linear region — one
                    // write per byte instead of two — and the caller gets a
                    // single bulk copy. This is what keeps a push decoder's
                    // per-literal cost level with the one-shot decoder, which
                    // gets to use its output `Vec` as the window.
                    //
                    // When the buffered input covers the whole run's worst case
                    // the inner loop needs no end-of-input test at all; the
                    // bound is a multiply, never a division (an integer divide
                    // per command is itself worth several cycles per byte).
                    let ample = reader.bits_available()
                        >= MAX_STEP_BITS.saturating_add(want.saturating_mul(MAX_LITERAL_BITS));
                    let header = &mut meta.header;
                    let (p1, p2) = (&mut *ctx.p1, &mut *ctx.p2);
                    let dst = ctx.window.linear_mut(want);
                    let mut done = 0usize;
                    let mut fault = None;
                    if ample {
                        for slot in dst.iter_mut() {
                            match decode_literal(reader, header, *p1, *p2) {
                                Ok(byte) => {
                                    *slot = byte;
                                    *p2 = *p1;
                                    *p1 = byte;
                                    done += 1;
                                }
                                Err(e) => {
                                    fault = Some(e);
                                    break;
                                }
                            }
                        }
                    } else {
                        let batch = dst.len();
                        while done < batch && reader.bits_available() >= MAX_STEP_BITS {
                            match decode_literal(reader, header, *p1, *p2) {
                                Ok(byte) => {
                                    dst[done] = byte;
                                    *p2 = *p1;
                                    *p1 = byte;
                                    done += 1;
                                }
                                Err(e) => {
                                    fault = Some(e);
                                    break;
                                }
                            }
                        }
                    }
                    if done > 0 {
                        out[written..written + done].copy_from_slice(&dst[..done]);
                        ctx.window.commit(done);
                        *ctx.total_out += done as u64;
                        written += done;
                        meta.produced += done;
                        meta.cmd = CmdState::Insert {
                            remaining: remaining - done,
                            copy_length,
                            implicit_zero,
                        };
                    }
                    if let Some(e) = fault {
                        // A hard format error: nothing to roll back to, and the
                        // bytes already produced stay produced.
                        return Err(e);
                    }
                    if done > 0 {
                        continue;
                    }
                }
                if written == out.len() {
                    return Ok((written, CommandStatus::NeedOutput));
                }
                let cursor = reader.save();
                let saved = CatSave::of(&meta.header.cat_l);
                match decode_literal(reader, &mut meta.header, *ctx.p1, *ctx.p2) {
                    Ok(byte) => {
                        out[written] = byte;
                        ctx.window.push(byte);
                        ctx.note_output(&out[written..written + 1]);
                        written += 1;
                        meta.produced += 1;
                        meta.cmd = CmdState::Insert {
                            remaining: remaining - 1,
                            copy_length,
                            implicit_zero,
                        };
                    }
                    Err(BrotliError::UnexpectedEof) => {
                        reader.restore(cursor);
                        saved.apply(&mut meta.header.cat_l);
                        return Ok((written, CommandStatus::NeedInput));
                    }
                    Err(e) => return Err(e),
                }
            }
            CmdState::Distance {
                copy_length,
                implicit_zero,
            } => {
                if reader.bits_available() >= MAX_STEP_BITS {
                    meta.cmd = resolve_distance(reader, meta, ctx, copy_length, implicit_zero)?;
                    continue;
                }
                let cursor = reader.save();
                let saved = CatSave::of(&meta.header.cat_d);
                let ring = *ctx.dist;
                match resolve_distance(reader, meta, ctx, copy_length, implicit_zero) {
                    Ok(next) => meta.cmd = next,
                    Err(BrotliError::UnexpectedEof) => {
                        reader.restore(cursor);
                        saved.apply(&mut meta.header.cat_d);
                        *ctx.dist = ring;
                        return Ok((written, CommandStatus::NeedInput));
                    }
                    Err(e) => return Err(e),
                }
            }
            CmdState::Copy {
                distance,
                remaining,
            } => {
                if remaining == 0 {
                    meta.cmd = CmdState::Begin;
                    continue;
                }
                if written == out.len() {
                    return Ok((written, CommandStatus::NeedOutput));
                }
                let n = ctx
                    .window
                    .copy_match(distance, remaining, &mut out[written..]);
                ctx.note_output(&out[written..written + n]);
                written += n;
                meta.produced += n;
                meta.cmd = CmdState::Copy {
                    distance,
                    remaining: remaining - n,
                };
            }
            CmdState::DictWord { pos } => {
                let total = ctx.dict_buf.len();
                if pos == total {
                    meta.cmd = CmdState::Begin;
                    continue;
                }
                if written == out.len() {
                    return Ok((written, CommandStatus::NeedOutput));
                }
                let n = (total - pos).min(out.len() - written);
                out[written..written + n].copy_from_slice(&ctx.dict_buf[pos..pos + n]);
                ctx.window.push_slice(&ctx.dict_buf[pos..pos + n]);
                ctx.note_output(&out[written..written + n]);
                written += n;
                meta.produced += n;
                meta.cmd = CmdState::DictWord { pos: pos + n };
            }
        }
    }
}

/// Decode one insert-and-copy command symbol and its extra-bit fields.
///
/// Consumes bits only; produces no output, so the caller can rewind it whole.
fn begin_command(reader: &mut BitReader<'_>, meta: &mut MetaBlockState) -> BrotliResult<CmdState> {
    meta.header.cat_i.tick(reader)?;
    let ic_tree = meta
        .header
        .ic_trees
        .get(meta.header.cat_i.btype)
        .ok_or(BrotliError::InvalidBlockType(meta.header.cat_i.btype as u8))?;
    let ic_symbol = ic_tree.decode_symbol(reader)?;
    if ic_symbol >= 704 {
        return Err(BrotliError::CorruptedData(format!(
            "invalid insert-and-copy symbol {ic_symbol}"
        )));
    }
    let (ins_code, copy_code, implicit_zero) = decompose_command(ic_symbol);
    let (ins_base, ins_extra_bits) = INSERT_LENGTH_CODES[ins_code as usize];
    let insert_length = (ins_base + reader.read_bits(ins_extra_bits as u32)?) as usize;
    let (copy_base, copy_extra_bits) = COPY_LENGTH_CODES[copy_code as usize];
    let copy_length = (copy_base + reader.read_bits(copy_extra_bits as u32)?) as usize;

    if meta.produced + insert_length > meta.mlen {
        return Err(BrotliError::CorruptedData(
            "insert length exceeds meta-block length".to_string(),
        ));
    }
    Ok(CmdState::Insert {
        remaining: insert_length,
        copy_length,
        implicit_zero,
    })
}

/// Decode one literal, honouring the literal block switch and the context map.
///
/// `p1` and `p2` are the two most recently produced bytes, the RFC 7932
/// Section 7.1 context. Taking them by value rather than through the context
/// keeps this callable from inside a mutable borrow of the window.
#[inline]
fn decode_literal(
    reader: &mut BitReader<'_>,
    header: &mut MetaBlockHeader,
    p1: u8,
    p2: u8,
) -> BrotliResult<u8> {
    header.cat_l.tick(reader)?;
    let btype = header.cat_l.btype;
    let mode = *header
        .context_modes
        .get(btype)
        .ok_or(BrotliError::InvalidBlockType(btype as u8))?;
    let context = literal_context_id(mode, p1, p2);
    let tree_idx = header.cmapl.tree_index(btype, context);
    let tree = header.literal_trees.get(tree_idx).ok_or_else(|| {
        BrotliError::InvalidContextMap(format!("literal tree {tree_idx} out of range"))
    })?;
    Ok(tree.decode_symbol(reader)? as u8)
}

/// Decode the distance of the current command and decide between a backward
/// reference and a static-dictionary word.
///
/// Consumes bits only; the dictionary word is materialised into
/// `ctx.dict_buf` after the last bit read, so this whole step is rewindable.
fn resolve_distance(
    reader: &mut BitReader<'_>,
    meta: &mut MetaBlockState,
    ctx: &mut CommandCtx<'_>,
    copy_length: usize,
    implicit_zero: bool,
) -> BrotliResult<CmdState> {
    let produced_total = usize::try_from(*ctx.total_out).unwrap_or(usize::MAX);
    let max_distance = ctx.dist.window_size.min(produced_total);

    let (distance, is_code_zero) = if implicit_zero {
        (ctx.dist.last_distance(), true)
    } else {
        meta.header.cat_d.tick(reader)?;
        let btype = meta.header.cat_d.btype;
        let context = distance_context_id(copy_length);
        let tree_idx = meta.header.cmapd.tree_index(btype, context);
        let tree = meta.header.distance_trees.get(tree_idx).ok_or_else(|| {
            BrotliError::InvalidContextMap(format!("distance tree {tree_idx} out of range"))
        })?;
        let dsym = tree.decode_symbol(reader)? as u32;
        decode_distance(
            reader,
            dsym,
            ctx.dist,
            meta.header.ndirect,
            meta.header.npostfix,
            meta.header.postfix_mask,
        )?
    };

    if distance <= max_distance {
        if !is_code_zero {
            ctx.dist.push_distance(distance);
        }
        if meta.produced + copy_length > meta.mlen {
            return Err(BrotliError::CorruptedData(
                "copy length exceeds meta-block length".to_string(),
            ));
        }
        return Ok(CmdState::Copy {
            distance,
            remaining: copy_length,
        });
    }

    // Static dictionary reference (RFC 7932 Section 8). Never pushed onto the
    // distance ring buffer.
    if !(dictionary::MIN_DICTIONARY_WORD_LENGTH..=dictionary::MAX_DICTIONARY_WORD_LENGTH)
        .contains(&copy_length)
    {
        return Err(BrotliError::InvalidDistance {
            distance,
            max_distance,
        });
    }
    let word_id = (distance - max_distance - 1) as u64;
    let ndbits = dictionary::NDBITS[copy_length] as u64;
    let index = (word_id & ((1 << ndbits) - 1)) as u32;
    let transform_id = (word_id >> ndbits) as usize;
    if transform_id >= dictionary::NUM_TRANSFORMS {
        return Err(BrotliError::InvalidDistance {
            distance,
            max_distance,
        });
    }
    let word = dictionary::lookup_word(copy_length, index)?;
    ctx.dict_buf.clear();
    dictionary::apply_transform_to(word, transform_id, ctx.dict_buf)?;
    if meta.produced + ctx.dict_buf.len() > meta.mlen {
        return Err(BrotliError::CorruptedData(
            "dictionary word exceeds meta-block length".to_string(),
        ));
    }
    Ok(CmdState::DictWord { pos: 0 })
}
