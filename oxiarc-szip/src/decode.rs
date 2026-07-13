use crate::{SzipError, bitreader::BitReader, params::SzipParams};

// ───────────────────────────────────────────────────────────────────────────
// Constants (CCSDS-121.0-B-2 §5.3 / libaec)
// ───────────────────────────────────────────────────────────────────────────

/// Zero-block run length that signals "remainder of segment" (ROS).
const ROS: u64 = 5;

/// Zero-block segment length in blocks: zero-block runs never cross a
/// 64-block boundary within an RSI.
const ZERO_SEGMENT_BLOCKS: usize = 64;

/// Largest legal second-extension codeword (libaec's `SE_TABLE_SIZE`): with
/// γ = (d₀+d₁)(d₀+d₁+1)/2 + d₁ and d₀+d₁ ≤ 12, γ ≤ 90.
const SE_MAX_GAMMA: u64 = 90;

// ───────────────────────────────────────────────────────────────────────────
// Public entry point
// ───────────────────────────────────────────────────────────────────────────

/// Decode an AEC/SZIP compressed byte slice into raw sample bytes.
///
/// The bit-stream framing follows CCSDS-121.0-B-2 §5.2 / libaec exactly:
///
/// - Every coded block starts with its option ID (`id_len()` bits): `0` for
///   the low-entropy options (a further extension bit selects zero-block
///   runs or the second-extension option), all-ones for no-compression, and
///   `k + 1` for the sample-split (Golomb-Rice) options.
/// - When `params.nn_preprocess` is `true`, the first sample of each
///   reference sample interval is a verbatim reference sample. It is sample
///   #0 of that RSI's first block and is read *after* that block's option ID
///   (and after the low-entropy extension bit, where applicable).
/// - Blocks always hold `pixels_per_block` samples; a trailing partial RSI
///   consists of `ceil(remaining / pixels_per_block)` full blocks whose
///   padding samples are decoded and discarded.
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

    let block_size = params.pixels_per_block as usize;

    // Full reference-sample interval in blocks. `reference_sample_interval`
    // of 0 treats the whole stream as one RSI.
    let rsi_blocks = if params.reference_sample_interval == 0 {
        params.samples.div_ceil(block_size)
    } else {
        params.reference_sample_interval as usize / block_size
    };

    let ctx = Ctx {
        bpp: params.bits_per_pixel,
        block_size,
        id_len: params.id_len(),
        id_no_compress: params.id_no_compress(),
        k_max: params.k_max(),
        pp: params.nn_preprocess,
        rsi_blocks,
        xmax: params.xmax() as u32,
    };

    let mut out: Vec<u32> = Vec::with_capacity(params.samples);
    let mut reader = BitReader::new(input, params.msb);

    while out.len() < params.samples {
        let remaining = params.samples - out.len();
        // The encoder always emits whole blocks; a trailing partial RSI
        // holds ceil(remaining / J) blocks with padded samples we discard.
        let blocks_this_rsi = ctx.rsi_blocks.min(remaining.div_ceil(block_size));

        let buf = decode_rsi(&mut reader, &ctx, blocks_this_rsi)?;

        let take = remaining.min(buf.len());
        out.extend_from_slice(&buf[..take]);

        // After each full RSI the stream may optionally be padded to the
        // next byte boundary (libaec's AEC_PAD_RSI). Skip that padding,
        // using `bits_consumed()` to confirm progress in debug builds.
        if params.rsi_byte_align {
            let before = reader.bits_consumed();
            reader.align_to_byte();
            debug_assert!(
                reader.bits_consumed() >= before,
                "align_to_byte moved backwards: before={before}, after={}",
                reader.bits_consumed()
            );
        }
    }

    Ok(samples_to_bytes(&out, params))
}

// ───────────────────────────────────────────────────────────────────────────
// Per-RSI decoding
// ───────────────────────────────────────────────────────────────────────────

/// Immutable decoding parameters shared by every block decoder.
struct Ctx {
    bpp: u8,
    block_size: usize,
    id_len: u8,
    id_no_compress: u32,
    k_max: u8,
    pp: bool,
    /// Blocks per *full* reference sample interval.
    rsi_blocks: usize,
    /// Maximum representable sample value, truncated to 32 bits.
    xmax: u32,
}

