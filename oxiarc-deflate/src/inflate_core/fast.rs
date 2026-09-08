//! The fast symbol loop: the path a decode spends essentially all of its
//! time in.
//!
//! Split out of the parent module because it is the *other* half of the
//! decoder's seam. [`super::step_state`] is the resumable authority — it
//! computes what it needs, checks that it is there, and can stop anywhere —
//! while this loop is entered only when input and output are both plentiful
//! and therefore needs no per-step checks at all. Keeping them apart also
//! keeps the register allocator apart (see the note on
//! [`symbols`]'s `#[inline(never)]`).
//!
//! A child module rather than a sibling, so it can reach the parent's
//! private decoder state directly.

use oxiarc_core::error::{OxiArcError, Result};

use super::{InflateCore, InflateState, clamp_usize};
use crate::decode_table::{self, DecodeTable};
use crate::sink::{FastRegion, InflateSink};

/// One bulk refill loads 8 bytes; a full literal-plus-match iteration
/// consumes at most 48 bits, so 8 bytes of lookahead always covers the
/// top-of-loop refill.
const FAST_INPUT_MARGIN: usize = 8;

/// Output room the fast loop requires to start an iteration: the longest
/// DEFLATE match, so no step inside it has to check its own room.
///
/// Shrinking this to the literal batch and letting a match that does not
/// fit publish `CopyMatch` state for the careful path *does* work and does
/// keep the fast loop running to the last few bytes of every call — worth
/// roughly 3 % on a push decoder fed 64 KiB slices. It is deliberately not
/// done: running the fast loop further into the output also changes where
/// its bulk refills fall, and with them how many trailing bytes end up
/// absorbed in the accumulator rather than left in the caller's slice —
/// which `stream::tests::reset_clears_the_accumulator_but_next_member_keeps_it`
/// pins as observable behaviour (`buffered_bits`, `take_buffered_byte`,
/// `progress.consumed`). Not worth 3 %.
const FAST_OUTPUT_MARGIN: usize = 258;

/// Longest DEFLATE Huffman code (RFC 1951 §3.2.7).
const MAX_CODE_BITS: u8 = 15;

/// Bits the accumulator is guaranteed to hold at the top of every fast-loop
/// iteration, checked once per iteration.
///
/// `BitCache::refill_bulk` tops a cache holding at most 55 bits up to
/// 56..=63 whenever 8 input bytes are available, which the loop guard
/// requires. 56 covers the widest iteration: a 15-bit length code, its 5
/// extra bits, a 15-bit distance code and its 13 extra bits (48), or three
/// 15-bit literals read out of one 32-bit peek.
const FAST_MIN_BITS: u8 = 56;

/// Literals decoded from one 32-bit peek before the accumulator is topped
/// up again.
///
/// Three fit: a literal code is at most [`MAX_CODE_BITS`] bits, and the
/// loop only decodes another one while fewer than `32 - MAX_CODE_BITS`
/// bits of the peek have been used, so every code is read from real
/// accumulator bits. Real streams spend most of their symbols here (an
/// image row is essentially all literals), and this amortises the loop
/// guards, the refill and the `consume` across the batch.
///
/// The batch stops as soon as a symbol is *not* a literal, and that test is
/// deliberately a branch rather than a branchless commit: a fully
/// branchless pair (always decode the second entry, select the stored byte
/// and the advance) was measured 7 % *slower* on image rows — 278-280 MB/s
/// against 298-301 — because it puts a second dependent table load on the
/// critical path of every match as well.
const FAST_LITERAL_BATCH: usize = 3;

