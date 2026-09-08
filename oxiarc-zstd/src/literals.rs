//! Literals section decoding for Zstandard.
//!
//! The literals section contains literal bytes that are copied directly
//! to the output, either uncompressed or Huffman-encoded.

use crate::backward_bits::{BitCursor, FseBitReader};
use crate::huffman::{HuffmanEntry, HuffmanTable, read_huffman_table};
use crate::{LiteralsBlockType, MAX_BLOCK_SIZE};
use oxiarc_core::error::{OxiArcError, Result};

/// Decoded literals section header.
#[derive(Debug)]
pub struct LiteralsHeader {
    /// Block type.
    pub block_type: LiteralsBlockType,
    /// Regenerated (uncompressed) size.
    pub regenerated_size: usize,
    /// Compressed size (for compressed types).
    pub compressed_size: usize,
    /// Number of streams (1 or 4).
    pub num_streams: usize,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Parse literals section header.
pub fn parse_literals_header(data: &[u8]) -> Result<LiteralsHeader> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty literals section".to_string(),
        });
    }

    let byte0 = data[0];
    let block_type = LiteralsBlockType::from_bits(byte0 & 0x03);
    let size_format = (byte0 >> 2) & 0x03;

    match block_type {
        LiteralsBlockType::Raw | LiteralsBlockType::Rle => {
            // Raw and RLE: only regenerated size, no compressed size
            let (regenerated_size, header_size) = match size_format {
                0 | 2 => {
                    // 5 bits, 1 byte header
                    if data.is_empty() {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    ((byte0 >> 3) as usize, 1)
                }
                1 => {
                    // 12 bits, 2 byte header
                    if data.len() < 2 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    let size = ((byte0 >> 4) as usize) | ((data[1] as usize) << 4);
                    (size, 2)
                }
                3 => {
                    // 20 bits, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    let size = ((byte0 >> 4) as usize)
                        | ((data[1] as usize) << 4)
                        | ((data[2] as usize) << 12);
                    (size, 3)
                }
                _ => unreachable!(),
            };

            check_regenerated_size(regenerated_size)?;
            Ok(LiteralsHeader {
                block_type,
                regenerated_size,
                compressed_size: if block_type == LiteralsBlockType::Rle {
                    1
                } else {
                    regenerated_size
                },
                num_streams: 1,
                header_size,
            })
        }
        LiteralsBlockType::Compressed | LiteralsBlockType::Treeless => {
            // Compressed: both sizes, possibly 4 streams
            let (regenerated_size, compressed_size, num_streams, header_size) = match size_format {
                0 => {
                    // Single stream, 10 bits each, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined =
                        ((byte0 >> 4) as u32) | ((data[1] as u32) << 4) | ((data[2] as u32) << 12);
                    let regen = (combined & 0x3FF) as usize;
                    let comp = ((combined >> 10) & 0x3FF) as usize;
                    (regen, comp, 1, 3)
                }
                1 => {
                    // 4 streams, 10 bits each, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined =
                        ((byte0 >> 4) as u32) | ((data[1] as u32) << 4) | ((data[2] as u32) << 12);
                    let regen = (combined & 0x3FF) as usize;
                    let comp = ((combined >> 10) & 0x3FF) as usize;
                    (regen, comp, 4, 3)
                }
                2 => {
                    // 4 streams, 14 bits each, 4 byte header
                    if data.len() < 4 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined = ((byte0 >> 4) as u32)
                        | ((data[1] as u32) << 4)
                        | ((data[2] as u32) << 12)
                        | ((data[3] as u32) << 20);
                    let regen = (combined & 0x3FFF) as usize;
                    let comp = ((combined >> 14) & 0x3FFF) as usize;
                    (regen, comp, 4, 4)
                }
                3 => {
                    // 4 streams, 18 bits each, 5 byte header
                    if data.len() < 5 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined = ((byte0 >> 4) as u64)
                        | ((data[1] as u64) << 4)
                        | ((data[2] as u64) << 12)
                        | ((data[3] as u64) << 20)
                        | ((data[4] as u64) << 28);
                    let regen = (combined & 0x3FFFF) as usize;
                    let comp = ((combined >> 18) & 0x3FFFF) as usize;
                    (regen, comp, 4, 5)
                }
                _ => unreachable!(),
            };

            check_regenerated_size(regenerated_size)?;
            Ok(LiteralsHeader {
                block_type,
                regenerated_size,
                compressed_size,
                num_streams,
                header_size,
            })
        }
    }
}

/// Reject a literals section that claims to regenerate more than a block can.
///
/// `Regenerated_Size` is a 20-bit field, so a three-byte header can claim just
/// under 1 MiB — but the literals of a block are part of that block's output,
/// which RFC 8878 caps at `Block_Maximum_Decompressed_Size` (at most 128 KiB).
/// Checking it here, before anything is sized from the field, is what keeps the
/// decoder's working set at the "one block" it advertises: without it a
/// four-byte RLE literals section would size a one-megabyte buffer, and a
/// compressed one would reserve the same, only to be rejected afterwards by the
/// block's own output ceiling.
fn check_regenerated_size(regenerated_size: usize) -> Result<()> {
    if regenerated_size > MAX_BLOCK_SIZE {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!(
                "literals regenerated size {regenerated_size} exceeds the maximum block size {MAX_BLOCK_SIZE}"
            ),
        });
    }
    Ok(())
}

