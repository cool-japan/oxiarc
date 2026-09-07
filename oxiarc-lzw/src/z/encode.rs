//! Incremental encoder for the UNIX `compress(1)` (`.Z`) code stream.
//!
//! A byte-exact port of 4.3BSD `compress`'s `compress()` / `output()` /
//! `cl_block()`, including the parts that look like accidents and are in
//! fact load-bearing for interoperability:
//!
//! * codes are emitted into a staging buffer of exactly eight codes
//!   (`n_bits` bytes); the buffer is flushed whole,
//! * a code-width increase or a table clear flushes the *whole* staging
//!   buffer even when it holds fewer than eight codes, so the reader can
//!   step over the padding,
//! * the width-increase test runs with the `free_ent` value from *before*
//!   the current step's new entry, i.e. one step later than a naive port,
//! * the block-mode reset is the reference's compression-ratio heuristic
//!   (`CHECK_GAP` = 10 000 input bytes, ratio in 8 fractional bits), which
//!   only starts once the table is full.
//!
//! Verified byte-identical to `compress -b N -c` for eleven payload shapes
//! across every width 9..=16 (88 streams); see the track handoff.

use crate::dictionary::LzwCodeIndex;

use super::header::{CLEAR, FIRST, INIT_BITS, ZHeader};

/// Input bytes between two block-mode compression-ratio checks.
const CHECK_GAP: u64 = 10_000;

/// Push encoder for a `.Z` code stream (the bytes *after* the header).
#[derive(Debug)]
pub(crate) struct ZEncoder {
    header: ZHeader,
    /// `1 << max_bits`: one past the largest assignable code.
    max_max_code: u32,
    /// Current code width in bits.
    n_bits: u8,
    /// Width-growth trigger: grow as soon as `free_ent > max_code`.
    max_code: u32,
    /// Next code slot to be filled.
    free_ent: u32,
    /// Set by [`Self::cl_block`]; consumed by the next [`Self::output`].
    clear_flg: bool,
    /// `(prefix, byte) -> code` lookup over the code table.
    table: LzwCodeIndex,
    /// Best compression ratio seen so far, in 8 fractional bits.
    ratio: u64,
    /// Input position of the next ratio check.
    checkpoint: u64,
    /// Input bytes consumed.
    in_count: u64,
    /// Output bytes emitted, header included (the ratio's denominator).
    bytes_out: u64,
    /// Staging buffer holding up to eight codes at the current width.
    group: [u8; 16],
    /// Bits used in `group`.
    offset: usize,
    /// Code of the string matched so far.
    ent: Option<u16>,
    /// Whether the first input byte has been seen.
    started: bool,
}

