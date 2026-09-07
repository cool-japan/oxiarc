//! Canonical LZH/LHA decompression (`-lh4-`/`-lh5-`/`-lh6-`/`-lh7-`).
//!
//! Translated from the reference `lhasa` decoder (`lib/lh_new_decoder.c`).
//! Bits are read most-significant-first via [`MsbBitReader`]. See `encode.rs`
//! for the full block/bitstream specification.

use crate::huffman::{LzhHuffmanTree, read_code_tree, read_offset_tree, read_temp_tree};
use crate::methods::LzhMethod;
use oxiarc_core::MsbBitReader;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{DecompressStatus, Decompressor};
use std::io::Read;

/// Initial capacity cap when reading a *stored* (`-lh0-`) entry, so a crafted
/// header declaring a huge uncompressed size cannot force one giant allocation
/// before a single byte has been read.
const STORED_READ_CHUNK: usize = 64 * 1024;

/// History ring buffer, mirroring `lhasa`'s `LHANewDecoder` ring.
///
/// The buffer holds `1 << history_bits` bytes, pre-filled with ASCII space
/// (`0x20`) exactly as canonical LHA (`init_ring_buffer`) — real encoders may
/// emit copies that reference this initial fill, so it must be reproduced
/// byte-for-byte. Copies address the buffer modulo its (power-of-two) size, so
/// they can reference the full window regardless of how much has been emitted.
#[derive(Debug)]
struct History {
    /// Ring storage; length is a power of two.
    buf: Vec<u8>,
    /// Write cursor (`ringbuf_pos`).
    pos: usize,
    /// `buf.len() - 1`, for fast modular indexing.
    mask: usize,
    /// Accumulated decompressed output.
    output: Vec<u8>,
}

impl History {
    /// Create a space-filled ring of `1 << history_bits` bytes.
    fn new(history_bits: u8) -> Self {
        let size = 1usize << history_bits;
        Self {
            buf: vec![b' '; size],
            pos: 0,
            mask: size - 1,
            output: Vec::new(),
        }
    }

    /// Emit one byte to both the output and the ring (`output_byte`).
    #[inline]
    fn push(&mut self, byte: u8) {
        self.output.push(byte);
        self.buf[self.pos] = byte;
        self.pos = (self.pos + 1) & self.mask;
    }

    /// Copy `count` bytes starting `offset + 1` bytes back (`copy_from_history`).
    fn copy(&mut self, offset: usize, count: usize) -> Result<()> {
        let size = self.mask + 1;
        if offset >= size {
            return Err(OxiArcError::invalid_distance(offset + 1, size));
        }
        let start = self.pos + size - offset - 1;
        for i in 0..count {
            let byte = self.buf[(start + i) & self.mask];
            self.push(byte);
        }
        Ok(())
    }

    /// Preload dictionary bytes into the ring without emitting output.
    ///
    /// Only the last `ring_size` bytes are retained; the write cursor advances
    /// past them so a subsequent copy that references the dictionary resolves
    /// to the same byte the encoder saw.
    fn preload(&mut self, dict: &[u8]) {
        let size = self.mask + 1;
        let start = dict.len().saturating_sub(size);
        for &b in &dict[start..] {
            self.buf[self.pos] = b;
            self.pos = (self.pos + 1) & self.mask;
        }
    }
}

/// LZH decompressor.
#[derive(Debug)]
pub struct LzhDecoder {
    /// Compression method.
    method: LzhMethod,
    /// Expected uncompressed size.
    uncompressed_size: u64,
    /// Optional preloaded dictionary (applied to the history at decode time).
    dictionary: Vec<u8>,
    /// Last decode result (for [`output`](Self::output)).
    output_buf: Vec<u8>,
    /// How much of `output_buf` has already been handed to a
    /// [`Decompressor::decompress`] caller. Only that trait impl uses it;
    /// [`output`](Self::output) and [`decode`](Self::decode) are unaffected.
    output_pos: usize,
    /// Whether decoding is finished.
    finished: bool,
}

