//! TIFF predictors (tag 317), forward and reverse.
//!
//! Two predictors are defined:
//!
//! * **Horizontal differencing** (value 2) — each sample is stored as the
//!   difference from the sample `SamplesPerPixel` positions to its left, using
//!   **whole-sample carry-propagating arithmetic in the file's byte order**.
//!   Doing it per byte silently drops carries and corrupts every 16/32/64-bit
//!   image; that is the classic bug this module exists to not have.
//! * **Floating point** (value 3, TIFF Technical Note 3) — each scanline is
//!   byte-plane transposed (most-significant plane first, regardless of the
//!   file's byte order) and a byte-wise horizontal delta with stride
//!   `SamplesPerPixel` is applied to the transposed stream (libtiff's
//!   `fpDiff`/`fpAcc`).
//!
//! Both run **before** the host-endian swap, because both are defined on the
//! bytes as they appear in the file.
//!
//! ```
//! use oxiarc_tiff::{Endian, apply_predictor_forward, apply_predictor_reverse};
//! use oxiarc_tiff::tags::Predictor;
//!
//! // Two 16-bit little-endian samples, 0x00FF then 0x0100: the delta is 1,
//! // which only a whole-sample subtraction produces.
//! let mut data = vec![0xFF, 0x00, 0x00, 0x01];
//! apply_predictor_forward(&mut data, Predictor::Horizontal, 2, 1, 2, Endian::Little)?;
//! assert_eq!(data, vec![0xFF, 0x00, 0x01, 0x00]);
//! apply_predictor_reverse(&mut data, Predictor::Horizontal, 2, 1, 2, Endian::Little)?;
//! assert_eq!(data, vec![0xFF, 0x00, 0x00, 0x01]);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use crate::byteorder::Endian;
use crate::error::{FormatError, Result, TiffError, UnsupportedError};
use crate::tags::{CompressionMethod, Predictor};

/// Reads a `bytes_per_sample`-wide unsigned sample in `endian` order.
fn read_sample(bytes: &[u8], bytes_per_sample: usize, endian: Endian) -> u64 {
    match bytes_per_sample {
        2 => endian.u16_at(bytes, 0).map_or(0, u64::from),
        4 => endian.u32_at(bytes, 0).map_or(0, u64::from),
        8 => endian.u64_at(bytes, 0).unwrap_or(0),
        _ => bytes.first().copied().map_or(0, u64::from),
    }
}

/// Writes a `bytes_per_sample`-wide unsigned sample in `endian` order.
fn write_sample(bytes: &mut [u8], bytes_per_sample: usize, endian: Endian, value: u64) {
    match bytes_per_sample {
        2 => copy_into(bytes, &endian.put_u16(value as u16)),
        4 => copy_into(bytes, &endian.put_u32(value as u32)),
        8 => copy_into(bytes, &endian.put_u64(value)),
        _ => {
            if let Some(b) = bytes.first_mut() {
                *b = value as u8;
            }
        }
    }
}

fn copy_into(dst: &mut [u8], src: &[u8]) {
    let take = dst.len().min(src.len());
    if let Some(d) = dst.get_mut(..take) {
        if let Some(s) = src.get(..take) {
            d.copy_from_slice(s);
        }
    }
}

/// Reconstructs original values from horizontal deltas (decode direction).
fn undifference_row(row: &mut [u8], bytes_per_sample: usize, stride: usize, endian: Endian) {
    if bytes_per_sample == 0 || stride == 0 {
        return;
    }
    if bytes_per_sample == 1 {
        accumulate_bytes(row, stride);
        return;
    }
    let sample_count = row.len() / bytes_per_sample;
    for j in stride..sample_count {
        let cur = j * bytes_per_sample;
        let prev = (j - stride) * bytes_per_sample;
        let Some(prev_slice) = row.get(prev..prev + bytes_per_sample) else {
            break;
        };
        let previous = read_sample(prev_slice, bytes_per_sample, endian);
        let Some(cur_slice) = row.get(cur..cur + bytes_per_sample) else {
            break;
        };
        let delta = read_sample(cur_slice, bytes_per_sample, endian);
        let Some(out) = row.get_mut(cur..cur + bytes_per_sample) else {
            break;
        };
        write_sample(out, bytes_per_sample, endian, previous.wrapping_add(delta));
    }
}

