//! Zstandard frame parsing and decompression.
//!
//! Handles the top-level frame format including header, blocks, and checksum.

use crate::literals::LiteralsDecoder;
use crate::sequences::{Sequence, SequencesDecoder};
use crate::xxhash::xxhash64_checksum;
use crate::{BlockType, MAX_BLOCK_SIZE, MAX_WINDOW_SIZE, ZSTD_MAGIC};
/// Longest run this module materialises with a byte loop instead of calling
/// `memmove`/`memcpy` through `Vec`.
///
/// Smaller than [`crate::short_copy::SHORT_COPY`] on purpose: the window ring
/// can copy a short run through a split slice borrow, which the compiler
/// vectorises, but a `Vec` being *appended to* has no safe slice over its
/// spare capacity, so the loop here is one bounds-checked read and one `push`
/// per byte. That is cheaper than a call up to about this length and dearer
/// beyond it.
const SHORT_MATCH: usize = 16;
use oxiarc_core::error::{OxiArcError, Result};

/// Zstandard frame magic as a little-endian `u32`.
pub(crate) const ZSTD_MAGIC_U32: u32 = 0xFD2F_B528;

/// Frame header descriptor flags.
const FHD_SINGLE_SEGMENT: u8 = 0x20;
const FHD_CONTENT_CHECKSUM: u8 = 0x04;
const FHD_DICT_ID_FLAG_MASK: u8 = 0x03;
const FHD_CONTENT_SIZE_FLAG_MASK: u8 = 0xC0;

/// Zstandard frame header.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    /// Window size for decompression buffer, clamped to [`MAX_WINDOW_SIZE`].
    pub window_size: usize,
    /// Window size exactly as declared by the frame, with no clamping.
    ///
    /// The incremental decoder compares this against its configured
    /// `max_window` *before* allocating anything, so a frame declaring a
    /// multi-gigabyte window is refused rather than clamped.
    pub declared_window_size: u64,
    /// Uncompressed content size (if known).
    pub content_size: Option<u64>,
    /// Dictionary ID (if present), RFC 8878 §3.1.1.1.1.6.
    ///
    /// A non-zero value names the dictionary the frame was compressed against;
    /// every decoding path refuses such a frame unless the caller supplied a
    /// dictionary (see [`require_dictionary`]).
    pub dict_id: Option<u32>,
    /// Whether content checksum is present.
    pub has_checksum: bool,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Reserve capacity for the decoded output buffer without trusting the
/// attacker-controlled `Frame_Content_Size` field.
///
/// The requested `content_size` is clamped to `window_size` (itself already
/// clamped to [`MAX_WINDOW_SIZE`]) before reserving, so a frame declaring a
/// content size near `u64::MAX` cannot force an allocation request that
/// exceeds `isize::MAX` (which would otherwise panic with "capacity
/// overflow") nor an unbounded/OOM-inducing allocation. Uses `try_reserve`
/// so that even the clamped amount failing to allocate becomes a clean
/// `Result::Err` instead of an abort.
fn reserve_output_capacity(
    output: &mut Vec<u8>,
    content_size: u64,
    window_size: usize,
) -> Result<()> {
    let capped = content_size
        .min(window_size as u64)
        .min(MAX_WINDOW_SIZE as u64) as usize;
    output
        .try_reserve(capped)
        .map_err(|e| OxiArcError::CorruptedData {
            offset: 0,
            message: format!(
                "failed to reserve {} bytes for declared content size {}: {}",
                capped, content_size, e
            ),
        })
}

/// Append an LZ77 back-reference of `len` bytes from `offset` bytes before the
/// end of `out`.
///
/// # Why not one byte at a time
///
/// The obvious loop is `out.push(out[start + i % offset])`: an integer
/// division and a bounds-checked load *per output byte*. This copies in runs
/// instead, doubling the run length each round.
///
/// The output is periodic with period `offset` from `out.len() - offset`
/// onwards (that is what a back-reference means), so it is periodic with every
/// multiple of `offset` as well. After `copied` bytes the periodic region
/// behind the write cursor is `offset + copied` long, and copying from the
/// largest whole multiple of `offset` inside it is both phase-correct and
/// non-overlapping — so the runs go `offset, 2·offset, 4·offset, …` and a
/// 64 KiB match at `offset` 1 costs 17 `extend_from_within` calls instead of
/// 65 536 divisions. A non-overlapping match (`offset >= len`) is one call.
///
/// # Panics
///
/// Debug-asserts `0 < offset <= out.len()`; callers validate the offset
/// against the reachable history first, so a corrupt offset is an error long
/// before it reaches here.
#[inline]
fn append_match(out: &mut Vec<u8>, offset: usize, len: usize) {
    debug_assert!(offset > 0 && offset <= out.len());
    if len == 0 {
        return;
    }
    if len <= SHORT_MATCH {
        // A short match — the commonest shape in text and in image rows —
        // costs more in `memmove` call overhead than in copied bytes.
        // `out[i - offset]` *is* the definition of a back-reference, so the
        // byte loop needs no `%` and no phase bookkeeping; it just must not be
        // used for long matches, where the library routine wins.
        out.reserve(len);
        let base = out.len();
        for i in 0..len {
            let byte = out[base + i - offset];
            out.push(byte);
        }
        return;
    }
    out.reserve(len);
    let mut copied = 0usize;
    while copied < len {
        // Bytes behind the write cursor that already repeat with period
        // `offset`, rounded down to a whole number of periods.
        let periodic = offset + copied;
        let distance = periodic - periodic % offset;
        let run = (len - copied).min(distance);
        let from = out.len() - distance;
        out.extend_from_within(from..from + run);
        copied += run;
    }
}

