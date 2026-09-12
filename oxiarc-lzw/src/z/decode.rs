//! Incremental decoder for the UNIX `compress(1)` (`.Z`) code stream.
//!
//! This is a faithful port of the `getcode()`/decompress loop shared by
//! 4.3BSD `compress`, `ncompress` and GNU `gzip`'s `unlzw.c`, with three
//! deliberate differences, all of them documented on [`super`]:
//!
//! 1. it never reads past the end of the supplied bytes (the reference
//!    decoders can emit one extra code built partly from a stale I/O
//!    buffer when a stream is truncated),
//! 2. an out-of-table code is a reported error rather than "corrupt input"
//!    on stderr, and
//! 3. output can be bounded, and the bound is enforced *during* decoding.
//!
//! Where the references themselves differ — on a leading CLEAR they split
//! three ways, listed on [`super`] — this follows GNU `gzip`'s `unlzw.c`,
//! the strictest of them: its `oldcode == -1` guard runs *before* the CLEAR
//! handling, so the first code of a stream must be a literal byte and a
//! leading CLEAR is [`LzwError::InvalidCode`]`(256)` rather than a table
//! reset.
//!
//! The decoder is a *push* core: [`ZDecoder::decode`] consumes as much of a
//! chunk as it can and keeps back fewer than `max_bits` bytes (one code
//! group) for the next call, so a `Read` adapter over it holds a bounded
//! working set rather than the whole compressed stream. The bound is
//! asserted by `the_carry_never_grows_past_one_code_group`.

use crate::error::{LzwError, Result};

use super::header::{CLEAR, FIRST, INIT_BITS, ZHeader};
use super::sink::ZSink;

/// A view over `carry ++ input` that costs no allocation and no copy.
///
/// The decoder keeps the tail of the previous chunk (never more than one
/// code group) in a small `Vec`; codes may straddle the join, so both
/// halves have to be addressable as one byte sequence.
struct SplitInput<'a> {
    head: &'a [u8],
    tail: &'a [u8],
}

impl SplitInput<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Byte at logical index `index`, or 0 past the end.
    ///
    /// Reading past the end is only reachable from [`ZDecoder::read_code`],
    /// which has already checked that the *code* fits; the padding zeros
    /// cover the one or two bytes of a three-byte load that the code itself
    /// does not reach.
    #[inline]
    fn byte(&self, index: usize) -> u8 {
        if index < self.head.len() {
            self.head[index]
        } else {
            self.tail
                .get(index - self.head.len())
                .copied()
                .unwrap_or_default()
        }
    }

    /// Replace `out` with everything from `start` onwards.
    fn copy_tail_into(&self, start: usize, out: &mut Vec<u8>) {
        out.clear();
        if start < self.head.len() {
            out.extend_from_slice(&self.head[start..]);
            out.extend_from_slice(self.tail);
        } else if let Some(rest) = self.tail.get(start - self.head.len()..) {
            out.extend_from_slice(rest);
        }
    }
}

/// Push decoder for a `.Z` code stream (the bytes *after* the 3-byte
/// header).
#[derive(Debug)]
pub(crate) struct ZDecoder {
    header: ZHeader,
    /// `1 << max_bits`: one past the largest assignable code.
    max_max_code: u32,
    /// Current code width in bits.
    n_bits: u8,
    /// Width-growth trigger: grow as soon as `free_ent > max_code`.
    max_code: u32,
    /// Next code slot to be filled.
    free_ent: u32,
    /// Parent code of every table entry.
    prefix: Vec<u16>,
    /// Last byte of every table entry.
    suffix: Vec<u8>,
    /// Previous code (`-1` in the reference, i.e. "nothing decoded yet").
    old_code: Option<u16>,
    /// First byte of the string the previous code expanded to.
    fin_char: u8,
    /// Scratch used to reverse a code's byte string.
    stack: Vec<u8>,
    /// Unconsumed bytes from the current group's first byte onwards.
    carry: Vec<u8>,
    /// Bits consumed within the current group of eight codes, always
    /// `0 <= bit_pos < 8 * n_bits`.
    ///
    /// This mirrors the reference decoder's `offset`, which returns to zero
    /// every time `getcode()` refills its `n_bits`-byte input buffer — i.e.
    /// every eight codes. Keeping the same invariant here is what lets
    /// [`ZDecoder::decode`] drop consumed bytes from [`Self::carry`] instead
    /// of retaining every byte since the last width change.
    bit_pos: usize,
    /// Bytes still to be discarded because a group alignment ran past the
    /// end of everything fed so far.
    skip_bytes: usize,
}

