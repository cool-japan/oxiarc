//! Backward (LIFO) bitstream reader for Zstandard entropy streams.
//!
//! Zstandard's FSE and Huffman bitstreams are written forward but read
//! *backward*: the encoder appends bit-fields least-significant-bit first into
//! a little-endian bit sequence, terminates it with a single `1` sentinel bit
//! and zero-pads to a byte boundary. The decoder locates the sentinel in the
//! **last** byte and reads fields back in reverse write order.
//!
//! # Why a 64-bit container
//!
//! The obvious implementation reads each field straight out of the input
//! slice: locate the covering bytes, gather them, shift and mask. That is one
//! bounds-checked byte load per byte of the field's window — up to five per
//! `read_bits` — and a Huffman literals stream calls it *once per output
//! byte*, a sequence stream six to nine times per sequence. Measured on a
//! 288 KiB TIFF-strip frame that gather loop alone held the decoder to
//! ~135 MB/s.
//!
//! [`FseBitReader`] instead keeps a 64-bit register (`container`) holding the
//! eight bytes around the read cursor, plus `bits_consumed`, the number of
//! bits already taken from the top of it — the reference `BIT_DStream_t`
//! layout. A peek is then one shift pair with no memory access at all, and the
//! container is reloaded (one 8-byte little-endian read) only once every four
//! bytes consumed. `bits_remaining()` is *derived* from the pair, so the value
//! callers see — including the "{n} bits left" corruption message — is
//! bit-for-bit what the byte-gathering reader reported.
//!
//! # Invariants
//!
//! * `bits_consumed <= 31` before any peek, so `container << bits_consumed`
//!   never shifts by 64 and a 32-bit field is always fully covered
//!   (`31 + 32 <= 64`).
//! * `byte_ptr * 8 + 64 - bits_consumed` is the bit position of the read
//!   cursor, counting from bit 0 of `data[0]`, and equals the old reader's
//!   `bits_remaining` after the same sequence of operations.
//! * Bits below position 0 read as zero and bits at or above the sentinel are
//!   shifted out, so a stream may legitimately be over-read (which sets
//!   [`FseBitReader::is_overflowed`]) without ever reading out of bounds.

use oxiarc_core::error::{OxiArcError, Result};

/// The register-resident part of a backward bitstream reader.
///
/// Splitting this out of [`FseBitReader`] is what lets an inner loop keep its
/// whole position in registers: a `&mut FseBitReader` forces every
/// `peek`/`skip` through memory (the compiler cannot prove the reference is not
/// aliased), and the resulting store-to-load forwarding stall is paid once per
/// decoded symbol — four times per round in the interleaved literals loop, six
/// to nine times per sequence. [`FseBitReader::detach`] hands the loop a `Copy`
/// cursor to work on, in the spirit of
/// `oxiarc_core::BitReader::detach`/`BitCache` in the inflate decoder. Nothing
/// writes a cursor back: a decode either runs a bitstream to its end or fails,
/// so the reader it came from is not used again.
///
/// The input slice is *not* part of the cursor: it is loop-invariant, so the
/// loop keeps it in its own local and passes it to [`BitCursor::refill`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct BitCursor {
    /// The eight bytes starting at `byte_ptr`, little-endian; bits outside the
    /// input read as zero.
    container: u64,
    /// Index in the input of the container's lowest byte. Goes negative once
    /// the cursor has walked off the front of the stream.
    byte_ptr: i64,
    /// Bits already consumed from the *top* of `container`.
    bits_consumed: u32,
}

/// Largest field any caller reads in one call (offset codes reach 31 bits,
/// literal/match-length extra bits 16, FSE states 9, Huffman prefixes 12).
const MAX_FIELD_BITS: u8 = 32;

/// Reload threshold for a caller that may read up to [`MAX_FIELD_BITS`] at
/// once: refill as soon as four whole bytes have been consumed.
const REFILL_AT: u32 = 32;

/// Largest field a *narrow* caller reads (a Huffman prefix is at most
/// `HUF_TABLELOG_MAX` = 12 bits; an FSE-coded Huffman weight at most 6).
const MAX_NARROW_BITS: u8 = 12;