/// Append `src` to `out`, inline for the short literal runs a compressed block
/// is mostly made of.
///
/// `Vec::extend_from_slice` is a `memcpy` call whatever the length; a literal
/// run between two matches is typically a handful of bytes.
#[inline]
fn append_literals(out: &mut Vec<u8>, src: &[u8]) {
    if src.len() > SHORT_MATCH {
        out.extend_from_slice(src);
        return;
    }
    out.reserve(src.len());
    for &byte in src {
        out.push(byte);
    }
}

/// Parse frame header.
pub fn parse_frame_header(data: &[u8]) -> Result<FrameHeader> {
    if data.len() < 5 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "truncated frame header".to_string(),
        });
    }

    // Check magic
    if data[0..4] != ZSTD_MAGIC {
        return Err(OxiArcError::invalid_magic(ZSTD_MAGIC, &data[0..4]));
    }

    let descriptor = data[4];
    let single_segment = (descriptor & FHD_SINGLE_SEGMENT) != 0;
    let has_checksum = (descriptor & FHD_CONTENT_CHECKSUM) != 0;
    let dict_id_flag = descriptor & FHD_DICT_ID_FLAG_MASK;
    let content_size_flag = (descriptor & FHD_CONTENT_SIZE_FLAG_MASK) >> 6;

    let mut pos = 5;

    // Unclamped Window_Size exactly as declared by the frame.
    let mut declared_window: u64 = 0;

    // Window descriptor (absent if single segment)
    let window_size = if single_segment {
        0 // Will be determined from content size
    } else {
        if data.len() <= pos {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "missing window descriptor".to_string(),
            });
        }
        let wd = data[pos];
        pos += 1;

        let exponent = (wd >> 3) as u32;
        let mantissa = (wd & 0x07) as u32;
        let base = 1u64 << (10 + exponent);
        let window = base + (base >> 3) * mantissa as u64;
        declared_window = window;
        window.min(MAX_WINDOW_SIZE as u64) as usize
    };

    // Dictionary ID
    let dict_id = match dict_id_flag {
        0 => None,
        1 => {
            if data.len() <= pos {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "missing dictionary ID".to_string(),
                });
            }
            let id = data[pos] as u32;
            pos += 1;
            Some(id)
        }
        2 => {
            if data.len() < pos + 2 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated dictionary ID".to_string(),
                });
            }
            let id = u16::from_le_bytes([data[pos], data[pos + 1]]) as u32;
            pos += 2;
            Some(id)
        }
        3 => {
            if data.len() < pos + 4 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated dictionary ID".to_string(),
                });
            }
            let id = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            pos += 4;
            Some(id)
        }
        _ => unreachable!(),
    };

    // Content size
    let content_size = if single_segment || content_size_flag != 0 {
        let size_bytes = match content_size_flag {
            0 => 1, // Single segment implies 1 byte
            1 => 2,
            2 => 4,
            3 => 8,
            _ => unreachable!(),
        };

        if data.len() < pos + size_bytes {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "truncated content size".to_string(),
            });
        }

        let size = match size_bytes {
            1 => data[pos] as u64,
            2 => {
                let s = u16::from_le_bytes([data[pos], data[pos + 1]]) as u64;
                s + 256 // Add 256 for 2-byte size
            }
            4 => {
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as u64
            }
            8 => u64::from_le_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
            ]),
            _ => unreachable!(),
        };
        pos += size_bytes;
        Some(size)
    } else {
        None
    };

    // Adjust window size for single segment: the whole content is the window.
    let window_size = if single_segment {
        declared_window = content_size.unwrap_or(MAX_WINDOW_SIZE as u64);
        declared_window.min(MAX_WINDOW_SIZE as u64) as usize
    } else {
        window_size
    };

    Ok(FrameHeader {
        window_size,
        declared_window_size: declared_window,
        content_size,
        dict_id,
        has_checksum,
        header_size: pos,
    })
}

/// RFC 8878 `Block_Maximum_Decompressed_Size` for a frame: `min(Window_Size,
/// 128 KiB)`, further bounded by a declared `Frame_Content_Size`.
///
/// Derived from what the *format* declares and nothing else, so a block that
/// regenerates more than this is always a format error — never a budget
/// overrun. Both decoding paths call this, so the legacy one-shot decoder and
/// [`crate::ZstdStream`] agree on the ceiling byte for byte.
pub(crate) fn block_rfc_max(header: &FrameHeader) -> usize {
    let declared = usize::try_from(header.declared_window_size).unwrap_or(usize::MAX);
    let mut rfc_max = MAX_BLOCK_SIZE.min(declared.max(1));
    if let Some(content_size) = header.content_size {
        rfc_max = rfc_max.min(usize::try_from(content_size).unwrap_or(usize::MAX));
    }
    rfc_max
}

