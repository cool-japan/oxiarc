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
use crate::shared_dict;
use crate::tables::{COPY_LENGTH_CODES, INSERT_LENGTH_CODES, decompose_command};

use super::window::{BrotliWindow, SHORT_MATCH};

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
    /// Emitting a copy out of the attached *shared* dictionary
    /// ([`crate::shared_dict`]).
    SharedCopy {
        /// Offset of the next source byte inside the shared dictionary.
        offset: usize,
        /// Bytes still to take from the dictionary.
        remaining: usize,
        /// The command's backward distance, needed for the `tail`.
        distance: usize,
        /// Bytes of the copy that run past the end of the dictionary and
        /// continue in the produced output.
        tail: usize,
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
    /// The attached shared (custom LZ77) dictionary; empty when none.
    pub(crate) shared: &'a [u8],
    /// How many commands the fast path has run.
    ///
    /// A fast path that silently never engages is indistinguishable from a
    /// correct decoder by every output-comparing test in the suite, so the
    /// count is observable and asserted. It is per-stream and `cfg(test)`, so
    /// it neither costs a release build anything nor can be inflated by
    /// another test running in the same process.
    #[cfg(test)]
    pub(crate) fast_path_commands: &'a mut u64,
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

/// Bits a whole fast-path command can consume beyond its literals: the
/// insert-and-copy step ([`MAX_STEP_BITS`]) plus the distance step
/// ([`MAX_STEP_BITS`] again).
const FAST_COMMAND_BITS: usize = MAX_STEP_BITS * 2;

/// Run whole commands with no per-step checkpointing while both the buffered
/// input and the caller's output slice can certainly hold one.
///
/// The resumable loop below costs about six state-machine round trips per
/// command — every one writing a [`CmdState`] back to memory and re-entering
/// the dispatch — which the one-shot decoder does not pay at all. On
/// literal-dense data, where commands are short, that bookkeeping and not the
/// window traffic is what separates the two decoders (measured: forcing the
/// window to a cache-resident 64 KiB, and separately writing literals to a
/// stack buffer instead of the window, each changed the time by under 3 %).
///
/// Every guard is checked *before* a byte is produced, and the bit cursor and
/// block-switch state are rewound if a command turns out not to fit, so
/// falling back to the resumable path is always sound.
///
/// Returns the bytes written into `out`.
fn fast_commands(
    reader: &mut BitReader<'_>,
    meta: &mut MetaBlockState,
    ctx: &mut CommandCtx<'_>,
    out: &mut [u8],
    written: &mut usize,
) -> BrotliResult<()> {
    debug_assert_eq!(meta.cmd, CmdState::Begin, "fast path entered mid-command");
    while meta.produced < meta.mlen {
        if reader.bits_available() < FAST_COMMAND_BITS || *written == out.len() {
            break;
        }
        let cursor = reader.save();
        let saved_i = CatSave::of(&meta.header.cat_i);
        let CmdState::Insert {
            remaining: insert_length,
            copy_length,
            implicit_zero,
        } = begin_command(reader, meta)?
        else {
            unreachable!("begin_command always yields CmdState::Insert")
        };

        // The whole command must fit in what is left of `out`, or this command
        // belongs to the resumable path — that is the only reason to rewind,
        // because it is the only guard that must be decided before a byte is
        // produced.
        let space = out.len() - *written;
        if insert_length.saturating_add(copy_length) > space {
            reader.restore(cursor);
            saved_i.apply(&mut meta.header.cat_i);
            break;
        }

        // Literals, straight into the caller's slice, then mirrored into the
        // ring in one bulk copy. `ample` says the buffered input covers the
        // whole run's worst case, in which case the inner loop needs no
        // end-of-input test at all; when it does not, the run simply stops
        // early and the resumable path picks up the remainder by count.
        //
        // Requiring `ample` to *enter* the fast path — which is what this used
        // to do — silently disabled it for every stream smaller than
        // `insert_length * 69` bits: a 35 KB repetitive payload compresses to
        // ~150 bytes, so its 86-literal insert "needed" 774 bytes of input
        // that will never exist, and the fast path ran zero times.
        let ample = reader.bits_available()
            >= FAST_COMMAND_BITS.saturating_add(insert_length.saturating_mul(MAX_LITERAL_BITS));
        let (done, fault) = {
            let header = &mut *meta.header;
            let at = *written;
            decode_literal_run(
                reader,
                header,
                ctx.p1,
                ctx.p2,
                &mut out[at..at + insert_length],
                ample,
            )
        };
        if done > 0 {
            *ctx.total_out += done as u64;
            *written += done;
            meta.produced += done;
        }
        if let Some(e) = fault {
            meta.cmd = CmdState::Insert {
                remaining: insert_length - done,
                copy_length,
                implicit_zero,
            };
            return Err(e);
        }
        if done < insert_length {
            // The buffered input ran out mid-insert. Everything produced stays
            // produced; the resumable path resumes by count and reports
            // `NeedInput` with the bit cursor exactly where this run left it.
            meta.cmd = CmdState::Insert {
                remaining: insert_length - done,
                copy_length,
                implicit_zero,
            };
            break;
        }

        // An insert that completes the meta-block makes the copy part of the
        // command irrelevant (Section 9.3). `meta.cmd` is already `Begin` — see
        // the note at the end of the loop.
        if meta.produced == meta.mlen {
            break;
        }

        // The distance step is one rewindable step, so it needs its own worst
        // case buffered; otherwise hand it to the resumable path, which knows
        // how to roll it back.
        if reader.bits_available() < MAX_STEP_BITS {
            meta.cmd = CmdState::Distance {
                copy_length,
                implicit_zero,
            };
            break;
        }

        match resolve_distance(reader, meta, ctx, copy_length, implicit_zero)? {
            CmdState::Copy {
                distance,
                remaining,
            } => {
                let at = *written;
                let n = copy_into_pending(ctx.window, distance, remaining, out, at);
                ctx.note_output(&out[at..at + n]);
                *written += n;
                meta.produced += n;
                if n < remaining {
                    // Cannot happen while the space guard holds, but leaving
                    // the state correct costs nothing and keeps the fallback
                    // exact if the guard is ever loosened.
                    meta.cmd = CmdState::Copy {
                        distance,
                        remaining: remaining - n,
                    };
                    break;
                }
            }
            other => {
                // A shared- or static-dictionary word: rare, and its emission
                // is the resumable path's job.
                meta.cmd = other;
                break;
            }
        }
        // `meta.cmd` is deliberately *not* written here. This loop is entered
        // at a command boundary, so it is already `CmdState::Begin`, and every
        // exit that needs a different value sets one; `CmdState` is 40 bytes,
        // and storing it once per command is measurable on command-dense data.
        #[cfg(test)]
        {
            *ctx.fast_path_commands += 1;
        }
    }
    Ok(())
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
    let outcome = command_loop(reader, meta, ctx, out, &mut written);
    // Mirror everything this call produced into the window, in one bulk copy.
    //
    // Nothing inside the loop above writes to the ring: literals and matches go
    // to `out` only, and `copy_into_pending` resolves a match's source across
    // the ring/`out` boundary. That is what removes the second write per
    // produced byte — the cost that made a bounded push decoder slower than the
    // one-shot one on match-dense data. It also runs on the error path, so the
    // window is consistent whatever happens.
    ctx.window.push_slice(&out[..written]);
    outcome.map(|status| (written, status))
}

/// The body of [`run_commands`], with a single exit so its caller can mirror
/// the produced bytes into the window exactly once.
fn command_loop(
    reader: &mut BitReader<'_>,
    meta: &mut MetaBlockState,
    ctx: &mut CommandCtx<'_>,
    out: &mut [u8],
    written_out: &mut usize,
) -> BrotliResult<CommandStatus> {
    let written = written_out;
    loop {
        if meta.produced == meta.mlen {
            return Ok(CommandStatus::Finished);
        }
        match meta.cmd {
            CmdState::Begin => {
                // Try to run whole commands with no per-step bookkeeping. This
                // has to be attempted at *every* command boundary, not once per
                // call: a `NeedOutput` return leaves the loop mid-command, and
                // entering the fast path only at the top of `run_commands`
                // reached 1.6 % of commands (measured) instead of 98 %.
                if *written < out.len() && reader.bits_available() >= FAST_COMMAND_BITS {
                    let before = *written;
                    fast_commands(reader, meta, ctx, out, written)?;
                    if *written > before || meta.cmd != CmdState::Begin {
                        continue;
                    }
                }
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
                        return Ok(CommandStatus::NeedInput);
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
                // Hot path: decode a run of literals straight into the
                // caller's slice — one write per byte, no per-literal
                // checkpoint, no ring traffic at all. The window is caught up
                // once, in bulk, when this call ends.
                let want = remaining.min(out.len() - *written);
                if want > 0 && reader.bits_available() >= MAX_STEP_BITS {
                    // When the buffered input covers the whole run's worst case
                    // the inner loop needs no end-of-input test at all; the
                    // bound is a multiply, never a division (an integer divide
                    // per command is itself worth several cycles per byte).
                    let ample = reader.bits_available()
                        >= MAX_STEP_BITS.saturating_add(want.saturating_mul(MAX_LITERAL_BITS));
                    let header = &mut *meta.header;
                    let at = *written;
                    let (done, fault) = decode_literal_run(
                        reader,
                        header,
                        ctx.p1,
                        ctx.p2,
                        &mut out[at..at + want],
                        ample,
                    );
                    if done > 0 {
                        *ctx.total_out += done as u64;
                        *written += done;
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
                if *written == out.len() {
                    return Ok(CommandStatus::NeedOutput);
                }
                let cursor = reader.save();
                let saved = CatSave::of(&meta.header.cat_l);
                match decode_literal(reader, &mut meta.header, *ctx.p1, *ctx.p2) {
                    Ok(byte) => {
                        let at = *written;
                        out[at] = byte;
                        ctx.note_output(&out[at..at + 1]);
                        *written += 1;
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
                        return Ok(CommandStatus::NeedInput);
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
                        return Ok(CommandStatus::NeedInput);
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
                if *written == out.len() {
                    return Ok(CommandStatus::NeedOutput);
                }
                let at = *written;
                let n = copy_into_pending(ctx.window, distance, remaining, out, at);
                ctx.note_output(&out[at..at + n]);
                *written += n;
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
                if *written == out.len() {
                    return Ok(CommandStatus::NeedOutput);
                }
                let at = *written;
                let n = (total - pos).min(out.len() - at);
                out[at..at + n].copy_from_slice(&ctx.dict_buf[pos..pos + n]);
                ctx.note_output(&out[at..at + n]);
                *written += n;
                meta.produced += n;
                meta.cmd = CmdState::DictWord { pos: pos + n };
            }
            CmdState::SharedCopy {
                offset,
                remaining,
                distance,
                tail,
            } => {
                if remaining == 0 {
                    // A copy longer than the dictionary tail continues in the
                    // produced output at the same distance: the dictionary sits
                    // immediately before the reachable window. The ring holds
                    // `min(1 << WBITS, produced)` bytes, exactly the reach the
                    // one-shot decoder has into its output `Vec`, so both
                    // decoders accept and reject the same straddling copies.
                    //
                    // The reachable history is the ring plus everything
                    // produced during this call and not yet mirrored, hence
                    // the `+ *written`; that sum does not depend on where the
                    // call boundary happens to fall.
                    let reach = ctx.window.filled() + *written;
                    meta.cmd = if tail == 0 {
                        CmdState::Begin
                    } else if distance <= reach {
                        CmdState::Copy {
                            distance,
                            remaining: tail,
                        }
                    } else {
                        return Err(BrotliError::InvalidDistance {
                            distance,
                            max_distance: reach,
                        });
                    };
                    continue;
                }
                if *written == out.len() {
                    return Ok(CommandStatus::NeedOutput);
                }
                let at = *written;
                let n = remaining.min(out.len() - at);
                let src = &ctx.shared[offset..offset + n];
                out[at..at + n].copy_from_slice(src);
                ctx.note_output(&out[at..at + n]);
                *written += n;
                meta.produced += n;
                meta.cmd = CmdState::SharedCopy {
                    offset: offset + n,
                    remaining: remaining - n,
                    distance,
                    tail,
                };
            }
        }
    }
}

/// Copy an LZ77 match into `out`, resolving its source across the boundary
/// between the mirrored window and the bytes produced during this call.
///
/// The window ring holds everything produced *before* this `run_commands`
/// call; `out[..at]` holds everything produced during it, not yet mirrored.
/// Together they are the decoder's history, so a match at `distance` reads its
/// first `distance - at` bytes (if any) out of the ring and the rest out of
/// `out` itself — which is exactly what the one-shot decoder does, one load and
/// one store per byte, with no ring write at all.
///
/// Deferring the mirror to one bulk `push_slice` per call is what removes the
/// push decoder's second write per produced byte. The caller must have
/// validated `distance <= window.filled() + at`.
///
/// Returns the number of bytes written, `min(count, out.len() - at)`.
fn copy_into_pending(
    window: &BrotliWindow,
    distance: usize,
    count: usize,
    out: &mut [u8],
    at: usize,
) -> usize {
    let total = count.min(out.len() - at);
    if total == 0 {
        return 0;
    }
    // Bytes whose source predates this call come out of the ring. `from_ring`
    // is never more than `distance - at`, so the read stops at the ring's write
    // cursor and can never reach bytes that do not exist yet.
    let from_ring = total.min(distance.saturating_sub(at));
    if from_ring > 0 {
        window.read_back(distance - at, &mut out[at..at + from_ring]);
    }
    let start = at + from_ring;
    let end = at + total;
    if start == end {
        return total;
    }
    // `start >= distance` here: either `distance <= at <= start`, or the ring
    // leg above ran to `at + (distance - at) == distance`.
    debug_assert!(start >= distance);
    let n = end - start;
    if n <= SHORT_MATCH {
        for j in start..end {
            out[j] = out[j - distance];
        }
        return total;
    }
    // Expand by doubling. Each bulk copy moves `out[src..]` forward by
    // `distance + filled`; for that to reproduce the period, `filled` must stay
    // a **multiple of `distance`** — a shift of `distance + filled` is then a
    // whole number of periods. `filled` starts at zero or at a whole number of
    // periods and each run is itself `distance + filled` long, so the invariant
    // holds by induction; only the last run may be shorter, and it ends the
    // loop. (Seeding with a fixed 32 bytes instead is the bug this comment
    // exists to prevent: it silently mis-decodes every period that does not
    // divide 32.)
    let mut filled = 0usize;
    if distance < SHORT_MATCH {
        // Materialise whole periods byte-wise first, so the doubling does not
        // start with a 1-, 2- and 4-byte `memmove` call.
        filled = (SHORT_MATCH.div_ceil(distance) * distance).min(n);
        for j in start..start + filled {
            out[j] = out[j - distance];
        }
    }
    let src = start - distance;
    while filled < n {
        let run = (n - filled).min(distance + filled);
        out.copy_within(src..src + run, start + filled);
        filled += run;
    }
    total
}

/// Decode a run of literals straight into `dst`, updating the two context
/// bytes as it goes.
///
/// Returns how many literals reached `dst` and the error that stopped the run,
/// if any. Nothing else is touched: the caller mirrors `dst[..done]` into the
/// window and advances its own counters, so this function is shared by the
/// fast command path and the resumable one and they cannot drift apart.
///
/// `ample` means the caller has already proved that `dst.len()` literals fit in
/// the buffered input's worst case, so the end-of-input test is hoisted out of
/// the loop entirely.
///
/// Writing to the caller's slice rather than to the window's linear region is
/// deliberate: the ring is up to 4 MiB and cold, the caller's buffer is tens of
/// kilobytes and hot, and one bulk `push_slice` into the ring afterwards moves
/// the same bytes as a streaming copy instead of a scattered store per byte.
///
/// The header is borrowed field by field. The one-shot decoder destructures its
/// header into locals before its command loop; going through
/// `Box<MetaBlockHeader>` on every literal instead costs a reload per field per
/// byte, which is what this hoist removes.
#[inline]
fn decode_literal_run(
    reader: &mut BitReader<'_>,
    header: &mut MetaBlockHeader,
    p1: &mut u8,
    p2: &mut u8,
    dst: &mut [u8],
    ample: bool,
) -> (usize, Option<BrotliError>) {
    let cat_l = &mut header.cat_l;
    let modes = &header.context_modes[..];
    let cmapl = &header.cmapl;
    let trees = &header.literal_trees[..];
    let (mut p1v, mut p2v) = (*p1, *p2);
    let mut done = 0usize;
    let mut fault = None;
    while done < dst.len() {
        if !ample && reader.bits_available() < MAX_STEP_BITS {
            break;
        }
        match decode_one_literal(reader, cat_l, modes, cmapl, trees, p1v, p2v) {
            Ok(byte) => {
                dst[done] = byte;
                p2v = p1v;
                p1v = byte;
                done += 1;
            }
            Err(e) => {
                fault = Some(e);
                break;
            }
        }
    }
    *p1 = p1v;
    *p2 = p2v;
    (done, fault)
}

/// Decode one insert-and-copy command symbol and its extra-bit fields.
///
/// Consumes bits only; produces no output, so the caller can rewind it whole.
#[inline(always)]
fn begin_command(reader: &mut BitReader<'_>, meta: &mut MetaBlockState) -> BrotliResult<CmdState> {
    meta.header.cat_i.tick(reader)?;
    let Some(ic_tree) = meta.header.ic_trees.get(meta.header.cat_i.btype) else {
        return Err(BrotliError::InvalidBlockType(meta.header.cat_i.btype as u8));
    };
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
/// [`decode_literal`] with the header's fields already borrowed apart.
///
/// The hot literal loop calls this so that `cat_l`, the context modes, the
/// context map and the prefix codes are addressed directly rather than through
/// the `Box<MetaBlockHeader>` on every byte.
#[inline(always)]
fn decode_one_literal(
    reader: &mut BitReader<'_>,
    cat_l: &mut crate::decompress::BlockCategory,
    modes: &[crate::context::ContextMode],
    cmapl: &crate::context::ContextMap,
    trees: &[crate::huffman::HuffmanTree],
    p1: u8,
    p2: u8,
) -> BrotliResult<u8> {
    cat_l.tick(reader)?;
    let btype = cat_l.btype;
    let Some(&mode) = modes.get(btype) else {
        return Err(BrotliError::InvalidBlockType(btype as u8));
    };
    let context = literal_context_id(mode, p1, p2);
    let tree_idx = cmapl.tree_index(btype, context);
    let tree = trees.get(tree_idx).ok_or_else(|| {
        BrotliError::InvalidContextMap(format!("literal tree {tree_idx} out of range"))
    })?;
    Ok(tree.decode_symbol(reader)? as u8)
}

#[inline]
fn decode_literal(
    reader: &mut BitReader<'_>,
    header: &mut MetaBlockHeader,
    p1: u8,
    p2: u8,
) -> BrotliResult<u8> {
    header.cat_l.tick(reader)?;
    let btype = header.cat_l.btype;
    let Some(&mode) = header.context_modes.get(btype) else {
        return Err(BrotliError::InvalidBlockType(btype as u8));
    };
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
#[inline(always)]
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

    let source = shared_dict::classify_distance(distance, max_distance, ctx.shared.len());

    if let shared_dict::DistanceSource::Output = source {
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

    if let shared_dict::DistanceSource::Shared { offset, available } = source {
        // A shared-dictionary reference is an ordinary backward reference into
        // the extended history, so — unlike a static dictionary word — it does
        // go onto the distance ring.
        if !is_code_zero {
            ctx.dist.push_distance(distance);
        }
        if meta.produced + copy_length > meta.mlen {
            return Err(BrotliError::CorruptedData(
                "copy length exceeds meta-block length".to_string(),
            ));
        }
        let from_dict = copy_length.min(available);
        return Ok(CmdState::SharedCopy {
            offset,
            remaining: from_dict,
            distance,
            tail: copy_length - from_dict,
        });
    }

    // Static dictionary reference (RFC 7932 Section 8). Never pushed onto the
    // distance ring buffer.
    let shared_dict::DistanceSource::Static { word_id } = source else {
        unreachable!("classify_distance returns exactly three variants")
    };
    if !(dictionary::MIN_DICTIONARY_WORD_LENGTH..=dictionary::MAX_DICTIONARY_WORD_LENGTH)
        .contains(&copy_length)
    {
        return Err(BrotliError::InvalidDistance {
            distance,
            max_distance,
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference: LZ77 as the specification writes it, one byte at a time
    /// over a single flat history. The whole point of [`copy_into_pending`] is
    /// that a history split between a ring and the caller's slice, at an
    /// arbitrary split point, still produces exactly these bytes.
    fn reference(history: &mut Vec<u8>, distance: usize, count: usize) {
        for _ in 0..count {
            let byte = history[history.len() - distance];
            history.push(byte);
        }
    }

    /// Drive `copy_into_pending` with the history split at every interesting
    /// place: entirely in the ring, entirely in the caller's slice, and
    /// straddling the two.
    fn check(mirrored: usize, pending: usize, distance: usize, count: usize) {
        let total = mirrored + pending;
        assert!(distance <= total, "test asks for an invalid distance");
        let history: Vec<u8> = (0..total as u32)
            .map(|i| (i.wrapping_mul(83).wrapping_add(7) % 251) as u8)
            .collect();

        // The ring holds the first `mirrored` bytes; `out[..pending]` holds the
        // rest, and the copy lands at `out[pending..]`.
        let mut window = BrotliWindow::with_target((total.max(count) * 4).next_power_of_two());
        window.push_slice(&history[..mirrored]);
        let mut out = vec![0u8; pending + count];
        out[..pending].copy_from_slice(&history[mirrored..]);

        let n = copy_into_pending(&window, distance, count, &mut out, pending);
        assert_eq!(
            n, count,
            "short copy: mirrored {mirrored} pending {pending}"
        );

        let mut expected = history.clone();
        reference(&mut expected, distance, count);
        assert_eq!(
            &out[pending..],
            &expected[total..],
            "mirrored {mirrored} pending {pending} distance {distance} count {count}"
        );
    }

    #[test]
    fn a_copy_agrees_with_flat_lz77_at_every_split() {
        for &distance in &[1usize, 2, 3, 5, 7, 16, 31, 32, 33, 64, 100, 255] {
            for &count in &[1usize, 2, 31, 32, 33, 64, 129, 300, 1000] {
                // The split walks from "all history is in the ring" to "all of
                // it is in the caller's slice", including both ends.
                for &pending in &[0usize, 1, distance / 2, distance, distance + 1, 300] {
                    let total = (distance + pending).max(distance);
                    let mirrored = total - pending;
                    if mirrored + pending < distance {
                        continue;
                    }
                    check(mirrored, pending, distance, count);
                }
            }
        }
    }

    #[test]
    fn a_long_periodic_copy_is_exact_for_every_small_period() {
        // The doubling expansion shifts by `distance + filled`; if `filled` is
        // ever not a whole number of periods the output is subtly wrong, and
        // only a period that does not divide the seed reveals it.
        for distance in 1..=70usize {
            for count in [distance, distance + 1, 71, 1024, 5000] {
                check(distance, 0, distance, count);
                check(0, distance, distance, count);
                check(distance / 2, distance - distance / 2, distance, count);
            }
        }
    }

    #[test]
    fn a_copy_is_truncated_to_the_space_available() {
        let mut window = BrotliWindow::with_target(1 << 12);
        window.push_slice(b"0123456789");
        let mut out = vec![0u8; 4];
        assert_eq!(copy_into_pending(&window, 10, 10, &mut out, 0), 4);
        assert_eq!(&out, b"0123");
        // The resumable path asks for the rest with the first four bytes now
        // part of the pending region.
        let mut out2 = vec![0u8; 4 + 6];
        out2[..4].copy_from_slice(b"0123");
        assert_eq!(copy_into_pending(&window, 10, 6, &mut out2, 4), 6);
        assert_eq!(&out2, b"0123456789");
    }
}