/// Encodes horizontal deltas from original values (encode direction).
fn difference_row(row: &mut [u8], bytes_per_sample: usize, stride: usize, endian: Endian) {
    if bytes_per_sample == 0 || stride == 0 {
        return;
    }
    if bytes_per_sample == 1 {
        differentiate_bytes(row, stride);
        return;
    }
    let sample_count = row.len() / bytes_per_sample;
    for j in (stride..sample_count).rev() {
        let cur = j * bytes_per_sample;
        let prev = (j - stride) * bytes_per_sample;
        let Some(cur_slice) = row.get(cur..cur + bytes_per_sample) else {
            continue;
        };
        let current = read_sample(cur_slice, bytes_per_sample, endian);
        let Some(prev_slice) = row.get(prev..prev + bytes_per_sample) else {
            continue;
        };
        let previous = read_sample(prev_slice, bytes_per_sample, endian);
        let Some(out) = row.get_mut(cur..cur + bytes_per_sample) else {
            continue;
        };
        write_sample(
            out,
            bytes_per_sample,
            endian,
            current.wrapping_sub(previous),
        );
    }
}

/// Undoes a byte-wise horizontal delta (running sum) with the given stride.
fn accumulate_bytes(row: &mut [u8], stride: usize) {
    if stride == 0 {
        return;
    }
    if stride == 1 {
        let Some((first, rest)) = row.split_first_mut() else {
            return;
        };
        let mut acc = *first;
        for byte in rest {
            acc = acc.wrapping_add(*byte);
            *byte = acc;
        }
        return;
    }
    for i in stride..row.len() {
        let previous = row.get(i - stride).copied().unwrap_or(0);
        if let Some(byte) = row.get_mut(i) {
            *byte = byte.wrapping_add(previous);
        }
    }
}

/// Applies a byte-wise horizontal delta with the given stride.
fn differentiate_bytes(row: &mut [u8], stride: usize) {
    if stride == 0 {
        return;
    }
    if stride == 1 {
        let Some(&first) = row.first() else {
            return;
        };
        let mut prev = first;
        for byte in row.iter_mut().skip(1) {
            let cur = *byte;
            *byte = cur.wrapping_sub(prev);
            prev = cur;
        }
        return;
    }
    for i in (stride..row.len()).rev() {
        let previous = row.get(i - stride).copied().unwrap_or(0);
        if let Some(byte) = row.get_mut(i) {
            *byte = byte.wrapping_sub(previous);
        }
    }
}

/// Byte-plane index that holds byte `byte` of every sample.
///
/// Plane 0 always holds the most-significant byte, regardless of the file's
/// byte order; only the reassembly into sample layout depends on the order.
const fn plane_of_byte(byte: usize, bytes_per_sample: usize, endian: Endian) -> usize {
    match endian {
        Endian::Big => byte,
        Endian::Little => bytes_per_sample - byte - 1,
    }
}

/// De-interleaves byte planes back into sample layout, width known at compile time.
fn undo_byte_planes<const N: usize>(
    row: &mut [u8],
    planes: &[u8],
    sample_count: usize,
    endian: Endian,
) {
    if planes.len() != N * sample_count || row.len() != planes.len() {
        return;
    }
    let ordered: [&[u8]; N] = core::array::from_fn(|byte| {
        let plane = plane_of_byte(byte, N, endian);
        planes
            .get(plane * sample_count..(plane + 1) * sample_count)
            .unwrap_or(&[])
    });
    for (i, chunk) in row.chunks_exact_mut(N).enumerate() {
        for (dst, plane) in chunk.iter_mut().zip(ordered) {
            *dst = plane.get(i).copied().unwrap_or(0);
        }
    }
}

/// Width-generic fallback for [`undo_byte_planes`].
fn undo_byte_planes_generic(
    row: &mut [u8],
    planes: &[u8],
    bytes_per_sample: usize,
    sample_count: usize,
    endian: Endian,
) {
    for sample in 0..sample_count {
        for byte in 0..bytes_per_sample {
            let plane = plane_of_byte(byte, bytes_per_sample, endian);
            let value = planes
                .get(plane * sample_count + sample)
                .copied()
                .unwrap_or(0);
            if let Some(slot) = row.get_mut(bytes_per_sample * sample + byte) {
                *slot = value;
            }
        }
    }
}