/// Refuse a frame that names a dictionary the caller did not supply.
///
/// A frame carrying a non-zero `Dictionary_ID` (RFC 8878 §3.1.1.1.1.6) cannot
/// be decoded without that dictionary: its matches reach into content the
/// decoder does not have, and its first block may reference the dictionary's
/// entropy tables. Decoding it anyway would produce silently wrong bytes, so
/// every entry point — the legacy one-shot decoders and [`crate::ZstdStream`]
/// alike — refuses it with the same [`OxiArcError::InvalidHeader`].
///
/// `Dictionary_ID` 0 means "no dictionary", whatever the header flag's width
/// says, and a caller-supplied dictionary satisfies any ID: raw content
/// dictionaries carry no identifier to match against.
pub(crate) fn require_dictionary(header: &FrameHeader, have_dictionary: bool) -> Result<()> {
    if have_dictionary {
        return Ok(());
    }
    match header.dict_id.filter(|id| *id != 0) {
        Some(id) => Err(OxiArcError::invalid_header(format!(
            "Zstandard frame requires dictionary ID {id:#010x} but no dictionary was supplied"
        ))),
        None => Ok(()),
    }
}

/// Add `more` bytes to a block's regenerated size, refusing to exceed
/// [`block_rfc_max`].
///
/// Checked *before* the bytes are produced, so an over-large block is refused
/// without first materialising it.
pub(crate) fn charge_block(produced: usize, more: usize, rfc_max: usize) -> Result<usize> {
    let total = produced
        .checked_add(more)
        .ok_or_else(|| OxiArcError::corrupted(0, "block regenerated size overflowed"))?;
    if total > rfc_max {
        return Err(OxiArcError::corrupted(
            0,
            format!("block regenerated size {total} exceeds the frame maximum {rfc_max}"),
        ));
    }
    Ok(total)
}

/// Measure the run of skippable frames (RFC 8878 §3.1.2) sitting in front of a
/// Zstandard frame.
///
/// Returns how many bytes they occupy, so the caller can start the real frame
/// after them. A skippable frame carries user metadata and no content: the
/// reference decoder walks straight past one, and `zstd -d` decodes
/// `[skippable][frame]` exactly like `[frame]`. [`crate::ZstdStream`] does the
/// same in single-frame mode, so the legacy one-shot decoders do too — a
/// container that prefixes its payload with metadata must not decode on one
/// path and fail on the other.
///
/// A *recognised* skippable frame that is cut short is an error wherever it
/// sits, with the same messages [`crate::ZstdStream`] produces; bytes that are
/// not a skippable magic simply end the run and are left to
/// [`parse_frame_header`].
fn skippable_prefix_len(data: &[u8]) -> Result<usize> {
    let mut pos = 0usize;
    while data.len() - pos >= 4 {
        let magic = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        if !(crate::SKIPPABLE_MAGIC_LOW..=crate::SKIPPABLE_MAGIC_HIGH).contains(&magic) {
            break;
        }
        if data.len() - pos < 8 {
            return Err(OxiArcError::CorruptedData {
                offset: (pos + 4) as u64,
                message: "truncated skippable frame size".to_string(),
            });
        }
        let skip_size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        // 64-bit arithmetic: `pos + 8 + skip_size` can overflow `usize` on a
        // 32-bit target.
        let end = pos as u64 + 8 + u64::from(skip_size);
        if end > data.len() as u64 {
            return Err(OxiArcError::CorruptedData {
                offset: (pos + 8) as u64,
                message: "truncated skippable frame payload".to_string(),
            });
        }
        pos = end as usize;
    }
    Ok(pos)
}

/// Total size of a frame header (magic included) given its first bytes.
///
/// Returns `None` when fewer than 5 bytes are available — the frame header
/// descriptor at offset 4 is what tells the decoder how many more bytes the
/// header occupies. The result is at most 18 (4 magic + 1 descriptor +
/// 1 window descriptor + 4 dictionary ID + 8 content size), so an incremental
/// decoder can buffer the header without an unbounded read-ahead.
pub(crate) fn frame_header_len(data: &[u8]) -> Option<usize> {
    if data.len() < 5 {
        return None;
    }
    let descriptor = data[4];
    let single_segment = (descriptor & FHD_SINGLE_SEGMENT) != 0;
    let dict_id_flag = descriptor & FHD_DICT_ID_FLAG_MASK;
    let content_size_flag = (descriptor & FHD_CONTENT_SIZE_FLAG_MASK) >> 6;

    let mut len = 5;
    if !single_segment {
        len += 1;
    }
    len += match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    if single_segment || content_size_flag != 0 {
        len += match content_size_flag {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        };
    }
    Some(len)
}

/// Zstandard decoder.
pub struct ZstdDecoder {
    /// Literals decoder.
    literals_decoder: LiteralsDecoder,
    /// Sequences decoder.
    sequences_decoder: SequencesDecoder,
    /// Output buffer (sliding window).
    output: Vec<u8>,
    /// Literals scratch, reused for every block (grow-only; see
    /// [`LiteralsDecoder::decode_into`]).
    lit_buf: Vec<u8>,
    /// Sequences scratch, reused for every block.
    seq_buf: Vec<Sequence>,
    /// Window size.
    window_size: usize,
    /// Optional dictionary for decompression.
    dictionary: Option<Vec<u8>>,
}