impl ZDecoder {
    /// Create a decoder for a stream with the given header.
    pub(crate) fn new(header: ZHeader) -> Self {
        let max_max_code = header.max_max_code();
        let capacity = max_max_code as usize;
        Self {
            header,
            max_max_code,
            n_bits: INIT_BITS,
            max_code: (1u32 << INIT_BITS) - 1,
            free_ent: if header.block_mode {
                FIRST
            } else {
                u32::from(CLEAR)
            },
            prefix: vec![0; capacity],
            suffix: vec![0; capacity],
            old_code: None,
            fin_char: 0,
            stack: Vec::with_capacity(1024),
            carry: Vec::new(),
            bit_pos: 0,
            skip_bytes: 0,
        }
    }

    /// The header this decoder was built for.
    pub(crate) fn header(&self) -> ZHeader {
        self.header
    }

    /// Feed `input` and write everything it decodes to `sink`.
    ///
    /// Bytes that cannot yet form a complete code are retained internally
    /// and used by the next call. After a successful call that retention is
    /// strictly smaller than one code group (`max_bits` bytes, so at most 15
    /// on a 16-bit stream): a code that would not fit is all that is ever
    /// held back, never the bytes behind it. Only the error path can leave
    /// more, because it stops mid-chunk.
    ///
    /// # Errors
    ///
    /// - [`LzwError::InvalidCode`] for a code above the current table, and
    ///   for a first code that is not a single byte,
    /// - whatever `sink` returns when its bound is exceeded.
    pub(crate) fn decode<S: ZSink>(&mut self, input: &[u8], sink: &mut S) -> Result<()> {
        let skipped = self.skip_bytes.min(input.len());
        self.skip_bytes -= skipped;
        let input = &input[skipped..];
        if input.is_empty() && self.carry.is_empty() {
            return Ok(());
        }

        let carry = core::mem::take(&mut self.carry);
        let view = SplitInput {
            head: &carry,
            tail: input,
        };
        let mut base = 0usize;
        let result = self.run(&view, sink, &mut base);
        // Retain the unconsumed tail even on the error path, so a caller
        // that recovers does not silently lose bytes.
        view.copy_tail_into(base.min(view.len()), &mut self.carry);
        result
    }