/// Why the fast loop stopped.
enum FastExit {
    /// Ran out of the input or output margin the fast loop requires.
    Boundary,
    /// A code did not resolve from the accumulator; hand over to the
    /// careful path, which reports the error precisely.
    Careful,
    /// End-of-block symbol consumed.
    EndOfBlock,
    /// A length was decoded but its distance symbol did not resolve: the
    /// careful path must decode the distance.
    ///
    /// There is deliberately no "distance extra bits did not resolve"
    /// variant: with [`FAST_MIN_BITS`] held at the top of the iteration, a
    /// distance symbol that resolves has its extra bits in hand too.
    PendingDistance(u16),
    /// A literal/length code above 285.
    BadLitLen(u16),
    /// A distance code of 30 or 31.
    BadDist(u16),
    /// A sink rejected a write (bad distance, oversized copy).
    Sink(OxiArcError),
}

/// The fast loop: one packed-table lookup per symbol, the bit accumulator
/// and the output cursor in locals, and one bulk refill per iteration.
///
/// Entered only while at least [`FAST_INPUT_MARGIN`] input bytes and
/// [`FAST_OUTPUT_MARGIN`] output bytes are available, which is what makes
/// every refill and every write inside it unconditionally safe:
///
/// * with 8 input bytes in hand `BitCache::refill_bulk` always brings the
///   accumulator to at least 56 bits, and a literal-plus-match iteration
///   consumes at most 48, so no inner refill is ever needed;
/// * with 258 bytes of room the longest possible match fits, so the only
///   output check is the one at the top of the loop.
///
/// `LIMITED` is a const generic so the unlimited instantiation carries no
/// budget arithmetic at all.
///
/// Returns `Ok(true)` when it made progress the caller should re-drive from
/// (end of block, or a hand-over with pending state published), `Ok(false)`
/// when the careful path must take over.
/// `#[inline(never)]`: the loop must not share a register allocation with
/// [`step_state`], which is an order of magnitude larger. Inlined into
/// `drive` together with the careful path, the bit accumulator, the output
/// cursor and the input cursor all spill to the stack — measured as one
/// `str` of the input cursor *per refill* and a stack reload of the input
/// slice pointer on every iteration. One call per `inflate()` call (each
/// decoding thousands of symbols) is free by comparison.
#[inline(never)]
pub(super) fn symbols<S: InflateSink, const LIMITED: bool>(
    core: &mut InflateCore,
    litlen: &DecodeTable,
    dist: &DecodeTable,
    sink: &mut S,
    input: &[u8],
    in_pos: &mut usize,
) -> Result<bool> {
    let mut allowed = usize::MAX;
    if LIMITED {
        allowed = clamp_usize(core.allowance(core.total_in + *in_pos as u64));
        if allowed < FAST_OUTPUT_MARGIN {
            return Ok(false);
        }
    }
    // The input cursor lives in a local for the whole loop: behind the
    // `&mut` it is a store per refill.
    let mut ip = *in_pos;
    if input.len() - ip < FAST_INPUT_MARGIN {
        return Ok(false);
    }
    let Some(region) = sink.fast_region() else {
        return Ok(false);
    };
    let FastRegion {
        dst,
        pos: start_pos,
        history,
    } = region;
    if dst.len() - start_pos < FAST_OUTPUT_MARGIN {
        return Ok(false);
    }

    let mut pos = start_pos;
    let mut cache = core.cache;
    let hist_len = history.len();

    let exit = loop {
        if input.len() - ip < FAST_INPUT_MARGIN || dst.len() - pos < FAST_OUTPUT_MARGIN {
            break FastExit::Boundary;
        }
        if LIMITED && allowed < FAST_OUTPUT_MARGIN {
            break FastExit::Boundary;
        }

        // One bulk refill per iteration. With [`FAST_INPUT_MARGIN`] bytes in
        // hand `refill_bulk` always brings the accumulator to at least
        // [`FAST_MIN_BITS`], and the check below turns that into a single
        // invariant the rest of the iteration can rely on: a literal costs
        // at most 15 bits, three of them at most 32, and a full
        // length-plus-distance pair at most 48, so **no inner step needs an
        // availability check of its own**. The branch is never taken (the
        // refill's own guard is `len > 55`); it exists so the invariant is
        // structural instead of a chain of arithmetic arguments.
        if let Some(rest) = input.get(ip..) {
            ip += cache.refill_bulk(rest);
        }
        if cache.available() < FAST_MIN_BITS {
            break FastExit::Careful;
        }
        let bits = cache.peek_bits(32);

        // ── literal / length symbol ─────────────────────────────────────
        let entry = litlen.entry(bits);
        let code_bits = decode_table::entry_len(entry);

        if entry & decode_table::LITERAL != 0 {
            let Some(slots) = dst
                .get_mut(pos..)
                .and_then(<[u8]>::first_chunk_mut::<FAST_LITERAL_BATCH>)
            else {
                break FastExit::Careful;
            };
            slots[0] = decode_table::entry_payload(entry) as u8;
            let mut used = code_bits;
            let mut count = 1usize;
            while count < FAST_LITERAL_BATCH && used <= 32 - MAX_CODE_BITS {
                let next = litlen.entry(bits >> used);
                let next_bits = decode_table::entry_len(next);
                if next & decode_table::LITERAL == 0 || next_bits == 0 {
                    break;
                }
                let byte = decode_table::entry_payload(next) as u8;
                match count {
                    1 => slots[1] = byte,
                    _ => slots[2] = byte,
                }
                used += next_bits;
                count += 1;
            }
            pos += count;
            cache.consume(used);
            if LIMITED {
                allowed -= count;
            }
            continue;
        }

        if entry & decode_table::END_OF_BLOCK != 0 {
            cache.consume(code_bits);
            break FastExit::EndOfBlock;
        }
        if entry & decode_table::INVALID != 0 {
            if code_bits == 0 {
                // A slot no code reaches (an incomplete literal/length
                // code): the careful path reports it precisely, and nothing
                // has been consumed, so it re-decodes from these bits.
                break FastExit::Careful;
            }
            cache.consume(code_bits);
            break FastExit::BadLitLen(decode_table::entry_payload(entry) as u16);
        }

        // ── length: base and extra-bit count come from the same entry ───
        let extra_bits = decode_table::entry_extra(entry);
        let length = decode_table::entry_payload(entry) as usize
            + ((bits >> code_bits) & low_mask(extra_bits)) as usize;
        cache.consume(code_bits + extra_bits);

        // ── distance ────────────────────────────────────────────────────
        // A second peek: a 15-bit length code with 5 extra bits followed by
        // a 15-bit distance code with 13 extra bits is 48 bits, more than
        // one 32-bit peek can hold. After the consume above the accumulator
        // still holds at least 36 bits, and this needs at most 28.
        //
        // Folding both into a single peek where they happen to fit (usually
        // they do) was measured *slower* — 8 % on PNG-filtered rows, 3 % on
        // long-match data — because the extra branch and the wide fallback
        // it needs cost more than the peek and `consume` they save.
        let dbits = cache.peek_bits(32);
        let dist_entry = dist.entry(dbits);
        let dist_bits = decode_table::entry_len(dist_entry);
        if dist_entry & decode_table::INVALID != 0 {
            if dist_bits == 0 {
                // A slot no distance code reaches. The length code is
                // already consumed, so the careful path must resume at the
                // distance symbol — where it reports the error precisely.
                break FastExit::PendingDistance(length as u16);
            }
            cache.consume(dist_bits);
            break FastExit::BadDist(decode_table::entry_payload(dist_entry) as u16);
        }
        let dist_extra = decode_table::entry_extra(dist_entry);
        let distance = decode_table::entry_payload(dist_entry) as usize
            + ((dbits >> dist_bits) & low_mask(dist_extra)) as usize;
        cache.consume(dist_bits + dist_extra);

        // ── match copy ──────────────────────────────────────────────────
        let history_total = pos + hist_len;
        if distance > history_total {
            break FastExit::Sink(OxiArcError::invalid_distance(distance, history_total));
        }
        if distance <= pos {
            pos = copy_match_words(dst, pos, distance, length);
        } else {
            match copy_straddling_match(dst, history, pos, distance, length) {
                Some(next) => pos = next,
                None => {
                    break FastExit::Sink(OxiArcError::invalid_distance(distance, history_total));
                }
            }
        }
        if LIMITED {
            allowed -= length;
        }
    };

    *in_pos = ip;
    core.cache = cache;
    core.total_out += (pos - start_pos) as u64;
    sink.commit(pos);

    match exit {
        FastExit::Boundary | FastExit::Careful => Ok(false),
        FastExit::EndOfBlock => {
            core.finish_block();
            Ok(true)
        }
        FastExit::PendingDistance(length) => {
            core.pending_length = length;
            core.state = InflateState::DistSymbol;
            Ok(true)
        }
        FastExit::BadLitLen(code) => {
            Err(core.fail_corrupted(format!("Invalid literal/length code: {}", code)))
        }
        FastExit::BadDist(code) => {
            Err(core.fail_corrupted(format!("Invalid distance code: {}", code)))
        }
        FastExit::Sink(error) => Err(core.latch(error)),
    }
}

