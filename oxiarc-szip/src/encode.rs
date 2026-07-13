use crate::{SzipError, bitreader::BitWriter, params::SzipParams};

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Encode raw sample values into an AEC/SZIP-compatible bit stream.
///
/// This implementation uses the **no-compression** option ID for every block,
/// which is always valid per the CCSDS-121.0-B-2 specification. The output
/// is therefore a lossless but uncompressed AEC stream that any conforming
/// decoder — including the libaec reference implementation — can consume
/// correctly (when `params.msb` is `true`, the standard bit ordering).
///
/// Framing follows CCSDS-121.0-B-2 §5.2 / libaec exactly:
///
/// - Every coded block starts with its option ID; the block body follows.
/// - Blocks always contain `pixels_per_block` samples. A trailing partial
///   block is padded by repeating the last sample value (matching libaec's
///   flush behaviour); the decoder discards the padding.
/// - When `params.nn_preprocess` is `true`, the first sample of each
///   reference sample interval is the verbatim reference sample (sample #0
///   of that RSI's first block, *after* the block's option ID) and every
///   other sample is the theta-clamped prediction residual of the unit-delay
///   predictor.
///
/// # Errors
///
/// - [`SzipError::InputTooShort`] if `samples.len() < params.samples`.
/// - [`SzipError::SampleOutOfRange`] if any encoded sample exceeds
///   [`SzipParams::xmax`].
/// - [`SzipError::InvalidParam`] if `params` fail validation.
///
/// # Note
///
/// This encoder is provided primarily to enable round-trip and
/// interoperability testing of the decoder. It does not attempt to compress
/// the data.
pub fn encode(samples: &[u64], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    params.validate()?;

    if params.samples == 0 {
        return Ok(Vec::new());
    }
    if samples.len() < params.samples {
        return Err(SzipError::InputTooShort {
            need: params.samples,
            have: samples.len(),
        });
    }

    let data = &samples[..params.samples];
    let xmax = params.xmax();
    for (index, &value) in data.iter().enumerate() {
        if value > xmax {
            return Err(SzipError::SampleOutOfRange {
                index,
                value,
                max: xmax,
            });
        }
    }

    let bpp = params.bits_per_pixel;
    let block_size = params.pixels_per_block as usize;
    let id_len = params.id_len();
    let id_no_compress = params.id_no_compress();

    // Reference sample interval in samples. `0` means the whole stream is a
    // single RSI.
    let rsi_samples = if params.reference_sample_interval == 0 {
        params.samples.div_ceil(block_size) * block_size
    } else {
        params.reference_sample_interval as usize
    };

    let mut writer = BitWriter::new(params.msb);
    let mut offset = 0usize;

    while offset < data.len() {
        let chunk_end = (offset + rsi_samples).min(data.len());
        let chunk = &data[offset..chunk_end];
        let blocks = chunk.len().div_ceil(block_size);

        // Pad the trailing partial block (if any) by repeating the last
        // sample, exactly as libaec's encoder does when flushing.
        let mut padded: Vec<u32> = chunk.iter().map(|&v| v as u32).collect();
        if let Some(&last) = padded.last() {
            padded.resize(blocks * block_size, last);
        }

        // Apply the unit-delay predictor across the whole RSI if requested.
        // Slot 0 keeps the verbatim reference sample.
        let coded = if params.nn_preprocess {
            preprocess_rsi(&padded, xmax)
        } else {
            padded
        };

        for block in coded.chunks_exact(block_size) {
            // Option ID first, then the block's samples — the reference
            // sample (when preprocessing) is sample #0 of the RSI's first
            // block and sits *inside* the block, after the ID.
            writer.write_bits(id_no_compress, id_len);
            for &value in block {
                writer.write_bits(value, bpp);
            }
        }

        // If byte-alignment is requested between RSIs, pad to the next byte
        // boundary so the decoder can call `align_to_byte()` symmetrically.
        if params.rsi_byte_align {
            writer.align_to_byte();
        }

        offset = chunk_end;
    }

    Ok(writer.finish())
}

// ───────────────────────────────────────────────────────────────────────────
// NN preprocessing (forward pass — applied before AEC coding)
// ───────────────────────────────────────────────────────────────────────────

/// Apply the unit-delay predictor (forward pass) to one RSI of samples.
///
/// Implements the CCSDS-121.0-B-2 §4 preprocessor for unsigned samples,
/// identical to libaec's `preprocess_unsigned`: each non-reference sample is
/// replaced by the theta-clamped mapped prediction residual
///
/// - Δ ≥ 0, Δ ≤ θ → 2·Δ
/// - Δ < 0, |Δ| ≤ θ → 2·|Δ| − 1
/// - otherwise      → θ + |Δ|
///
/// where the prediction is the previous sample `x[i-1]` and
/// `θ = min(x[i-1], xmax − x[i-1])`. The clamping guarantees every residual
/// fits in `bits_per_pixel` bits. Slot 0 keeps the verbatim reference
/// sample.
fn preprocess_rsi(x: &[u32], xmax: u64) -> Vec<u32> {
    let mut d = Vec::with_capacity(x.len());
    let Some(&first) = x.first() else {
        return d;
    };
    d.push(first);

    for pair in x.windows(2) {
        let prev = u64::from(pair[0]);
        let cur = u64::from(pair[1]);
        let mapped = if cur >= prev {
            let delta = cur - prev;
            // Δ ≥ 0 implies Δ ≤ xmax − prev, so the only reachable clamp is
            // θ = prev; θ + Δ then equals the current sample itself.
            if delta <= prev { 2 * delta } else { cur }
        } else {
            let delta = prev - cur;
            // Δ < 0 implies |Δ| ≤ prev, so the only reachable clamp is
            // θ = xmax − prev; θ + |Δ| then equals xmax − cur.
            if delta <= xmax - prev {
                2 * delta - 1
            } else {
                xmax - cur
            }
        };
        d.push(mapped as u32);
    }

    d
}

// ───────────────────────────────────────────────────────────────────────────
// Byte/sample conversion helpers
// ───────────────────────────────────────────────────────────────────────────

/// Convert raw uncompressed bytes to a `Vec<u64>` sample array, interpreting
/// each sample as `bytes_per_sample` bytes in big-endian order.
///
/// This is the inverse of [`crate::decode::samples_to_bytes`] and is used
/// internally and in integration tests.
pub fn bytes_to_samples(bytes: &[u8], params: &SzipParams) -> Vec<u64> {
    let bps = params.bytes_per_sample();
    bytes
        .chunks_exact(bps)
        .map(|chunk| match bps {
            1 => chunk[0] as u64,
            2 => u16::from_be_bytes([chunk[0], chunk[1]]) as u64,
            4 => u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as u64,
            _ => u64::from_be_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]),
        })
        .collect()
}

/// Encode a raw byte slice (as produced by the decoder output format) back
/// into an AEC stream. This is a convenience wrapper around [`encode`] for
/// callers who work in bytes rather than `u64` sample arrays.
///
/// # Errors
///
/// In addition to everything [`encode`] rejects, returns
/// [`SzipError::InvalidParam`] when `input.len()` is not a whole multiple of
/// [`SzipParams::bytes_per_sample`] (a trailing partial sample would
/// otherwise be silently dropped).
pub fn encode_bytes(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    params.validate()?;
    if input.len() % params.bytes_per_sample() != 0 {
        return Err(SzipError::InvalidParam(
            "input length is not a multiple of bytes_per_sample",
        ));
    }
    let samples = bytes_to_samples(input, params);
    encode(&samples, params)
}
