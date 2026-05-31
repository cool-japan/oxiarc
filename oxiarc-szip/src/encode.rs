use crate::{SzipError, bitreader::BitWriter, params::SzipParams};

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Encode raw sample values into an AEC/SZIP-compatible bit stream.
///
/// This implementation uses the **no-compression** option ID for every block,
/// which is always valid per the CCSDS-121.0-B-2 specification.  The output
/// is therefore a lossless but uncompressed AEC stream that the decoder can
/// consume correctly.
///
/// When `params.nn_preprocess` is `true`, the unit-delay NN predictor is
/// applied before coding so that the decoder can correctly invert it.
///
/// # Note
/// This encoder is provided primarily to enable round-trip testing of the
/// decoder.  It does not attempt to compress the data.
pub fn encode(samples: &[u64], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    params.validate()?;

    if samples.is_empty() || params.samples == 0 {
        return Ok(Vec::new());
    }

    let bpp = params.bits_per_pixel;
    let ppb = params.pixels_per_block as usize;
    let id_no_compress = params.id_no_compress();
    let id_len = params.id_len();

    // If reference_sample_interval == 0, treat the whole stream as one RSI.
    let rsi_samples = if params.reference_sample_interval == 0 {
        params.samples
    } else {
        params.reference_sample_interval as usize
    };

    // Apply NN preprocessing if requested.
    let working: Vec<u64> = if params.nn_preprocess {
        apply_nn_preprocess(samples, params, rsi_samples)
    } else {
        samples.to_vec()
    };

    let mut writer = BitWriter::new(params.msb);
    let mut processed = 0usize;

    while processed < params.samples {
        // ── Reference sample (verbatim bpp bits) ──
        let ref_val = working[processed];
        write_sample(&mut writer, ref_val, bpp);
        processed += 1;

        // ── Encode blocks until the end of this RSI ──
        let rsi_end = (processed - 1 + rsi_samples).min(params.samples);

        while processed < rsi_end {
            let block_end = (processed + ppb).min(rsi_end);
            let block_len = block_end - processed;

            // Emit the no-compression option ID.
            writer.write_bits(id_no_compress, id_len);

            // Write block_len samples verbatim at bpp bits each.
            for &val in working[processed..(processed + block_len)].iter() {
                write_sample(&mut writer, val, bpp);
            }

            processed += block_len;
        }

        // If byte-alignment is requested between RSIs, pad to the next byte
        // boundary so the decoder can call `align_to_byte()` symmetrically.
        if params.rsi_byte_align {
            writer.align_to_byte();
        }
    }

    Ok(writer.finish())
}

// ───────────────────────────────────────────────────────────────────────────
// NN preprocessing (forward pass — applied before AEC coding)
// ───────────────────────────────────────────────────────────────────────────

/// Apply the unit-delay NN predictor (forward pass).
///
/// For each non-reference sample, compute the sigma-mapped residual relative
/// to the previous reconstructed sample:
///
/// - σ(delta) = 2·delta       if delta ≥ 0
/// - σ(delta) = −2·delta − 1  if delta <  0
fn apply_nn_preprocess(samples: &[u64], params: &SzipParams, rsi_samples: usize) -> Vec<u64> {
    let xmax = params.xmax() as i64;
    let mut out = Vec::with_capacity(samples.len());
    let mut processed = 0usize;

    while processed < samples.len() {
        // Reference sample: written verbatim.
        let ref_val = samples[processed];
        out.push(ref_val);
        processed += 1;

        let rsi_end = (processed - 1 + rsi_samples).min(samples.len());
        let mut prev = ref_val as i64;

        while processed < rsi_end {
            let curr = samples[processed] as i64;
            let delta = curr - prev;
            let sigma = sigma_map(delta);
            out.push(sigma);
            // Clamp to simulate the reconstructed value at the decoder.
            prev = curr.clamp(0, xmax);
            processed += 1;
        }
    }

    out
}

/// Forward sigma map.
fn sigma_map(delta: i64) -> u64 {
    if delta >= 0 {
        (2 * delta) as u64
    } else {
        (-2 * delta - 1) as u64
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Helpers
// ───────────────────────────────────────────────────────────────────────────

/// Write a single `bpp`-bit sample to the writer.
fn write_sample(writer: &mut BitWriter, value: u64, bpp: u8) {
    if bpp <= 32 {
        writer.write_bits(value as u32, bpp);
    } else {
        // For bpp > 32, split into two write_bits calls.
        let high_bits = bpp - 32;
        let high = (value >> 32) as u32;
        let low = value as u32;
        writer.write_bits(high, high_bits);
        writer.write_bits(low, 32);
    }
}

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
/// into a compressed AEC stream.  This is a convenience wrapper around
/// [`encode`] for callers who work in bytes rather than `u64` sample arrays.
pub fn encode_bytes(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    let samples = bytes_to_samples(input, params);
    encode(&samples, params)
}