impl ZstdDecoder {
    /// Create a new decoder.
    pub fn new() -> Self {
        Self {
            literals_decoder: LiteralsDecoder::new(),
            sequences_decoder: SequencesDecoder::new(),
            output: Vec::new(),
            lit_buf: Vec::new(),
            seq_buf: Vec::new(),
            window_size: MAX_WINDOW_SIZE,
            dictionary: None,
        }
    }

    /// Set a dictionary for decompression.
    ///
    /// Must match the dictionary used during compression.
    pub fn set_dictionary(&mut self, dict: &[u8]) {
        if dict.is_empty() {
            self.dictionary = None;
        } else {
            self.dictionary = Some(dict.to_vec());
        }
    }

    /// Refuse a formatted (RFC 8878 §5) dictionary before decoding anything.
    ///
    /// Only raw content dictionaries are implemented. A formatted dictionary
    /// must not be seeded as if it were content: its `Magic_Number`,
    /// `Dictionary_ID` and entropy tables are not part of the history, so a
    /// frame built against it would decode to silently wrong bytes.
    fn check_dictionary(&self) -> Result<()> {
        if self
            .dictionary
            .as_deref()
            .is_some_and(crate::dict::is_formatted_dictionary)
        {
            return Err(crate::dict::formatted_dictionary_error());
        }
        Ok(())
    }

    /// Decode a complete Zstandard frame.
    ///
    /// The frame starts at `data[0]`, after any run of skippable frames (RFC
    /// 8878 §3.1.2) prefixed to it — metadata the reference decoder walks past
    /// too. Bytes after the frame are ignored.
    ///
    /// Every call decodes one complete frame from a clean slate: the previous
    /// call's output and entropy tables are dropped first, so a decoder reused
    /// after a *failed* frame cannot carry that frame's partial output into
    /// this one, and a `Treeless` literals section or a `Repeat` FSE mode at
    /// the start of this frame is rejected rather than silently decoded with
    /// the previous frame's tables. A configured dictionary survives.
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::InvalidHeader`] when the frame names a
    /// `Dictionary_ID` (RFC 8878 §3.1.1.1.1.6) and no dictionary was supplied
    /// — the frame's matches would reach into content this decoder does not
    /// have, so decoding it would produce silently wrong bytes.
    ///
    /// Returns [`OxiArcError::UnsupportedMethod`] when the configured
    /// dictionary is a *formatted* one (RFC 8878 §5 `Magic_Number`
    /// `0xEC30A437`, as written by `zstd --train`). Only raw content
    /// dictionaries are implemented, and a formatted dictionary must not be
    /// mistaken for content: its header and entropy tables would seed the
    /// history with bytes that are not part of it, silently producing wrong
    /// output.
    ///
    /// Returns [`OxiArcError::CorruptedData`] when a block regenerates more
    /// than the frame's `Block_Maximum_Decompressed_Size`
    /// (`min(Window_Size, 128 KiB)`, further bounded by a declared
    /// `Frame_Content_Size`), and for the usual malformed/truncated frames.
    ///
    /// This decoder grows its output `Vec` without a ceiling; see
    /// [`crate::decompress_with_limit`] for the bounded counterpart.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_zstd::{ZstdDecoder, compress_with_level};
    ///
    /// let frame = compress_with_level(b"one frame", 3).expect("compress");
    /// let mut decoder = ZstdDecoder::new();
    /// assert_eq!(decoder.decode_frame(&frame).expect("decode"), b"one frame");
    /// ```
    pub fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let (decompressed, _consumed) = decompress_frame_with_decoder(data, self)?;
        Ok(decompressed)
    }

    /// Decode a compressed block, refusing one that regenerates more than
    /// `rfc_max` bytes (the frame's `Block_Maximum_Decompressed_Size`).
    fn decode_compressed_block(&mut self, data: &[u8], rfc_max: usize) -> Result<()> {
        // Literals and sequences go into buffers owned by the decoder and
        // reused for every block of every frame: decoding into a fresh `Vec`
        // per block put an allocation (and its first-touch page faults) on the
        // steady-state path, which is exactly what `ZstdStream` avoids.
        let Self {
            literals_decoder,
            sequences_decoder,
            lit_buf,
            seq_buf,
            ..
        } = self;
        let (literals_size, literals_len) = literals_decoder.decode_into(data, lit_buf)?;
        if literals_size > data.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "literals section overruns the block".to_string(),
            });
        }
        sequences_decoder.decode_into(&data[literals_size..], seq_buf)?;

        self.execute_sequences(literals_len, rfc_max)
    }

    /// Execute sequences to produce output.
    ///
    /// Every write is charged against `rfc_max` *before* it happens, so a block
    /// claiming to regenerate more than RFC 8878 allows is refused without
    /// first materialising it — the same rule, in the same order, as
    /// [`crate::ZstdStream`]'s sequence executor.
    fn execute_sequences(&mut self, literals_len: usize, rfc_max: usize) -> Result<()> {
        let mut lit_pos = 0usize;
        let mut produced = 0usize;
        let Self {
            ref mut output,
            ref lit_buf,
            ref seq_buf,
            ref dictionary,
            ..
        } = *self;
        let literals = &lit_buf[..literals_len];
        let sequences: &[Sequence] = seq_buf;
        let dict = dictionary.as_deref().unwrap_or(&[]);
        let dict_len = dict.len();

        for seq in sequences {
            // A `Sequence` holds `u32`s because the format bounds all three
            // below `2^32`; widening here is lossless on every pointer width
            // this crate supports (32 and 64 bits) and keeps the arithmetic
            // below in the type the buffers are indexed with.
            let literal_length = seq.literal_length as usize;
            let match_length = seq.match_length as usize;
            let seq_offset = seq.offset as usize;

            // Copy literals
            if literal_length > 0 {
                let end = lit_pos
                    .checked_add(literal_length)
                    .filter(|end| *end <= literals.len())
                    .ok_or_else(|| OxiArcError::CorruptedData {
                        offset: 0,
                        message: "literal length exceeds available literals".to_string(),
                    })?;
                produced = charge_block(produced, literal_length, rfc_max)?;
                append_literals(output, &literals[lit_pos..end]);
                lit_pos = end;
            }

            // Copy match
            if match_length > 0 {
                produced = charge_block(produced, match_length, rfc_max)?;
                let max_offset = output.len() + dict_len;
                if seq_offset == 0 || seq_offset > max_offset {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: format!(
                            "invalid offset {} (output length {}, dict length {})",
                            seq_offset,
                            output.len(),
                            dict_len
                        ),
                    });
                }

                if seq_offset <= output.len() {
                    // Normal case: the whole match lives in the output buffer.
                    append_match(output, seq_offset, match_length);
                } else {
                    // Dictionary reference: the match starts inside the
                    // dictionary. The logical buffer is `[dict | output]`, so
                    // copy the dictionary-resident prefix as one slice and
                    // then continue inside the output — where the remaining
                    // bytes start at output[0], i.e. at a distance of exactly
                    // `output.len()` behind the write cursor.
                    let combined = dict_len + output.len();
                    let start_in_dict = combined - seq_offset;
                    let from_dict = (dict_len - start_in_dict).min(match_length);
                    output.extend_from_slice(&dict[start_in_dict..start_in_dict + from_dict]);
                    let rest = match_length - from_dict;
                    if rest > 0 {
                        let offset = output.len();
                        append_match(output, offset, rest);
                    }
                }
            }
        }

        // Copy remaining literals
        if lit_pos < literals.len() {
            let rest = literals.len() - lit_pos;
            charge_block(produced, rest, rfc_max)?;
            append_literals(output, &literals[lit_pos..]);
        }

        Ok(())
    }

    /// Reset decoder state for a new frame.
    ///
    /// [`ZstdDecoder::decode_frame`] does this for you on every call; the
    /// method stays public for callers that want to drop a large output buffer
    /// early.
    ///
    /// Clears the output buffer, the repeat offsets **and the entropy tables**:
    /// the literals Huffman table (used by `Treeless` literal sections) and the
    /// three sequence FSE tables (used by `CompressionMode::Repeat`). Those
    /// tables are per-frame state; leaving them in place made a reused decoder
    /// silently accept a `Treeless`/`Repeat` block at the start of a *new*
    /// frame — decoding it with the previous frame's table instead of
    /// rejecting it as corrupt.
    pub fn reset(&mut self) {
        self.output.clear();
        self.literals_decoder.reset();
        self.sequences_decoder.reset();
    }
}