/// A match that reaches back past the start of `dst`: the prefix comes from
/// `history`, the rest from bytes this call has just written.
///
/// Returns the new cursor, or `None` when the split does not land inside
/// either buffer (which the caller reports as an invalid distance).
///
/// Out of line on purpose. It can only run while `pos < 32 KiB`, i.e. for
/// the first few matches of a call, so a call costs nothing — while leaving
/// it inline grows [`symbols`], which is the one function whose code layout
/// the decoder's throughput depends on.
///
/// The tail is [`copy_within_dst`], **not** [`copy_match_words`]: the cursor
/// has moved by `take` since the fast loop's output guard ran, so the 258
/// bytes of room that guard proved no longer cover this write — `pos` is now
/// exactly `distance`, which the loop bounds only by `dst.len()`.
/// `copy_match_words`'s word-wide tail needs eight bytes of slack past the
/// cursor and may have none here; `copy_within_dst` checks for that slack
/// and falls back to a byte loop without it. Getting this wrong dropped the
/// last one to seven bytes of such a match with no error at all — see
/// `tests/inflate_fast_boundary.rs`.
#[inline(never)]
fn copy_straddling_match(
    dst: &mut [u8],
    history: &[u8],
    pos: usize,
    distance: usize,
    length: usize,
) -> Option<usize> {
    let from_history = distance.checked_sub(pos)?;
    let take = from_history.min(length);
    let start = history.len().checked_sub(from_history)?;
    let src = history.get(start..start.checked_add(take)?)?;
    dst.get_mut(pos..pos.checked_add(take)?)?
        .copy_from_slice(src);
    let mut cursor = pos + take;
    if take < length {
        cursor = copy_within_dst(dst, cursor, distance, length - take);
    }
    Some(cursor)
}