/// Literals decoder state.
#[derive(Debug)]
pub struct LiteralsDecoder {
    /// Huffman table for compressed literals.
    huffman_table: Option<HuffmanTable>,
}

impl LiteralsDecoder {
    /// Create a new literals decoder.
    pub fn new() -> Self {
        Self {
            huffman_table: None,
        }
    }

    /// Forget any Huffman table carried over from a previous block.
    ///
    /// A `Treeless` literals section reuses the table decoded by an earlier
    /// block *of the same frame*. Frames are independent, so a decoder reused
    /// across frames must clear the table or it would silently accept a
    /// `Treeless` section in a new frame's first block.
    pub fn reset(&mut self) {
        self.huffman_table = None;
    }

    /// Decode literals section, allocating a fresh buffer for the result.
    ///
    /// Equivalent to [`decode_into`](Self::decode_into) with a fresh `Vec`;
    /// kept for the encoder's self-verification gate and for tests, both of
    /// which want an owned buffer and neither of which is on a hot path.
    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, usize)> {
        let mut out = Vec::new();
        let (consumed, len) = self.decode_into(data, &mut out)?;
        out.truncate(len);
        Ok((out, consumed))
    }

    /// Decode a literals section into the front of `scratch`.
    ///
    /// Returns `(input bytes the section occupies, literal bytes produced)`;
    /// the literals are `scratch[..produced]`.
    ///
    /// # Why the buffer is grow-only
    ///
    /// `scratch` is *not* cleared or truncated: it keeps whatever length it
    /// reached on an earlier block, and only the prefix this call fills is
    /// meaningful. A block's literals are at most
    /// `Block_Maximum_Decompressed_Size`, so the buffer settles at one block
    /// within the first few blocks of a stream and is never resized again —
    /// which matters because `Vec::resize` zeroes what it adds, and those
    /// zeroes are overwritten immediately by the literals themselves. On a
    /// literal-dense frame that memset was a second full pass over every byte
    /// of output.
    ///
    /// Nothing is sized from the header until the section has been checked
    /// against the bytes actually present, so a truncated block still cannot
    /// buy an allocation.
    pub fn decode_into(&mut self, data: &[u8], scratch: &mut Vec<u8>) -> Result<(usize, usize)> {
        let header = parse_literals_header(data)?;
        let content = &data[header.header_size..];
        let regenerated = header.regenerated_size;

        match header.block_type {
            LiteralsBlockType::Raw => {
                if content.len() < regenerated {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated raw literals".to_string(),
                    });
                }
                grow_to(scratch, regenerated);
                scratch[..regenerated].copy_from_slice(&content[..regenerated]);
                Ok((header.header_size + regenerated, regenerated))
            }
            LiteralsBlockType::Rle => {
                if content.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE byte".to_string(),
                    });
                }
                grow_to(scratch, regenerated);
                scratch[..regenerated].fill(content[0]);
                Ok((header.header_size + 1, regenerated))
            }
            LiteralsBlockType::Compressed => {
                if content.len() < header.compressed_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated compressed literals".to_string(),
                    });
                }

                let (table, table_size) = read_huffman_table(content)?;
                self.huffman_table = Some(table);

                if table_size > header.compressed_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "Huffman table exceeds compressed literals size".to_string(),
                    });
                }
                let stream_data = &content[table_size..header.compressed_size];
                grow_to(scratch, regenerated);
                self.decode_huffman_streams(
                    stream_data,
                    &mut scratch[..regenerated],
                    header.num_streams,
                )?;

                Ok((header.header_size + header.compressed_size, regenerated))
            }
            LiteralsBlockType::Treeless => {
                if self.huffman_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "treeless literals without previous table".to_string(),
                    });
                }

                if content.len() < header.compressed_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated treeless literals".to_string(),
                    });
                }

                let stream_data = &content[..header.compressed_size];
                grow_to(scratch, regenerated);
                self.decode_huffman_streams(
                    stream_data,
                    &mut scratch[..regenerated],
                    header.num_streams,
                )?;

                Ok((header.header_size + header.compressed_size, regenerated))
            }
        }
    }

    /// Decode Huffman-compressed streams into `dst`, which is exactly the
    /// regenerated size.
    fn decode_huffman_streams(
        &self,
        data: &[u8],
        dst: &mut [u8],
        num_streams: usize,
    ) -> Result<()> {
        let table = self
            .huffman_table
            .as_ref()
            .ok_or_else(|| OxiArcError::CorruptedData {
                offset: 0,
                message: "no Huffman table".to_string(),
            })?;

        if num_streams == 1 {
            decode_stream_checked(data, dst, table)
        } else {
            decode_four_streams(dst, data, table)
        }
    }
}

/// Make `buf` at least `len` bytes long without ever shrinking it.
#[inline]
fn grow_to(buf: &mut Vec<u8>, len: usize) {
    if buf.len() < len {
        buf.resize(len, 0);
    }
}