impl LzhDecoder {
    /// Create a new LZH decoder.
    pub fn new(method: LzhMethod, uncompressed_size: u64) -> Self {
        Self {
            method,
            uncompressed_size,
            dictionary: Vec::new(),
            output_buf: Vec::new(),
            output_pos: 0,
            finished: false,
        }
    }

    /// Construct a decoder pre-loaded with a custom dictionary.
    ///
    /// The dictionary is written into the sliding-window history so that
    /// back-references produced by an encoder that used the same dictionary are
    /// valid from the very first byte of compressed output. If `dict` is larger
    /// than the window, only the last window's worth of bytes are used.
    pub fn with_dictionary(method: LzhMethod, uncompressed_size: u64, dict: &[u8]) -> Self {
        let mut dec = Self::new(method, uncompressed_size);
        dec.set_dictionary(dict);
        dec
    }

    /// Preload a custom dictionary into the sliding-window history.
    ///
    /// Must be called before any data is decoded.
    pub fn set_dictionary(&mut self, dict: &[u8]) {
        self.dictionary = dict.to_vec();
    }

    /// Reset the decoder.
    pub fn reset(&mut self) {
        self.output_buf.clear();
        self.output_pos = 0;
        self.finished = false;
    }

    /// Decode compressed data.
    pub fn decode<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        if self.method.is_stored() {
            return self.decode_stored(reader);
        }

        if matches!(
            self.method,
            LzhMethod::Lh1 | LzhMethod::Lh2 | LzhMethod::Lh3 | LzhMethod::Lzs | LzhMethod::Lz5
        ) {
            return self.decode_whole_stream(reader);
        }

        if let LzhMethod::Unknown(id) = self.method {
            return Err(OxiArcError::unsupported_method(
                String::from_utf8_lossy(&id).into_owned(),
            ));
        }

