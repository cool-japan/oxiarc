use crate::{SzipError, bitreader::BitReader, params::SzipParams};

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Decode an AEC/SZIP compressed byte slice into raw sample bytes.
///
/// The returned bytes are packed according to `params.bits_per_pixel`:
///
/// - bpp ≤  8 → 1 byte per sample
/// - bpp ≤ 16 → 2 bytes per sample, big-endian
/// - bpp ≤ 32 → 4 bytes per sample, big-endian
///
/// The big-endian packing matches the CCSDS convention for data words and is
/// consistent with how a downstream HDF5 byte-order converter expects them.
pub fn decode(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError> {
    params.validate()?;

    if params.samples == 0 {
        return Ok(Vec::new());
    }

    let bpp = params.bits_per_pixel;
    let ppb = params.pixels_per_block as usize;
    let id_len = params.id_len();
    let id_no_compress = params.id_no_compress();

    // If reference_sample_interval == 0 the whole stream is one RSI.
    let rsi_samples = if params.reference_sample_interval == 0 {
        params.samples
    } else {
        params.reference_sample_interval as usize
    };

    let mut out: Vec<u64> = Vec::with_capacity(params.samples);
    // Track which output indices are RSI-reference samples so that the NN
    // inverse-preprocessing step can reset the predictor correctly.
    let mut rsi_ref_indices: Vec<usize> = Vec::new();

    let mut reader = BitReader::new(input, params.msb);
    let mut processed = 0usize;

    while processed < params.samples {
        // ── Reference sample (verbatim bpp bits) at the start of each RSI ──
        let ref_val = reader.read_bits(bpp)? as u64;
        rsi_ref_indices.push(out.len());
        out.push(ref_val);
        processed += 1;

        // ── Decode blocks until the end of this RSI ──
        let rsi_end = (processed - 1 + rsi_samples).min(params.samples);

        while processed < rsi_end {
            let block_end = (processed + ppb).min(rsi_end);
            let block_len = block_end - processed;

            // Read the option ID.
            let id = reader.read_bits(id_len)?;

            if id == 0 {
                // Low-entropy block (zero-block or second-extension).
                let block_samples = decode_low_entropy_block(&mut reader, block_len)?;
                out.extend_from_slice(&block_samples);
            } else if id == id_no_compress {
                // No-compression block: read block_len verbatim samples.
                for _ in 0..block_len {
                    let v = reader.read_bits(bpp)? as u64;
                    out.push(v);
                }
            } else {
                // k-split Golomb-Rice block: k = id - 1.
                let k = (id - 1) as u8;
                let block_samples = decode_ksplit_block(&mut reader, block_len, k)?;
                out.extend_from_slice(&block_samples);
            }

            processed = out.len();
        }

        // After each RSI the stream may optionally be padded to the next byte
        // boundary (CHIP hardware mode). If `rsi_byte_align` is set, skip
        // any padding bits, using `bits_consumed()` to confirm progress.
        if params.rsi_byte_align {
            let before = reader.bits_consumed();
            reader.align_to_byte();
            // In debug builds, confirm that we actually advanced (or were
            // already aligned) without decoding backwards.
            debug_assert!(
                reader.bits_consumed() >= before,
                "align_to_byte moved backwards: before={before}, after={}",
                reader.bits_consumed()
            );
        }
    }

    // Apply inverse NN preprocessing if requested.
    if params.nn_preprocess {
        inverse_nn_preprocess(&mut out, params, &rsi_ref_indices)?;
    }

    // Pack the u64 sample values into bytes.
    Ok(samples_to_bytes(&out, params))
}

// ───────────────────────────────────────────────────────────────────────────
// Low-entropy block decoder (option ID = 0)
// ───────────────────────────────────────────────────────────────────────────

/// Decode a low-entropy block (option ID = 0).
///
/// After the option ID has already been consumed by the caller, the next bit
/// distinguishes between two sub-options:
///
/// - `0` → zero block: all `block_len` samples are 0.
/// - `1` → second-extension block: decode `block_len` values using k=0
///   Golomb-Rice (unary coding).
fn decode_low_entropy_block(
    reader: &mut BitReader<'_>,
    block_len: usize,
) -> Result<Vec<u64>, SzipError> {
    let sub_opt = reader.read_bits(1)?;
    let mut samples = Vec::with_capacity(block_len);
    if sub_opt == 0 {
        // Zero block: all block_len samples are 0.
        samples.resize(block_len, 0u64);
    } else {
        // Second-extension: decode block_len values with k=0 Golomb-Rice.
        // The fundamental sequence (k=0) encodes a value `v` as `v` unary
        // '1' bits followed by a terminating '0' bit.
        for _ in 0..block_len {
            let v = read_unary(reader)?;
            samples.push(v);
        }
    }
    Ok(samples)
}

// ───────────────────────────────────────────────────────────────────────────
// Golomb-Rice k-split block decoder
// ───────────────────────────────────────────────────────────────────────────

/// Decode `block_len` samples from a Golomb-Rice k-split block.
///
/// For each sample:
/// 1. Read a unary-coded quotient: count consecutive '1' bits until a '0' bit.
/// 2. Read `k` low bits as the remainder.
/// 3. Encoded value = `(quotient << k) | remainder`.
fn decode_ksplit_block(
    reader: &mut BitReader<'_>,
    block_len: usize,
    k: u8,
) -> Result<Vec<u64>, SzipError> {
    let mut samples = Vec::with_capacity(block_len);
    for _ in 0..block_len {
        let high = read_unary(reader)?;
        let low = if k > 0 {
            reader.read_bits(k)? as u64
        } else {
            0
        };
        let value = (high << k) | low;
        samples.push(value);
    }
    Ok(samples)
}

// ───────────────────────────────────────────────────────────────────────────
// Unary code reader
// ───────────────────────────────────────────────────────────────────────────

/// Read a unary-coded non-negative integer.
///
/// Counts the number of consecutive `1` bits until a `0` terminator bit is
/// found. The terminator bit is consumed but not counted.
fn read_unary(reader: &mut BitReader<'_>) -> Result<u64, SzipError> {
    let mut count: u64 = 0;
    loop {
        let bit = reader.read_bits(1)?;
        if bit == 0 {
            break;
        }
        count += 1;
    }
    Ok(count)
}

// ───────────────────────────────────────────────────────────────────────────
// Inverse NN (unit-delay nearest-neighbour) preprocessing
// ───────────────────────────────────────────────────────────────────────────

/// Apply the inverse of the unit-delay NN predictor to the decoded sample
/// array.
///
/// The AEC stream, when `nn_preprocess` is enabled, stores σ-mapped residuals
/// rather than raw sample values.  This function converts those residuals back
/// to the original samples.
///
/// `rsi_ref_indices` must list the index of every RSI reference sample (i.e.
/// the samples that were encoded verbatim without NN preprocessing).
fn inverse_nn_preprocess(
    out: &mut [u64],
    params: &SzipParams,
    rsi_ref_indices: &[usize],
) -> Result<(), SzipError> {
    if out.is_empty() {
        return Ok(());
    }

    let xmax = params.xmax() as i64;

    // Build a fast lookup set from the RSI reference indices.
    // Since the indices are generated in order during decoding, we can use a
    // sorted slice and binary search, which avoids allocating a HashSet.
    let mut ref_iter = rsi_ref_indices.iter().peekable();
    // The first element is always a reference sample; skip it.
    ref_iter.next();

    let mut prev = out[0] as i64;

    for (i, sample) in out.iter_mut().enumerate().skip(1) {
        // Check whether index i is a reference sample.
        let is_ref = match ref_iter.peek() {
            Some(&&idx) if idx == i => {
                ref_iter.next();
                true
            }
            _ => false,
        };

        if is_ref {
            // Reference sample is verbatim; just reset the predictor.
            prev = *sample as i64;
        } else {
            // Non-reference sample: apply the inverse sigma map and add to
            // the previous reconstructed sample.
            let sigma = *sample;
            let delta = inv_sigma(sigma);
            let reconstructed = (prev + delta).clamp(0, xmax);
            *sample = reconstructed as u64;
            prev = reconstructed;
        }
    }

    Ok(())
}

/// Inverse of the CCSDS sigma map:
///
/// ```text
/// σ(x) = 2·x       if x >= 0  (even → positive)
/// σ(x) = −2·x − 1  if x <  0  (odd  → negative)
/// ```
///
/// The inverse is therefore:
///
/// ```text
/// σ⁻¹(s) = s/2        if s is even
/// σ⁻¹(s) = −(s+1)/2   if s is odd
/// ```
fn inv_sigma(sigma: u64) -> i64 {
    if sigma & 1 == 0 {
        (sigma >> 1) as i64
    } else {
        // Avoid overflow for large sigma values by casting carefully.
        let s = sigma as i128;
        (-(s + 1) / 2) as i64
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Sample-to-bytes conversion
// ───────────────────────────────────────────────────────────────────────────

/// Pack `u64` sample values into a flat byte array.
///
/// Each sample occupies `params.bytes_per_sample()` bytes in big-endian order,
/// matching the CCSDS convention for MSB-first data words.
pub fn samples_to_bytes(out: &[u64], params: &SzipParams) -> Vec<u8> {
    let bps = params.bytes_per_sample();
    let mut bytes = Vec::with_capacity(out.len() * bps);
    for &v in out {
        match bps {
            1 => bytes.push(v as u8),
            2 => bytes.extend_from_slice(&(v as u16).to_be_bytes()),
            4 => bytes.extend_from_slice(&(v as u32).to_be_bytes()),
            _ => bytes.extend_from_slice(&v.to_be_bytes()),
        }
    }
    bytes
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::{SzipParams, decode, encode};

    fn make_params(bpp: u8, ppb: u32, samples: usize) -> SzipParams {
        SzipParams {
            bits_per_pixel: bpp,
            pixels_per_block: ppb,
            samples,
            reference_sample_interval: ppb, // one block per RSI
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn to_samples_8bpp(bytes: &[u8]) -> Vec<u64> {
        bytes.iter().map(|&b| b as u64).collect()
    }

    fn to_samples_16bpp_be(bytes: &[u8]) -> Vec<u64> {
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]) as u64)
            .collect()
    }

    fn to_samples_32bpp_be(bytes: &[u8]) -> Vec<u64> {
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]) as u64)
            .collect()
    }

    // ── round-trip tests ─────────────────────────────────────────────────

    #[test]
    fn round_trip_all_zeros_8bpp() {
        let params = make_params(8, 8, 64);
        let samples: Vec<u64> = vec![0u64; 64];
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "all-zeros round-trip failed");
    }

    #[test]
    fn round_trip_ramp_8bpp() {
        let params = make_params(8, 8, 32);
        let samples: Vec<u64> = (0..32u64).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "ramp round-trip failed");
    }

    #[test]
    fn round_trip_single_value_16bpp() {
        let params = make_params(16, 8, 16);
        let samples: Vec<u64> = vec![12345u64; 16];
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_16bpp_be(&decoded);
        assert_eq!(decoded_samples, samples);
    }

    #[test]
    fn round_trip_max_values_8bpp() {
        let params = make_params(8, 8, 32);
        let samples: Vec<u64> = vec![255u64; 32];
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "max-values round-trip failed");
    }

    #[test]
    fn round_trip_32bpp() {
        let params = make_params(32, 8, 16);
        let samples: Vec<u64> = vec![
            100_000u64,
            200_000,
            0,
            4_294_967_295,
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12,
        ];
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_32bpp_be(&decoded);
        assert_eq!(decoded_samples, samples, "32bpp round-trip failed");
    }

    #[test]
    fn round_trip_multiple_rsi_blocks() {
        // 4 RSI groups of 8 samples each = 32 samples total
        let params = SzipParams {
            bits_per_pixel: 8,
            pixels_per_block: 8,
            samples: 32,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        };
        let samples: Vec<u64> = (0..32u64).map(|i| i * 7 % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "multi-RSI round-trip failed");
    }

    #[test]
    fn round_trip_with_nn_preprocess() {
        let mut params = make_params(8, 8, 32);
        params.nn_preprocess = true;
        // Use a smooth ramp so the NN predictor residuals are small.
        let samples: Vec<u64> = (0..32u64).map(|i| i + 50).collect(); // 50..82
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "nn-preprocess round-trip failed");
    }

    #[test]
    fn round_trip_ppb_16() {
        let params = make_params(8, 16, 64);
        let samples: Vec<u64> = (0..64u64).map(|i| i * 3 % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "ppb=16 round-trip failed");
    }

    #[test]
    fn round_trip_ppb_32() {
        let params = make_params(8, 32, 64);
        let samples: Vec<u64> = (0..64u64).map(|i| (i * 5 + 10) % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "ppb=32 round-trip failed");
    }

    #[test]
    fn empty_input_returns_empty() {
        let params = make_params(8, 8, 0);
        let result = decode(&[], &params).expect("empty decode failed");
        assert!(result.is_empty(), "expected empty output for 0 samples");
    }

    #[test]
    fn invalid_bpp_zero_rejected() {
        let params = SzipParams {
            bits_per_pixel: 0, // invalid
            pixels_per_block: 8,
            samples: 8,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        };
        assert!(decode(&[0u8; 64], &params).is_err());
    }

    #[test]
    fn invalid_bpp_too_large_rejected() {
        let params = SzipParams {
            bits_per_pixel: 33, // invalid: >32
            pixels_per_block: 8,
            samples: 8,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        };
        assert!(decode(&[0u8; 64], &params).is_err());
    }

    #[test]
    fn invalid_ppb_rejected() {
        let params = SzipParams {
            bits_per_pixel: 8,
            pixels_per_block: 7, // invalid: not 8/16/32
            samples: 8,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        };
        assert!(decode(&[0u8; 64], &params).is_err());
    }

    // ── bitreader round-trip test ─────────────────────────────────────────

    #[test]
    fn bitwriter_bitreader_msb_round_trip() {
        use crate::bitreader::{BitReader, BitWriter};
        let patterns: &[(u32, u8)] = &[
            (0b1011, 4),
            (0b00000001, 8),
            (0b11111111, 8),
            (0b1, 1),
            (0b101010, 6),
        ];
        let mut writer = BitWriter::new(true);
        for &(val, bits) in patterns {
            writer.write_bits(val, bits);
        }
        let data = writer.finish();

        let mut reader = BitReader::new(&data, true);
        for &(expected, bits) in patterns {
            let got = reader.read_bits(bits).expect("read_bits failed");
            assert_eq!(
                got, expected,
                "MSB round-trip mismatch: {expected} vs {got}"
            );
        }
    }

    #[test]
    fn bitwriter_bitreader_lsb_round_trip() {
        use crate::bitreader::{BitReader, BitWriter};
        let patterns: &[(u32, u8)] = &[
            (0b1011, 4),
            (0b00000001, 8),
            (0b11111111, 8),
            (0b1, 1),
            (0b101010, 6),
        ];
        let mut writer = BitWriter::new(false);
        for &(val, bits) in patterns {
            writer.write_bits(val, bits);
        }
        let data = writer.finish();

        let mut reader = BitReader::new(&data, false);
        for &(expected, bits) in patterns {
            let got = reader.read_bits(bits).expect("read_bits failed");
            assert_eq!(
                got, expected,
                "LSB round-trip mismatch: {expected} vs {got}"
            );
        }
    }

    #[test]
    fn bitreader_bits_consumed_tracks_correctly() {
        use crate::bitreader::BitReader;
        // 3 bits + 5 bits = 8 bits = 1 byte consumed.
        let data = [0b10110101u8];
        let mut reader = BitReader::new(&data, true);
        assert_eq!(reader.bits_consumed(), 0);
        reader.read_bits(3).expect("read 3 bits");
        assert_eq!(reader.bits_consumed(), 3);
        reader.read_bits(5).expect("read 5 bits");
        assert_eq!(reader.bits_consumed(), 8);
    }

    #[test]
    fn bitreader_align_to_byte_pads_partial() {
        use crate::bitreader::{BitReader, BitWriter};
        // Write 3 bits then align to byte boundary.
        let mut writer = BitWriter::new(true);
        writer.write_bits(0b101, 3);
        let data = writer.finish();

        let mut reader = BitReader::new(&data, true);
        reader.read_bits(3).expect("read 3 bits");
        assert_eq!(reader.bits_consumed(), 3);
        reader.align_to_byte();
        // After aligning, consumed should be rounded up to 8.
        assert_eq!(reader.bits_consumed(), 8);
    }

    #[test]
    fn bitwriter_align_to_byte_flushes_partial() {
        use crate::bitreader::BitWriter;
        let mut writer = BitWriter::new(true);
        writer.write_bits(0b101, 3);
        writer.align_to_byte(); // should flush partial byte
        writer.write_bits(0b11001100, 8);
        let data = writer.finish();
        // First byte: 101xxxxx, second: 11001100
        assert_eq!(data.len(), 2);
        assert_eq!(data[0] & 0b1110_0000, 0b1010_0000);
        assert_eq!(data[1], 0b1100_1100);
    }

    #[test]
    fn round_trip_rsi_byte_align() {
        // Test that the rsi_byte_align flag works end-to-end across multiple
        // RSI boundaries.
        let params = SzipParams {
            bits_per_pixel: 8,
            pixels_per_block: 8,
            samples: 32,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: true,
        };
        let samples: Vec<u64> = (0..32u64).map(|i| i * 5 % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "rsi_byte_align round-trip failed");
    }
}