/// Decode one Huffman-coded literals stream into `dst`, validating every step.
///
/// This is the *reference* inner loop: it rejects a code the table does not
/// define, a stream that runs out of bits early, and a stream that is not
/// consumed to its first bit, with the crate's pinned messages. The
/// interleaved fast path falls back to running this over each stream in order
/// whenever anything is wrong, so what a caller sees on corrupt input — the
/// message *and* which of the four streams reports it — is exactly what a
/// purely sequential decoder reported before.
fn decode_stream_checked(data: &[u8], dst: &mut [u8], table: &HuffmanTable) -> Result<()> {
    let mut reader = FseBitReader::new(data)?;
    let entries = table.entries();
    let mask = entries.len() - 1;
    let max_bits = table.max_bits();

    for slot in dst.iter_mut() {
        let entry = entries[reader.peek_bits(max_bits) as usize & mask];
        if entry.num_bits == 0 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "invalid Huffman code in literals stream".to_string(),
            });
        }
        reader.skip_bits(entry.num_bits);
        if reader.is_overflowed() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "Huffman literals stream exhausted early".to_string(),
            });
        }
        *slot = entry.symbol;
    }

    if !reader.is_finished() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "Huffman literals stream not fully consumed".to_string(),
        });
    }

    Ok(())
}

/// Decode one symbol into `out`, reloading the container when needed.
///
/// The lookup is `entries[prefix & mask]` with `mask == entries.len() - 1`,
/// which the compiler can prove in range, so the hot path carries neither a
/// bounds check nor a `Result`. Nor does it test the decoded code: the caller
/// has already established [`HuffmanTable::is_complete`], so every prefix
/// decodes to a symbol. Whether the *stream* was consumed exactly is checked
/// once per stream afterwards.
#[inline(always)]
fn decode_symbol(
    cursor: &mut BitCursor,
    data: &[u8],
    entries: &[HuffmanEntry],
    mask: usize,
    max_bits: u8,
    out: &mut u8,
) {
    let entry = entries[cursor.peek_narrow(max_bits) as usize & mask];
    cursor.skip_narrow(data, entry.num_bits);
    *out = entry.symbol;
}

/// Decode one symbol *without* reloading the bit container.
///
/// Paired with [`GROUP`] and one [`BitCursor::refill`] per group: four 12-bit
/// codes fit in a freshly reloaded 64-bit container, so the inner loop reloads
/// on a fixed schedule and contains no data-dependent branch at all. The
/// conditional reload it replaces fired roughly once in six symbols and, being
/// driven by code lengths, mispredicted at about that rate — at ~15 cycles a
/// mispredict, the single largest cost in this loop.
#[inline(always)]
fn decode_symbol_fast(
    cursor: &mut BitCursor,
    entries: &[HuffmanEntry],
    mask: usize,
    max_bits: u8,
    out: &mut u8,
) {
    let entry = entries[cursor.peek_narrow(max_bits) as usize & mask];
    cursor.advance(entry.num_bits);
    *out = entry.symbol;
}

/// Symbols decoded per stream between container reloads.
///
/// Four Huffman codes are at most 48 bits (`HUF_TABLELOG_MAX` is 12), which
/// fits under a 64-bit container reloaded to at most 7 consumed bits.
const GROUP: usize = 4;

/// Split a four-stream literals section into its four bitstreams.
///
/// # Why interleave
///
/// RFC 8878 splits the literals into four independent streams precisely so a
/// decoder can work on them at once: each symbol's bit position depends on the
/// previous symbol's code length, so decoding one stream to its end is a
/// strictly serial chain of `peek -> table load -> skip`. Running the four
/// chains in lockstep gives the processor four independent chains to overlap,
/// which is where most of this decoder's literal throughput comes from (the
/// measured effect on a level-3 RGB8 TIFF strip, whose bytes are almost all
/// Huffman-coded literals, was roughly 2x).
///
/// Streams 1-3 each regenerate `ceil(total / 4)` bytes and stream 4 the rest,
/// so the lockstep loop runs for `len(stream 4)` rounds and streams 1-3 finish
/// their tail three-at-a-time.
fn split_four_streams(data: &[u8]) -> Result<[&[u8]; 4]> {
    // Read jump table (6 bytes: 3 x 2-byte offsets)
    if data.len() < 6 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "truncated 4-stream jump table".to_string(),
        });
    }

    // The jump table holds the sizes of streams 1-3 (RFC 8878 §4.2.2);
    // stream 4 occupies the remainder.
    let size_1 = u16::from_le_bytes([data[0], data[1]]) as usize;
    let size_2 = u16::from_le_bytes([data[2], data[3]]) as usize;
    let size_3 = u16::from_le_bytes([data[4], data[5]]) as usize;

    let stream_data = &data[6..];

    // Cumulative boundaries, validated against the available bytes so the
    // sub-slices below are always in range (and monotonic by construction).
    let jump1 = size_1;
    let jump2 = jump1 + size_2;
    let jump3 = jump2 + size_3;
    if jump3 >= stream_data.len() {
        // `>=`: stream 4 must be non-empty too.
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "invalid 4-stream jump table (streams exceed section)".to_string(),
        });
    }

    Ok([
        &stream_data[..jump1],
        &stream_data[jump1..jump2],
        &stream_data[jump2..jump3],
        &stream_data[jump3..],
    ])
}

