//! Tests for the LZX decoder.
//!
//! Every bitstream here is assembled bit by bit by the test itself from the
//! format description, never produced by the decoder's own inverse, so a
//! shared misreading of the format cannot round-trip past these checks. The
//! position-slot tables are regenerated from the recurrence the
//! specification states rather than compared against a copy of themselves.

use super::*;

/// Window exponent used by most tests: 32 KiB, the format minimum, which
/// makes a frame and the whole window the same size and therefore exercises
/// window wrapping on the very second frame.
const WB: u8 = 15;

/// Main-tree alphabet size for [`WB`].
const MAIN_SYMBOLS: usize = NUM_CHARS + 30 * 8;

// ---------------------------------------------------------------------------
// Bit-level encoder used to build test streams
// ---------------------------------------------------------------------------

/// Writes the 16-bit little-endian, MSB-first bitstream the decoder reads.
struct BitWriter {
    out: Vec<u8>,
    buffer: u32,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            buffer: 0,
            bits: 0,
        }
    }

    fn write(&mut self, value: u32, n: u32) {
        if n == 0 {
            return;
        }
        assert!(n <= 17, "LZX never writes more than 17 bits at once");
        let mask = (1u32 << n) - 1;
        self.buffer = (self.buffer << n) | (value & mask);
        self.bits += n;
        while self.bits >= 16 {
            let word = ((self.buffer >> (self.bits - 16)) & 0xFFFF) as u16;
            self.out.extend_from_slice(&word.to_le_bytes());
            self.bits -= 16;
        }
        if self.bits == 0 {
            self.buffer = 0;
        } else {
            self.buffer &= (1u32 << self.bits) - 1;
        }
    }

    /// Pad with 0 to 15 zero bits so the next write starts on a word.
    fn pad_to_word(&mut self) {
        if self.bits > 0 {
            let n = 16 - self.bits;
            self.write(0, n);
        }
    }

    /// Pad with 1 to 16 zero bits, as an uncompressed block header requires.
    fn pad_at_least_one(&mut self) {
        let n = 16 - self.bits;
        self.write(0, n);
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        assert_eq!(self.bits, 0, "raw bytes must start on a word boundary");
        self.out.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        self.pad_to_word();
        self.out
    }
}

/// Canonical code assignment: codes ascend within each length, lengths
/// ascend overall. Written independently of [`HuffTable`] so the two can be
/// checked against each other.
fn canonical_codes(lengths: &[u8]) -> Vec<(u32, u32)> {
    let mut out = vec![(0u32, 0u32); lengths.len()];
    let max = lengths.iter().copied().max().unwrap_or(0);
    let mut code = 0u32;
    for len in 1..=max {
        for (symbol, &have) in lengths.iter().enumerate() {
            if have == len {
                out[symbol] = (code, u32::from(len));
                code += 1;
            }
        }
        code <<= 1;
    }
    out
}

/// Give every symbol in `used` the same code length, padding the live set
/// with unused symbols up to a power of two so the code is exactly complete.
fn uniform_lengths(alphabet: usize, used: &[usize]) -> Vec<u8> {
    let mut symbols: Vec<usize> = used.to_vec();
    symbols.sort_unstable();
    symbols.dedup();
    assert!(!symbols.is_empty());
    let mut size = 2usize;
    while size < symbols.len() {
        size *= 2;
    }
    let mut candidate = 0usize;
    while symbols.len() < size {
        if !symbols.contains(&candidate) {
            symbols.push(candidate);
        }
        candidate += 1;
    }
    let bits = size.trailing_zeros() as u8;
    let mut lengths = vec![0u8; alphabet];
    for symbol in symbols {
        lengths[symbol] = bits;
    }
    lengths
}

/// Emit the pretree and the delta symbols for `lengths[first..last]`.
///
/// Only single-delta pretree symbols are used, which keeps the encoder
/// trivial; the run symbols 17, 18 and 19 are covered directly by
/// [`read_lengths_decodes_runs_and_deltas`].
fn write_lengths(writer: &mut BitWriter, previous: &[u8], new: &[u8], first: usize, last: usize) {
    let deltas: Vec<usize> = (first..last)
        .map(|index| (i16::from(previous[index]) - i16::from(new[index])).rem_euclid(17) as usize)
        .collect();
    let pretree = uniform_lengths(PRETREE_ELEMENTS, &deltas);
    for &len in &pretree {
        writer.write(u32::from(len), 4);
    }
    let codes = canonical_codes(&pretree);
    for delta in deltas {
        let (code, len) = codes[delta];
        writer.write(code, len);
    }
}

/// Codes for the three alphabets of one compressed block.
struct BlockCodes {
    main: Vec<(u32, u32)>,
    length: Vec<(u32, u32)>,
    aligned: Vec<(u32, u32)>,
}