/// Interleaves sample layout into byte planes, width known at compile time.
fn apply_byte_planes<const N: usize>(
    row: &mut [u8],
    samples: &[u8],
    sample_count: usize,
    endian: Endian,
) {
    if samples.len() != N * sample_count || row.len() != samples.len() || sample_count == 0 {
        return;
    }
    for (sample, chunk) in samples.chunks_exact(N).enumerate() {
        for (byte, value) in chunk.iter().enumerate() {
            let plane = plane_of_byte(byte, N, endian);
            if let Some(slot) = row.get_mut(plane * sample_count + sample) {
                *slot = *value;
            }
        }
    }
}

/// Width-generic fallback for [`apply_byte_planes`].
fn apply_byte_planes_generic(
    row: &mut [u8],
    samples: &[u8],
    bytes_per_sample: usize,
    sample_count: usize,
    endian: Endian,
) {
    for sample in 0..sample_count {
        for byte in 0..bytes_per_sample {
            let plane = plane_of_byte(byte, bytes_per_sample, endian);
            let value = samples
                .get(bytes_per_sample * sample + byte)
                .copied()
                .unwrap_or(0);
            if let Some(slot) = row.get_mut(plane * sample_count + sample) {
                *slot = value;
            }
        }
    }
}

/// Validates a floating-point scanline and returns its sample count.
fn float_row_sample_count(row_len: usize, bytes_per_sample: usize) -> Result<Option<usize>> {
    if bytes_per_sample == 0 || row_len == 0 {
        return Ok(None);
    }
    if row_len % bytes_per_sample != 0 {
        return Err(TiffError::Format(FormatError::Codec {
            method: 0,
            message: format!(
                "floating-point predictor: scanline of {row_len} bytes is not a multiple of the \
                 {bytes_per_sample}-byte sample size"
            ),
        }));
    }
    Ok(Some(row_len / bytes_per_sample))
}

/// Reverses the floating-point predictor for one scanline.
fn undo_float_predictor_row(
    row: &mut [u8],
    bytes_per_sample: usize,
    stride: usize,
    endian: Endian,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    let Some(sample_count) = float_row_sample_count(row.len(), bytes_per_sample)? else {
        return Ok(());
    };
    accumulate_bytes(row, stride.max(1));
    scratch.clear();
    scratch.extend_from_slice(row);
    match bytes_per_sample {
        2 => undo_byte_planes::<2>(row, scratch, sample_count, endian),
        4 => undo_byte_planes::<4>(row, scratch, sample_count, endian),
        8 => undo_byte_planes::<8>(row, scratch, sample_count, endian),
        _ => undo_byte_planes_generic(row, scratch, bytes_per_sample, sample_count, endian),
    }
    Ok(())
}

/// Applies the floating-point predictor to one scanline.
fn apply_float_predictor_row(
    row: &mut [u8],
    bytes_per_sample: usize,
    stride: usize,
    endian: Endian,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    let Some(sample_count) = float_row_sample_count(row.len(), bytes_per_sample)? else {
        return Ok(());
    };
    scratch.clear();
    scratch.extend_from_slice(row);
    match bytes_per_sample {
        2 => apply_byte_planes::<2>(row, scratch, sample_count, endian),
        4 => apply_byte_planes::<4>(row, scratch, sample_count, endian),
        8 => apply_byte_planes::<8>(row, scratch, sample_count, endian),
        _ => apply_byte_planes_generic(row, scratch, bytes_per_sample, sample_count, endian),
    }
    differentiate_bytes(row, stride.max(1));
    Ok(())
}

/// Runs `op` over each scanline of `data`.
fn for_each_row<F>(
    data: &mut [u8],
    bytes_per_sample: usize,
    stride: usize,
    width: usize,
    mut op: F,
) -> Result<()>
where
    F: FnMut(&mut [u8]) -> Result<()>,
{
    let row_bytes = width
        .saturating_mul(stride)
        .saturating_mul(bytes_per_sample);
    if row_bytes == 0 {
        return Ok(());
    }
    let len = data.len();
    let mut start = 0usize;
    while start < len {
        let end = start.saturating_add(row_bytes).min(len);
        let Some(row) = data.get_mut(start..end) else {
            break;
        };
        op(row)?;
        start = end;
    }
    Ok(())
}

