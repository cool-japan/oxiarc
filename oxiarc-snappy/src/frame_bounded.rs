//! Bounded (memory-budgeted) Snappy framed decoding.
//!
//! The Snappy framing format declares no *total* uncompressed size, so the
//! only way to bound a decode is to enforce a cap as the output grows. Each
//! chunk, however, declares its own uncompressed size — the block-format
//! varint for compressed chunks, the chunk length for uncompressed ones — so
//! the budget can be enforced *before* each chunk is decoded, which is what
//! [`decompress_frame_with_limit`] does. A decompression bomb is rejected
//! without its expansion ever being allocated.

use crate::crc32c::masked_crc32c;
use crate::decompress;
use crate::error::SnappyError;
use crate::frame::{
    CHUNK_TYPE_COMPRESSED, CHUNK_TYPE_STREAM_ID, CHUNK_TYPE_UNCOMPRESSED, MAX_CHUNK_WIRE_LEN,
    MAX_UNCOMPRESSED_CHUNK_SIZE, STREAM_BODY, STREAM_IDENTIFIER,
};

/// Decompress a standard Snappy frame from memory, refusing to produce more
/// than `max_output` bytes.
///
/// This is the one-shot equivalent of
/// `FrameDecoder::new(input).with_max_output_size(max_output)`, returning a
/// typed [`SnappyError`] instead of an `io::Error` — it is what callers that
/// need a decompression-bomb guard on untrusted `.sz` input (for example the
/// OxiArc CLI's `--memory-limit`) route through.
///
/// # Enforcement
///
/// The Snappy framing format declares no total uncompressed size, but every
/// *chunk* declares its own: the block-format varint for compressed chunks
/// and the chunk length for uncompressed ones. The budget is therefore
/// checked per chunk **before that chunk's output is decoded or allocated**,
/// so a bomb is rejected without its expansion being materialised. Peak
/// output allocation is bounded by `max_output`; the transient per-chunk
/// scratch is bounded by the framing format's 64 KiB chunk cap.
///
/// # Errors
///
/// Returns [`SnappyError::TotalOutputExceeded`] as soon as a chunk would push
/// the cumulative output past `max_output`, and the usual
/// truncation/corruption errors otherwise.
///
/// # Example
///
/// ```rust
/// use oxiarc_snappy::{FrameEncoder, decompress_frame_with_limit, SnappyError};
/// use std::io::Write;
///
/// let data = vec![0u8; 1_000_000];
/// let mut frame = Vec::new();
/// {
///     let mut encoder = FrameEncoder::new(&mut frame);
///     encoder.write_all(&data).expect("write");
///     encoder.finish().expect("finish");
/// }
///
/// // Generous budget: decodes normally.
/// assert_eq!(decompress_frame_with_limit(&frame, 4 << 20).expect("decode"), data);
///
/// // Tight budget: rejected while decoding, not after.
/// assert!(matches!(
///     decompress_frame_with_limit(&frame, 64 * 1024),
///     Err(SnappyError::TotalOutputExceeded { .. })
/// ));
/// ```
pub fn decompress_frame_with_limit(input: &[u8], max_output: u64) -> Result<Vec<u8>, SnappyError> {
    // [`crate::FrameEncoder`] writes the stream identifier lazily, so encoding
    // no data at all yields a zero-byte frame. [`crate::FrameDecoder`] accepts
    // that as an empty stream; stay consistent with it.
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut pos = 0usize;

    // Stream identifier.
    if input.len() < STREAM_IDENTIFIER.len() {
        return Err(SnappyError::UnexpectedEof {
            context: "stream identifier",
        });
    }
    if input[..STREAM_IDENTIFIER.len()] != STREAM_IDENTIFIER[..] {
        return Err(SnappyError::InvalidStreamIdentifier);
    }
    pos += STREAM_IDENTIFIER.len();

    let mut output: Vec<u8> = Vec::new();

    // Reject a chunk that would push the cumulative output past the budget,
    // using the size the chunk *declares* — i.e. before it is decoded.
    let check_budget = |produced: usize, incoming: usize| -> Result<(), SnappyError> {
        let projected = (produced as u64).saturating_add(incoming as u64);
        if projected > max_output {
            return Err(SnappyError::TotalOutputExceeded {
                produced: projected,
                max: max_output,
            });
        }
        Ok(())
    };

    while pos < input.len() {
        if pos + 4 > input.len() {
            return Err(SnappyError::UnexpectedEof {
                context: "chunk header",
            });
        }
        let chunk_type = input[pos];
        let chunk_body_len = (input[pos + 1] as usize)
            | ((input[pos + 2] as usize) << 8)
            | ((input[pos + 3] as usize) << 16);
        pos += 4;

        if pos + chunk_body_len > input.len() {
            return Err(SnappyError::UnexpectedEof {
                context: "chunk body",
            });
        }
        let chunk_body = &input[pos..pos + chunk_body_len];
        pos += chunk_body_len;

        match chunk_type {
            CHUNK_TYPE_COMPRESSED => {
                if chunk_body.len() < 4 {
                    return Err(SnappyError::CorruptedData {
                        message: "compressed chunk too short for checksum".to_string(),
                    });
                }
                if chunk_body.len() > MAX_CHUNK_WIRE_LEN {
                    return Err(SnappyError::ChunkTooLarge {
                        size: chunk_body.len(),
                        max: MAX_CHUNK_WIRE_LEN,
                    });
                }
                let expected_checksum = u32::from_le_bytes([
                    chunk_body[0],
                    chunk_body[1],
                    chunk_body[2],
                    chunk_body[3],
                ]);
                let payload = &chunk_body[4..];

                // Declared-size budget check, then a bounded block decode as
                // defense in depth (the block decoder re-checks the same
                // declared length against the remaining budget).
                let declared = decompress::get_decompress_len(payload)?;
                check_budget(output.len(), declared)?;
                let remaining = usize::try_from(max_output.saturating_sub(output.len() as u64))
                    .unwrap_or(usize::MAX);
                let decompressed = decompress::decompress_with_limit(payload, remaining)?;

                if decompressed.len() > MAX_UNCOMPRESSED_CHUNK_SIZE {
                    return Err(SnappyError::ChunkTooLarge {
                        size: decompressed.len(),
                        max: MAX_UNCOMPRESSED_CHUNK_SIZE,
                    });
                }
                let computed_checksum = masked_crc32c(&decompressed);
                if expected_checksum != computed_checksum {
                    return Err(SnappyError::ChecksumMismatch {
                        expected: expected_checksum,
                        computed: computed_checksum,
                    });
                }
                output.extend_from_slice(&decompressed);
            }
            CHUNK_TYPE_UNCOMPRESSED => {
                if chunk_body.len() < 4 {
                    return Err(SnappyError::CorruptedData {
                        message: "uncompressed chunk too short for checksum".to_string(),
                    });
                }
                if chunk_body.len() > MAX_CHUNK_WIRE_LEN {
                    return Err(SnappyError::ChunkTooLarge {
                        size: chunk_body.len(),
                        max: MAX_CHUNK_WIRE_LEN,
                    });
                }
                let expected_checksum = u32::from_le_bytes([
                    chunk_body[0],
                    chunk_body[1],
                    chunk_body[2],
                    chunk_body[3],
                ]);
                let raw_data = &chunk_body[4..];
                check_budget(output.len(), raw_data.len())?;

                let computed_checksum = masked_crc32c(raw_data);
                if expected_checksum != computed_checksum {
                    return Err(SnappyError::ChecksumMismatch {
                        expected: expected_checksum,
                        computed: computed_checksum,
                    });
                }
                output.extend_from_slice(raw_data);
            }
            // A repeated stream identifier is legal, but its body must match.
            CHUNK_TYPE_STREAM_ID if chunk_body != STREAM_BODY => {
                return Err(SnappyError::InvalidStreamIdentifier);
            }
            CHUNK_TYPE_STREAM_ID => {}
            0x02..=0x7F => {
                return Err(SnappyError::InvalidChunkType { chunk_type });
            }
            _ => {
                // 0x80..=0xFE: skippable chunk, already consumed above.
            }
        }
    }

    Ok(output)
}