/// Builds LZX streams block by block, carrying the previous block's code
/// lengths forward exactly as the format's delta coding requires.
struct StreamEncoder {
    writer: BitWriter,
    main_previous: Vec<u8>,
    length_previous: Vec<u8>,
}

impl StreamEncoder {
    fn new(intel_filesize: Option<u32>) -> Self {
        let mut writer = BitWriter::new();
        match intel_filesize {
            Some(size) => {
                writer.write(1, 1);
                writer.write(size >> 16, 16);
                writer.write(size & 0xFFFF, 16);
            }
            None => writer.write(0, 1),
        }
        Self {
            writer,
            main_previous: vec![0u8; MAIN_SYMBOLS],
            length_previous: vec![0u8; NUM_SECONDARY_LENGTHS],
        }
    }

    fn header(&mut self, block_type: u32, block_length: u32) {
        self.writer.write(block_type, 3);
        self.writer.write(block_length >> 8, 16);
        self.writer.write(block_length & 0xFF, 8);
    }

    /// Open a verbatim or aligned-offset block and return its code tables.
    fn compressed_block(
        &mut self,
        block_length: u32,
        main: &[u8],
        length: &[u8],
        aligned: Option<[u8; ALIGNED_ELEMENTS]>,
    ) -> BlockCodes {
        self.header(if aligned.is_some() { 2 } else { 1 }, block_length);
        if let Some(aligned) = aligned {
            for &len in &aligned {
                self.writer.write(u32::from(len), 3);
            }
        }
        write_lengths(&mut self.writer, &self.main_previous, main, 0, NUM_CHARS);
        write_lengths(
            &mut self.writer,
            &self.main_previous,
            main,
            NUM_CHARS,
            MAIN_SYMBOLS,
        );
        write_lengths(
            &mut self.writer,
            &self.length_previous,
            length,
            0,
            NUM_SECONDARY_LENGTHS,
        );
        self.main_previous = main.to_vec();
        self.length_previous = length.to_vec();
        BlockCodes {
            main: canonical_codes(main),
            length: canonical_codes(length),
            aligned: canonical_codes(&aligned.unwrap_or([0u8; ALIGNED_ELEMENTS])),
        }
    }

    /// Emit a complete uncompressed block.
    fn uncompressed_block(&mut self, repeated: [u32; 3], payload: &[u8]) {
        self.header(3, payload.len() as u32);
        self.writer.pad_at_least_one();
        for value in repeated {
            let bytes = value.to_le_bytes();
            self.writer.push_bytes(&bytes);
        }
        self.writer.push_bytes(payload);
        if payload.len() % 2 == 1 {
            self.writer.push_bytes(&[0]);
        }
    }

    fn emit(&mut self, code: (u32, u32)) {
        assert!(code.1 > 0, "symbol has no code");
        self.writer.write(code.0, code.1);
    }

    fn finish(self) -> Vec<u8> {
        self.writer.finish()
    }
}

/// Main-tree symbol for a match with the given position slot and length.
fn match_symbol(slot: usize, length: usize) -> usize {
    assert!(length >= MIN_MATCH);
    let primary = (length - MIN_MATCH).min(NUM_PRIMARY_LENGTHS);
    NUM_CHARS + slot * 8 + primary
}

fn decode_all(stream: &[u8], out_len: usize) -> Result<Vec<u8>> {
    LzxDecoder::new(WB)?.decompress(stream, out_len)
}

// ---------------------------------------------------------------------------
// Table and helper checks
// ---------------------------------------------------------------------------

/// The position-slot tables are regenerated from the recurrence in the
/// specification: slot pairs share an extra-bit count that climbs by one per
/// pair after the first and saturates at 17, and each base is the running sum
/// of `1 << extra_bits`.
#[test]
fn position_tables_match_recurrence() {
    let mut extra = [0u8; MAX_POSITION_SLOTS];
    let mut bits = 0u8;
    let mut slot = 0usize;
    while slot + 1 < MAX_POSITION_SLOTS {
        extra[slot] = bits;
        extra[slot + 1] = bits;
        if slot != 0 && bits < 17 {
            bits += 1;
        }
        slot += 2;
    }
    if slot < MAX_POSITION_SLOTS {
        extra[slot] = bits;
    }
    assert_eq!(extra, EXTRA_BITS);

    let mut base = [0u32; MAX_POSITION_SLOTS];
    let mut running = 0u32;
    for (index, slot) in base.iter_mut().enumerate() {
        *slot = running;
        running += 1u32 << extra[index];
    }
    assert_eq!(base, POSITION_BASE);
}

#[test]
fn position_slot_counts_match_spec() {
    for bits in MIN_WINDOW_BITS..=19 {
        assert_eq!(position_slots(bits), usize::from(bits) * 2);
    }
    assert_eq!(position_slots(20), 42);
    assert_eq!(position_slots(21), 50);
}