/// Decode the four interleaved Huffman streams of a literals section.
fn decode_four_streams(dst: &mut [u8], data: &[u8], table: &HuffmanTable) -> Result<()> {
    let streams = split_four_streams(data)?;

    // Streams 1-3 each regenerate ceil(total/4) bytes; stream 4 the rest.
    let total_size = dst.len();
    let quarter = total_size.div_ceil(4);
    let size4 = total_size
        .checked_sub(quarter * 3)
        .ok_or_else(|| OxiArcError::CorruptedData {
            offset: 0,
            message: "4-stream literals size too small".to_string(),
        })?;
    debug_assert!(size4 <= quarter);

    let (d0, rest) = dst.split_at_mut(quarter);
    let (d1, rest) = rest.split_at_mut(quarter);
    let (d2, d3) = rest.split_at_mut(quarter);
    debug_assert_eq!(d3.len(), size4);

    if interleaved_pass(streams, [d0, d1, d2, d3], table) {
        return Ok(());
    }

    // Anything wrong at all — an undefined code, a stream exhausted early, a
    // stream left unconsumed, a missing sentinel — is reported by re-running
    // the checked decoder in stream order, so the error is identical to the
    // sequential decoder's.
    decode_stream_checked(streams[0], d0, table)?;
    decode_stream_checked(streams[1], d1, table)?;
    decode_stream_checked(streams[2], d2, table)?;
    decode_stream_checked(streams[3], d3, table)?;
    // Unreachable: the two passes consume the same bits from the same table,
    // so whatever the fast pass rejected the checked pass rejects as well.
    // Reported rather than ignored, because returning bytes that failed
    // validation would be the one outcome worse than a redundant error.
    Err(OxiArcError::CorruptedData {
        offset: 0,
        message: "Huffman literals streams failed validation".to_string(),
    })
}

/// Run the four streams in lockstep. Returns `false` if *anything* was wrong,
/// leaving the diagnosis to [`decode_stream_checked`].
///
/// The four positions are held as detached [`BitCursor`] locals rather than
/// through `&mut FseBitReader`, so the whole working set of the loop — four
/// 64-bit containers, four byte pointers, four consumed-bit counts — stays in
/// registers. Driving the same loop through references costs a store and a
/// dependent load per decoded symbol.
fn interleaved_pass(streams: [&[u8]; 4], dst: [&mut [u8]; 4], table: &HuffmanTable) -> bool {
    let readers = (
        FseBitReader::new(streams[0]),
        FseBitReader::new(streams[1]),
        FseBitReader::new(streams[2]),
        FseBitReader::new(streams[3]),
    );
    let (mut k0, mut k1, mut k2, mut k3) = match readers {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a.detach(), b.detach(), c.detach(), d.detach()),
        _ => return false,
    };
    let [b0, b1, b2, b3] = streams;

    // The loop below trusts every lookup, so a table with an undefined prefix
    // has to be refused here rather than decoded.
    if !table.is_complete() {
        return false;
    }
    let entries = table.entries();
    let mask = entries.len() - 1;
    let max_bits = table.max_bits();
    let [d0, d1, d2, d3] = dst;
    let common = d3.len();
    if common > d0.len() || d1.len() != d0.len() || d2.len() != d0.len() {
        return false;
    }

    // Rounds where all four streams still have a symbol to produce.
    let (head0, tail0) = d0.split_at_mut(common);
    let (head1, tail1) = d1.split_at_mut(common);
    let (head2, tail2) = d2.split_at_mut(common);

    // Body: whole groups of four symbols per stream, one reload each.
    let whole = common - common % GROUP;
    let (body0, rest0) = head0.split_at_mut(whole);
    let (body1, rest1) = head1.split_at_mut(whole);
    let (body2, rest2) = head2.split_at_mut(whole);
    let (body3, rest3) = d3.split_at_mut(whole);
    for (((c0, c1), c2), c3) in body0
        .chunks_exact_mut(GROUP)
        .zip(body1.chunks_exact_mut(GROUP))
        .zip(body2.chunks_exact_mut(GROUP))
        .zip(body3.chunks_exact_mut(GROUP))
    {
        k0.refill(b0);
        for slot in c0.iter_mut() {
            decode_symbol_fast(&mut k0, entries, mask, max_bits, slot);
        }
        k1.refill(b1);
        for slot in c1.iter_mut() {
            decode_symbol_fast(&mut k1, entries, mask, max_bits, slot);
        }
        k2.refill(b2);
        for slot in c2.iter_mut() {
            decode_symbol_fast(&mut k2, entries, mask, max_bits, slot);
        }
        k3.refill(b3);
        for slot in c3.iter_mut() {
            decode_symbol_fast(&mut k3, entries, mask, max_bits, slot);
        }
    }

    // Leaving the scheduled-reload body, a cursor can be up to
    // `GROUP * 12 = 48` bits into its container (plus the 7 it started with).
    // The conditional-reload tail below *peeks before it skips*, and a 12-bit
    // peek needs `bits_consumed + 12 <= 64`, so the cursors are topped up here
    // rather than left to the first peek. Only a table with the format's
    // maximum log of 12 can actually reach the boundary — this crate's encoder
    // caps literal codes at 11 — which is exactly why it is fixed
    // structurally instead of being left to the debug assertion.
    k0.refill(b0);
    k1.refill(b1);
    k2.refill(b2);
    k3.refill(b3);

    // Tail of the four-abreast region: back to a conditional reload.
    for (((o0, o1), o2), o3) in rest0
        .iter_mut()
        .zip(rest1.iter_mut())
        .zip(rest2.iter_mut())
        .zip(rest3.iter_mut())
    {
        decode_symbol(&mut k0, b0, entries, mask, max_bits, o0);
        decode_symbol(&mut k1, b1, entries, mask, max_bits, o1);
        decode_symbol(&mut k2, b2, entries, mask, max_bits, o2);
        decode_symbol(&mut k3, b3, entries, mask, max_bits, o3);
    }

    // Streams 1-3 carry one more symbol than stream 4 at most, but the tail is
    // written as a loop rather than a single step so a future change to the
    // size split cannot silently drop symbols.
    for ((o0, o1), o2) in tail0.iter_mut().zip(tail1.iter_mut()).zip(tail2.iter_mut()) {
        decode_symbol(&mut k0, b0, entries, mask, max_bits, o0);
        decode_symbol(&mut k1, b1, entries, mask, max_bits, o1);
        decode_symbol(&mut k2, b2, entries, mask, max_bits, o2);
    }

    k0.is_finished() && k1.is_finished() && k2.is_finished() && k3.is_finished()
}