impl Default for ZstdDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress a single Zstandard frame.
///
/// Decodes the frame starting at `data[0]` and ignores everything after it;
/// use [`decompress_multi_frame`] for a concatenated stream. Skippable frames
/// (RFC 8878 §3.1.2) in front of it are metadata and are walked past, exactly
/// as `zstd -d`, [`crate::ZstdStream`] and [`crate::decompress_with_limit`] do;
/// a *truncated* one is still an error.
///
/// # Resource policy
///
/// Unbounded on purpose: the output `Vec` grows as the frame dictates and *is*
/// the window, so a frame declaring an 11 MB `Window_Size` costs nothing here
/// and is accepted. Only the format's own ceilings apply — a block may not
/// regenerate more than `min(Window_Size, 128 KiB)` (further bounded by a
/// declared `Frame_Content_Size`), which is what stops a single crafted block
/// from expanding without limit. For untrusted input use
/// [`crate::decompress_with_limit`], which bounds the total output *and*
/// refuses an over-large declared window.
///
/// # Errors
///
/// [`OxiArcError::InvalidHeader`] when the frame names a `Dictionary_ID` and
/// no dictionary was supplied (use [`decompress_with_dict`] then), plus the
/// usual corrupted-data errors. See [`ZstdDecoder::decode_frame`].
///
/// # Example
///
/// ```rust
/// use oxiarc_zstd::{compress_with_level, decompress};
///
/// let frame = compress_with_level(b"one frame", 3).expect("compress");
/// assert_eq!(decompress(&frame).expect("decode"), b"one frame");
/// ```
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZstdDecoder::new();
    decoder.decode_frame(data)
}

/// Decompress a single Zstandard frame using a raw-content dictionary.
///
/// Identical to [`decompress`] but seeds the match history with `dict`, which
/// also satisfies a frame that names a `Dictionary_ID`. Only raw content
/// dictionaries are supported; a formatted one (`zstd --train`) is refused —
/// see [`ZstdDecoder::decode_frame`].
pub fn decompress_with_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZstdDecoder::new();
    decoder.set_dictionary(dict);
    decoder.decode_frame(data)
}