/// Reload threshold for a narrow caller. Chosen as large as the container
/// allows — a peek needs `bits_consumed + n <= 64`, and the largest
/// `bits_consumed` reachable before a peek is `REFILL_AT_NARROW - 1` — so a
/// literals stream refills once every ~6 symbols instead of every ~4.
///
/// Used on the *tail* of a literals stream; the body reloads on a fixed
/// schedule through [`BitCursor::advance`] + [`BitCursor::refill`] instead,
/// because a conditional reload in that loop is a data-dependent branch that
/// mispredicts.
const REFILL_AT_NARROW: u32 = 64 - MAX_NARROW_BITS as u32;

/// Load the eight bytes at `byte_ptr` little-endian, zero-filling anything
/// outside `data`.
#[inline(always)]
fn load_container(data: &[u8], byte_ptr: i64) -> u64 {
    let len = data.len() as i64;
    if byte_ptr >= 0 && byte_ptr + 8 <= len {
        // The common case: a whole register inside the slice. One bounds
        // check for eight bytes, and no call.
        let p = byte_ptr as usize;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&data[p..p + 8]);
        return u64::from_le_bytes(bytes);
    }
    load_container_edge(data, byte_ptr)
}

/// The straddling case of [`load_container`], kept out of line so the hot path
/// is a load and a compare.
///
/// Reached at most twice per stream in a well-formed decode (the initial load
/// of a stream shorter than eight bytes, and the final refill once the cursor
/// nears byte 0) and repeatedly only while an over-read is being detected.
#[cold]
#[inline(never)]
fn load_container_edge(data: &[u8], byte_ptr: i64) -> u64 {
    let len = data.len() as i64;
    let mut value = 0u64;
    for k in 0..8i64 {
        let idx = byte_ptr + k;
        if idx >= 0 && idx < len {
            value |= u64::from(data[idx as usize]) << (8 * k as u32);
        }
    }
    value
}

impl BitCursor {
    /// Peek `n` bits (`n <= 32`) below the current position.
    ///
    /// If fewer than `n` bits remain, the missing low bits read as zero
    /// (mirroring the reference container behaviour near the stream start).
    #[inline(always)]
    pub(crate) fn peek(&self, n: u8) -> u32 {
        debug_assert!(n <= MAX_FIELD_BITS);
        debug_assert!(self.bits_consumed + u32::from(n) <= 64);
        // `>> 1` then `>> (63 - n)` is the reference `BIT_lookBits` shift
        // pair: it totals `64 - n` and stays defined at `n == 0`, where a
        // single `>> 64` would not.
        let shifted = self.container << self.bits_consumed;
        (((shifted >> 1) >> (63 - u32::from(n))) & 0xFFFF_FFFF) as u32
    }

    /// Consume `n` bits (`n <= 32`), reloading the container when at least
    /// four bytes have gone.
    #[inline(always)]
    pub(crate) fn skip(&mut self, data: &[u8], n: u8) {
        debug_assert!(n <= MAX_FIELD_BITS);
        self.bits_consumed += u32::from(n);
        if self.bits_consumed >= REFILL_AT {
            self.refill(data);
        }
    }

    /// Peek `n` bits with `1 <= n <= 12` — a Huffman prefix or an FSE-coded
    /// weight.
    ///
    /// One shift pair instead of [`peek`](Self::peek)'s three: the `>> 1`
    /// there exists only so `n == 0` stays defined, and a Huffman table log is
    /// never zero. Worth spelling out separately because this runs once per
    /// literal byte.
    #[inline(always)]
    pub(crate) fn peek_narrow(&self, n: u8) -> u32 {
        debug_assert!((1..=MAX_NARROW_BITS).contains(&n));
        debug_assert!(self.bits_consumed + u32::from(n) <= 64);
        ((self.container << self.bits_consumed) >> (64 - u32::from(n))) as u32
    }

    /// Consume `n` bits from a caller whose fields never exceed
    /// [`MAX_NARROW_BITS`], reloading only near the top of the container.
    #[inline(always)]
    pub(crate) fn skip_narrow(&mut self, data: &[u8], n: u8) {
        debug_assert!(n <= MAX_NARROW_BITS);
        self.bits_consumed += u32::from(n);
        if self.bits_consumed >= REFILL_AT_NARROW {
            self.refill(data);
        }
    }