#[test]
fn window_exponent_outside_range_is_rejected() {
    for bits in [0u8, 14, 22, 31] {
        assert!(
            LzxDecoder::new(bits).is_err(),
            "window exponent {bits} must be rejected"
        );
    }
    for bits in MIN_WINDOW_BITS..=MAX_WINDOW_BITS {
        assert!(LzxDecoder::new(bits).is_ok(), "exponent {bits} is valid");
    }
}

/// The decoding table must agree with an independently written canonical
/// code assignment for every symbol, at several distinct code lengths.
#[test]
fn huffman_table_matches_independent_canonical_codes() {
    let lengths: Vec<u8> = vec![2, 0, 3, 1, 4, 4, 0, 0];
    let table = HuffTable::build(&lengths).expect("complete code");
    let codes = canonical_codes(&lengths);

    for (symbol, &(code, len)) in codes.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let mut writer = BitWriter::new();
        writer.write(code, len);
        // Pad so the reader always has a full word to work with.
        writer.write(0, 16);
        let stream = writer.finish();
        let mut reader = BitReader::new(&stream, BitState::default());
        assert_eq!(
            table.decode(&mut reader).expect("decodes"),
            symbol as u16,
            "symbol {symbol} with code {code:b}/{len}"
        );
    }
}

#[test]
fn huffman_table_rejects_incomplete_and_oversubscribed() {
    assert!(HuffTable::build(&[1, 0, 0]).is_err(), "incomplete");
    assert!(HuffTable::build(&[1, 1, 1]).is_err(), "over-subscribed");
    assert!(HuffTable::build(&[17, 0]).is_err(), "length above 16");
    assert!(HuffTable::build(&[1, 1]).is_ok(), "complete");
}

#[test]
fn empty_huffman_table_decodes_to_error() {
    let table = HuffTable::build(&[0, 0, 0]).expect("all-zero lengths are legal");
    let stream = [0xFFu8; 8];
    let mut reader = BitReader::new(&stream, BitState::default());
    assert!(table.decode(&mut reader).is_err());
}

/// Pretree symbols 17, 18 and 19 and the modulo-17 delta rule, checked
/// against a hand-computed expectation.
#[test]
fn read_lengths_decodes_runs_and_deltas() {
    // Live pretree symbols, uniform 3-bit codes: ranks follow symbol order.
    let live = [0usize, 1, 2, 9, 16, 17, 18, 19];
    let pretree = uniform_lengths(PRETREE_ELEMENTS, &live);
    let codes = canonical_codes(&pretree);

    let mut writer = BitWriter::new();
    for &len in &pretree {
        writer.write(u32::from(len), 4);
    }
    // lengths[0]: delta 9 against a previous 5 gives (5 - 9) mod 17 = 13.
    writer.write(codes[9].0, codes[9].1);
    // lengths[1..6]: symbol 19, run bit 1 gives 5, repeated delta 2 gives 3.
    writer.write(codes[19].0, codes[19].1);
    writer.write(1, 1);
    writer.write(codes[2].0, codes[2].1);
    // lengths[6..13]: symbol 17, 4-bit run field 3 gives 7 zeros.
    writer.write(codes[17].0, codes[17].1);
    writer.write(3, 4);
    // lengths[13..30]: symbol 18, 5-bit run field 0 gives 20, clamped to 30.
    writer.write(codes[18].0, codes[18].1);
    writer.write(0, 5);
    writer.write(0, 16);
    let stream = writer.finish();

    let mut lengths = vec![5u8; 30];
    let mut reader = BitReader::new(&stream, BitState::default());
    read_lengths(&mut reader, &mut lengths, 0, 30).expect("lengths decode");

    let mut expected = vec![0u8; 30];
    expected[0] = 13;
    for slot in expected.iter_mut().take(6).skip(1) {
        *slot = 3;
    }
    assert_eq!(lengths, expected);
}

#[test]
fn apply_delta_wraps_modulo_seventeen() {
    assert_eq!(apply_delta(5, 9), 13);
    assert_eq!(apply_delta(0, 0), 0);
    assert_eq!(apply_delta(16, 16), 0);
    assert_eq!(apply_delta(0, 1), 16);
    for previous in 0..=16u8 {
        for delta in 0..=16u8 {
            assert!(apply_delta(previous, delta) <= 16);
        }
    }
}

// ---------------------------------------------------------------------------
// Uncompressed blocks
// ---------------------------------------------------------------------------

#[test]
fn uncompressed_block_roundtrip() {
    let payload: Vec<u8> = (0..200u32).map(|value| (value * 7) as u8).collect();
    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([1, 1, 1], &payload);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, payload.len()).expect("uncompressed block decodes");
    assert_eq!(decoded, payload);
}