/// Rejects predictor/bit-depth and predictor/compression combinations that the
/// spec does not define.
///
/// Horizontal differencing is defined only for 8/16/32/64-bit samples, and no
/// predictor is meaningful with a codec that has its own spatial model (JPEG,
/// CCITT, WebP).
///
/// # Errors
/// [`UnsupportedError::PredictorForBitDepth`] or
/// [`UnsupportedError::PredictorForCompression`].
pub fn validate(
    predictor: Predictor,
    bits_per_sample: &[u16],
    compression: CompressionMethod,
) -> Result<()> {
    if predictor == Predictor::None {
        return Ok(());
    }
    if !compression.allows_predictor() {
        return Err(TiffError::Unsupported(
            UnsupportedError::PredictorForCompression {
                predictor: predictor.to_u16(),
                compression: compression.to_u16(),
            },
        ));
    }
    match predictor {
        Predictor::Horizontal => {
            for bits in bits_per_sample {
                if !matches!(bits, 8 | 16 | 32 | 64) {
                    return Err(TiffError::Unsupported(
                        UnsupportedError::PredictorForBitDepth {
                            predictor: predictor.to_u16(),
                            bits: *bits,
                        },
                    ));
                }
            }
            Ok(())
        }
        Predictor::FloatingPoint => {
            for bits in bits_per_sample {
                if !matches!(bits, 16 | 24 | 32 | 64) {
                    return Err(TiffError::Unsupported(
                        UnsupportedError::PredictorForBitDepth {
                            predictor: predictor.to_u16(),
                            bits: *bits,
                        },
                    ));
                }
            }
            Ok(())
        }
        other => Err(TiffError::Unsupported(
            UnsupportedError::PredictorForBitDepth {
                predictor: other.to_u16(),
                bits: bits_per_sample.first().copied().unwrap_or(0),
            },
        )),
    }
}

/// Undoes a predictor over a decoded chunk, in place, in the file's byte order.
///
/// `stride` is `SamplesPerPixel` for chunky data and **1** for planar data;
/// `width` is the chunk's coded width in pixels.
///
/// # Errors
/// [`FormatError::Codec`] when a floating-point scanline length is not a whole
/// multiple of the sample size, and
/// [`UnsupportedError::PredictorForBitDepth`] for an unknown predictor value.
pub fn apply_predictor_reverse(
    data: &mut [u8],
    predictor: Predictor,
    bytes_per_sample: usize,
    stride: usize,
    width: usize,
    endian: Endian,
) -> Result<()> {
    match predictor {
        Predictor::None => Ok(()),
        Predictor::Horizontal => for_each_row(data, bytes_per_sample, stride, width, |row| {
            undifference_row(row, bytes_per_sample, stride, endian);
            Ok(())
        }),
        Predictor::FloatingPoint => {
            let mut scratch = Vec::new();
            for_each_row(data, bytes_per_sample, stride, width, |row| {
                undo_float_predictor_row(row, bytes_per_sample, stride, endian, &mut scratch)
            })
        }
        other => Err(TiffError::Unsupported(
            UnsupportedError::PredictorForBitDepth {
                predictor: other.to_u16(),
                bits: (bytes_per_sample * 8) as u16,
            },
        )),
    }
}