    /// Consume `n` bits *without* reloading the container.
    ///
    /// For a loop that reloads on a fixed schedule rather than on a
    /// data-dependent test — see [`refill`](Self::refill). The caller owes the
    /// invariant that every following [`peek`](Self::peek) /
    /// [`peek_narrow`](Self::peek_narrow) before the next reload still fits in
    /// the container, which those methods assert in debug builds:
    ///
    /// * literals: four Huffman codes, at most 12 bits each, per reload;
    /// * sequences: `offset` + `match_length` extra bits (at most 47) per
    ///   reload, then `literal_length` extra plus the three state updates (at
    ///   most 42).
    #[inline(always)]
    pub(crate) fn advance(&mut self, n: u8) {
        debug_assert!(n <= MAX_FIELD_BITS);
        self.bits_consumed += u32::from(n);
        debug_assert!(self.bits_consumed <= 64);
    }

    /// Slide the container down over the bytes already consumed.
    #[inline(always)]
    pub(crate) fn refill(&mut self, data: &[u8]) {
        let whole_bytes = i64::from(self.bits_consumed >> 3);
        self.byte_ptr -= whole_bytes;
        self.bits_consumed &= 7;
        self.container = load_container(data, self.byte_ptr);
    }

    /// Number of unread data bits. Negative if the stream was over-read.
    #[inline]
    pub(crate) fn bits_remaining(&self) -> i64 {
        self.byte_ptr * 8 + 64 - i64::from(self.bits_consumed)
    }

    /// Whether the stream was consumed exactly.
    #[inline]
    pub(crate) fn is_finished(&self) -> bool {
        self.bits_remaining() == 0
    }
}

/// FSE/Huffman backward bitstream reader (RFC 8878).
///
/// Each [`read_bits`](Self::read_bits) returns the `n` bits immediately below
/// the current position, exactly like the reference `BIT_readBits`. Reads past
/// the beginning of the stream ("overflow") yield zero bits and set an
/// internal flag, matching `BIT_DStream_overflow`; callers decide whether that
/// terminates decoding (Huffman-weight streams) or is an error (sequence
/// streams).
pub(crate) struct FseBitReader<'a> {
    /// Input bytes.
    data: &'a [u8],
    /// Position within them.
    cursor: BitCursor,
}