/// Decompress a single Zstandard frame, returning the decompressed data and
/// the number of bytes consumed from `data`.
///
/// This allows callers to locate the end of one frame in a concatenated
/// stream and proceed to the next. The count includes any skippable frames
/// walked past in front of the Zstandard frame (see [`decompress`]); unlike
/// [`decompress_multi_frame`], nothing *else* is tolerated in front of it.
/// Same resource policy as [`decompress`].
pub fn decompress_frame(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut decoder = ZstdDecoder::new();
    decompress_frame_with_decoder(data, &mut decoder)
}

/// Decompress one or more concatenated Zstandard frames.
///
/// Skippable frames (magic `0x184D2A50`–`0x184D2A5F`) are dropped, wherever
/// they sit — including in front of the first Zstandard frame, which
/// [`decompress`] also walks past.
///
/// # Where the stream ends
///
/// Bytes that do not start a recognisable frame are tolerated **only after at
/// least one complete Zstandard frame has been decoded**: iteration stops
/// there and the accumulated output is returned, which is what lets a
/// container pad its payload. The very same bytes *before* any frame are an
/// error rather than an empty payload — leading garbage, a magic cut short by
/// end of input, or a truncated skippable frame all fail. A recognised but
/// truncated frame (Zstandard or skippable) is always an error, wherever it
/// sits. [`crate::ZstdStream`] and
/// [`crate::decompress_multi_frame_with_limit`] classify every one of these
/// cases identically; only the declared-window policy differs (see
/// [`decompress`]).
///
/// # Errors
///
/// [`OxiArcError::InvalidMagic`] for leading bytes that are neither a
/// Zstandard nor a skippable frame magic, and the usual corrupted-data errors
/// for a malformed or truncated frame.
///
/// # Example
///
/// ```rust
/// use oxiarc_zstd::{compress_with_level, decompress_multi_frame};
///
/// let mut stream = compress_with_level(b"one ", 3).expect("compress");
/// stream.extend_from_slice(&compress_with_level(b"two", 3).expect("compress"));
/// assert_eq!(decompress_multi_frame(&stream).expect("decode"), b"one two");
///
/// // Garbage before any frame is refused, not silently decoded as empty.
/// assert!(decompress_multi_frame(b"not a zstd stream").is_err());
/// ```
pub fn decompress_multi_frame(data: &[u8]) -> Result<Vec<u8>> {
    multi_frame_scan(data, decompress_frame)
}

/// Decompress one or more concatenated Zstandard frames using a dictionary.
///
/// Identical to [`decompress_multi_frame`] — including where the stream ends
/// — but applies `dict` to every frame. Each frame is decoded with a fresh
/// [`ZstdDecoder`] initialised with the supplied dictionary, so back-references
/// can resolve into the dictionary just as they do on the encoder side.
pub fn decompress_multi_frame_with_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    multi_frame_scan(data, |frame| {
        // Each frame is independent, so a fresh decoder per frame keeps FSE
        // table state from bleeding across a frame boundary while still
        // applying the dictionary.
        let mut decoder = ZstdDecoder::new();
        decoder.set_dictionary(dict);
        decompress_frame_with_decoder(frame, &mut decoder)
    })
}

/// Walk a concatenated Zstandard stream, decoding each frame with `decode_one`.
///
/// The frame-boundary rules are documented on [`decompress_multi_frame`] and
/// mirror [`crate::ZstdStream`]'s state machine exactly: a skippable frame is
/// dropped without counting as a decoded frame, unrecognised bytes end the
/// stream only once a real frame has been decoded, and everything else is an
/// error.
fn multi_frame_scan(
    data: &[u8],
    mut decode_one: impl FnMut(&[u8]) -> Result<(Vec<u8>, usize)>,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut pos = 0usize;
    // Skippable frames deliberately do not count: `ZstdStream` increments its
    // own counter only when a real frame ends, so `[skippable][2 stray bytes]`
    // is an error on both paths while `[skippable]` alone is a clean, empty
    // stream.
    let mut frames_decoded = 0usize;

    while pos < data.len() {
        // Fewer than 4 bytes left: a short tail after a complete frame is a
        // clean end, the same bytes before any frame are a truncated magic.
        if data.len() - pos < 4 {
            if frames_decoded > 0 {
                break;
            }
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "truncated Zstandard frame magic".to_string(),
            });
        }
        let magic = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);

        if magic == ZSTD_MAGIC_U32 {
            let (decompressed, consumed) = decode_one(&data[pos..])?;
            if consumed == 0 {
                // Unreachable today (a frame header is at least five bytes), but
                // a zero-width frame would spin this loop forever rather than
                // fail, so it is refused explicitly.
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "Zstandard frame consumed no input".to_string(),
                });
            }
            output.extend_from_slice(&decompressed);
            pos += consumed;
            frames_decoded += 1;
        } else if (crate::SKIPPABLE_MAGIC_LOW..=crate::SKIPPABLE_MAGIC_HIGH).contains(&magic) {
            // Skippable frame: 4 bytes magic + 4 bytes size + <size> bytes data.
            if data.len() - pos < 8 {
                return Err(OxiArcError::CorruptedData {
                    offset: (pos + 4) as u64,
                    message: "truncated skippable frame size".to_string(),
                });
            }
            let skip_size =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            // 64-bit arithmetic: `pos + 8 + skip_size` can overflow `usize` on
            // a 32-bit target.
            let end = pos as u64 + 8 + u64::from(skip_size);
            if end > data.len() as u64 {
                return Err(OxiArcError::CorruptedData {
                    offset: (pos + 8) as u64,
                    message: "truncated skippable frame payload".to_string(),
                });
            }
            pos = end as usize;
        } else {
            if frames_decoded > 0 {
                // Trailing bytes that start no known frame: stop here and
                // return what was decoded.
                break;
            }
            return Err(OxiArcError::invalid_magic(ZSTD_MAGIC, &data[pos..pos + 4]));
        }
    }

    Ok(output)
}