/// Mask of the low `count` bits (`count <= 13` in every call site).
#[inline(always)]
fn low_mask(count: u8) -> u32 {
    (1u32 << count) - 1
}

/// Write one byte at `at`, which the fast loop's output margin has already
/// proven to be in bounds.
#[inline(always)]
fn write_byte(dst: &mut [u8], at: usize, byte: u8) {
    debug_assert!(at < dst.len());
    if let Some(slot) = dst.get_mut(at) {
        *slot = byte;
    }
}

/// Read eight bytes as a little-endian word (`0` if fewer remain).
#[inline(always)]
fn load_u64(buf: &[u8], at: usize) -> u64 {
    match buf.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        Some(chunk) => u64::from_le_bytes(*chunk),
        None => 0,
    }
}

/// Write eight bytes as a little-endian word (a no-op if fewer remain).
#[inline(always)]
fn store_u64(buf: &mut [u8], at: usize, value: u64) {
    if let Some(chunk) = buf.get_mut(at..).and_then(<[u8]>::first_chunk_mut::<8>) {
        *chunk = value.to_le_bytes();
    }
}

/// The copy the fast loop performs inline: a short match whose source does
/// not overlap its destination, which is the overwhelming majority.
///
/// Everything else defers to [`copy_within_dst`], which stays out of line
/// so the hot loop keeps its registers (measured: with the whole copy
/// inlined the loop spills the bit accumulator).
///
/// **Precondition:** `pos + WORD_COPY_MAX + 8 <= dst.len()`. The word-wide
/// tail store needs eight bytes of slack past the last byte of the match,
/// and it does *not* check for them — an out-of-range word store would
/// silently write nothing and drop the last one to seven bytes of the
/// match. The fast loop's `FAST_OUTPUT_MARGIN` guard supplies that slack,
/// but only for the cursor the guard itself saw: a caller that has already
/// advanced the cursor inside the iteration (the second half of a match
/// that starts in the history window) must use [`copy_within_dst`], which
/// checks. The `debug_assert` below is what stops that mistake from being
/// reintroduced silently.
#[inline(always)]
fn copy_match_words(dst: &mut [u8], pos: usize, distance: usize, length: usize) -> usize {
    debug_assert!(
        pos + WORD_COPY_MAX + 8 <= dst.len(),
        "copy_match_words needs a full output margin; use copy_within_dst"
    );
    if distance >= length && length <= WORD_COPY_MAX {
        // `src + length <= pos`, so the 8-byte reads below stay inside the
        // bytes already written and never touch the destination. The whole
        // copy is at most `WORD_COPY_MAX` bytes and the fast loop holds
        // 258 bytes of room, so every 8-byte access below is in bounds.
        let src = pos - distance;
        let mut k = 0usize;
        while k + 8 <= length {
            let word = load_u64(dst, src + k);
            store_u64(dst, pos + k, word);
            k += 8;
        }
        if k < length {
            // In bounds by the precondition: `k < length <= WORD_COPY_MAX`
            // and `k` is a multiple of 8, so `pos + k + 8 <= pos +
            // WORD_COPY_MAX + 8 <= dst.len()`;
            // and `src + length <= pos`, so the source word is inside the
            // bytes already written. Checking it again here costs two
            // branches in the decoder's most frequent copy (measured: 8-16 %
            // on match-heavy shapes).
            merge_word(dst, pos + k, load_u64(dst, src + k), length - k);
        }
        return pos + length;
    }
    copy_within_dst(dst, pos, distance, length)
}