/// Decode one reference sample interval of `blocks_this_rsi` blocks into a
/// flat sample buffer (`blocks_this_rsi * block_size` samples), applying the
/// inverse preprocessor if configured.
fn decode_rsi(
    reader: &mut BitReader<'_>,
    ctx: &Ctx,
    blocks_this_rsi: usize,
) -> Result<Vec<u32>, SzipError> {
    let mut buf: Vec<u32> = Vec::with_capacity(blocks_this_rsi * ctx.block_size);
    let mut block_idx = 0usize;

    while block_idx < blocks_this_rsi {
        // A verbatim reference sample leads the first block of each RSI when
        // preprocessing is active. It follows the block's option ID.
        let has_ref = ctx.pp && block_idx == 0;
        let id = reader.read_bits(ctx.id_len)?;

        if id == 0 {
            // Low-entropy options: one extension bit selects between a
            // zero-block run (0) and the second-extension option (1). The
            // reference sample follows the extension bit.
            let extension = reader.read_bits(1)?;
            if has_ref {
                buf.push(reader.read_bits(ctx.bpp)?);
            }
            if extension == 0 {
                block_idx =
                    decode_zero_blocks(reader, ctx, &mut buf, block_idx, blocks_this_rsi, has_ref)?;
            } else {
                decode_se_block(reader, ctx, &mut buf, has_ref)?;
                block_idx += 1;
            }
        } else if id == ctx.id_no_compress {
            // No-compression block: all block_size samples verbatim. With
            // preprocessing, sample #0 of the RSI's first block is the raw
            // reference sample and the rest are mapped residuals.
            for _ in 0..ctx.block_size {
                buf.push(reader.read_bits(ctx.bpp)?);
            }
            block_idx += 1;
        } else {
            // Sample-split (Golomb-Rice) block: k = id - 1.
            let k = (id - 1) as u8;
            if k > ctx.k_max {
                return Err(SzipError::InvalidBlockOption { id, bpp: ctx.bpp });
            }
            if has_ref {
                buf.push(reader.read_bits(ctx.bpp)?);
            }
            decode_split_block(reader, &mut buf, ctx.block_size - usize::from(has_ref), k)?;
            block_idx += 1;
        }
    }

    if ctx.pp {
        inverse_preprocess_rsi(&mut buf, ctx.xmax);
    }

    Ok(buf)
}

// ───────────────────────────────────────────────────────────────────────────
// Zero-block runs (option ID = 0, extension bit = 0)
// ───────────────────────────────────────────────────────────────────────────

/// Decode a zero-block run. The FS codeword after the (already consumed)
/// extension bit and optional reference sample encodes the run length:
///
/// - FS + 1 ∈ 1..=4 → that many zero blocks
/// - FS + 1 == 5    → ROS: zero blocks up to the end of the current
///   64-block segment or RSI, whichever is nearer
/// - FS + 1 >  5    → FS zero blocks (runs ≥ 5 are stored off by one)
///
/// Returns the block index after the run.
fn decode_zero_blocks(
    reader: &mut BitReader<'_>,
    ctx: &Ctx,
    buf: &mut Vec<u32>,
    block_idx: usize,
    blocks_this_rsi: usize,
    has_ref: bool,
) -> Result<usize, SzipError> {
    let fs = read_fs(reader)?;

    let zero_blocks: u64 = if fs + 1 == ROS {
        // ROS fills to the nearer of the RSI end or the 64-block segment
        // boundary (computed against the *full* RSI, exactly like libaec;
        // in a trailing partial RSI the excess is padding we never emit).
        let to_rsi_end = (ctx.rsi_blocks - block_idx) as u64;
        let to_segment_end = (ZERO_SEGMENT_BLOCKS - (block_idx % ZERO_SEGMENT_BLOCKS)) as u64;
        to_rsi_end.min(to_segment_end)
    } else if fs + 1 > ROS {
        fs
    } else {
        fs + 1
    };

    // A run that claims more blocks than the full RSI has left is corrupt
    // (libaec's rsi_size bound check).
    let available = (ctx.rsi_blocks - block_idx) as u64;
    if zero_blocks > available {
        return Err(SzipError::LengthMismatch {
            expected: usize::try_from(available * ctx.block_size as u64).unwrap_or(usize::MAX),
            actual: usize::try_from(zero_blocks.saturating_mul(ctx.block_size as u64))
                .unwrap_or(usize::MAX),
        });
    }

    // Emit only the blocks that exist in this (possibly trailing-partial)
    // RSI; the remainder of a ROS run is discarded padding and consumes no
    // input bits.
    let fill_blocks = (zero_blocks as usize).min(blocks_this_rsi - block_idx);
    let fill_samples = fill_blocks * ctx.block_size - usize::from(has_ref);
    buf.resize(buf.len() + fill_samples, 0);

    Ok(block_idx + fill_blocks)
}

