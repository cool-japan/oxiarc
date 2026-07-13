//! `-lh1-` (LZHUF: 4KB LZSS + adaptive Huffman) codec.
//!
//! `-lh1-` is the compression method used by LHarc 1.x, still ubiquitous in
//! real-world Japanese LZH archives. The algorithm follows the classic
//! Okumura/Yoshizaki `LZHUF.C` faithfully:
//!
//! * 4096-byte ring buffer initialised to spaces (`0x20`), write cursor
//!   starting at `RING_SIZE - MAX_MATCH`.
//! * A single adaptive Huffman tree over 314 symbols (256 literals + match
//!   lengths 3..=60), rebuilt when the root frequency reaches `0x8000`.
//! * Match positions encoded with a static canonical Huffman prefix for the
//!   upper 6 bits and 6 raw bits for the lower part.
//! * MSB-first bit packing (unlike lh4-lh7 in this crate, which use the
//!   LSB-first `oxiarc_core` bitstream).
//!
//! The decoder is a byte-exact port of an implementation validated against
//! archives produced by reference tools. The encoder is a straightforward
//! greedy matcher that produces spec-conformant streams (used both for
//! `-lh1-` archive creation and hermetic test fixtures); it favours clarity
//! over speed.

use oxiarc_core::error::{OxiArcError, Result};

/// Ring buffer size (lh1 uses a 4KB window).
const RING_SIZE: usize = 4096;
/// Maximum match length.
const MAX_MATCH: usize = 60;
/// Threshold below which runs are emitted as literals.
const THRESHOLD: usize = 2;
/// Number of character codes (256 literals + match length codes).
const NUM_CHAR: usize = 256 - THRESHOLD + MAX_MATCH; // 314
/// Total number of Huffman table nodes.
const TABLE_SIZE: usize = NUM_CHAR * 2 - 1; // 627
/// Root node position.
const ROOT: usize = TABLE_SIZE - 1; // 626
/// Frequency ceiling (tree is rebuilt when reached).
const MAX_FREQ: u32 = 0x8000;

/// MSB-first bit reader. Reads past the end of the data return 0 bits,
/// matching the zero-padding convention of `LZHUF.C`. Such reads flip the
/// [`exhausted`](BitReader::exhausted) flag so callers can distinguish
/// genuine stream content from fabricated zero padding and refuse to decode
/// unbounded output from a truncated/malformed stream.
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
    /// Set once a bit has been requested beyond the end of `data`.
    exhausted: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
            exhausted: false,
        }
    }

    fn get_bit(&mut self) -> u32 {
        let bit = match self.data.get(self.byte_pos) {
            Some(&byte) => u32::from((byte >> (7 - self.bit_pos)) & 1),
            None => {
                // No real input remains; LZHUF.C convention is to feed zero
                // bits, but we record the over-read so the decoder can stop.
                self.exhausted = true;
                0
            }
        };
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        bit
    }

    fn get_bits(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.get_bit();
        }
        value
    }
}

/// MSB-first bit writer (`Putcode` convention from `LZHUF.C`).
struct BitWriter {
    out: Vec<u8>,
    current: u8,
    filled: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            current: 0,
            filled: 0,
        }
    }

    fn put_bit(&mut self, bit: u32) {
        self.current = (self.current << 1) | (bit & 1) as u8;
        self.filled += 1;
        if self.filled == 8 {
            self.out.push(self.current);
            self.current = 0;
            self.filled = 0;
        }
    }

    /// Emit the top `len` bits of `code` (starting from bit 15), matching
    /// `Putcode` in `LZHUF.C`.
    fn put_code(&mut self, len: u32, code: u16) {
        for i in 0..len {
            self.put_bit(u32::from((code >> (15 - i)) & 1));
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.current <<= 8 - self.filled;
            self.out.push(self.current);
        }
        self.out
    }
}

/// LZHUF adaptive Huffman tree.
struct AdaptiveHuffman {
    freq: Vec<u32>,
    /// Parent nodes (leaves are stored at position `TABLE_SIZE + symbol`).
    prnt: Vec<usize>,
    son: Vec<usize>,
}