/// Write the low `len < 8` bytes of `source` at `pos`, leaving every byte
/// from `pos + len` on **unchanged**.
///
/// The obvious byte loop costs two accesses per byte, and a DEFLATE match
/// is most often 3-8 bytes long, so this tail is the copy the decoder
/// performs most. Storing a whole word instead would clobber the `8 - len`
/// bytes after the match — which the decoder promises never to touch
/// outside the output it reports — so those bytes are read and merged back
/// in: the store is a word wide, its *effect* is exactly `len` bytes.
///
/// Measured on 1 MiB payloads, `inflate_into`, against the byte loop:
/// PNG-filtered rows +32 %, text +23 %, many-blocks text +19 %, long-match
/// JSON +5 %, image rows (no matches) unchanged.
#[inline(always)]
fn write_partial_word(dst: &mut [u8], pos: usize, source: u64, len: usize) {
    debug_assert!(len < 8);
    if len < 8 && pos + 8 <= dst.len() {
        merge_word(dst, pos, source, len);
        return;
    }
    let bytes = source.to_le_bytes();
    let mut i = 0usize;
    while i < len {
        let byte = bytes.get(i).copied().unwrap_or(0);
        write_byte(dst, pos + i, byte);
        i += 1;
    }
}

/// The merge itself, without the room check: `dst[pos..pos + 8]` must exist.
///
/// `load_u64`/`store_u64` are themselves bounds checked (they read zero and
/// write nothing out of range), so a violation could only lose bytes, never
/// be unsound — and the `debug_assert` catches it in every test build.
#[inline(always)]
fn merge_word(dst: &mut [u8], pos: usize, source: u64, len: usize) {
    debug_assert!(len < 8 && pos + 8 <= dst.len());
    let current = load_u64(dst, pos);
    // Bytes at or above `len` keep their current value.
    let keep = u64::MAX << (len * 8);
    store_u64(dst, pos, (source & !keep) | (current & keep));
}