/// Decompress a single Zstandard frame using a caller-supplied decoder.
///
/// Returns the decompressed bytes and the number of bytes consumed from
/// `data` (the frame boundary).  The decoder's dictionary, if any, is used
/// for match resolution.
fn decompress_frame_with_decoder(
    data: &[u8],
    decoder: &mut ZstdDecoder,
) -> Result<(Vec<u8>, usize)> {
    // Every call decodes one complete frame from scratch. Without this, a
    // decoder reused after a *failed* `decode_frame` would carry that frame's
    // partial output into the next one — 131 076 bytes where 4 were expected,
    // silently, whenever the following frame declares neither a checksum nor a
    // `Frame_Content_Size` to catch it — and a `Treeless` literals section or a
    // `Repeat` FSE mode at the start of a new frame would borrow the previous
    // frame's entropy tables instead of being rejected. `ZstdStream::begin_frame`
    // resets exactly the same state for exactly these reasons.
    decoder.reset();
    decoder.check_dictionary()?;
    // Skippable frames in front of the real one are metadata, not content: the
    // reference decoder and `ZstdStream` both walk past them, so this path
    // does too. `multi_frame_scan` handles them itself and only ever calls in
    // on a Zstandard magic, so `skipped` is 0 there.
    let skipped = skippable_prefix_len(data)?;
    let data = &data[skipped..];
    let header = parse_frame_header(data)?;
    require_dictionary(&header, decoder.dictionary.is_some())?;
    decoder.window_size = header.window_size;
    // RFC 8878 `Block_Maximum_Decompressed_Size` for this frame; the same
    // ceiling `ZstdStream` derives, so both paths accept and refuse the same
    // blocks.
    let rfc_max = block_rfc_max(&header);

    if let Some(size) = header.content_size {
        reserve_output_capacity(&mut decoder.output, size, header.window_size)?;
    }

    let mut pos = header.header_size;

    loop {
        if data.len() < pos + 3 {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "truncated block header".to_string(),
            });
        }

        let block_header = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], 0]);
        pos += 3;

        let last_block = (block_header & 1) != 0;
        let block_type = BlockType::from_bits(((block_header >> 1) & 0x03) as u8)?;
        let block_size = ((block_header >> 3) & 0x1FFFFF) as usize;

        if block_size > MAX_BLOCK_SIZE {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: format!("block size {} exceeds maximum", block_size),
            });
        }

        if block_type == BlockType::Reserved {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "reserved block type".to_string(),
            });
        }

        // `Raw` and `Rle` blocks declare their regenerated size in the block
        // header, so the RFC ceiling is checked exactly, before a byte of the
        // payload is read (an `Rle` block is one input byte and would otherwise
        // expand first). A `Compressed` block is charged inside
        // `execute_sequences`, which is the only place its regenerated size
        // becomes known.
        if matches!(block_type, BlockType::Raw | BlockType::Rle) {
            charge_block(0, block_size, rfc_max)?;
        }

        let compressed_size = match block_type {
            BlockType::Rle => 1,
            _ => block_size,
        };

        if data.len() < pos + compressed_size {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "truncated block data".to_string(),
            });
        }

        let block_data = &data[pos..pos + compressed_size];
        pos += compressed_size;

        match block_type {
            BlockType::Raw => {
                decoder.output.extend_from_slice(block_data);
            }
            BlockType::Rle => {
                decoder
                    .output
                    .extend(std::iter::repeat_n(block_data[0], block_size));
            }
            BlockType::Compressed => {
                decoder.decode_compressed_block(block_data, rfc_max)?;
            }
            BlockType::Reserved => {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "reserved block type".to_string(),
                });
            }
        }

        if last_block {
            break;
        }
    }

    // Verify checksum if present.
    if header.has_checksum {
        if data.len() < pos + 4 {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "missing content checksum".to_string(),
            });
        }

        let expected = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let computed = xxhash64_checksum(&decoder.output);

        if expected != computed {
            return Err(OxiArcError::CrcMismatch { expected, computed });
        }
        pos += 4;
    }

    // Verify content size if known.
    if let Some(expected_size) = header.content_size {
        if decoder.output.len() as u64 != expected_size {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: format!(
                    "content size mismatch: expected {}, got {}",
                    expected_size,
                    decoder.output.len()
                ),
            });
        }
    }

    let decompressed = std::mem::take(&mut decoder.output);
    Ok((decompressed, skipped + pos))
}