impl ZEncoder {
    /// Create an encoder for a stream with the given header.
    ///
    /// The caller writes the three header bytes; `bytes_out` starts at
    /// [`ZHeader::LEN`] so the ratio heuristic sees the same denominator
    /// the reference does.
    pub(crate) fn new(header: ZHeader) -> Self {
        let max_max_code = header.max_max_code();
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
            clear_flg: false,
            table: LzwCodeIndex::with_capacity(max_max_code as usize),
            ratio: 0,
            checkpoint: CHECK_GAP,
            in_count: 1,
            bytes_out: ZHeader::LEN as u64,
            group: [0u8; 16],
            offset: 0,
            ent: None,
            started: false,
        }
    }

    /// Feed `data`, appending complete code groups to `out`.
    pub(crate) fn push(&mut self, data: &[u8], out: &mut Vec<u8>) {
        let mut rest = data;
        if !self.started {
            let Some((&first, tail)) = data.split_first() else {
                return;
            };
            self.ent = Some(u16::from(first));
            self.started = true;
            rest = tail;
        }

        for &byte in rest {
            self.in_count += 1;
            let Some(ent) = self.ent else {
                // Only reachable if `push` is called after `finish`, which
                // the `Write` adapter forbids; re-seed rather than drop the
                // byte on the floor.
                self.ent = Some(u16::from(byte));
                continue;
            };
            if let Some(code) = self.table.find(ent, byte) {
                self.ent = Some(code);
                continue;
            }
            self.output(ent, out);
            if self.free_ent < self.max_max_code {
                // `free_ent < max_max_code <= 65536`, so the cast is exact.
                self.table.insert(ent, byte, self.free_ent as u16);
                self.free_ent += 1;
            } else if self.header.block_mode && self.in_count >= self.checkpoint {
                self.cl_block(out);
            }
            self.ent = Some(u16::from(byte));
        }
    }

    /// Emit the final code and flush the trailing partial group.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) {
        if let Some(ent) = self.ent.take() {
            self.output(ent, out);
        }
        if self.offset > 0 {
            let bytes = self.offset.div_ceil(8);
            out.extend_from_slice(&self.group[..bytes]);
            self.bytes_out += bytes as u64;
            self.offset = 0;
        }
    }

    /// Pack one code into the staging group, flushing and re-widening
    /// exactly the way `compress`'s `output()` does.
    fn output(&mut self, code: u16, out: &mut Vec<u8>) {
        let n = usize::from(self.n_bits);
        let shift = self.offset & 7;
        let mut at = self.offset >> 3;
        let mut bits = n;
        let mut value = u32::from(code);

        // The first byte keeps the bits of the previous code that share it;
        // every later byte is assigned outright, which is what zeroes the
        // padding above a code that ends mid-byte.
        let keep = (1u16 << shift) as u8;
        let keep_mask = keep.wrapping_sub(1);
        self.group[at] = (self.group[at] & keep_mask) | ((value << shift) as u8);
        at += 1;
        bits -= 8 - shift;
        value >>= 8 - shift;
        if bits >= 8 {
            self.group[at] = value as u8;
            at += 1;
            value >>= 8;
            bits -= 8;
        }
        if bits > 0 {
            self.group[at] = value as u8;
        }

        self.offset += n;
        if self.offset == n * 8 {
            out.extend_from_slice(&self.group[..n]);
            self.bytes_out += n as u64;
            self.offset = 0;
        }

        // `free_ent` here is the value from before this step's new entry,
        // which is what makes the reader and the writer widen in lockstep.
        if self.free_ent > self.max_code || self.clear_flg {
            if self.offset > 0 {
                // Flush the whole group: the reader will skip the padding.
                out.extend_from_slice(&self.group[..n]);
                self.bytes_out += n as u64;
                self.offset = 0;
            }
            if self.clear_flg {
                self.n_bits = INIT_BITS;
                self.max_code = (1u32 << INIT_BITS) - 1;
                self.clear_flg = false;
            } else {
                self.n_bits += 1;
                self.max_code = if self.n_bits == self.header.max_bits {
                    self.max_max_code
                } else {
                    (1u32 << self.n_bits) - 1
                };
            }
        }
    }

    /// The reference's block-mode reset heuristic: once the table is full,
    /// clear it whenever the compression ratio stops improving.
    fn cl_block(&mut self, out: &mut Vec<u8>) {
        self.checkpoint = self.in_count + CHECK_GAP;
        let ratio = if self.in_count > 0x007F_FFFF {
            // The shift below would overflow the reference's 32-bit
            // arithmetic here, so it divides the other way round.
            let scaled = self.bytes_out >> 8;
            // `scaled` can only be zero for a stream shorter than 256 bytes,
            // which cannot reach this branch; the reference's own guard is
            // kept as `checked_div`.
            self.in_count.checked_div(scaled).unwrap_or(0x7FFF_FFFF)
        } else {
            // `bytes_out` starts at 3 and only grows, so this cannot divide
            // by zero.
            (self.in_count << 8) / self.bytes_out
        };
        if ratio > self.ratio {
            self.ratio = ratio;
        } else {
            self.ratio = 0;
            self.table.clear();
            self.free_ent = FIRST;
            self.clear_flg = true;
            self.output(CLEAR, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoder_that_never_saw_a_byte_emits_nothing() {
        let header = ZHeader::new(16, true).expect("header");
        let mut encoder = ZEncoder::new(header);
        let mut out = Vec::new();
        encoder.push(&[], &mut out);
        encoder.finish(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn feeding_in_pieces_matches_feeding_at_once() {
        let payload = b"the same bytes, arriving differently. ".repeat(500);
        let header = ZHeader::new(13, true).expect("header");

        let mut whole = Vec::new();
        let mut encoder = ZEncoder::new(header);
        encoder.push(&payload, &mut whole);
        encoder.finish(&mut whole);

        for chunk in [1usize, 2, 7, 64, 4096] {
            let mut piecewise = Vec::new();
            let mut encoder = ZEncoder::new(header);
            for piece in payload.chunks(chunk) {
                encoder.push(piece, &mut piecewise);
            }
            encoder.finish(&mut piecewise);
            assert_eq!(piecewise, whole, "chunk size {chunk}");
        }
    }
}