impl AdaptiveHuffman {
    fn new() -> Self {
        let mut freq = vec![0u32; TABLE_SIZE + 1];
        let mut prnt = vec![0usize; TABLE_SIZE + NUM_CHAR];
        let mut son = vec![0usize; TABLE_SIZE];

        for i in 0..NUM_CHAR {
            freq[i] = 1;
            son[i] = i + TABLE_SIZE;
            prnt[i + TABLE_SIZE] = i;
        }
        let mut i = 0;
        let mut j = NUM_CHAR;
        while j <= ROOT {
            freq[j] = freq[i] + freq[i + 1];
            son[j] = i;
            prnt[i] = j;
            prnt[i + 1] = j;
            i += 2;
            j += 1;
        }
        freq[TABLE_SIZE] = u32::MAX; // sentinel
        prnt[ROOT] = 0;

        Self { freq, prnt, son }
    }

    /// Rebuild the tree, halving all frequencies (triggered at `MAX_FREQ`).
    fn reconst(&mut self) {
        // Collect leaves in the first half, halving frequencies to (f+1)/2.
        let mut j = 0;
        for i in 0..TABLE_SIZE {
            if self.son[i] >= TABLE_SIZE {
                self.freq[j] = self.freq[i].div_ceil(2);
                self.son[j] = self.son[i];
                j += 1;
            }
        }
        // Merge children pairwise, inserting internal nodes in frequency order.
        let mut i = 0;
        let mut j = NUM_CHAR;
        while j < TABLE_SIZE {
            let f = self.freq[i] + self.freq[i + 1];
            self.freq[j] = f;
            let mut k = j - 1;
            while k > 0 && f < self.freq[k] {
                k -= 1;
            }
            if f < self.freq[k] {
                // Defensive branch (unreachable: freq is ascending so k=0
                // still satisfies f >= freq[0]).
                k = 0;
            } else {
                k += 1;
            }
            self.freq.copy_within(k..j, k + 1);
            self.freq[k] = f;
            self.son.copy_within(k..j, k + 1);
            self.son[k] = i;
            i += 2;
            j += 1;
        }
        // Re-link parents.
        for i in 0..TABLE_SIZE {
            let k = self.son[i];
            self.prnt[k] = i;
            if k < TABLE_SIZE {
                self.prnt[k + 1] = i;
            }
        }
    }

    /// Increment the frequency of `symbol`, swapping out-of-order nodes.
    fn update(&mut self, symbol: usize) {
        if self.freq[ROOT] == MAX_FREQ {
            self.reconst();
        }
        let mut c = self.prnt[symbol + TABLE_SIZE];
        loop {
            self.freq[c] += 1;
            let k = self.freq[c];

            // Swap nodes when the ordering invariant is violated.
            let mut l = c + 1;
            if k > self.freq[l] {
                while k > self.freq[l + 1] {
                    l += 1;
                }
                self.freq[c] = self.freq[l];
                self.freq[l] = k;

                let i = self.son[c];
                self.prnt[i] = l;
                if i < TABLE_SIZE {
                    self.prnt[i + 1] = l;
                }

                let j = self.son[l];
                self.son[l] = i;
                self.prnt[j] = c;
                if j < TABLE_SIZE {
                    self.prnt[j + 1] = c;
                }
                self.son[c] = j;

                c = l;
            }

            c = self.prnt[c];
            if c == 0 {
                break; // Reached the root.
            }
        }
    }

    /// Decode one symbol.
    fn decode_char(&mut self, reader: &mut BitReader<'_>) -> usize {
        let mut c = self.son[ROOT];
        // Root to leaf: bit 0 selects the smaller child, 1 the larger.
        while c < TABLE_SIZE {
            c += reader.get_bit() as usize;
            c = self.son[c];
        }
        let symbol = c - TABLE_SIZE;
        self.update(symbol);
        symbol
    }

    /// Encode one symbol.
    fn encode_char(&mut self, writer: &mut BitWriter, symbol: usize) {
        let mut code: u16 = 0;
        let mut len: u32 = 0;
        let mut k = self.prnt[symbol + TABLE_SIZE];
        // Walk from leaf to root, accumulating bits in reverse.
        loop {
            code >>= 1;
            if k & 1 == 1 {
                code |= 0x8000;
            }
            len += 1;
            k = self.prnt[k];
            if k == ROOT {
                break;
            }
        }
        writer.put_code(len, code);
        self.update(symbol);
    }
}