/// An odd-length uncompressed payload is followed by one padding byte; a
/// decoder that forgets it desynchronises on the next block.
#[test]
fn uncompressed_block_odd_length_skips_pad_byte() {
    let first: Vec<u8> = b"odd length payload!".to_vec();
    assert_eq!(first.len() % 2, 1);
    let second: Vec<u8> = b"second block".to_vec();

    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([1, 1, 1], &first);
    encoder.uncompressed_block([2, 3, 4], &second);
    let stream = encoder.finish();

    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    let decoded = decode_all(&stream, expected.len()).expect("two blocks decode");
    assert_eq!(decoded, expected);
}

// ---------------------------------------------------------------------------
// Verbatim blocks
// ---------------------------------------------------------------------------

#[test]
fn verbatim_block_literals_only() {
    let text = b"hello world";
    let live: Vec<usize> = {
        let mut set: Vec<usize> = text.iter().map(|&b| usize::from(b)).collect();
        set.sort_unstable();
        set.dedup();
        set
    };
    let main = uniform_lengths(MAIN_SYMBOLS, &live);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(text.len() as u32, &main, &length, None);
    for &byte in text {
        encoder.emit(codes.main[usize::from(byte)]);
    }
    let stream = encoder.finish();

    let decoded = decode_all(&stream, text.len()).expect("verbatim block decodes");
    assert_eq!(decoded, text);
}

/// `abcabc`: three literals then a match of length three at distance three,
/// which lands on position slot 4 with one verbatim extra bit.
#[test]
fn verbatim_block_literal_then_match() {
    let slot = 4usize;
    assert_eq!(EXTRA_BITS[slot], 1);
    // distance + 2 == POSITION_BASE[slot] + extra
    let extra_value = 3u32 + 2 - POSITION_BASE[slot];
    assert!(extra_value < 2);

    let symbol = match_symbol(slot, 3);
    let live = vec![97usize, 98, 99, symbol];
    let main = uniform_lengths(MAIN_SYMBOLS, &live);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(6, &main, &length, None);
    for byte in [97usize, 98, 99] {
        encoder.emit(codes.main[byte]);
    }
    encoder.emit(codes.main[symbol]);
    encoder.writer.write(extra_value, 1);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, 6).expect("match decodes");
    assert_eq!(decoded, b"abcabc");
}

/// Matches of nine bytes or more take a second symbol from the length tree.
#[test]
fn length_tree_extends_long_matches() {
    let slot = 3usize; // distance 1
    let total = 40usize; // 1 literal + 39 repeats
    let match_length = total - 1;
    assert!(match_length >= MIN_MATCH + NUM_PRIMARY_LENGTHS);
    let footer = match_length - MIN_MATCH - NUM_PRIMARY_LENGTHS;

    let symbol = match_symbol(slot, match_length);
    let main = uniform_lengths(MAIN_SYMBOLS, &[usize::from(b'Z'), symbol]);
    let length = uniform_lengths(NUM_SECONDARY_LENGTHS, &[footer]);

    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(total as u32, &main, &length, None);
    encoder.emit(codes.main[usize::from(b'Z')]);
    encoder.emit(codes.main[symbol]);
    encoder.emit(codes.length[footer]);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, total).expect("long match decodes");
    assert_eq!(decoded, vec![b'Z'; total]);
}

/// When a match needs both a length-tree footer and offset extra bits, the
/// footer comes first. Swapping the two produces a stream that still decodes
/// to *something*, which is exactly why the order needs its own test.
#[test]
fn length_footer_precedes_offset_extra_bits() {
    let slot = 6usize; // base 8, two extra bits
    assert_eq!(EXTRA_BITS[slot], 2);
    let distance = 9usize;
    let extra_value = (distance + 2) as u32 - POSITION_BASE[slot];
    let literals = 16usize;
    let match_length = 20usize;
    let footer = match_length - MIN_MATCH - NUM_PRIMARY_LENGTHS;

    let symbol = match_symbol(slot, match_length);
    let alphabet: Vec<u8> = (b'a'..b'a' + 16).collect();
    let mut live: Vec<usize> = alphabet.iter().map(|&b| usize::from(b)).collect();
    live.push(symbol);
    let main = uniform_lengths(MAIN_SYMBOLS, &live);
    let length = uniform_lengths(NUM_SECONDARY_LENGTHS, &[footer]);

    let total = literals + match_length;
    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(total as u32, &main, &length, None);
    for &byte in &alphabet {
        encoder.emit(codes.main[usize::from(byte)]);
    }
    encoder.emit(codes.main[symbol]);
    encoder.emit(codes.length[footer]);
    encoder.writer.write(extra_value, 2);
    let stream = encoder.finish();

    let mut expected = alphabet.clone();
    for _ in 0..match_length {
        let byte = expected[expected.len() - distance];
        expected.push(byte);
    }
    let decoded = decode_all(&stream, total).expect("decodes");
    assert_eq!(decoded, expected);
}