impl Default for LiteralsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a real four-stream Huffman literals section for `payload`, or
    /// `None` if the encoder decided the payload is not worth compressing.
    fn huffman_section(payload: &[u8]) -> Option<Vec<u8>> {
        let section = crate::compressed_block::encode_literals_section_for_test(payload).ok()?;
        let header = parse_literals_header(&section).ok()?;
        if header.block_type == LiteralsBlockType::Compressed && header.num_streams == 4 {
            Some(section)
        } else {
            None
        }
    }

    /// Build a `table_log`-12 Huffman table — the format's maximum, which this
    /// crate's own encoder never emits (it caps literal codes at 11 bits).
    ///
    /// Weights `[1, 1, 2, ..., 11]` sum to 2048, so the table log is 12 and the
    /// implied last symbol takes weight 12; the two weight-1 symbols get the
    /// 12-bit codes.
    fn twelve_bit_table() -> HuffmanTable {
        let weights: Vec<u8> = vec![1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let table = HuffmanTable::from_stored_weights(&weights).expect("valid table log 12");
        assert_eq!(table.max_bits(), 12);
        assert!(table.is_complete());
        table
    }

    /// The canonical code for `symbol` in `table`, as `(code, num_bits)`.
    fn code_for(table: &HuffmanTable, symbol: u8) -> (u64, u8) {
        let entries = table.entries();
        let index = entries
            .iter()
            .position(|e| e.symbol == symbol)
            .expect("symbol is in the table");
        let num_bits = entries[index].num_bits;
        let code = (index >> (table.max_bits() - num_bits)) as u64;
        (code, num_bits)
    }

    /// Encode `symbols` into one backward literals bitstream.
    fn encode_stream(table: &HuffmanTable, symbols: &[u8]) -> Vec<u8> {
        let mut writer = crate::bitwriter::BackwardBitWriter::new();
        // The decoder reads the *last* field written first, so the symbols go
        // in in reverse order.
        for &symbol in symbols.iter().rev() {
            let (code, num_bits) = code_for(table, symbol);
            writer.write_bits(code, num_bits);
        }
        writer.finish()
    }

    /// A twelve-bit table is the one case where four scheduled-reload symbols
    /// can leave a cursor 55 bits into its 64-bit container, so the
    /// conditional-reload tail must not peek before topping it up.
    ///
    /// Four 12-bit codes followed by more symbols in the same stream is
    /// exactly the shape that exercises the body-to-tail boundary; without the
    /// reload there the fifth symbol reads three zero bits in place of real
    /// ones and the stream decodes to the wrong bytes (or, in a debug build,
    /// trips the container assertion).
    #[test]
    fn a_twelve_bit_table_survives_the_body_to_tail_boundary() {
        let table = twelve_bit_table();
        // Symbols 0 and 1 carry the 12-bit codes; symbol 12 (the implied last
        // weight) carries a 1-bit code. Four 12-bit codes are 48 bits, and the
        // tail's *length* is what decides where the stream's sentinel lands
        // and therefore how far into its container a cursor starts: only a
        // stream whose total bit count is 1, 2 or 3 modulo 8 leaves a cursor
        // above 52 consumed bits after the group, which is where a 12-bit peek
        // stops fitting. One-bit tail symbols are what reach those offsets —
        // an all-12-bit stream is always a multiple of 4 bits and never does.
        for tail in 1..=3usize {
            let per_stream = 4 + tail;
            let symbols: Vec<u8> = (0..per_stream)
                .map(|i| if i < 4 { (i % 2) as u8 } else { 12u8 })
                .collect();
            let streams: Vec<Vec<u8>> = (0..4).map(|_| encode_stream(&table, &symbols)).collect();
            let borrowed = [
                streams[0].as_slice(),
                streams[1].as_slice(),
                streams[2].as_slice(),
                streams[3].as_slice(),
            ];

            let mut fast = vec![0u8; per_stream * 4];
            {
                let (a, rest) = fast.split_at_mut(per_stream);
                let (b, rest) = rest.split_at_mut(per_stream);
                let (c, d) = rest.split_at_mut(per_stream);
                assert!(
                    interleaved_pass(borrowed, [a, b, c, d], &table),
                    "tail {tail}: the interleaved pass rejected a valid stream"
                );
            }

            let mut checked = vec![0u8; per_stream * 4];
            for (i, chunk) in checked.chunks_mut(per_stream).enumerate() {
                decode_stream_checked(borrowed[i], chunk, &table).expect("stream decodes");
            }

            let expected: Vec<u8> = symbols.repeat(4);
            assert_eq!(checked, expected, "tail {tail}: reference decode is wrong");
            assert_eq!(fast, expected, "tail {tail}: fast path disagrees");
        }
    }

    /// The interleaved fast path and the checked sequential decoder must
    /// produce the same bytes from the same section.
    ///
    /// The fast path exists only because it is faster; nothing else about it
    /// is allowed to differ, and it is normally unobservable (the checked
    /// decoder runs only after a failure). This drives both over real
    /// encoder-produced sections and compares them directly.
    #[test]
    fn the_interleaved_pass_agrees_with_the_checked_decoder() {
        let mut compared = 0usize;
        for len in [1100usize, 4096, 20_000, 65_000, 120_000] {
            for seed_shift in 0..3u32 {
                // Skewed bytes, so the encoder really builds a Huffman table.
                let mut state = 0x1234_5678u32 ^ seed_shift.wrapping_mul(0x9E37_79B9);
                let payload: Vec<u8> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        let value = (state >> 16) & 0xFF;
                        // A long-tailed distribution drawn from an independent
                        // part of the state, so the encoder really builds a
                        // Huffman table instead of falling back to Raw.
                        match (state >> 8) & 0x7 {
                            0..=4 => b'a' + (value % 4) as u8,
                            5..=6 => b'A' + (value % 8) as u8,
                            _ => b'0' + (value % 10) as u8,
                        }
                    })
                    .collect();
                let Some(section) = huffman_section(&payload) else {
                    continue;
                };

                // Fast path, through the public entry point.
                let mut decoder = LiteralsDecoder::new();
                let mut fast = Vec::new();
                let (_, produced) = decoder
                    .decode_into(&section, &mut fast)
                    .expect("section decodes");
                assert_eq!(produced, payload.len());
                assert_eq!(&fast[..produced], &payload[..], "fast path is wrong");

                // Checked path, stream by stream, over the same section.
                let header = parse_literals_header(&section).expect("header");
                let content = &section[header.header_size..];
                let (table, table_size) = read_huffman_table(content).expect("table");
                assert!(table.is_complete(), "a real table has no undefined prefix");
                let streams = split_four_streams(&content[table_size..header.compressed_size])
                    .expect("jump table");
                let quarter = produced.div_ceil(4);
                let mut checked = vec![0u8; produced];
                let (c0, rest) = checked.split_at_mut(quarter);
                let (c1, rest) = rest.split_at_mut(quarter);
                let (c2, c3) = rest.split_at_mut(quarter);
                for (stream, dst) in streams.iter().zip([c0, c1, c2, c3]) {
                    decode_stream_checked(stream, dst, &table).expect("stream decodes");
                }
                assert_eq!(checked, payload, "checked path is wrong");
                assert_eq!(&fast[..produced], &checked[..], "the two paths disagree");
                compared += 1;
            }
        }
        assert!(compared >= 5, "only {compared} sections were four-stream");
    }

    /// Deterministic xorshift, so the sweeps below are reproducible without a
    /// dependency.
    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Build a `table_log`-11 Huffman table — the longest code this crate's
    /// own encoder emits, so the sweep below covers both the shape real frames
    /// carry and the format's maximum.
    ///
    /// Weights `[1, 1, 2, ..., 10]` sum to 1024, so the table log is 11 and
    /// the implied last symbol takes weight 11 (a one-bit code).
    fn eleven_bit_table() -> HuffmanTable {
        let weights: Vec<u8> = vec![1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let table = HuffmanTable::from_stored_weights(&weights).expect("valid table log 11");
        assert_eq!(table.max_bits(), 11);
        assert!(table.is_complete());
        table
    }

    /// The distinct symbols a table decodes to, longest code first.
    fn symbols_of(table: &HuffmanTable) -> Vec<u8> {
        let mut seen: Vec<u8> = Vec::new();
        for entry in table.entries() {
            if !seen.contains(&entry.symbol) {
                seen.push(entry.symbol);
            }
        }
        seen
    }

    /// Every `(streams 1-3 length, stream 4 length)` split the four-stream
    /// layout can produce must decode identically through the interleaved fast
    /// path and through the sequential checked decoder.
    ///
    /// The fast path splits each stream three ways — whole groups of
    /// [`GROUP`] symbols under a scheduled reload, a conditionally-reloading
    /// tail while all four streams are still live, and the one-to-three
    /// symbols streams 1-3 carry beyond stream 4 — and tops the containers up
    /// at the first boundary. An off-by-one in any of those three regions, or
    /// a missing reload between them, decodes to the wrong bytes rather than
    /// to an error, so it has to be pinned by a differential rather than by a
    /// round-trip. Both table logs are exercised because only a 12-bit table
    /// can reach the reload budget's edge.
    #[test]
    fn every_stream_length_split_agrees_with_the_checked_decoder() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut cases = 0usize;
        for table in [eleven_bit_table(), twelve_bit_table()] {
            let pool = symbols_of(&table);
            for quarter in 0..=19usize {
                for short in 0..=3usize {
                    if short > quarter {
                        continue;
                    }
                    let size4 = quarter - short;
                    let lengths = [quarter, quarter, quarter, size4];
                    let symbols: Vec<Vec<u8>> = lengths
                        .iter()
                        .map(|&n| {
                            (0..n)
                                .map(|_| pool[(next_rand(&mut state) as usize) % pool.len()])
                                .collect()
                        })
                        .collect();
                    let encoded: Vec<Vec<u8>> =
                        symbols.iter().map(|s| encode_stream(&table, s)).collect();
                    let borrowed = [
                        encoded[0].as_slice(),
                        encoded[1].as_slice(),
                        encoded[2].as_slice(),
                        encoded[3].as_slice(),
                    ];

                    let total = quarter * 3 + size4;
                    let mut fast = vec![0xCCu8; total];
                    {
                        let (a, rest) = fast.split_at_mut(quarter);
                        let (b, rest) = rest.split_at_mut(quarter);
                        let (c, d) = rest.split_at_mut(quarter);
                        assert!(
                            interleaved_pass(borrowed, [a, b, c, d], &table),
                            "log {} quarter {quarter} short {short}: valid streams rejected",
                            table.max_bits()
                        );
                    }

                    let mut checked = vec![0xCCu8; total];
                    {
                        let (a, rest) = checked.split_at_mut(quarter);
                        let (b, rest) = rest.split_at_mut(quarter);
                        let (c, d) = rest.split_at_mut(quarter);
                        for (stream, dst) in borrowed.iter().zip([a, b, c, d]) {
                            decode_stream_checked(stream, dst, &table).expect("stream decodes");
                        }
                    }

                    let expected: Vec<u8> = symbols.concat();
                    assert_eq!(
                        checked,
                        expected,
                        "log {} quarter {quarter} short {short}: reference decode is wrong",
                        table.max_bits()
                    );
                    assert_eq!(
                        fast,
                        expected,
                        "log {} quarter {quarter} short {short}: fast path disagrees",
                        table.max_bits()
                    );
                    cases += 1;
                }
            }
        }
        assert!(cases > 100, "only {cases} splits exercised");
    }

    /// Decode a four-stream section the way a purely sequential decoder would:
    /// split the streams, then run the checked decoder over each in order.
    fn sequential_four_streams(
        stream_data: &[u8],
        regenerated: usize,
        table: &HuffmanTable,
    ) -> Result<Vec<u8>> {
        let streams = split_four_streams(stream_data)?;
        let quarter = regenerated.div_ceil(4);
        let _size4 =
            regenerated
                .checked_sub(quarter * 3)
                .ok_or_else(|| OxiArcError::CorruptedData {
                    offset: 0,
                    message: "4-stream literals size too small".to_string(),
                })?;
        let mut out = vec![0u8; regenerated];
        {
            let (d0, rest) = out.split_at_mut(quarter);
            let (d1, rest) = rest.split_at_mut(quarter);
            let (d2, d3) = rest.split_at_mut(quarter);
            decode_stream_checked(streams[0], d0, table)?;
            decode_stream_checked(streams[1], d1, table)?;
            decode_stream_checked(streams[2], d2, table)?;
            decode_stream_checked(streams[3], d3, table)?;
        }
        Ok(out)
    }

    /// A corrupted four-stream literals section must produce *exactly* what a
    /// sequential decoder produces — the same bytes when it still decodes, and
    /// the same error message when it does not.
    ///
    /// This is the guarantee the fast path is documented to keep and the one
    /// thing about it a caller can observe: it drops the per-symbol validity
    /// test and the per-symbol over-read test, and recovers them by re-running
    /// [`decode_stream_checked`] over each stream in order. Two failures would
    /// be invisible to a round-trip test and are caught here — the fast path
    /// *accepting* bytes the checked decoder refuses (silent corruption), and
    /// the fast path *refusing* a stream the checked decoder accepts, which
    /// would surface as the otherwise-unreachable "failed validation" error.
    #[test]
    fn a_corrupt_four_stream_section_matches_the_sequential_decoder_exactly() {
        let mut state = 0x1357_9BDF_2468_ACE0u64;
        let payload: Vec<u8> = (0..1600)
            .map(|_| {
                let r = next_rand(&mut state);
                match r & 0x7 {
                    0..=4 => b'a' + ((r >> 8) % 4) as u8,
                    5..=6 => b'A' + ((r >> 8) % 8) as u8,
                    _ => b'0' + ((r >> 8) % 10) as u8,
                }
            })
            .collect();
        let section = huffman_section(&payload).expect("a four-stream section");
        let header = parse_literals_header(&section).expect("header");
        let content = &section[header.header_size..];
        let (table, table_size) = read_huffman_table(content).expect("table");
        let stream_start = header.header_size + table_size;
        let stream_end = header.header_size + header.compressed_size;
        assert!(stream_end - stream_start > 6, "jump table plus streams");

        let mut agreed = 0usize;
        let mut refused = 0usize;
        for offset in stream_start..stream_end {
            for bit in [0u8, 3, 6] {
                let mut mutated = section.clone();
                mutated[offset] ^= 1 << bit;

                let expected = sequential_four_streams(
                    &mutated[stream_start..stream_end],
                    header.regenerated_size,
                    &table,
                );
                let mut scratch = vec![0x5Au8; 16];
                let got = LiteralsDecoder::new()
                    .decode_into(&mutated, &mut scratch)
                    .map(|(_, produced)| scratch[..produced].to_vec());

                match (&expected, &got) {
                    (Ok(a), Ok(b)) => assert_eq!(
                        a, b,
                        "offset {offset} bit {bit}: fast path decoded different bytes"
                    ),
                    (Err(a), Err(b)) => {
                        assert_eq!(
                            a.to_string(),
                            b.to_string(),
                            "offset {offset} bit {bit}: different error"
                        );
                        refused += 1;
                    }
                    (Ok(_), Err(e)) => panic!(
                        "offset {offset} bit {bit}: fast path refused a stream the \
                         sequential decoder accepts: {e}"
                    ),
                    (Err(e), Ok(_)) => panic!(
                        "offset {offset} bit {bit}: fast path accepted a stream the \
                         sequential decoder refuses: {e}"
                    ),
                }
                agreed += 1;
            }
        }
        assert!(agreed > 500, "only {agreed} mutations compared");
        assert!(
            refused > 100,
            "only {refused} mutations were refused at all"
        );
    }

    /// The literals scratch buffer is grow-only and shared across blocks, so a
    /// short section decoded after a long one must never expose the long one's
    /// tail.
    ///
    /// `decode_into` reports `(consumed, produced)` and leaves everything above
    /// `produced` untouched on purpose — that is what removes the per-block
    /// `resize` memset. The contract only holds if every caller slices to
    /// `produced`; this pins the buffer's own half of it, in all four literals
    /// block types.
    #[test]
    fn a_reused_scratch_never_exposes_the_previous_block() {
        let long: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let mut decoder = LiteralsDecoder::new();
        let mut scratch = Vec::new();

        let long_section = crate::compressed_block::encode_literals_section_for_test(&long)
            .expect("encode the long section");
        let (_, produced) = decoder
            .decode_into(&long_section, &mut scratch)
            .expect("long section decodes");
        assert_eq!(&scratch[..produced], &long[..]);
        let grown = scratch.len();
        assert!(grown >= long.len());

        for short in [
            b"tiny".to_vec(),
            vec![b'Z'; 9],
            (0..37u8).collect::<Vec<u8>>(),
        ] {
            let section = crate::compressed_block::encode_literals_section_for_test(&short)
                .expect("encode the short section");
            let (_, produced) = decoder
                .decode_into(&section, &mut scratch)
                .expect("short section decodes");
            assert_eq!(produced, short.len(), "produced length");
            assert_eq!(&scratch[..produced], &short[..], "short section bytes");
            assert_eq!(
                scratch.len(),
                grown,
                "the scratch must not shrink or regrow"
            );
        }
    }

    /// Arbitrary bytes offered as a literals section must be parsed or
    /// refused, never panic, and never report more literals than the buffer
    /// it filled.
    ///
    /// The section header is the first attacker-controlled length in a block,
    /// and the rewritten decoder sizes a grow-only buffer from it and hands
    /// the caller a `(consumed, produced)` pair instead of a `Vec` whose
    /// length speaks for itself. Both halves of that contract are checked
    /// here on inputs no encoder would ever produce: `produced` must be
    /// within the buffer, `consumed` within the input, and neither the
    /// four-stream jump table nor the Huffman table reader may index out of
    /// bounds.
    #[test]
    fn arbitrary_bytes_are_parsed_or_refused_but_never_panic() {
        let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut scratch = Vec::new();
        let mut decoded = 0usize;
        for len in [1usize, 2, 3, 4, 5, 6, 7, 8, 12, 20, 40, 100, 300, 1000] {
            for _ in 0..200 {
                let data: Vec<u8> = (0..len)
                    .map(|_| (next_rand(&mut state) >> 24) as u8)
                    .collect();
                let mut decoder = LiteralsDecoder::new();
                if let Ok((consumed, produced)) = decoder.decode_into(&data, &mut scratch) {
                    assert!(
                        consumed <= data.len(),
                        "consumed {consumed} of {}",
                        data.len()
                    );
                    assert!(
                        produced <= scratch.len(),
                        "produced {produced} of a {}-byte buffer",
                        scratch.len()
                    );
                    assert!(produced <= MAX_BLOCK_SIZE, "produced {produced}");
                    decoded += 1;
                }
                // A `Treeless` section is only meaningful after a table has
                // been decoded, so drive that path too by reusing a decoder
                // that has one.
                let mut warm = LiteralsDecoder::new();
                let _ = warm.decode_into(&data, &mut scratch);
                let _ = warm.decode_into(&data, &mut scratch);
            }
        }
        assert!(
            decoded > 100,
            "only {decoded} random sections decoded at all"
        );
    }

    #[test]
    fn test_parse_raw_literals_small() {
        // Raw literals, size format 0/2, 5 bits size
        let data = [0b00001000]; // type=0 (raw), size_format=0, size=1
        let header = parse_literals_header(&data).expect("valid decode operation");

        assert_eq!(header.block_type, LiteralsBlockType::Raw);
        assert_eq!(header.regenerated_size, 1);
        assert_eq!(header.header_size, 1);
    }

    #[test]
    fn test_parse_rle_literals() {
        // RLE literals
        let data = [0b00001001]; // type=1 (RLE), size_format=0, size=1
        let header = parse_literals_header(&data).expect("valid decode operation");

        assert_eq!(header.block_type, LiteralsBlockType::Rle);
        assert_eq!(header.regenerated_size, 1);
        assert_eq!(header.compressed_size, 1);
    }

    #[test]
    fn test_decode_raw_literals() {
        // Raw literals with actual data
        let mut data = vec![0b00101000]; // type=0, size_format=0, size=5
        data.extend_from_slice(b"Hello");

        let mut decoder = LiteralsDecoder::new();
        let (literals, consumed) = decoder.decode(&data).expect("valid decode operation");

        assert_eq!(literals, b"Hello");
        assert_eq!(consumed, 6);
    }

    #[test]
    fn test_decode_rle_literals() {
        // RLE: repeat 'A' 5 times
        let data = [0b00101001, b'A']; // type=1, size=5, byte='A'

        let mut decoder = LiteralsDecoder::new();
        let (literals, consumed) = decoder.decode(&data).expect("valid decode operation");

        assert_eq!(literals, vec![b'A'; 5]);
        assert_eq!(consumed, 2);
    }
}