/// Static position tables for the upper 6 bits (`d_code` / `d_len` of
/// `LZHUF.C`), generated from the canonical Huffman code with length counts
/// {3 x 1, 4 x 3, 5 x 8, 6 x 12, 7 x 24, 8 x 16}.
///
/// Returns `(d_code, d_len, p_code, p_len)`:
/// * `d_code[i]` / `d_len[i]` — decoder lookup keyed by the next 8 stream bits.
/// * `p_code[j]` / `p_len[j]` — encoder table keyed by the upper 6 position bits.
fn build_position_tables() -> ([u8; 256], [u8; 256], [u8; 64], [u8; 64]) {
    const LEN_COUNTS: [(u8, usize); 6] = [(3, 1), (4, 3), (5, 8), (6, 12), (7, 24), (8, 16)];

    let mut p_len = [0u8; 64];
    let mut idx = 0;
    for (len, count) in LEN_COUNTS {
        for _ in 0..count {
            p_len[idx] = len;
            idx += 1;
        }
    }

    // Assign left-justified 8-bit canonical codes.
    let mut p_code = [0u8; 64];
    let mut code: u32 = 0;
    let mut prev_len = p_len[0];
    for i in 0..64 {
        code <<= p_len[i] - prev_len;
        prev_len = p_len[i];
        p_code[i] = ((code << (8 - p_len[i])) & 0xFF) as u8;
        code += 1;
    }

    let mut d_code = [0u8; 256];
    let mut d_len = [0u8; 256];
    for j in 0..64 {
        let len = p_len[j] as u32;
        let prefix = (p_code[j] as u32) >> (8 - len);
        for (i, (dc, dl)) in d_code.iter_mut().zip(d_len.iter_mut()).enumerate() {
            if (i as u32) >> (8 - len) == prefix {
                *dc = j as u8;
                *dl = p_len[j];
            }
        }
    }

    (d_code, d_len, p_code, p_len)
}

/// Decode a match position (upper 6 bits via the static Huffman prefix,
/// lower 6 bits raw).
fn decode_position(reader: &mut BitReader<'_>, d_code: &[u8; 256], d_len: &[u8; 256]) -> usize {
    let mut i = reader.get_bits(8) as usize;
    let c = (d_code[i] as usize) << 6;
    let extra = u32::from(d_len[i]) - 2;
    for _ in 0..extra {
        i = (i << 1) + reader.get_bit() as usize;
    }
    c | (i & 0x3F)
}

/// Encode a match position (`EncodePosition` from `LZHUF.C`).
fn encode_position(writer: &mut BitWriter, pos: usize, p_code: &[u8; 64], p_len: &[u8; 64]) {
    let upper = pos >> 6;
    writer.put_code(u32::from(p_len[upper]), (p_code[upper] as u16) << 8);
    writer.put_code(6, ((pos & 0x3F) as u16) << 10);
}

/// Decode `-lh1-` compressed data.
///
/// `original_size` is the expected number of uncompressed bytes (from the
/// LZH header); decoding stops once that many bytes have been produced.
///
/// # Errors
///
/// Returns an error if `original_size` does not fit in `usize`, or if the
/// compressed stream is truncated/malformed and is exhausted before
/// producing `original_size` bytes (guarding against a declared size that
/// far exceeds the real payload — a decompression-bomb / DoS vector).
pub fn decode_lh1(data: &[u8], original_size: u64) -> Result<Vec<u8>> {
    let expected = usize::try_from(original_size)
        .map_err(|_| OxiArcError::corrupted(0, "lh1: original size does not fit in memory"))?;

    let mut tree = AdaptiveHuffman::new();
    let mut reader = BitReader::new(data);
    let (d_code, d_len, _, _) = build_position_tables();

    // The ring buffer is initialised with spaces (0x20) per the LZHUF spec.
    let mut ring = [0x20u8; RING_SIZE];
    let mut r = RING_SIZE - MAX_MATCH;
    let mut out = Vec::with_capacity(expected.min(16 * 1024 * 1024));

    while out.len() < expected {
        let c = tree.decode_char(&mut reader);
        // A valid stream encodes exactly `expected` bytes; its final symbol
        // is fully contained in `data`. If decoding a symbol required bits
        // past the end of the real input, the stream is truncated/malformed
        // and any further output would be fabricated from zero padding.
        if reader.exhausted {
            return Err(OxiArcError::corrupted(
                reader.byte_pos as u64,
                "lh1: compressed stream exhausted before producing declared size",
            ));
        }
        if c < 256 {
            out.push(c as u8);
            ring[r] = c as u8;
            r = (r + 1) & (RING_SIZE - 1);
        } else {
            let pos = decode_position(&mut reader, &d_code, &d_len);
            if reader.exhausted {
                return Err(OxiArcError::corrupted(
                    reader.byte_pos as u64,
                    "lh1: compressed stream exhausted decoding match position",
                ));
            }
            let start = (r + RING_SIZE - pos - 1) & (RING_SIZE - 1);
            let length = c - 255 + THRESHOLD;
            for k in 0..length {
                let byte = ring[(start + k) & (RING_SIZE - 1)];
                out.push(byte);
                ring[r] = byte;
                r = (r + 1) & (RING_SIZE - 1);
                if out.len() >= expected {
                    break;
                }
            }
        }
    }

    Ok(out)
}