/// Applies a predictor to a chunk before compression, in the file's byte order.
///
/// The exact inverse of [`apply_predictor_reverse`].
///
/// # Errors
/// The same set as [`apply_predictor_reverse`].
pub fn apply_predictor_forward(
    data: &mut [u8],
    predictor: Predictor,
    bytes_per_sample: usize,
    stride: usize,
    width: usize,
    endian: Endian,
) -> Result<()> {
    match predictor {
        Predictor::None => Ok(()),
        Predictor::Horizontal => for_each_row(data, bytes_per_sample, stride, width, |row| {
            difference_row(row, bytes_per_sample, stride, endian);
            Ok(())
        }),
        Predictor::FloatingPoint => {
            let mut scratch = Vec::new();
            for_each_row(data, bytes_per_sample, stride, width, |row| {
                apply_float_predictor_row(row, bytes_per_sample, stride, endian, &mut scratch)
            })
        }
        other => Err(TiffError::Unsupported(
            UnsupportedError::PredictorForBitDepth {
                predictor: other.to_u16(),
                bits: (bytes_per_sample * 8) as u16,
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(
        original: &[u8],
        predictor: Predictor,
        bytes_per_sample: usize,
        stride: usize,
        width: usize,
        endian: Endian,
    ) {
        let mut data = original.to_vec();
        apply_predictor_forward(
            &mut data,
            predictor,
            bytes_per_sample,
            stride,
            width,
            endian,
        )
        .expect("forward");
        apply_predictor_reverse(
            &mut data,
            predictor,
            bytes_per_sample,
            stride,
            width,
            endian,
        )
        .expect("reverse");
        assert_eq!(data, original, "{predictor} round trip");
    }

    #[test]
    fn predictor_none_is_a_no_op() {
        let mut data = vec![1u8, 2, 3, 4];
        apply_predictor_reverse(&mut data, Predictor::None, 1, 1, 4, Endian::Little)
            .expect("reverse");
        assert_eq!(data, vec![1, 2, 3, 4]);
        apply_predictor_forward(&mut data, Predictor::None, 1, 1, 4, Endian::Little)
            .expect("forward");
        assert_eq!(data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn horizontal_8bit_matches_the_reference_definition() {
        // Original 10, 12, 11, 20 -> deltas 10, 2, -1, 9.
        let mut data = vec![10u8, 12, 11, 20];
        apply_predictor_forward(&mut data, Predictor::Horizontal, 1, 1, 4, Endian::Little)
            .expect("forward");
        assert_eq!(data, vec![10, 2, 255, 9]);
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 1, 1, 4, Endian::Little)
            .expect("reverse");
        assert_eq!(data, vec![10, 12, 11, 20]);
    }

    #[test]
    fn horizontal_16bit_propagates_carries_across_the_whole_sample() {
        // 0x00FF then 0x0100 little-endian: the delta must be 1, not (1, 1).
        let mut data = vec![0xFFu8, 0x00, 0x00, 0x01];
        apply_predictor_forward(&mut data, Predictor::Horizontal, 2, 1, 2, Endian::Little)
            .expect("forward");
        assert_eq!(data, vec![0xFF, 0x00, 0x01, 0x00]);
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 2, 1, 2, Endian::Little)
            .expect("reverse");
        assert_eq!(data, vec![0xFF, 0x00, 0x00, 0x01]);

        // Same data big-endian.
        let mut data = vec![0x00u8, 0xFF, 0x01, 0x00];
        apply_predictor_forward(&mut data, Predictor::Horizontal, 2, 1, 2, Endian::Big)
            .expect("forward");
        assert_eq!(data, vec![0x00, 0xFF, 0x00, 0x01]);
    }

    #[test]
    fn horizontal_stride_follows_samples_per_pixel() {
        // Three RGB pixels: differencing must be per channel.
        let original = vec![10u8, 20, 30, 12, 22, 32, 11, 21, 31];
        let mut data = original.clone();
        apply_predictor_forward(&mut data, Predictor::Horizontal, 1, 3, 3, Endian::Little)
            .expect("forward");
        assert_eq!(data, vec![10, 20, 30, 2, 2, 2, 255, 255, 255]);
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 1, 3, 3, Endian::Little)
            .expect("reverse");
        assert_eq!(data, original);
    }

    #[test]
    fn planar_data_uses_a_stride_of_one() {
        // A planar chunk carries one channel, so stride is 1 even though the
        // image has three samples per pixel.
        let original = vec![10u8, 12, 11, 20];
        round_trip(&original, Predictor::Horizontal, 1, 1, 4, Endian::Little);
    }

    #[test]
    fn horizontal_round_trips_at_every_supported_width() {
        for (bps, width) in [(1usize, 8usize), (2, 4), (4, 2), (8, 2)] {
            let original: Vec<u8> = (0u8..(bps * width) as u8)
                .map(|b| b.wrapping_mul(37))
                .collect();
            for endian in [Endian::Little, Endian::Big] {
                round_trip(&original, Predictor::Horizontal, bps, 1, width, endian);
            }
        }
    }

    #[test]
    fn horizontal_operates_per_scanline_not_across_the_chunk() {
        // Two rows of two 8-bit samples; the second row must not reference the first.
        let original = vec![10u8, 12, 200, 210];
        let mut data = original.clone();
        apply_predictor_forward(&mut data, Predictor::Horizontal, 1, 1, 2, Endian::Little)
            .expect("forward");
        assert_eq!(data, vec![10, 2, 200, 10]);
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 1, 1, 2, Endian::Little)
            .expect("reverse");
        assert_eq!(data, original);
    }

    #[test]
    fn float_predictor_round_trips_in_both_byte_orders() {
        for endian in [Endian::Little, Endian::Big] {
            for bps in [2usize, 4, 8] {
                let original: Vec<u8> = (0..(bps * 6) as u8).map(|b| b.wrapping_mul(29)).collect();
                round_trip(&original, Predictor::FloatingPoint, bps, 1, 6, endian);
                round_trip(&original, Predictor::FloatingPoint, bps, 3, 2, endian);
            }
        }
    }

    #[test]
    fn float_predictor_handles_a_generic_sample_width() {
        // 3-byte samples exercise the non-const-generic fallback.
        let original: Vec<u8> = (0u8..12).collect();
        round_trip(&original, Predictor::FloatingPoint, 3, 1, 4, Endian::Big);
        round_trip(&original, Predictor::FloatingPoint, 3, 1, 4, Endian::Little);
    }

    #[test]
    fn float_predictor_puts_the_msb_plane_first() {
        // One row of two big-endian f32 samples.
        let original = vec![0x01u8, 0x02, 0x03, 0x04, 0x11, 0x12, 0x13, 0x14];
        let mut data = original.clone();
        apply_predictor_forward(&mut data, Predictor::FloatingPoint, 4, 1, 2, Endian::Big)
            .expect("forward");
        // Planes: [01 11][02 12][03 13][04 14] then a byte-wise delta of stride 1.
        assert_eq!(data, vec![0x01, 0x10, 0xF1, 0x10, 0xF1, 0x10, 0xF1, 0x10]);
        apply_predictor_reverse(&mut data, Predictor::FloatingPoint, 4, 1, 2, Endian::Big)
            .expect("reverse");
        assert_eq!(data, original);
    }

    #[test]
    fn float_predictor_rejects_a_ragged_scanline() {
        let mut data = vec![0u8; 7];
        let err =
            apply_predictor_reverse(&mut data, Predictor::FloatingPoint, 4, 1, 2, Endian::Little)
                .expect_err("7 bytes is not two f32 samples");
        assert!(matches!(err, TiffError::Format(FormatError::Codec { .. })));
    }

    #[test]
    fn validation_rejects_undefined_combinations() {
        assert!(validate(Predictor::None, &[7], CompressionMethod::Jpeg).is_ok());
        assert!(validate(Predictor::Horizontal, &[8], CompressionMethod::Lzw).is_ok());
        assert!(validate(Predictor::Horizontal, &[16, 16], CompressionMethod::Deflate).is_ok());
        assert!(validate(Predictor::FloatingPoint, &[32], CompressionMethod::Deflate).is_ok());

        let err = validate(Predictor::Horizontal, &[12], CompressionMethod::Lzw)
            .expect_err("12-bit horizontal");
        assert!(matches!(
            err,
            TiffError::Unsupported(UnsupportedError::PredictorForBitDepth { .. })
        ));
        let err = validate(Predictor::Horizontal, &[8], CompressionMethod::Jpeg)
            .expect_err("predictor with JPEG");
        assert!(matches!(
            err,
            TiffError::Unsupported(UnsupportedError::PredictorForCompression { .. })
        ));
        let err = validate(Predictor::FloatingPoint, &[8], CompressionMethod::Deflate)
            .expect_err("8-bit float predictor");
        assert!(matches!(
            err,
            TiffError::Unsupported(UnsupportedError::PredictorForBitDepth { .. })
        ));
        assert!(validate(Predictor::Unknown(9), &[8], CompressionMethod::Lzw).is_err());
    }

    #[test]
    fn unknown_predictors_error_rather_than_corrupting() {
        let mut data = vec![1u8, 2, 3, 4];
        assert!(
            apply_predictor_reverse(&mut data, Predictor::Unknown(9), 1, 1, 4, Endian::Little)
                .is_err()
        );
        assert!(
            apply_predictor_forward(&mut data, Predictor::Unknown(9), 1, 1, 4, Endian::Little)
                .is_err()
        );
    }

    #[test]
    fn zero_geometry_is_a_no_op_not_a_panic() {
        let mut data = vec![1u8, 2, 3];
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 1, 1, 0, Endian::Little)
            .expect("zero width");
        assert_eq!(data, vec![1, 2, 3]);
        apply_predictor_reverse(&mut data, Predictor::Horizontal, 0, 1, 3, Endian::Little)
            .expect("zero sample width");
        let mut empty: Vec<u8> = Vec::new();
        apply_predictor_reverse(&mut empty, Predictor::FloatingPoint, 4, 1, 2, Endian::Big)
            .expect("empty chunk");
    }
}