    /// Decode as much of `view` as possible, leaving the group origin in
    /// `base`.
    fn run<S: ZSink>(
        &mut self,
        view: &SplitInput<'_>,
        sink: &mut S,
        base: &mut usize,
    ) -> Result<()> {
        let total_bits = view.len() * 8;

        loop {
            if self.free_ent > self.max_code {
                // The table outgrew the current width: the writer padded the
                // rest of this group of eight codes, so step over the same
                // padding before widening.
                self.align_group(base, view);
                self.n_bits += 1;
                self.max_code = if self.n_bits == self.header.max_bits {
                    self.max_max_code
                } else {
                    (1u32 << self.n_bits) - 1
                };
                continue;
            }

            let n = usize::from(self.n_bits);
            if *base * 8 + self.bit_pos + n > total_bits {
                // Not a complete code: wait for more input (or, at end of
                // stream, stop — `.Z` has no end-of-information code).
                break;
            }
            let code = self.read_code(*base, view, n);
            if self.bit_pos == n * 8 {
                // Eight codes make one group: this is exactly where the
                // reference's `getcode()` refills its `n_bits`-byte buffer.
                // Moving the origin here keeps `bit_pos` below one group and
                // lets `decode` release the bytes behind it.
                *base += n;
                self.bit_pos = 0;
            }

            if code == CLEAR && self.header.block_mode {
                if self.old_code.is_none() {
                    // A CLEAR as the *first* code of a stream is corrupt
                    // input, not a reset. GNU `gzip`'s `unlzw.c` runs its
                    // `oldcode == -1` guard *before* the CLEAR handling, so
                    // the first code must be a literal byte:
                    //
                    // ```c
                    //     if (oldcode == -1) {
                    //         if (256 <= code) gzip_error("corrupt input.");
                    //         ...
                    //     }
                    //     if (code == CLEAR && block_mode) { ... }
                    // ```
                    //
                    // Verified against gzip 1.14 (GNU/Linux), which answers
                    // `gzip: <name>.Z: corrupt input.` and writes nothing;
                    // on GNU systems `uncompress` is `gunzip`, so it agrees.
                    // The BSD decoders accept the stream, but not even in
                    // the same way (FreeBSD/Apple `gzip` as a reset, BSD
                    // `uncompress` as a literal — see the module docs of
                    // `super`), and no encoder in circulation emits a
                    // leading CLEAR, so accepting one would only ever be
                    // decode surface for crafted input.
                    return Err(LzwError::InvalidCode(CLEAR));
                }
                self.prefix.fill(0);
                // One lower than the encoder's `FIRST`: the next code burns
                // slot 256 (which is the CLEAR code and therefore never
                // referenced), after which the two sides are back in step.
                self.free_ent = u32::from(CLEAR);
                // `old_code` is deliberately *not* cleared — the reference
                // does not clear it either — so back-to-back CLEARs take
                // this same path rather than the rejection above.
                self.align_group(base, view);
                self.n_bits = INIT_BITS;
                self.max_code = (1u32 << INIT_BITS) - 1;
                continue;
            }

            let Some(old) = self.old_code else {
                // The first data code of a stream must be a literal byte.
                if code >= CLEAR {
                    return Err(LzwError::InvalidCode(code));
                }
                self.fin_char = code as u8;
                self.old_code = Some(code);
                sink.write(&[self.fin_char])?;
                continue;
            };

            let in_code = code;
            self.stack.clear();
            let mut walk = code;
            if u32::from(walk) >= self.free_ent {
                // KwKwK is the *only* legal code at or above `free_ent`: it
                // names the entry this step is about to create. Two ways it
                // can still be corrupt input:
                //
                // * `walk > free_ent` — a code for an entry that does not
                //   exist and is not about to;
                // * the table is already full, so no entry is created this
                //   step and `walk` names a slot that can never exist. That
                //   case is reachable: at `max_bits == 9` the reference's
                //   width rule takes `n_bits` to 10 (one *past* `max_bits`,
                //   which is why `compress -b 9` emits 10-bit codes), so a
                //   crafted stream can present the code 512 while the table
                //   has exactly 512 slots. Accepting it stored 512 in
                //   `old_code`, and the next KwKwK walked `prefix[512]` —
                //   one past the end, an index-out-of-bounds panic on
                //   attacker-controlled input.
                if u32::from(walk) > self.free_ent || self.free_ent >= self.max_max_code {
                    return Err(LzwError::InvalidCode(walk));
                }
                // KwKwK: the code names the entry this step is about to
                // create, so its string is `string(old) ++ first(old)`.
                self.stack.push(self.fin_char);
                walk = old;
            }
            while walk >= CLEAR {
                let index = usize::from(walk);
                // `prefix[c] < c` holds for every entry this decoder stores,
                // so the walk strictly descends and terminates; the length
                // guard is a belt-and-braces bound in case a future change
                // breaks that invariant.
                if self.stack.len() > self.max_max_code as usize {
                    return Err(LzwError::InvalidCode(code));
                }
                self.stack.push(self.suffix[index]);
                walk = self.prefix[index];
            }
            self.fin_char = walk as u8;
            self.stack.push(self.fin_char);
            self.stack.reverse();
            sink.write(&self.stack)?;

            if self.free_ent < self.max_max_code {
                let index = self.free_ent as usize;
                self.prefix[index] = old;
                self.suffix[index] = self.fin_char;
                self.free_ent += 1;
            }
            self.old_code = Some(in_code);
        }

        Ok(())
    }