/// Find the longest ring-buffer match for `lookahead` given the current
/// write cursor `r`, simulating the decoder's copy semantics exactly
/// (including self-overlapping copies that read bytes written earlier in
/// the same match).
///
/// Returns `(length, position_code)` where `position_code` is the value the
/// decoder feeds into `start = r - pos - 1`; `(0, 0)` if no match of at
/// least `THRESHOLD + 1` bytes exists.
fn find_lh1_match(ring: &[u8; RING_SIZE], r: usize, lookahead: &[u8]) -> (usize, usize) {
    let max_len = lookahead.len().min(MAX_MATCH);
    if max_len <= THRESHOLD {
        return (0, 0);
    }

    let mask = RING_SIZE - 1;
    let mut best_len = THRESHOLD; // require strictly greater
    let mut best_pos = 0usize;

    for pos in 0..RING_SIZE {
        let start = (r + RING_SIZE - pos - 1) & mask;

        // Quick reject on the first byte (always read from the ring: the
        // copy cannot overlap its own first byte).
        if ring[start] != lookahead[0] {
            continue;
        }

        let mut len = 0usize;
        while len < max_len {
            let src = (start + len) & mask;
            // Simulate the decoder: positions already (re)written during
            // this copy hold the matched lookahead bytes, not the old ring
            // contents.
            let overlap = (src.wrapping_sub(r)) & mask;
            let byte = if overlap < len {
                lookahead[overlap]
            } else {
                ring[src]
            };
            if byte != lookahead[len] {
                break;
            }
            len += 1;
        }

        if len > best_len {
            best_len = len;
            best_pos = pos;
            if best_len >= max_len {
                break;
            }
        }
    }

    if best_len > THRESHOLD {
        (best_len, best_pos)
    } else {
        (0, 0)
    }
}

/// Encode data as an `-lh1-` stream (greedy matcher).
///
/// The output decodes byte-exactly with [`decode_lh1`] and any conformant
/// LZHUF decoder. Match finding is a straightforward O(n * window) scan —
/// correct and deterministic, but not tuned for large inputs.
pub fn encode_lh1(data: &[u8]) -> Vec<u8> {
    let mut tree = AdaptiveHuffman::new();
    let mut writer = BitWriter::new();
    let (_, _, p_code, p_len) = build_position_tables();

    let mut ring = [0x20u8; RING_SIZE];
    let mut r = RING_SIZE - MAX_MATCH;
    let mask = RING_SIZE - 1;

    let mut i = 0usize;
    while i < data.len() {
        let lookahead = &data[i..data.len().min(i + MAX_MATCH)];
        let (len, pos) = find_lh1_match(&ring, r, lookahead);

        if len > THRESHOLD {
            // Match symbol: 255 - THRESHOLD + length (=> length + 253).
            tree.encode_char(&mut writer, len + 255 - THRESHOLD);
            encode_position(&mut writer, pos, &p_code, &p_len);
            for k in 0..len {
                ring[r] = data[i + k];
                r = (r + 1) & mask;
            }
            i += len;
        } else {
            let byte = data[i];
            tree.encode_char(&mut writer, byte as usize);
            ring[r] = byte;
            r = (r + 1) & mask;
            i += 1;
        }
    }

    writer.finish()
}