/// Write a skippable Zstandard frame containing arbitrary user data.
///
/// `magic_nibble` selects which skippable magic to use; it is masked to the
/// lower 4 bits so the resulting magic is always in the range
/// `0x184D2A50`–`0x184D2A5F`.
pub fn write_skippable_frame(user_data: &[u8], magic_nibble: u8) -> Vec<u8> {
    let magic = crate::SKIPPABLE_MAGIC_LOW | (magic_nibble & 0xF) as u32;
    let mut out = Vec::with_capacity(8 + user_data.len());
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&(user_data.len() as u32).to_le_bytes());
    out.extend_from_slice(user_data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `append_match` must reproduce the byte-at-a-time definition of an LZ77
    /// back-reference for every offset and length, on both sides of the
    /// `SHORT_MATCH` split and through the pattern-doubling loop.
    ///
    /// The naive form here — `out.push(out[len - offset])`, one byte at a time
    /// — *is* the specification; the shipping code replaces it with a byte
    /// loop below `SHORT_MATCH` and with doubling `extend_from_within` runs
    /// above it, and the two must agree exactly, including the self-
    /// referential cases (`offset < len`) an RLE-style match relies on.
    #[test]
    fn append_match_matches_the_byte_at_a_time_definition() {
        let history: Vec<u8> = (0..200u16)
            .map(|i| (i.wrapping_mul(37) | 1) as u8)
            .collect();
        let mut checked = 0usize;
        for offset in 1..=history.len() {
            for len in [
                0usize, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 64, 100, 257, 1000,
            ] {
                let mut fast = history.clone();
                append_match(&mut fast, offset, len);

                let mut naive = history.clone();
                for _ in 0..len {
                    let byte = naive[naive.len() - offset];
                    naive.push(byte);
                }

                assert_eq!(fast, naive, "offset {offset} len {len}");
                checked += 1;
            }
        }
        assert!(checked > 3000, "only {checked} combinations");
    }

    #[test]
    fn test_parse_frame_header_minimal() {
        // Minimal frame: magic + descriptor (single segment, 1 byte content size)
        let mut data = Vec::new();
        data.extend_from_slice(&ZSTD_MAGIC);
        data.push(0x20); // Single segment flag
        data.push(5); // Content size = 5

        let header = parse_frame_header(&data).expect("operation failed");

        assert_eq!(header.content_size, Some(5));
        assert!(!header.has_checksum);
        assert!(header.dict_id.is_none());
    }

    #[test]
    fn test_parse_frame_header_with_checksum() {
        let mut data = Vec::new();
        data.extend_from_slice(&ZSTD_MAGIC);
        data.push(0x24); // Single segment + checksum
        data.push(10); // Content size = 10

        let header = parse_frame_header(&data).expect("operation failed");

        assert!(header.has_checksum);
        assert_eq!(header.content_size, Some(10));
    }

    #[test]
    fn test_invalid_magic() {
        let data = [0x00, 0x00, 0x00, 0x00, 0x00];
        let result = parse_frame_header(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decoder_creation() {
        let decoder = ZstdDecoder::new();
        assert_eq!(decoder.window_size, MAX_WINDOW_SIZE);
    }

    /// Regression test: a crafted frame declaring a `Frame_Content_Size`
    /// near `u64::MAX` must not panic (capacity overflow) or abort (OOM) —
    /// it must simply return `Err`. See `reserve_output_capacity`.
    #[test]
    fn test_decode_frame_huge_content_size_no_panic() {
        let mut data = Vec::new();
        data.extend_from_slice(&ZSTD_MAGIC);
        // Single-segment frame with an 8-byte content size field.
        data.push(0x20 | 0xC0); // FHD_SINGLE_SEGMENT | 8-byte content size flag
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        // No block data follows, so decoding must fail cleanly with a
        // corrupted-data error rather than panicking on the huge reserve.

        let result = std::panic::catch_unwind(|| {
            let mut decoder = ZstdDecoder::new();
            decoder.decode_frame(&data)
        });

        let decode_result = result.expect("decode_frame must not panic on huge content_size");
        assert!(decode_result.is_err());
    }

    /// Same regression, exercised through `reserve_output_capacity` directly
    /// to confirm the capped amount is well within `MAX_WINDOW_SIZE` and
    /// that `try_reserve` on an implausibly large request returns `Err`
    /// instead of aborting.
    #[test]
    fn test_reserve_output_capacity_caps_and_never_panics() {
        let mut output: Vec<u8> = Vec::new();
        // A content size near u64::MAX, with a window_size at the maximum
        // allowed, must be capped down to MAX_WINDOW_SIZE and succeed.
        let result = reserve_output_capacity(&mut output, u64::MAX, MAX_WINDOW_SIZE);
        assert!(result.is_ok());
        assert!(output.capacity() <= MAX_WINDOW_SIZE);

        // A merely large but "sane" content size should reserve exactly
        // that much when it is below the window size cap.
        let mut output2: Vec<u8> = Vec::new();
        let result2 = reserve_output_capacity(&mut output2, 1024, MAX_WINDOW_SIZE);
        assert!(result2.is_ok());
        assert!(output2.capacity() >= 1024);
    }

    #[test]
    fn test_block_type_parsing() {
        assert_eq!(
            BlockType::from_bits(0).expect("operation failed"),
            BlockType::Raw
        );
        assert_eq!(
            BlockType::from_bits(1).expect("operation failed"),
            BlockType::Rle
        );
        assert_eq!(
            BlockType::from_bits(2).expect("operation failed"),
            BlockType::Compressed
        );
        assert!(BlockType::from_bits(3).is_err());
    }
}