/// Slot 0 repeats R0 unchanged; slots 1 and 2 promote their entry to the
/// front of the queue. A stream that reuses several distances in turn only
/// decodes correctly if that promotion is right.
#[test]
fn repeated_offset_queue_promotes_entries() {
    // Literals "0123456789ABCDEF" then matches that exercise the queue.
    let alphabet: Vec<u8> = (b'A'..b'A' + 16).collect();
    let far_slot = 6usize; // base 8, 2 extra bits
    assert_eq!(EXTRA_BITS[far_slot], 2);

    let literal_symbols: Vec<usize> = alphabet.iter().map(|&b| usize::from(b)).collect();
    let sym_far = match_symbol(far_slot, 2);
    let sym_slot3 = match_symbol(3, 2); // distance 1
    let sym_r0 = match_symbol(0, 2);
    let sym_r1 = match_symbol(1, 2);
    let sym_r2 = match_symbol(2, 2);

    let mut live = literal_symbols.clone();
    live.extend_from_slice(&[sym_far, sym_slot3, sym_r0, sym_r1, sym_r2]);
    let main = uniform_lengths(MAIN_SYMBOLS, &live);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    // 16 literals, then: match d=9 (slot 6), match d=1 (slot 3),
    // then R0 (=1), R1 (=9), R2 (=1 from before the R1 promotion).
    let total = 16 + 2 * 5;
    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(total as u32, &main, &length, None);
    for &byte in &alphabet {
        encoder.emit(codes.main[usize::from(byte)]);
    }
    encoder.emit(codes.main[sym_far]);
    encoder.writer.write(9 + 2 - POSITION_BASE[far_slot], 2);
    encoder.emit(codes.main[sym_slot3]);
    encoder.emit(codes.main[sym_r0]);
    encoder.emit(codes.main[sym_r1]);
    encoder.emit(codes.main[sym_r2]);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, total).expect("LRU matches decode");

    // Reference model of the same queue, applied to the same distances.
    let mut expected = alphabet.clone();
    let mut queue = [1u32, 1, 1];
    let apply = |slot: usize, expected: &mut Vec<u8>, queue: &mut [u32; 3]| {
        let distance = match slot {
            6 => 9u32,
            3 => 1,
            other => queue[other],
        } as usize;
        if slot >= 3 {
            queue[2] = queue[1];
            queue[1] = queue[0];
            queue[0] = distance as u32;
        } else if slot != 0 {
            queue.swap(0, slot);
        }
        for _ in 0..2 {
            let byte = expected[expected.len() - distance];
            expected.push(byte);
        }
    };
    apply(6, &mut expected, &mut queue);
    apply(3, &mut expected, &mut queue);
    apply(0, &mut expected, &mut queue);
    apply(1, &mut expected, &mut queue);
    apply(2, &mut expected, &mut queue);

    assert_eq!(decoded, expected);
    assert_eq!(decoded.len(), total);
}

// ---------------------------------------------------------------------------
// Aligned-offset blocks
// ---------------------------------------------------------------------------

/// An aligned-offset block takes the low three bits of a distance from the
/// aligned tree: exactly those bits for slots with three extra bits, and the
/// remaining bits verbatim above that.
#[test]
fn aligned_block_uses_aligned_tree() {
    // Slot 8 has three extra bits (aligned symbol only); slot 12 has five
    // (two verbatim bits plus an aligned symbol).
    assert_eq!(EXTRA_BITS[8], 3);
    assert_eq!(EXTRA_BITS[12], 5);

    let aligned_lengths = [3u8; ALIGNED_ELEMENTS];
    let distance_a = POSITION_BASE[8] as usize - 2 + 5; // aligned symbol 5
    let distance_b = POSITION_BASE[12] as usize - 2 + (1 << 3) + 3; // v=1, a=3

    let literals = 96usize;
    assert!(distance_b < literals);

    let sym_a = match_symbol(8, 4);
    let sym_b = match_symbol(12, 4);
    let mut live: Vec<usize> = (0..literals).map(|index| index % 64 + 32).collect();
    live.push(sym_a);
    live.push(sym_b);
    let main = uniform_lengths(MAIN_SYMBOLS, &live);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let total = literals + 8;
    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(total as u32, &main, &length, Some(aligned_lengths));
    let mut expected = Vec::new();
    for index in 0..literals {
        let byte = (index % 64 + 32) as u8;
        encoder.emit(codes.main[usize::from(byte)]);
        expected.push(byte);
    }
    encoder.emit(codes.main[sym_a]);
    encoder.emit(codes.aligned[5]);
    for _ in 0..4 {
        let byte = expected[expected.len() - distance_a];
        expected.push(byte);
    }
    encoder.emit(codes.main[sym_b]);
    encoder.writer.write(1, 2);
    encoder.emit(codes.aligned[3]);
    for _ in 0..4 {
        let byte = expected[expected.len() - distance_b];
        expected.push(byte);
    }
    let stream = encoder.finish();

    let decoded = decode_all(&stream, total).expect("aligned block decodes");
    assert_eq!(decoded, expected);
}