/// Encode every byte as a literal, producing a valid (if uncompressed)
/// `-lh1-` stream. Retained for fixture generation and regression tests
/// that need a match-free stream.
#[doc(hidden)]
pub fn encode_lh1_literals(data: &[u8]) -> Vec<u8> {
    let mut tree = AdaptiveHuffman::new();
    let mut writer = BitWriter::new();
    for &byte in data {
        tree.encode_char(&mut writer, byte as usize);
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x1234_5678;
        let mut out = Vec::with_capacity(len + 4);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn position_tables_cover_all_prefixes() {
        let (_, d_len, _, _) = build_position_tables();
        assert!(
            d_len.iter().all(|&l| (3..=8).contains(&l)),
            "position table has unassigned prefixes"
        );
    }

    #[test]
    fn position_code_roundtrip() {
        // Every position 0..4096 must survive encode -> decode.
        let (d_code, d_len, p_code, p_len) = build_position_tables();
        for pos in (0..RING_SIZE).step_by(37).chain([0, 63, 64, 4095]) {
            let mut writer = BitWriter::new();
            encode_position(&mut writer, pos, &p_code, &p_len);
            let bytes = writer.finish();
            let mut reader = BitReader::new(&bytes);
            let decoded = decode_position(&mut reader, &d_code, &d_len);
            assert_eq!(decoded, pos, "position {pos} failed to roundtrip");
        }
    }

    #[test]
    fn literal_roundtrip_small() -> Result<()> {
        let original = "Hello LZHUF, こんにちは lh1!".as_bytes();
        let encoded = encode_lh1_literals(original);
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 literal roundtrip mismatch");
        Ok(())
    }

    #[test]
    fn literal_roundtrip_large_exercises_reconst() -> Result<()> {
        // More than MAX_FREQ (0x8000) symbols to exercise reconst().
        let original = make_data(40 * 1024);
        let encoded = encode_lh1_literals(&original);
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 large literal roundtrip mismatch");
        Ok(())
    }

    #[test]
    fn empty_input_decodes_to_empty() -> Result<()> {
        let decoded = decode_lh1(&[], 0)?;
        assert!(decoded.is_empty(), "empty data must decode to empty output");
        Ok(())
    }

    #[test]
    fn greedy_encoder_roundtrip_compressible() -> Result<()> {
        // Repetitive data exercises the match/copy path of the decoder,
        // including matches into the space-initialised ring region.
        let original: Vec<u8> = b"lh1 window test pattern    "
            .iter()
            .cycle()
            .take(10 * 1024)
            .copied()
            .collect();
        let encoded = encode_lh1(&original);
        assert!(
            encoded.len() < original.len(),
            "greedy lh1 encoder should compress repetitive data ({} vs {})",
            encoded.len(),
            original.len()
        );
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 greedy roundtrip mismatch");
        Ok(())
    }

    #[test]
    fn greedy_encoder_roundtrip_incompressible() -> Result<()> {
        let original = make_data(8 * 1024);
        let encoded = encode_lh1(&original);
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 incompressible roundtrip mismatch");
        Ok(())
    }

    #[test]
    fn greedy_encoder_roundtrip_beyond_window() -> Result<()> {
        // Data larger than the 4KB window with long-range repetitions.
        let mut original = make_data(3000);
        let tail = original.clone();
        original.extend_from_slice(&tail); // repeat at distance 3000 (< 4096)
        original.extend_from_slice(&make_data(6000)[3000..]); // fresh tail
        let encoded = encode_lh1(&original);
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 beyond-window roundtrip mismatch");
        Ok(())
    }

    #[test]
    fn truncated_stream_with_huge_size_errors_quickly() {
        // A tiny malformed payload declaring a gigantic uncompressed size
        // must NOT hang or balloon memory: the decoder has to detect that
        // the bitstream is exhausted and return an error promptly.
        // Without the exhaustion guard this loops pushing zero-padding
        // derived bytes until `out.len()` reaches ~4 GiB.
        let garbage = [0xFFu8; 8];
        let result = decode_lh1(&garbage, 1u64 << 32); // 4 GiB declared

        match result {
            Err(OxiArcError::CorruptedData { .. }) => {}
            Err(other) => panic!("expected CorruptedData error, got {other:?}"),
            Ok(out) => panic!(
                "truncated lh1 stream must error, but decoded {} bytes",
                out.len()
            ),
        }
    }

    #[test]
    fn empty_data_with_nonzero_size_errors() {
        // No input at all but a non-zero declared size is corruption.
        let result = decode_lh1(&[], 1024);
        assert!(
            matches!(result, Err(OxiArcError::CorruptedData { .. })),
            "empty payload with non-zero size must be rejected"
        );
    }

    #[test]
    fn overlapping_run_roundtrip() -> Result<()> {
        // Long single-byte run: forces self-overlapping copies.
        let original = vec![b'A'; 500];
        let encoded = encode_lh1(&original);
        let decoded = decode_lh1(&encoded, original.len() as u64)?;
        assert_eq!(decoded, original, "lh1 overlapping-run roundtrip mismatch");
        Ok(())
    }
}