        let mut bit_reader = MsbBitReader::new(reader);
        let output = self.decode_compressed(&mut bit_reader)?;
        self.output_buf = output.clone();
        Ok(output)
    }

    /// Decode a method whose codec consumes the whole compressed payload in one
    /// pass rather than through the shared lh4-lh7 block reader: `-lh1-`
    /// (LZHUF adaptive Huffman), `-lh2-`/`-lh3-` (LHarc 2.x) and `-lzs-`/
    /// `-lz5-` (LArc).
    fn decode_whole_stream<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut compressed = Vec::new();
        reader.read_to_end(&mut compressed)?;
        let size = self.uncompressed_size;
        let output = match self.method {
            LzhMethod::Lh1 => crate::lh1::decode_lh1(&compressed, size)?,
            LzhMethod::Lh2 => crate::legacy::decode_lh2(&compressed, size)?,
            LzhMethod::Lh3 => crate::legacy::decode_lh3(&compressed, size)?,
            LzhMethod::Lzs => crate::legacy::decode_lzs(&compressed, size)?,
            LzhMethod::Lz5 => crate::legacy::decode_lz5(&compressed, size)?,
            other => {
                return Err(OxiArcError::unsupported_method(
                    String::from_utf8_lossy(&other.id()).into_owned(),
                ));
            }
        };
        self.output_buf = output.clone();
        self.finished = true;
        Ok(output)
    }

    /// Decode stored (lh0 / lhd) data.
    fn decode_stored<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        // `uncompressed_size` comes from an untrusted LZH header, so it is
        // never allocated up front: a 30-byte archive may declare terabytes.
        // Read what the source actually has, growing geometrically but never
        // past the declared size, and treat a short read as the truncation it
        // is (the same outcome `read_exact` produced, without the allocation).
        let declared = self.uncompressed_size;
        let initial = declared.min(STORED_READ_CHUNK as u64) as usize;
        let mut output = Vec::with_capacity(initial);
        reader.take(declared).read_to_end(&mut output)?;
        if (output.len() as u64) != declared {
            // `expected` is the number of bytes still missing.
            let missing = declared - output.len() as u64;
            return Err(OxiArcError::unexpected_eof(
                usize::try_from(missing).unwrap_or(usize::MAX),
            ));
        }
        self.output_buf = output.clone();
        self.finished = true;
        Ok(output)
    }

    /// Decode a compressed lh4-lh7 stream.
    fn decode_compressed<R: Read>(&mut self, reader: &mut MsbBitReader<R>) -> Result<Vec<u8>> {
        let offset_bits = self.method.offset_bits();
        let max_offset_codes = self.method.max_offset_codes();

        let mut history = History::new(self.method.history_bits());
        if !self.dictionary.is_empty() {
            history.preload(&self.dictionary);
        }

        let mut block_remaining: u64 = 0;
        let mut code_tree: Option<LzhHuffmanTree> = None;
        let mut offset_tree: Option<LzhHuffmanTree> = None;

        while (history.output.len() as u64) < self.uncompressed_size {
            if block_remaining == 0 {
                // Start a new block: command count, then the three tables.
                block_remaining = reader.get_bits(16)? as u64;
                // `MsbBitReader` zero-pads past physical end-of-input, so a
                // truncated stream reads a fabricated (zero) count here. If any
                // padding bit was consumed to form the value the stream ended
                // before this block began: the count — and everything after —
                // is synthetic, so refuse rather than silently truncate. This
                // mirrors lh1's `exhausted` guard in `lh1::decode_lh1`.
                if reader.padding_bits() > 0 {
                    return Err(OxiArcError::corrupted(
                        reader.bits_read(),
                        "lh4-7: compressed stream exhausted reading block header",
                    ));
                }
                if block_remaining == 0 {
                    // A genuine zero-command block cannot advance toward the
                    // declared size; stop rather than spin. The post-loop
                    // length check below turns the resulting short output into
                    // an error instead of returning truncated data.
                    break;
                }
                let temp_tree = read_temp_tree(reader)?;
                code_tree = Some(read_code_tree(reader, &temp_tree)?);
                offset_tree = Some(read_offset_tree(reader, offset_bits, max_offset_codes)?);
                // The three tables may also have been read past EOF on a stream
                // truncated mid-header, in which case the trees are built from
                // zero padding rather than real descriptors.
                if reader.padding_bits() > 0 {
                    return Err(OxiArcError::corrupted(
                        reader.bits_read(),
                        "lh4-7: compressed stream exhausted reading block tables",
                    ));
                }
            }

            block_remaining -= 1;

            let ctree = code_tree
                .as_ref()
                .ok_or_else(|| OxiArcError::corrupted(reader.bits_read(), "missing code tree"))?;
            let code = ctree.decode(reader)?;
            // A code decoded from zero padding is fabricated: reject rather than
            // emit a byte the encoder never wrote.
            if reader.padding_bits() > 0 {
                return Err(OxiArcError::corrupted(
                    reader.bits_read(),
                    "lh4-7: compressed stream exhausted decoding symbol",
                ));
            }

            if code < 256 {
                history.push(code as u8);
            } else {
                let copy_count = code as usize - 256 + 3;
                let otree = offset_tree.as_ref().ok_or_else(|| {
                    OxiArcError::corrupted(reader.bits_read(), "missing offset tree")
                })?;
                let offset = Self::decode_offset(otree, reader)?;
                // The offset (tree symbol + raw extra bits) must likewise come
                // from real input, not past-EOF padding.
                if reader.padding_bits() > 0 {
                    return Err(OxiArcError::corrupted(
                        reader.bits_read(),
                        "lh4-7: compressed stream exhausted decoding match offset",
                    ));
                }
                history.copy(offset, copy_count)?;
            }
        }

        // A valid stream produces at least `uncompressed_size` bytes (a final
        // match may overshoot, which is trimmed below). Falling short means the
        // stream ended early — return an error instead of the truncated output.
        if (history.output.len() as u64) < self.uncompressed_size {
            return Err(OxiArcError::corrupted(
                reader.bits_read(),
                "lh4-7: compressed stream ended before producing declared size",
            ));
        }

        let mut output = std::mem::take(&mut history.output);
        output.truncate(self.uncompressed_size as usize);
        self.finished = true;
        Ok(output)
    }

    /// Decode an offset value (`read_offset_code`): the tree yields a bit
    /// length; `0 -> 0`, `1 -> 1`, otherwise `(1 << (len-1)) + get_bits(len-1)`.
    /// The returned value is `distance - 1`.
    fn decode_offset<R: Read>(
        offset_tree: &LzhHuffmanTree,
        reader: &mut MsbBitReader<R>,
    ) -> Result<usize> {
        let len = offset_tree.decode(reader)?;
        let offset = if len == 0 {
            0
        } else if len == 1 {
            1
        } else {
            let extra = reader.get_bits((len - 1) as u8)? as usize;
            (1usize << (len - 1)) + extra
        };
        Ok(offset)
    }

    /// Get the last decoded output.
    pub fn output(&self) -> &[u8] {
        &self.output_buf
    }

    /// Copy the not-yet-delivered part of `output_buf` into `output` for the
    /// [`Decompressor`] impl, advancing the delivery cursor.
    fn drain_decoded(&mut self, output: &mut [u8]) -> usize {
        let available = &self.output_buf[self.output_pos.min(self.output_buf.len())..];
        let to_copy = available.len().min(output.len());
        output[..to_copy].copy_from_slice(&available[..to_copy]);
        self.output_pos += to_copy;
        to_copy
    }

    /// Status for the [`Decompressor`] impl: undelivered staged output
    /// outranks "the stream is decoded".
    fn drain_status(&self) -> DecompressStatus {
        if self.output_pos < self.output_buf.len() {
            DecompressStatus::NeedsOutput
        } else {
            DecompressStatus::Done
        }
    }

    /// Check if decoding is finished.
    pub fn is_done(&self) -> bool {
        self.finished
    }
}