    /// Read one `n`-bit code, LSB-first, and advance the bit position.
    fn read_code(&mut self, base: usize, view: &SplitInput<'_>, n: usize) -> u16 {
        let at = base + (self.bit_pos >> 3);
        let shift = self.bit_pos & 7;
        let acc = u32::from(view.byte(at))
            | (u32::from(view.byte(at + 1)) << 8)
            | (u32::from(view.byte(at + 2)) << 16);
        let mask = (1u32 << n) - 1;
        let code = ((acc >> shift) & mask) as u16;
        self.bit_pos += n;
        code
    }

    /// Skip to the next boundary of a group of eight codes and make that
    /// boundary the new origin.
    ///
    /// The writer flushes a whole `n_bits`-byte group whenever the code
    /// width changes or the table is cleared, zero-padding the unused
    /// codes, and the reader must step over the same padding. Both sides
    /// measure the group from the position of the *previous* such event,
    /// which is why the origin moves here.
    fn align_group(&mut self, base: &mut usize, view: &SplitInput<'_>) {
        if self.bit_pos == 0 {
            // Already on a group boundary: the reference refills its buffer
            // here too, so there is no padding to step over.
            return;
        }
        // `bit_pos` is always inside the current group (the rollover in
        // `run` guarantees it), so the padding is whatever is left of this
        // one `n_bits`-byte group.
        self.bit_pos = 0;
        let target = *base + usize::from(self.n_bits);
        if target > view.len() {
            self.skip_bytes += target - view.len();
            *base = view.len();
        } else {
            *base = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::z::sink::VecZSink;

    fn decode_all(header: ZHeader, body: &[u8], chunk: usize) -> Result<Vec<u8>> {
        let mut decoder = ZDecoder::new(header);
        let mut out = Vec::new();
        {
            let mut sink = VecZSink::new(&mut out, usize::MAX);
            for piece in body.chunks(chunk.max(1)) {
                decoder.decode(piece, &mut sink)?;
            }
        }
        Ok(out)
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let header = ZHeader::new(16, true).expect("header");
        let payload = b"to be or not to be, that is the question. ".repeat(400);
        let stream = crate::z::compress(&payload, 16).expect("compress");
        let body = &stream[ZHeader::LEN..];
        let whole = decode_all(header, body, body.len()).expect("whole");
        assert_eq!(whole, payload);
        for chunk in [1usize, 2, 3, 7, 13, 16, 17, 64, 1024] {
            assert_eq!(
                decode_all(header, body, chunk).expect("chunked"),
                payload,
                "chunk size {chunk}"
            );
        }
    }

    /// Regression: a crafted stream used to panic with an index out of
    /// bounds.
    ///
    /// At `max_bits == 9` the reference's width rule takes `n_bits` to 10
    /// (see `tests/data/z/README.txt`), so codes up to 1023 are readable
    /// while the table holds exactly 512 entries. The code 512 was accepted
    /// as KwKwK even though the full table can never create that entry, and
    /// the next KwKwK then indexed `prefix[512]`.
    #[test]
    fn a_code_naming_a_slot_the_full_table_can_never_create_is_rejected() {
        // `1F 9D 09` = max_bits 9, non-block. Then 257 nine-bit zero codes
        // (which take `free_ent` to 512) plus the group padding, then two
        // ten-bit codes of 512.
        let mut stream = vec![0x1Fu8, 0x9D, 0x09];
        stream.extend(std::iter::repeat_n(0u8, 297));
        stream.extend_from_slice(&[0x00, 0x02, 0x08]);

        assert!(matches!(
            crate::z::decompress(&stream),
            Err(LzwError::InvalidCode(512))
        ));
        let mut dst = [0u8; 4096];
        assert!(crate::z::decompress_into(&stream, &mut dst).is_err());

        // Every truncation of it, at every offset, must also be safe.
        for cut in ZHeader::LEN..stream.len() {
            let _ = crate::z::decompress_with_limit(&stream[..cut], 1 << 20);
        }

        // And the same shape must still decode when the table is *not*
        // full: at `max_bits = 16` the code 512 is an ordinary KwKwK.
        let payload = b"kwkwk at a width with room to grow".repeat(40);
        let wide = crate::z::compress(&payload, 16).expect("compress");
        assert_eq!(crate::z::decompress(&wide).expect("decompress"), payload);
    }

    /// Regression: the carry used to be measured from the last *width
    /// change* rather than from the current group, so a 16-bit stream with
    /// no further width changes (and, in non-block mode, no CLEAR codes at
    /// all) retained every compressed byte fed so far — unbounded memory in
    /// `ZReader`, and quadratic time because every call re-copied it.
    #[test]
    fn the_carry_never_grows_past_one_code_group() {
        let mut state = 0x2545_F491u32;
        let payload: Vec<u8> = (0..600_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 11) as u8
            })
            .collect();
        for block_mode in [true, false] {
            let stream =
                crate::z::compress_with_block_mode(&payload, 16, block_mode).expect("compress");
            let header = ZHeader::parse(&stream).expect("header");
            let mut decoder = ZDecoder::new(header);
            let mut out = Vec::new();
            let mut worst = 0usize;
            {
                let mut sink = VecZSink::new(&mut out, usize::MAX);
                for piece in stream[ZHeader::LEN..].chunks(4096) {
                    decoder.decode(piece, &mut sink).expect("decode");
                    worst = worst.max(decoder.carry.len());
                    assert!(
                        decoder.bit_pos < usize::from(decoder.n_bits) * 8,
                        "bit_pos {} must stay inside one group",
                        decoder.bit_pos
                    );
                }
            }
            assert_eq!(out, payload, "block_mode {block_mode}");
            assert!(
                worst < usize::from(header.max_bits),
                "block_mode {block_mode}: carry reached {worst} bytes"
            );
        }
    }

