//! Meta-block prelude parsing for the incremental decoder (tier 1).
//!
//! RFC 7932 Section 9.2 puts a great deal of structure in front of the first
//! command of a meta-block: up to 256 literal, insert-and-copy and distance
//! prefix codes, two context maps of up to `256 x 64` and `256 x 4` entries,
//! block-switch codes for three categories, and the distance parameters. A
//! *resumable* parser for that sub-grammar would be far more state than the
//! command loop itself.
//!
//! The incremental decoder therefore parses the prelude **atomically**: it
//! saves the bit cursor, parses, and on
//! [`BrotliError::UnexpectedEof`](crate::error::BrotliError::UnexpectedEof)
//! rewinds and asks for more input. Re-parsing on retry is free in practice —
//! preludes are rare (one per meta-block, which is up to 16 MiB of output) and
//! small. The retry buffer is capped at [`MAX_METABLOCK_HEADER`]; a prelude
//! that does not complete within it is corruption, not slow input.
//!
//! The compressed case delegates to [`MetaBlockHeader::read`], the very same
//! parser the one-shot [`crate::decompress::decompress`] uses, so both
//! decoders consume header bits at identical positions — which is what the
//! `MetaBlockShape` differential test checks.

use crate::bit_reader::BitReader;
use crate::decompress::{MetaBlockHeader, read_window_bits};
use crate::error::{BrotliError, BrotliResult};

use super::budget::OutputBudget;

/// Largest number of unconsumed input bytes a meta-block prelude may span
/// before the stream is declared corrupt (1 MiB).
///
/// Real preludes stay well under 100 KiB even with the maximum 256 prefix
/// codes per category, so this only bounds the retry buffer of a hostile or
/// broken stream.
pub(crate) const MAX_METABLOCK_HEADER: usize = 1024 * 1024;

/// What a meta-block prelude turned out to describe.
pub(crate) enum MetaBlockStart {
    /// `ISLAST = 1, ISLASTEMPTY = 1`: the stream ends here, no more output.
    LastEmpty,
    /// A metadata meta-block: `skip` payload bytes follow on a byte boundary
    /// and produce no output.
    Metadata {
        /// `MSKIPLEN`, the number of payload bytes to discard.
        skip: usize,
        /// Whether this meta-block carried `ISLAST = 1`.
        is_last: bool,
    },
    /// An uncompressed meta-block: `mlen` raw bytes follow on a byte boundary.
    /// RFC 7932 only permits this when `ISLAST = 0`.
    Uncompressed {
        /// `MLEN`, the number of raw bytes to copy out.
        mlen: usize,
    },
    /// A compressed meta-block whose entire prelude has been parsed.
    Compressed {
        /// `MLEN`, the exact number of bytes this meta-block produces.
        mlen: usize,
        /// Whether this meta-block carried `ISLAST = 1`.
        is_last: bool,
        /// Prefix codes, context maps and block-switch state.
        header: Box<MetaBlockHeader>,
    },
}

/// Read the stream header (`WBITS`, RFC 7932 Section 9.1).
///
/// Kept here so the incremental decoder's stream-header step reads exactly the
/// bits the one-shot decoder reads.
pub(crate) fn read_stream_header(reader: &mut BitReader<'_>) -> BrotliResult<u32> {
    read_window_bits(reader)
}

/// Parse one meta-block prelude.
///
/// `total_out` is the number of bytes the stream has produced so far; together
/// with the meta-block's exact `MLEN` it lets `budget` reject an over-large
/// stream *before* any of the offending meta-block is decoded, and before its
/// prefix codes are even built.
pub(crate) fn parse_meta_block_start(
    reader: &mut BitReader<'_>,
    budget: &OutputBudget,
    total_out: u64,
) -> BrotliResult<MetaBlockStart> {
    let is_last = reader.read_bit()?;
    if is_last && reader.read_bit()? {
        return Ok(MetaBlockStart::LastEmpty);
    }

    // MNIBBLES (Section 9.2): 3 means a metadata meta-block.
    let mnibbles_code = reader.read_bits(2)?;
    if mnibbles_code == 3 {
        let skip = read_metadata_prelude(reader)?;
        return Ok(MetaBlockStart::Metadata { skip, is_last });
    }

    let mnibbles = mnibbles_code + 4;
    let mlen_minus_1 = reader.read_bits(mnibbles * 4)?;
    // Reject non-minimal length encodings: with 5 or 6 nibbles the most
    // significant nibble must be non-zero.
    if mnibbles > 4 && (mlen_minus_1 >> ((mnibbles - 1) * 4)) == 0 {
        return Err(BrotliError::CorruptedData(
            "non-minimal MLEN encoding".to_string(),
        ));
    }
    let mlen = mlen_minus_1 as usize + 1;

    // A meta-block emits exactly MLEN bytes, so this is an exact projection:
    // an over-budget stream is refused with nothing of it decoded.
    budget.check(total_out.saturating_add(mlen as u64))?;

    // ISUNCOMPRESSED is only present when ISLAST is 0.
    if !is_last {
        let is_uncompressed = reader.read_bit()?;
        if is_uncompressed {
            if reader.align_to_byte()? != 0 {
                return Err(BrotliError::CorruptedData(
                    "non-zero padding before uncompressed data".to_string(),
                ));
            }
            return Ok(MetaBlockStart::Uncompressed { mlen });
        }
    }

    let header = MetaBlockHeader::read(reader)?;
    Ok(MetaBlockStart::Compressed {
        mlen,
        is_last,
        header: Box::new(header),
    })
}

/// Read a metadata meta-block header (RFC 7932 Section 9.2) and return
/// `MSKIPLEN`, leaving the reader byte-aligned at the start of the payload.
///
/// The one-shot decoder's `skip_metadata_block` also discards the payload; the
/// incremental decoder must not, because `MSKIPLEN` can reach 2^24 and the
/// payload arrives across many calls.
fn read_metadata_prelude(reader: &mut BitReader<'_>) -> BrotliResult<usize> {
    // Reserved bit must be zero.
    if reader.read_bit()? {
        return Err(BrotliError::CorruptedData(
            "reserved bit set in metadata block".to_string(),
        ));
    }
    let mskipbytes = reader.read_bits(2)?;
    let mskiplen = if mskipbytes == 0 {
        0usize
    } else {
        let mut value = 0u32;
        for i in 0..mskipbytes {
            let byte = reader.read_bits(8)?;
            // Non-minimal encodings are invalid: the last byte must be
            // non-zero when more than one byte is used.
            if i + 1 == mskipbytes && mskipbytes > 1 && byte == 0 {
                return Err(BrotliError::CorruptedData(
                    "non-minimal MSKIPLEN encoding".to_string(),
                ));
            }
            value |= byte << (i * 8);
        }
        value as usize + 1
    };
    if reader.align_to_byte()? != 0 {
        return Err(BrotliError::CorruptedData(
            "non-zero padding in metadata block".to_string(),
        ));
    }
    Ok(mskiplen)
}