// ---------------------------------------------------------------------------
// Cross-block state
// ---------------------------------------------------------------------------

/// The second block's code lengths are deltas against the first block's, so a
/// decoder that resets its tables between blocks decodes garbage here.
#[test]
fn tree_lengths_carry_across_blocks() {
    let first_text = b"aaaabbbbccccdddd";
    let second_text = b"ddddccccbbbbaaaa";

    let first_live = vec![97usize, 98, 99, 100];
    let first_main = uniform_lengths(MAIN_SYMBOLS, &first_live);
    // A different, longer alphabet in the second block forces real deltas.
    let second_live = vec![97usize, 98, 99, 100, 101, 102, 103, 104];
    let second_main = uniform_lengths(MAIN_SYMBOLS, &second_live);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(first_text.len() as u32, &first_main, &length, None);
    for &byte in first_text {
        encoder.emit(codes.main[usize::from(byte)]);
    }
    let codes = encoder.compressed_block(second_text.len() as u32, &second_main, &length, None);
    for &byte in second_text {
        encoder.emit(codes.main[usize::from(byte)]);
    }
    let stream = encoder.finish();

    let mut expected = first_text.to_vec();
    expected.extend_from_slice(second_text);
    let decoded = decode_all(&stream, expected.len()).expect("two compressed blocks decode");
    assert_eq!(decoded, expected);
}

// ---------------------------------------------------------------------------
// x86 CALL translation
// ---------------------------------------------------------------------------

/// Build a two-frame stream whose second frame copies the `E8` sequence of
/// the first out of the sliding window.
fn e8_two_frame_stream() -> (Vec<u8>, usize) {
    let mut payload = vec![0x90u8; FRAME_SIZE];
    payload[16] = 0xE8;
    payload[17..21].copy_from_slice(&0x0000_0100i32.to_le_bytes());

    // Second frame: copy the five bytes at absolute offset 16, then filler.
    let distance = FRAME_SIZE - 16;
    let slot = 29usize;
    assert_eq!(EXTRA_BITS[slot], 13);
    let extra_value = (distance + 2) as u32 - POSITION_BASE[slot];
    assert!(extra_value < (1 << 13));

    let symbol = match_symbol(slot, 5);
    let filler = usize::from(b'x');
    let main = uniform_lengths(MAIN_SYMBOLS, &[filler, symbol]);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(Some(0x1000));
    encoder.uncompressed_block([1, 1, 1], &payload);
    let codes = encoder.compressed_block(16, &main, &length, None);
    encoder.emit(codes.main[symbol]);
    encoder.writer.write(extra_value, 13);
    for _ in 0..11 {
        encoder.emit(codes.main[filler]);
    }
    (encoder.finish(), FRAME_SIZE + 16)
}

/// Translation rewrites displacements in the emitted frame while the sliding
/// window keeps the original bytes, so a later match reproduces the
/// *untranslated* sequence and is then translated afresh for its own
/// position.
#[test]
fn e8_translation_uses_untranslated_window() {
    let (stream, out_len) = e8_two_frame_stream();
    let decoded = decode_all(&stream, out_len).expect("two frames decode");
    assert_eq!(decoded.len(), out_len);

    // Frame 0: the E8 at offset 16 carries absolute 0x100, so the emitted
    // displacement is 0x100 - 16.
    assert_eq!(decoded[16], 0xE8);
    assert_eq!(
        i32::from_le_bytes([decoded[17], decoded[18], decoded[19], decoded[20]]),
        0x100 - 16
    );

    // Frame 1 copies the window's untranslated 0x100 and translates it for
    // position 32768. Had the window been polluted with the translated value
    // this would read 0xF0 - 32768 instead.
    let base = FRAME_SIZE;
    assert_eq!(decoded[base], 0xE8);
    assert_eq!(
        i32::from_le_bytes([
            decoded[base + 1],
            decoded[base + 2],
            decoded[base + 3],
            decoded[base + 4],
        ]),
        0x100 - FRAME_SIZE as i32
    );
    assert_eq!(&decoded[base + 5..], &[b'x'; 11]);
}

/// With the header flag clear no translation happens at all, even though the
/// same `E8` bytes are present.
#[test]
fn e8_translation_absent_when_flag_clear() {
    let mut payload = vec![0x90u8; 64];
    payload[16] = 0xE8;
    payload[17..21].copy_from_slice(&0x0000_0100i32.to_le_bytes());

    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([1, 1, 1], &payload);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, payload.len()).expect("decodes");
    assert_eq!(decoded, payload);
}