    /// Every width, fed in every awkward chunk size, over a payload long
    /// enough that the group origin has to move many times.
    ///
    /// This is the sweep that covers [`ZDecoder::align_group`]'s
    /// `skip_bytes` path: with 1- and 2-byte chunks the padding after a
    /// width change routinely runs past the end of what has been fed, so the
    /// skip has to be carried into the next call.
    #[test]
    fn every_width_survives_every_chunk_size() {
        let mut state = 0x1357_9BDFu32;
        let mut payload = b"prefix that compresses well; ".repeat(600);
        for _ in 0..40_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            payload.push((state >> 7) as u8);
        }
        payload.extend_from_slice(&b"and a repetitive tail. ".repeat(600));

        let mut cases = 0usize;
        for max_bits in 9u8..=16 {
            for block_mode in [true, false] {
                let stream = crate::z::compress_with_block_mode(&payload, max_bits, block_mode)
                    .expect("compress");
                let header = ZHeader::parse(&stream).expect("header");
                let body = &stream[ZHeader::LEN..];
                for chunk in [1usize, 2, 3, 5, 9, 13, 16, 17, 64, 4096] {
                    assert_eq!(
                        decode_all(header, body, chunk).expect("chunked"),
                        payload,
                        "b{max_bits}/block={block_mode}/chunk={chunk}"
                    );
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 8 * 2 * 10);
    }

    #[test]
    fn chunking_survives_a_stream_with_clear_codes() {
        // Big enough, and mixed enough, that the block-mode ratio reset
        // actually fires at 12 bits.
        let mut state = 0x9E37_79B9u32;
        let mut entropy = Vec::new();
        for round in 0..40 {
            if round % 2 == 0 {
                entropy.extend_from_slice(&b"ABABABABABABABAB".repeat(400));
            } else {
                for _ in 0..6400 {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    entropy.push((state % 256) as u8);
                }
            }
        }
        let stream = crate::z::compress_with_block_mode(&entropy, 12, true).expect("compress");
        let header = ZHeader::parse(&stream).expect("header");
        let body = &stream[ZHeader::LEN..];
        for chunk in [1usize, 5, 13, 97, 4096] {
            assert_eq!(
                decode_all(header, body, chunk).expect("chunked"),
                entropy,
                "chunk size {chunk}"
            );
        }
    }
}