// ───────────────────────────────────────────────────────────────────────────
// Second-extension blocks (option ID = 0, extension bit = 1)
// ───────────────────────────────────────────────────────────────────────────

/// Decode a second-extension block: consecutive sample pairs (d₀, d₁) are
/// coded as a single FS codeword γ = (d₀+d₁)(d₀+d₁+1)/2 + d₁. When a
/// reference sample leads the block, the first codeword contributes only d₁
/// (its d₀ belongs to the reference slot and is discarded).
fn decode_se_block(
    reader: &mut BitReader<'_>,
    ctx: &Ctx,
    buf: &mut Vec<u32>,
    has_ref: bool,
) -> Result<(), SzipError> {
    let mut i = usize::from(has_ref);

    while i < ctx.block_size {
        let gamma = read_fs(reader)?;
        if gamma > SE_MAX_GAMMA {
            // The codeword exceeds what the option can legally encode
            // (libaec's SE_TABLE_SIZE bound) — reject the stream. The
            // low-entropy second-extension option is selected by ID 0.
            return Err(SzipError::InvalidBlockOption {
                id: 0,
                bpp: ctx.bpp,
            });
        }

        // Invert γ: find w = d₀ + d₁ (the largest w with w(w+1)/2 ≤ γ).
        let mut w: u64 = 0;
        while (w + 1) * (w + 2) / 2 <= gamma {
            w += 1;
        }
        let d1 = gamma - w * (w + 1) / 2;
        let d0 = w - d1;

        if i % 2 == 0 {
            buf.push(d0 as u32);
            i += 1;
        }
        buf.push(d1 as u32);
        i += 1;
    }

    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Golomb-Rice sample-split blocks (option ID = k + 1)
// ───────────────────────────────────────────────────────────────────────────

/// Decode `count` samples from a sample-split block: first the FS-coded
/// quotients of *all* samples, then the k-bit remainders of all samples,
/// grouped (CCSDS-121.0-B-2 §5.2; not interleaved per sample).
fn decode_split_block(
    reader: &mut BitReader<'_>,
    buf: &mut Vec<u32>,
    count: usize,
    k: u8,
) -> Result<(), SzipError> {
    let start = buf.len();
    for _ in 0..count {
        let quotient = read_fs(reader)?;
        // 32-bit wrapping matches libaec's uint32 arithmetic on corrupt
        // streams; k ≤ 29 makes the shift itself well-defined.
        buf.push((quotient as u32) << k);
    }
    if k > 0 {
        for slot in &mut buf[start..] {
            let remainder = reader.read_bits(k)?;
            *slot = slot.wrapping_add(remainder);
        }
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Fundamental-sequence reader
// ───────────────────────────────────────────────────────────────────────────

/// Read a fundamental-sequence (FS) codeword: the value is the number of
/// consecutive `0` bits before a terminating `1` bit (CCSDS-121.0-B-2
/// §5.1.2). The terminator is consumed but not counted.
fn read_fs(reader: &mut BitReader<'_>) -> Result<u64, SzipError> {
    let mut count: u64 = 0;
    while reader.read_bits(1)? == 0 {
        count += 1;
    }
    Ok(count)
}

// ───────────────────────────────────────────────────────────────────────────
// Inverse preprocessing (unit-delay predictor, unsigned samples)
// ───────────────────────────────────────────────────────────────────────────

/// Undo the CCSDS-121.0-B-2 §4 unsigned preprocessor across one RSI buffer.
///
/// Sample 0 is the verbatim reference sample; every following sample holds
/// the theta-clamped mapped residual D and is replaced by the reconstructed
/// value. With θ = min(prev, xmax − prev):
///
/// - D ≤ 2θ, D even → prev + D/2
/// - D ≤ 2θ, D odd  → prev − (D+1)/2
/// - D > 2θ         → xmax − D if prev is in the upper half-range, else D
///
/// The 32-bit wrapping arithmetic mirrors libaec's `flush_*` loops exactly,
/// including behaviour on out-of-spec residuals.
fn inverse_preprocess_rsi(buf: &mut [u32], xmax: u32) {
    let Some((first, rest)) = buf.split_first_mut() else {
        return;
    };
    let med = xmax / 2 + 1;
    let mut prev = *first;

    for slot in rest {
        let d = *slot;
        let half_d = (d >> 1) + (d & 1);
        // prev >= med ⇔ prev & med != 0 for in-range prev (med is a power
        // of two); mask ^ prev is then xmax - prev, i.e. θ when prev is in
        // the upper half-range, and prev itself (θ) otherwise.
        let mask = if prev & med != 0 { xmax } else { 0 };
        prev = if half_d <= (mask ^ prev) {
            let signed_half = (d >> 1) ^ (if d & 1 != 0 { u32::MAX } else { 0 });
            prev.wrapping_add(signed_half)
        } else {
            mask ^ d
        };
        *slot = prev;
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Sample-to-bytes conversion
// ───────────────────────────────────────────────────────────────────────────

/// Pack decoded sample values into a flat byte array.
///
/// Each sample occupies `params.bytes_per_sample()` bytes in big-endian
/// order, matching the CCSDS convention for MSB-first data words.
pub fn samples_to_bytes(out: &[u32], params: &SzipParams) -> Vec<u8> {
    let bps = params.bytes_per_sample();
    let mut bytes = Vec::with_capacity(out.len() * bps);
    for &v in out {
        match bps {
            1 => bytes.push(v as u8),
            2 => bytes.extend_from_slice(&(v as u16).to_be_bytes()),
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
    use crate::{SzipError, SzipParams, decode, encode, encode_bytes};

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
    fn round_trip_nn_preprocess_theta_clamp() {
        // Values near the range boundaries exercise the theta-clamped
        // branches of the CCSDS mapping (|delta| > theta).
        let mut params = make_params(8, 8, 16);
        params.samples = 16;
        params.reference_sample_interval = 16;
        params.nn_preprocess = true;
        let samples: Vec<u64> = vec![0, 255, 0, 200, 3, 250, 1, 128, 254, 2, 255, 0, 0, 255, 7, 9];
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "theta-clamp round-trip failed");
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
    fn round_trip_ppb_64() {
        let params = make_params(8, 64, 192);
        let samples: Vec<u64> = (0..192u64).map(|i| (i * 11 + 3) % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        let decoded_samples = to_samples_8bpp(&decoded);
        assert_eq!(decoded_samples, samples, "ppb=64 round-trip failed");
    }

    #[test]
    fn round_trip_partial_trailing_block() {
        // 13 samples with J=8: the encoder must pad the trailing block and
        // the decoder must discard the padding.
        let params = SzipParams {
            samples: 13,
            reference_sample_interval: 16,
            ..SzipParams::default()
        };
        let samples: Vec<u64> = (0..13u64).map(|i| (i * 37 + 5) % 256).collect();
        let compressed = encode(&samples, &params).expect("encode failed");
        let decoded = decode(&compressed, &params).expect("decode failed");
        assert_eq!(to_samples_8bpp(&decoded), samples);
    }

    #[test]
    fn empty_input_returns_empty() {
        let params = make_params(8, 8, 0);
        let result = decode(&[], &params).expect("empty decode failed");
        assert!(result.is_empty(), "expected empty output for 0 samples");
    }

    // ── spec-framing tests (CCSDS-121.0-B-2 §5.2 / libaec) ───────────────

    /// SZIP-01 regression: hand-computed no-compression stream with
    /// preprocessing. Per CCSDS-121.0-B-2 §5.2 the option ID precedes ALL
    /// pixels_per_block samples of the RSI's first block; the reference
    /// sample is sample #0 of that block.
    ///
    /// bpp=8 → id_len=3, no-compression ID = 0b111. Samples
    /// [10, 11, 9, 12, 12, 12, 13, 11] preprocess (theta-clamped unit-delay
    /// mapping) to [10(ref), 2, 3, 6, 0, 0, 2, 3], so the stream is the
    /// 3-bit ID followed by those eight 8-bit values:
    ///
    /// 111 00001010 00000010 00000011 00000110 00000000 00000000 00000010
    /// 00000011 + 5 pad bits
    #[test]
    fn spec_framing_option_id_before_reference_sample() {
        let mut params = make_params(8, 8, 8);
        params.nn_preprocess = true;
        let samples: Vec<u64> = vec![10, 11, 9, 12, 12, 12, 13, 11];
        let compressed = encode(&samples, &params).expect("encode failed");
        let expected: Vec<u8> = vec![0xE1, 0x40, 0x40, 0x60, 0xC0, 0x00, 0x00, 0x40, 0x60];
        assert_eq!(
            compressed, expected,
            "encoded framing does not match CCSDS-121.0-B-2 s5.2 layout"
        );
        let decoded = decode(&expected, &params).expect("decode failed");
        assert_eq!(to_samples_8bpp(&decoded), samples);
    }

    /// Hand-built zero-block run: two all-zero blocks encoded as a single
    /// low-entropy CDS. Stream: ID=000, extension=0, FS(1)=01 (run length
    /// m+1 = 2) → 000 0 01 + pad = 0x04.
    #[test]
    fn spec_zero_block_run_decodes() {
        let params = SzipParams {
            samples: 16,
            reference_sample_interval: 16,
            ..SzipParams::default()
        };
        let decoded = decode(&[0x04], &params).expect("zero-run decode failed");
        assert_eq!(decoded, vec![0u8; 16]);
    }

    /// Hand-built sample-split block (k = 0): ID=001 followed by eight FS
    /// codewords "1" (value 0 each) → 001 11111111 + pad = [0x3F, 0xE0].
    #[test]
    fn spec_split_block_k0_decodes() {
        let params = SzipParams {
            samples: 8,
            ..SzipParams::default()
        };
        let decoded = decode(&[0x3F, 0xE0], &params).expect("k-split decode failed");
        assert_eq!(decoded, vec![0u8; 8]);
    }

    /// Hand-built second-extension block: pairs (1,0) (0,1) (2,0) (0,0)
    /// give gammas 1, 2, 3, 0 → FS codes 01, 001, 0001, 1.
    /// Stream: 000 1 01 001 0001 1 + pad = [0x14, 0x8C].
    #[test]
    fn spec_second_extension_block_decodes() {
        let params = SzipParams {
            samples: 8,
            ..SzipParams::default()
        };
        let decoded = decode(&[0x14, 0x8C], &params).expect("SE decode failed");
        assert_eq!(decoded, vec![1, 0, 0, 1, 2, 0, 0, 0]);
    }

    // ── error-path tests ─────────────────────────────────────────────────

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
            pixels_per_block: 7, // invalid: not 8/16/32/64
            samples: 8,
            reference_sample_interval: 7,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        };
        assert!(decode(&[0u8; 64], &params).is_err());
    }

    #[test]
    fn invalid_rsi_not_block_multiple_rejected() {
        let params = SzipParams {
            reference_sample_interval: 12, // not a multiple of 8
            samples: 24,
            ..SzipParams::default()
        };
        assert!(matches!(
            decode(&[0u8; 64], &params),
            Err(SzipError::InvalidParam(_))
        ));
    }

    /// SZIP-02 regression: a sample buffer shorter than `params.samples`
    /// must return a typed error, not panic with a slice OOB.
    #[test]
    fn encode_short_buffer_returns_input_too_short() {
        let params = SzipParams {
            samples: 64,
            ..SzipParams::default()
        };
        let samples = vec![0u64; 10]; // shorter than params.samples
        match encode(&samples, &params) {
            Err(SzipError::InputTooShort { need, have }) => {
                assert_eq!(need, 64);
                assert_eq!(have, 10);
            }
            other => panic!("expected InputTooShort, got {other:?}"),
        }
    }

    /// SZIP-02 regression: `encode_bytes` with a trailing partial sample
    /// (input not a multiple of bytes_per_sample) must be rejected instead
    /// of silently dropping the tail.
    #[test]
    fn encode_bytes_partial_sample_rejected() {
        let params = SzipParams {
            bits_per_pixel: 16,
            samples: 3,
            ..SzipParams::default()
        };
        // 5 bytes is not a multiple of the 2-byte sample size.
        let result = encode_bytes(&[1, 2, 3, 4, 5], &params);
        assert!(matches!(result, Err(SzipError::InvalidParam(_))));
    }

    /// SZIP-02 regression: empty input with a non-zero declared sample
    /// count must error rather than silently produce an empty stream.
    #[test]
    fn encode_empty_buffer_nonzero_samples_rejected() {
        let params = SzipParams {
            samples: 8,
            ..SzipParams::default()
        };
        assert!(matches!(
            encode(&[], &params),
            Err(SzipError::InputTooShort { .. })
        ));
    }

    /// SZIP-03 regression: sample values exceeding `xmax()` must be
    /// rejected instead of silently truncated to the low bpp bits.
    #[test]
    fn encode_sample_out_of_range_rejected() {
        let params = SzipParams {
            samples: 8,
            ..SzipParams::default()
        };
        let mut samples = vec![0u64; 8];
        samples[3] = 300; // exceeds xmax()=255 for bpp=8
        match encode(&samples, &params) {
            Err(SzipError::SampleOutOfRange { index, value, max }) => {
                assert_eq!(index, 3);
                assert_eq!(value, 300);
                assert_eq!(max, 255);
            }
            other => panic!("expected SampleOutOfRange, got {other:?}"),
        }
    }

    /// SZIP-04 regression: a sample-split option ID whose k exceeds
    /// `k_max()` must be rejected as an invalid block option. For bpp=5
    /// (id_len=3) k_max is 3, so ID 5 (k=4) is out of spec.
    #[test]
    fn decode_ksplit_option_out_of_range_rejected() {
        let params = SzipParams {
            bits_per_pixel: 5,
            samples: 8,
            ..SzipParams::default()
        };
        assert_eq!(params.k_max(), 3);
        // First 3 bits = option ID 5 (0b101) → k = 4 > k_max.
        let stream = [0b1010_0000u8, 0, 0, 0, 0, 0, 0, 0];
        match decode(&stream, &params) {
            Err(SzipError::InvalidBlockOption { id, bpp }) => {
                assert_eq!(id, 5);
                assert_eq!(bpp, 5);
            }
            other => panic!("expected InvalidBlockOption, got {other:?}"),
        }
    }

    /// A zero-block run longer than the remaining RSI must be rejected.
    #[test]
    fn decode_zero_run_overrun_rejected() {
        // rsi = 16 samples = 2 blocks; encode a run of 3 blocks:
        // ID=000, ext=0, FS(2)=001 (m+1 = 3) → 0000001 + pad = 0x02.
        let params = SzipParams {
            samples: 16,
            reference_sample_interval: 16,
            ..SzipParams::default()
        };
        assert!(matches!(
            decode(&[0x02], &params),
            Err(SzipError::LengthMismatch { .. })
        ));
    }

    /// A second-extension codeword above the legal bound (90) must be
    /// rejected (libaec's SE_TABLE_SIZE check).
    #[test]
    fn decode_second_extension_gamma_overflow_rejected() {
        // ID=000, ext=1, then a FS codeword of 91+ zeros. 12 zero bytes
        // after the 4 header bits give ~92 zeros before any terminator.
        let params = SzipParams {
            samples: 8,
            ..SzipParams::default()
        };
        let mut stream = vec![0b0001_0000u8];
        stream.extend_from_slice(&[0u8; 12]);
        stream.push(0xFF);
        assert!(matches!(
            decode(&stream, &params),
            Err(SzipError::InvalidBlockOption { .. })
        ));
    }

    // ── bitreader round-trip tests ────────────────────────────────────────

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