/// Frames of ten bytes or fewer are never scanned.
#[test]
fn e8_translation_skips_short_frames() {
    let mut payload = vec![0x90u8; 10];
    payload[0] = 0xE8;
    payload[1..5].copy_from_slice(&0x0000_0100i32.to_le_bytes());

    let mut encoder = StreamEncoder::new(Some(0x1000));
    encoder.uncompressed_block([1, 1, 1], &payload);
    let stream = encoder.finish();

    let decoded = decode_all(&stream, payload.len()).expect("decodes");
    assert_eq!(decoded, payload);
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// The bitstream realigns to a 16-bit boundary at every frame boundary, so
/// the byte offset reached after one frame is exactly the size of that
/// frame's compressed data.
#[test]
fn input_position_tracks_frame_boundaries() {
    let (stream, out_len) = e8_two_frame_stream();
    let mut decoder = LzxDecoder::new(WB).expect("decoder");

    let first = decoder.decompress(&stream, FRAME_SIZE).expect("frame 0");
    assert_eq!(first.len(), FRAME_SIZE);
    let after_first = decoder.input_position();
    // Frame 0 is an uncompressed block: 8 bytes of header padding, 12 bytes
    // of stored offsets and the 32768-byte payload.
    assert_eq!(after_first, 8 + 12 + FRAME_SIZE);

    let second = decoder
        .decompress(&stream, out_len - FRAME_SIZE)
        .expect("frame 1");
    assert_eq!(second.len(), out_len - FRAME_SIZE);
    assert!(decoder.input_position() > after_first);
    assert!(decoder.input_position() <= stream.len());
}

/// Decoding frame by frame must produce the same bytes as decoding the whole
/// stream in one call.
#[test]
fn frame_by_frame_matches_single_call() {
    let (stream, out_len) = e8_two_frame_stream();
    let whole = decode_all(&stream, out_len).expect("single call");

    let mut decoder = LzxDecoder::new(WB).expect("decoder");
    let mut piecewise = decoder.decompress(&stream, FRAME_SIZE).expect("frame 0");
    piecewise.extend_from_slice(
        &decoder
            .decompress(&stream, out_len - FRAME_SIZE)
            .expect("frame 1"),
    );
    assert_eq!(whole, piecewise);
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[test]
fn undefined_block_type_is_rejected() {
    for block_type in [0u32, 4, 5, 6, 7] {
        let mut writer = BitWriter::new();
        writer.write(0, 1);
        writer.write(block_type, 3);
        writer.write(0, 16);
        writer.write(8, 8);
        writer.write(0, 16);
        let stream = writer.finish();
        assert!(
            decode_all(&stream, 8).is_err(),
            "block type {block_type} must be rejected"
        );
    }
}

#[test]
fn zero_length_block_is_rejected() {
    let mut writer = BitWriter::new();
    writer.write(0, 1);
    writer.write(3, 3);
    writer.write(0, 16);
    writer.write(0, 8);
    let stream = writer.finish();
    let result = decode_all(&stream, 8);
    assert!(matches!(result, Err(OxiArcError::CorruptedData { .. })));
}

/// A match cannot reach behind the start of the stream; the zero-filled
/// window must never be handed back as data.
#[test]
fn match_before_start_of_stream_is_rejected() {
    let slot = 6usize;
    let symbol = match_symbol(slot, 4);
    let main = uniform_lengths(MAIN_SYMBOLS, &[usize::from(b'Q'), symbol]);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(5, &main, &length, None);
    encoder.emit(codes.main[usize::from(b'Q')]);
    encoder.emit(codes.main[symbol]);
    // Distance 9 after a single literal reaches before the stream start.
    encoder.writer.write(9 + 2 - POSITION_BASE[slot], 2);
    let stream = encoder.finish();

    let result = decode_all(&stream, 5);
    assert!(
        matches!(result, Err(OxiArcError::InvalidDistance { .. })),
        "expected InvalidDistance, got {result:?}"
    );
}

/// A stored repeated offset of zero is not silently repaired.
#[test]
fn zero_stored_offset_is_rejected_when_used() {
    let payload = b"prefix bytes for the window".to_vec();
    let symbol = match_symbol(0, 4);
    let main = uniform_lengths(MAIN_SYMBOLS, &[usize::from(b'Q'), symbol]);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([0, 0, 0], &payload);
    let codes = encoder.compressed_block(4, &main, &length, None);
    encoder.emit(codes.main[symbol]);
    let stream = encoder.finish();

    let result = decode_all(&stream, payload.len() + 4);
    assert!(
        matches!(result, Err(OxiArcError::InvalidDistance { .. })),
        "expected InvalidDistance, got {result:?}"
    );
}

#[test]
fn truncated_stream_reports_eof() {
    let payload: Vec<u8> = (0..64u8).collect();
    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([1, 1, 1], &payload);
    let stream = encoder.finish();

    for cut in [1usize, 4, 12, 20, 40] {
        let truncated = &stream[..stream.len() - cut];
        assert!(
            decode_all(truncated, payload.len()).is_err(),
            "truncation by {cut} bytes must be reported"
        );
    }
}

/// A match that runs past the end of its block is rejected rather than
/// silently spilling into the next one.
#[test]
fn match_overrunning_its_block_is_rejected() {
    let slot = 3usize; // distance 1
    let symbol = match_symbol(slot, 8);
    let main = uniform_lengths(MAIN_SYMBOLS, &[usize::from(b'Q'), symbol]);
    let length = vec![0u8; NUM_SECONDARY_LENGTHS];

    // The block claims four bytes but the match alone produces eight.
    let mut encoder = StreamEncoder::new(None);
    let codes = encoder.compressed_block(4, &main, &length, None);
    encoder.emit(codes.main[usize::from(b'Q')]);
    encoder.emit(codes.main[symbol]);
    let stream = encoder.finish();

    let result = decode_all(&stream, 4);
    assert!(
        matches!(result, Err(OxiArcError::CorruptedData { .. })),
        "expected CorruptedData, got {result:?}"
    );
}

/// An over-subscribed pretree is rejected before any length is decoded.
#[test]
fn invalid_pretree_is_rejected() {
    let mut writer = BitWriter::new();
    writer.write(0, 1);
    writer.write(1, 3);
    writer.write(0, 16);
    writer.write(8, 8);
    // Twenty 4-bit pretree lengths, all 1: wildly over-subscribed.
    for _ in 0..PRETREE_ELEMENTS {
        writer.write(1, 4);
    }
    writer.write(0, 16);
    let stream = writer.finish();

    let result = decode_all(&stream, 8);
    assert!(matches!(result, Err(OxiArcError::CorruptedData { .. })));
}

/// Arbitrary bytes must never panic the decoder.
#[test]
fn arbitrary_input_never_panics() {
    let mut state = 0x1234_5678u32;
    for round in 0..256u32 {
        let len = 16 + (round as usize % 96);
        let mut data = vec![0u8; len];
        for slot in data.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *slot = (state >> 24) as u8;
        }
        let _ = decode_all(&data, 512);
    }
}

// ---------------------------------------------------------------------------
// Fixture shared with the Cabinet-level tests
// ---------------------------------------------------------------------------

/// Build a two-frame LZX stream split the way a Cabinet splits it: one
/// `CFDATA` record per 32 KiB output frame.
///
/// Frame 0 is an uncompressed block; frame 1 is a compressed block whose only
/// symbol is a match reaching back into frame 0. Decoding the second record
/// therefore needs the sliding window, the code lengths and the bit position
/// the first record left behind, which is exactly the state a per-record
/// decoder would throw away.
///
/// Returns the `CFDATA` payloads with their uncompressed sizes, and the bytes
/// the whole folder must decode to.
pub(crate) fn cab_two_record_fixture() -> (Vec<(Vec<u8>, u16)>, Vec<u8>) {
    let pattern: Vec<u8> = (0..FRAME_SIZE)
        .map(|index| ((index * 37 + 11) % 251) as u8)
        .collect();

    let distance = 32760usize;
    let slot = 29usize;
    let extra_value = (distance + 2) as u32 - POSITION_BASE[slot];
    let tail = 100usize;
    let footer = tail - MIN_MATCH - NUM_PRIMARY_LENGTHS;

    let symbol = match_symbol(slot, tail);
    let main = uniform_lengths(MAIN_SYMBOLS, &[symbol, usize::from(b'!')]);
    let length = uniform_lengths(NUM_SECONDARY_LENGTHS, &[footer]);

    let mut encoder = StreamEncoder::new(None);
    encoder.uncompressed_block([1, 1, 1], &pattern);
    // The uncompressed block occupies 4 header bytes, 12 stored offsets and
    // the payload; that is where the first frame's record ends.
    let split = 4 + 12 + FRAME_SIZE;
    let codes = encoder.compressed_block(tail as u32, &main, &length, None);
    // Symbol, then the length-tree footer, then the offset's extra bits.
    encoder.emit(codes.main[symbol]);
    encoder.emit(codes.length[footer]);
    encoder.writer.write(extra_value, 13);
    let stream = encoder.finish();

    let mut expected = pattern.clone();
    let start = FRAME_SIZE - distance;
    for index in 0..tail {
        expected.push(pattern[start + index]);
    }

    let records = vec![
        (stream[..split].to_vec(), FRAME_SIZE as u16),
        (stream[split..].to_vec(), tail as u16),
    ];
    (records, expected)
}

/// The fixture must be a genuinely valid stream in its own right.
#[test]
fn cab_fixture_decodes_as_one_stream() {
    let (records, expected) = cab_two_record_fixture();
    let stream: Vec<u8> = records.iter().flat_map(|(data, _)| data.clone()).collect();
    let decoded = decode_all(&stream, expected.len()).expect("fixture decodes");
    assert_eq!(decoded, expected);
}