impl<'a> FseBitReader<'a> {
    /// Create a reader positioned just below the stream's sentinel bit.
    ///
    /// # Errors
    ///
    /// The stream must be non-empty and its last byte non-zero (that byte
    /// carries the sentinel); both are corruption otherwise.
    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(OxiArcError::corrupted(0, "empty FSE bitstream"));
        }

        let last_byte = data[data.len() - 1];
        if last_byte == 0 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE bitstream has no sentinel bit (last byte is zero)",
            ));
        }

        // The sentinel is the highest set bit of the last byte; data bits sit
        // below it (and in all preceding bytes). `bits_consumed` starts at
        // `8 - sentinel_pos` (1..=8) so the cursor lands exactly on it — the
        // reference `8 - BIT_highbit32(lastByte)`.
        let sentinel_pos = 7 - last_byte.leading_zeros();
        let byte_ptr = data.len() as i64 - 8;
        Ok(Self {
            data,
            cursor: BitCursor {
                container: load_container(data, byte_ptr),
                byte_ptr,
                bits_consumed: 8 - sentinel_pos,
            },
        })
    }

    /// The input slice, for a loop that drives a detached [`BitCursor`].
    #[inline]
    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Take a copy of the position for a register-resident inner loop.
    #[inline]
    pub(crate) fn detach(&self) -> BitCursor {
        self.cursor
    }

    /// Number of unread data bits. Negative if the stream was over-read.
    #[inline]
    pub(crate) fn bits_remaining(&self) -> i64 {
        self.cursor.bits_remaining()
    }

    /// Whether more bits were consumed than the stream contains.
    #[inline]
    pub(crate) fn is_overflowed(&self) -> bool {
        self.bits_remaining() < 0
    }

    /// Whether the stream was consumed exactly (all data bits read, no
    /// overflow).
    #[inline]
    pub(crate) fn is_finished(&self) -> bool {
        self.cursor.is_finished()
    }

    /// Peek `n` bits (`n <= 32`) below the current position without consuming.
    #[inline]
    pub(crate) fn peek_bits(&self, n: u8) -> u32 {
        self.cursor.peek(n)
    }

    /// Consume `n` bits without returning them.
    #[inline]
    pub(crate) fn skip_bits(&mut self, n: u8) {
        self.cursor.skip(self.data, n);
    }

    /// Read `n` bits (`n <= 32`) from the stream.
    ///
    /// Over-reads return zero-padded values and mark the reader overflowed.
    #[inline]
    pub(crate) fn read_bits(&mut self, n: u8) -> u32 {
        let value = self.cursor.peek(n);
        self.cursor.skip(self.data, n);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-gathering reader this module replaced, kept verbatim as the
    /// differential oracle: `peek` as it was written, driven by an explicit
    /// `bits_remaining` cursor.
    ///
    /// Every behaviour the decoders depend on is encoded here — the
    /// zero-padding when the field runs off the front of the stream, the
    /// negative cursor after an over-read, and the exact `bits_remaining`
    /// value that reaches the "{n} bits left" corruption message — so a
    /// divergence in any of them fails as a test rather than as wrong output.
    fn peek_bits_reference(data: &[u8], bits_remaining: i64, n: u8) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let end = bits_remaining;
        if end <= 0 {
            return 0;
        }
        let start = end - i64::from(n);
        let lo = start.max(0);
        let first_byte = (lo / 8) as usize;
        let last_byte = ((end - 1) / 8) as usize;

        let mut window = 0u64;
        for (k, idx) in (first_byte..=last_byte).enumerate() {
            if let Some(&b) = data.get(idx) {
                window |= u64::from(b) << (8 * k);
            }
        }

        let shift = (lo - first_byte as i64 * 8) as u32;
        let avail = (end - lo) as u32;
        let field = (window >> shift) & ((1u64 << avail) - 1);
        let result = if start < 0 {
            field << ((-start) as u32)
        } else {
            field
        };
        result as u32
    }

    /// The initial cursor the old reader computed.
    fn initial_bits_remaining(data: &[u8]) -> i64 {
        let last_byte = data[data.len() - 1];
        let sentinel_pos = 7 - i64::from(last_byte.leading_zeros());
        (data.len() as i64 - 1) * 8 + sentinel_pos
    }

    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[test]
    fn empty_and_zero_sentinel_streams_are_refused() {
        assert!(FseBitReader::new(&[]).is_err());
        assert!(FseBitReader::new(&[0]).is_err());
        assert!(FseBitReader::new(&[0xFF, 0x00]).is_err());
        assert!(FseBitReader::new(&[0x00, 0x01]).is_ok());
    }

    #[test]
    fn initial_position_matches_the_byte_gathering_reader() {
        for len in 1..=40usize {
            for last in [0x01u8, 0x02, 0x40, 0x80, 0xFF, 0x81] {
                let mut data = vec![0xA5u8; len];
                data[len - 1] = last;
                let reader = FseBitReader::new(&data).expect("valid sentinel");
                assert_eq!(
                    reader.bits_remaining(),
                    initial_bits_remaining(&data),
                    "len {len} last {last:#04x}"
                );
            }
        }
    }

    /// The differential oracle: random data, random `(peek, skip)` patterns,
    /// asserting every observable at every step.
    #[test]
    fn every_observable_matches_the_byte_gathering_reader() {
        let mut state = 0x5EED_1234_ABCD_0001u64;
        let mut checks = 0usize;
        for len in [1usize, 2, 3, 4, 5, 7, 8, 9, 12, 16, 17, 31, 33, 64, 129] {
            for trial in 0..24 {
                let mut data = vec![0u8; len];
                for b in data.iter_mut() {
                    *b = (next_rand(&mut state) & 0xFF) as u8;
                }
                // Guarantee a sentinel.
                data[len - 1] |= 1 << (trial % 8);

                let mut reader = FseBitReader::new(&data).expect("valid sentinel");
                let mut cursor = initial_bits_remaining(&data);
                assert_eq!(reader.bits_remaining(), cursor);

                // Walk well past the front of the stream so the over-read
                // behaviour is exercised too.
                for step in 0..(len * 8 / 3 + 24) {
                    // A spread of widths including 0 and the 32-bit maximum.
                    let n = match (next_rand(&mut state) % 10) as u8 {
                        0 => 0,
                        1 => 1,
                        2 => 32,
                        3 => 31,
                        other => ((next_rand(&mut state) % 17) as u8) + other,
                    }
                    .min(32);

                    let expected = peek_bits_reference(&data, cursor, n);
                    let got = reader.peek_bits(n);
                    assert_eq!(
                        got, expected,
                        "peek({n}) at cursor {cursor}, len {len}, trial {trial}, step {step}"
                    );
                    // Peeking must not move the cursor.
                    assert_eq!(reader.bits_remaining(), cursor);

                    let read = reader.read_bits(n);
                    assert_eq!(read, expected, "read({n}) differs from peek({n})");
                    cursor -= i64::from(n);

                    assert_eq!(reader.bits_remaining(), cursor, "cursor after read({n})");
                    assert_eq!(reader.is_overflowed(), cursor < 0);
                    assert_eq!(reader.is_finished(), cursor == 0);
                    checks += 1;
                }
            }
        }
        assert!(checks > 20_000, "only {checks} comparisons");
    }

    #[test]
    fn skip_and_read_agree_step_for_step() {
        let mut state = 0xC0FF_EE00_1234_5678u64;
        for len in [1usize, 5, 8, 13, 40] {
            let mut data = vec![0u8; len];
            for b in data.iter_mut() {
                *b = (next_rand(&mut state) & 0xFF) as u8;
            }
            data[len - 1] |= 0x80;

            let mut skipper = FseBitReader::new(&data).expect("valid sentinel");
            let mut reader = FseBitReader::new(&data).expect("valid sentinel");
            for _ in 0..(len * 8 + 40) {
                let n = (next_rand(&mut state) % 33) as u8;
                let peeked = skipper.peek_bits(n);
                skipper.skip_bits(n);
                let read = reader.read_bits(n);
                assert_eq!(peeked, read);
                assert_eq!(skipper.bits_remaining(), reader.bits_remaining());
            }
        }
    }

    #[test]
    fn a_stream_consumed_exactly_reports_finished() {
        // 0b1_0110_1010: sentinel then 9 data bits (0x6A + one high bit).
        let data = [0x6A, 0x01];
        let mut reader = FseBitReader::new(&data).expect("valid sentinel");
        assert_eq!(reader.bits_remaining(), 8);
        assert!(!reader.is_finished());
        assert_eq!(reader.read_bits(8), 0x6A);
        assert!(reader.is_finished());
        assert!(!reader.is_overflowed());
        // One more bit tips it into overflow, reading zero.
        assert_eq!(reader.read_bits(1), 0);
        assert!(reader.is_overflowed());
        assert_eq!(reader.bits_remaining(), -1);
    }

    #[test]
    fn deep_over_read_stays_zero_and_never_panics() {
        let data = [0xFF, 0xFF, 0x80];
        let mut reader = FseBitReader::new(&data).expect("valid sentinel");
        let mut total = 0u64;
        for _ in 0..4096 {
            total |= u64::from(reader.read_bits(32));
        }
        assert!(reader.is_overflowed());
        // The first reads return real bits; everything after the front of the
        // stream is zero, and the cursor keeps counting down.
        assert_eq!(reader.bits_remaining(), 23 - 4096 * 32);
        let _ = total;
    }

    #[test]
    fn the_refill_boundary_is_crossed_without_drift() {
        // 3-bit reads walk the cursor across every byte boundary and force a
        // refill every 32 bits; the derived cursor must stay exact.
        let data: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(37) | 1).collect();
        let mut reader = FseBitReader::new(&data).expect("valid sentinel");
        let mut cursor = initial_bits_remaining(&data);
        for _ in 0..200 {
            let expected = peek_bits_reference(&data, cursor, 3);
            assert_eq!(reader.read_bits(3), expected, "cursor {cursor}");
            cursor -= 3;
            assert_eq!(reader.bits_remaining(), cursor);
        }
    }
}