impl Decompressor for LzhDecoder {
    /// Decode the whole compressed stream (this decoder is one-shot
    /// internally) and hand it back to the caller **one output buffer at a
    /// time**.
    ///
    /// The decoded bytes are staged in `output_buf` and drained across calls:
    /// while any remain undelivered the status is
    /// [`NeedsOutput`](DecompressStatus::NeedsOutput), and only the call that
    /// delivers the last byte reports [`Done`](DecompressStatus::Done).
    /// Reporting `Done` on the first call — as this did before 0.4.2 — meant
    /// every caller that stops at `Done` silently received a truncated result:
    /// [`decompress_all`](Decompressor::decompress_all) uses a 32 KiB buffer,
    /// so any entry larger than that came back cut to 32 KiB with no error.
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            let written = self.drain_decoded(output);
            return Ok((0, written, self.drain_status()));
        }

        let mut cursor = std::io::Cursor::new(input);
        self.decode(&mut cursor)?;

        let consumed = cursor.position() as usize;
        self.output_pos = 0;
        let written = self.drain_decoded(output);

        Ok((consumed, written, self.drain_status()))
    }

    fn reset(&mut self) {
        LzhDecoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Decompress LZH data.
pub fn decode_lzh(data: &[u8], method: LzhMethod, uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut decoder = LzhDecoder::new(method, uncompressed_size);
    let mut cursor = std::io::Cursor::new(data);
    decoder.decode(&mut cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_stored() {
        let data = b"Hello, World!";
        let result =
            decode_lzh(data, LzhMethod::Lh0, data.len() as u64).expect("decompression failed");
        assert_eq!(result, data);
    }

    #[test]
    fn test_history_overlapping_copy_period_4() {
        // Reproduces the exact "ABAB" + Match{length:16,distance:4} scenario:
        // push A,B,A,B then copy(offset=3, count=16) must repeat the 4-byte
        // "ABAB" period exactly, giving "ABABABABABABABAB" (16 bytes).
        let mut h = History::new(14);
        for b in *b"ABAB" {
            h.push(b);
        }
        h.copy(3, 16).expect("copy");
        assert_eq!(
            String::from_utf8_lossy(&h.output),
            "ABAB".to_string() + &"AB".repeat(8)
        );
    }

    #[test]
    fn test_history_space_prefill_and_copy() {
        // A copy that reaches before any emitted byte must read the space fill.
        let mut h = History::new(14);
        h.push(b'A');
        // offset 4 => 5 bytes back from pos(=1): positions [-4..], all spaces.
        h.copy(4, 3).expect("copy");
        assert_eq!(&h.output, b"A   ");
    }
}