/// [`write_partial_word`] sourced from `dst` itself, with the room check.
#[inline]
fn copy_short_tail(dst: &mut [u8], src: usize, pos: usize, len: usize) {
    if src + 8 <= dst.len() {
        write_partial_word(dst, pos, load_u64(dst, src), len);
        return;
    }
    let mut i = 0usize;
    while i < len {
        let byte = dst.get(src + i).copied().unwrap_or(0);
        write_byte(dst, pos + i, byte);
        i += 1;
    }
}

/// Above this many bytes a `memmove` (which `copy_within` lowers to, and
/// which is vector-width on every platform we ship on) beats an explicit
/// 8-byte loop; below it the call overhead dominates.
///
/// A DEFLATE match averages well under 16 bytes on text and image data, so
/// most copies take the word path, while the long runs of a highly
/// repetitive stream take the vector path. Measured on 1 MiB payloads:
/// forcing *everything* down the word path costs 6 % on repetitive JSON
/// (matches averaging ~50 bytes), and the threshold itself was swept —
/// 64 beats 32 (text +4 %, JSON +9 %) and beats 128 (text +3 %).
const WORD_COPY_MAX: usize = 64;

/// The LZ77 copy, entirely inside `dst`.
///
/// Returns the new cursor. Semantically identical to
/// `sink::BoundedSink::copy_match`'s in-buffer path — the differential in
/// `sink.rs`'s tests pins both against a byte-at-a-time reference — but a
/// short match never pays for a `memmove` call:
///
/// * `distance == 1` — a byte fill.
/// * `2..=7` — the period is shorter than a machine word, so one 8-byte
///   tile of the pattern is materialised and stored repeatedly, advancing
///   by the largest multiple of the period that fits in a word (the phase
///   realigns at every step). A long run switches to pattern doubling:
///   once `n` phase-aligned bytes exist, copying them forward by `n`
///   extends the pattern, so the run costs `log2` non-overlapping
///   `memmove`s instead of a store per word.
/// * `>= 8` — whole 8-byte words, in runs of at most `distance` so every
///   source byte is one this call has already written, with a `memmove` for
///   runs above [`WORD_COPY_MAX`].
///
/// **No write ever goes past `pos + length`**: the caller's buffer beyond
/// the reported output is never touched.
#[inline]
fn copy_within_dst(dst: &mut [u8], pos: usize, distance: usize, length: usize) -> usize {
    let src = pos - distance;

    if distance == 1 {
        let byte = dst.get(src).copied().unwrap_or(0);
        if let Some(run) = dst.get_mut(pos..pos + length) {
            run.fill(byte);
        }
        return pos + length;
    }

    if distance < 8 {
        let mut tile = [0u8; 8];
        for (i, slot) in tile.iter_mut().enumerate() {
            *slot = dst.get(src + i % distance).copied().unwrap_or(0);
        }
        // Largest multiple of the period that fits in one word.
        let step = distance * (8 / distance);
        if length > WORD_COPY_MAX {
            // Materialise one phase-aligned tile, then double it.
            let head = step.min(length);
            for i in 0..head {
                let byte = tile.get(i).copied().unwrap_or(0);
                write_byte(dst, pos + i, byte);
            }
            let mut have = head;
            while have < length {
                // Non-overlapping, and `have` is a multiple of the period
                // except possibly on the final (clamped) copy, which no
                // later copy reads.
                let take = have.min(length - have);
                dst.copy_within(pos..pos + take, pos + have);
                have += take;
            }
            return pos + length;
        }
        let word = u64::from_le_bytes(tile);
        let mut done = 0usize;
        while done + 8 <= length {
            store_u64(dst, pos + done, word);
            done += step;
        }
        // `done` is a multiple of the period, so the tail continues the
        // pattern at phase zero.
        if done < length {
            write_partial_word(dst, pos + done, word, length - done);
        }
        return pos + length;
    }

    let mut done = 0usize;
    while done < length {
        // Only bytes already materialised may serve as the source, which is
        // also what gives the standard LZ77 pattern repetition when
        // `length > distance`.
        let step = (length - done).min(distance);
        if step > WORD_COPY_MAX {
            dst.copy_within(src + done..src + done + step, pos + done);
            done += step;
            continue;
        }
        let mut k = 0usize;
        while k + 8 <= step {
            let word = load_u64(dst, src + done + k);
            store_u64(dst, pos + done + k, word);
            k += 8;
        }
        if k < step {
            copy_short_tail(dst, src + done + k, pos + done + k, step - k);
        }
        done += step;
    }
    pos + length
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The LZ77 copy, one byte at a time: the definition every fast path
    /// below has to reproduce.
    fn reference(base: &[u8], pos: usize, distance: usize, length: usize) -> Vec<u8> {
        let mut out = base[..pos].to_vec();
        for _ in 0..length {
            let byte = out[out.len() - distance];
            out.push(byte);
        }
        out[pos..].to_vec()
    }

    /// A buffer whose first `pos` bytes are recognisable history and whose
    /// tail is a distinct filler, so a copy that writes one byte too far is
    /// visible.
    fn buffer(pos: usize, slack: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(pos + slack);
        for i in 0..pos {
            buf.push((i % 251) as u8);
        }
        buf.resize(pos + slack, 0xA5);
        buf
    }

    /// Sweep every distance and length the fast loop can hand the copy
    /// paths, against the byte-at-a-time definition — and assert that
    /// **nothing past the match is touched**, which is the promise that
    /// lets the decoder write straight into a caller's buffer.
    ///
    /// The sweep matters because the two paths take different branches at
    /// `distance == 1`, `distance < 8`, `distance < length` and
    /// `length > WORD_COPY_MAX`, and because the partial-word tail
    /// (`merge_word`) is exactly the kind of code that is silently wrong
    /// for one length.
    #[test]
    fn copy_paths_agree_with_a_byte_at_a_time_reference() {
        const POS: usize = 1024;
        const SLACK: usize = 400;
        let base = buffer(POS, SLACK);

        for distance in 1..=96usize {
            for length in 1..=300usize {
                if length > SLACK - 8 {
                    continue;
                }
                let expect = reference(&base, POS, distance, length);

                let mut fast = base.clone();
                let end = copy_match_words(&mut fast, POS, distance, length);
                assert_eq!(end, POS + length, "cursor: d={distance} l={length}");
                assert_eq!(
                    &fast[POS..POS + length],
                    &expect[..],
                    "copy_match_words: d={distance} l={length}"
                );
                assert_eq!(
                    &fast[POS + length..],
                    &base[POS + length..],
                    "copy_match_words wrote past the match: d={distance} l={length}"
                );

                let mut slow = base.clone();
                let end = copy_within_dst(&mut slow, POS, distance, length);
                assert_eq!(end, POS + length, "cursor: d={distance} l={length}");
                assert_eq!(
                    &slow[POS..POS + length],
                    &expect[..],
                    "copy_within_dst: d={distance} l={length}"
                );
                assert_eq!(
                    &slow[POS + length..],
                    &base[POS + length..],
                    "copy_within_dst wrote past the match: d={distance} l={length}"
                );
            }
        }
    }

    /// A match that starts in the history window and finishes inside `dst`
    /// must be exact for every split, including the ones whose tail lands in
    /// the last eight bytes of the caller's buffer.
    ///
    /// That case is what the fast loop's once-per-iteration output guard does
    /// **not** cover: the guard proves 258 bytes of room for the cursor it
    /// saw, and this copy resumes at `pos + take`, which is exactly
    /// `distance` and can be within eight bytes of the end of `dst`. A
    /// word-wide store there writes nothing at all (`store_u64` is bounds
    /// checked), which silently drops the last one to seven bytes of the
    /// match. `dst` here is sized to end exactly at `pos + length`, so any
    /// such drop shows up as a mismatch and any overshoot as a panic.
    #[test]
    fn a_straddling_copy_is_exact_including_at_the_end_of_the_buffer() {
        for hist_len in [1usize, 7, 8, 9, 64, 300] {
            let history: Vec<u8> = (0..hist_len).map(|i| (i % 251) as u8).collect();
            for pos in [0usize, 1, 3, 8, 33] {
                for length in 1..=120usize {
                    for from_history in 1..=hist_len.min(40) {
                        let distance = pos + from_history;
                        // `dst` ends exactly at the match, so a word store
                        // past `pos + length` has nowhere to land.
                        for slack in [0usize, 1, 7, 8, 64] {
                            let mut dst = vec![0u8; pos + length + slack];
                            for (i, slot) in dst.iter_mut().take(pos).enumerate() {
                                *slot = (200 + i % 41) as u8;
                            }
                            let tail_marker = 0xA5u8;
                            for slot in dst.iter_mut().skip(pos) {
                                *slot = tail_marker;
                            }

                            // Byte-at-a-time reference over history ++ dst[..pos].
                            let mut reference: Vec<u8> = history.clone();
                            reference.extend_from_slice(&dst[..pos]);
                            for _ in 0..length {
                                let byte = reference[reference.len() - distance];
                                reference.push(byte);
                            }
                            let want = &reference[hist_len + pos..];

                            let end =
                                copy_straddling_match(&mut dst, &history, pos, distance, length)
                                    .expect("the split lands inside both buffers");
                            assert_eq!(
                                end,
                                pos + length,
                                "cursor: h={hist_len} pos={pos} d={distance} l={length}"
                            );
                            assert_eq!(
                                &dst[pos..pos + length],
                                want,
                                "bytes: h={hist_len} pos={pos} d={distance} l={length} slack={slack}"
                            );
                            assert!(
                                dst[pos + length..].iter().all(|&b| b == tail_marker),
                                "wrote past the match: h={hist_len} pos={pos} d={distance} \
                                 l={length} slack={slack}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A split that does not land inside both buffers is reported, not
    /// silently truncated — the fast loop turns `None` into
    /// `InvalidDistance`.
    #[test]
    fn a_straddling_copy_reports_a_split_it_cannot_serve() {
        let history = [1u8, 2, 3, 4];
        let mut dst = [0u8; 64];
        // Reaching further back than the history holds.
        assert!(copy_straddling_match(&mut dst, &history, 2, 12, 4).is_none());
        // A distance that does not actually straddle is still served.
        assert!(copy_straddling_match(&mut dst, &history, 2, 6, 4).is_some());
    }

    /// The partial-word tail must write exactly `len` bytes for every
    /// `len` and leave the rest of the word alone — checked both with room
    /// for the eight-byte store (the merge path) and without it (the byte
    /// loop), since only the first can be reached from the fast loop but
    /// both are compiled.
    #[test]
    fn partial_word_writes_exactly_its_length() {
        const POS: usize = 4;
        for len in 0..8usize {
            // `slack == 8` takes the merged word store; anything smaller
            // has no room for it and falls back to the byte loop. The
            // contract needs `POS + len <= dst.len()`, so the smallest
            // legal slack is `len` itself.
            for slack in len..=8usize {
                let mut buf = vec![0xA5u8; POS + slack];
                let source = u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
                write_partial_word(&mut buf, POS, source, len);
                for (index, &byte) in buf.iter().enumerate() {
                    let want = if (POS..POS + len).contains(&index) {
                        (index - POS + 1) as u8
                    } else {
                        0xA5
                    };
                    assert_eq!(byte, want, "len={len} slack={slack} index={index}");
                }
            }
        }
    }
}
