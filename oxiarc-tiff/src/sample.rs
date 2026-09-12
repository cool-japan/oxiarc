//! Sample types, typed sample buffers and bit-level (un)packing.
//!
//! The decode pipeline works entirely in native-endian `Vec<u8>`; [`Samples`]
//! is materialised once, at the API boundary. That is what lets the crate be
//! `#![forbid(unsafe_code)]` without a `bytecast` module: the one extra pass is
//! memory-bandwidth bound and small next to decoding.
//!
//! ```
//! use oxiarc_tiff::sample::{f16_bits_to_f32, f32_to_f16_bits, SampleType, Samples};
//! use oxiarc_tiff::tags::SampleFormat;
//!
//! assert_eq!(SampleType::resolve(12, SampleFormat::Uint).expect("12-bit"), SampleType::U16);
//! assert_eq!(SampleType::F32.byte_width(), 4);
//! assert!((f16_bits_to_f32(0x3C00) - 1.0).abs() < f32::EPSILON);
//! assert_eq!(f32_to_f16_bits(-2.0), 0xC000);
//!
//! let samples = Samples::U16(vec![1, 2, 3]);
//! assert_eq!(samples.len(), 3);
//! assert_eq!(samples.as_u16(), Some(&[1u16, 2, 3][..]));
//! ```

use crate::byteorder::Endian;
use crate::error::{Result, TiffError, UnsupportedError, UsageError};
use crate::limits::Limits;
use crate::tags::{FillOrder, SampleFormat};

/// The native slot a decoded sample occupies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleType {
    /// 8-bit unsigned.
    U8,
    /// 16-bit unsigned.
    U16,
    /// 32-bit unsigned.
    U32,
    /// 64-bit unsigned.
    U64,
    /// 8-bit signed.
    I8,
    /// 16-bit signed.
    I16,
    /// 32-bit signed.
    I32,
    /// 64-bit signed.
    I64,
    /// IEEE-754 binary16, carried as raw bits.
    F16,
    /// IEEE-754 binary32.
    F32,
    /// IEEE-754 binary64.
    F64,
}

impl SampleType {
    /// Bytes one decoded sample occupies.
    #[must_use]
    pub const fn byte_width(self) -> usize {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 | Self::F16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    /// `true` for the floating-point slots.
    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F16 | Self::F32 | Self::F64)
    }

    /// `true` for the signed integer slots.
    #[must_use]
    pub const fn is_signed_int(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    /// Chooses the slot for a `(BitsPerSample, SampleFormat)` pair.
    ///
    /// `Void` and unknown formats are treated as unsigned integers, which is
    /// what libtiff does; the caller records a warning.
    ///
    /// # Errors
    /// [`UnsupportedError::BitsPerSample`] for a depth of 0 or above 64, and
    /// [`UnsupportedError::SampleFormat`] for the complex formats.
    pub fn resolve(bits: u16, format: SampleFormat) -> Result<Self> {
        if bits == 0 || bits > 64 {
            return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
                vec![bits],
            )));
        }
        match format {
            SampleFormat::IeeeFp => match bits {
                16 => Ok(Self::F16),
                24 | 32 => Ok(Self::F32),
                64 => Ok(Self::F64),
                other => Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
                    vec![other],
                ))),
            },
            SampleFormat::Int => Ok(match bits {
                1..=8 => Self::I8,
                9..=16 => Self::I16,
                17..=32 => Self::I32,
                _ => Self::I64,
            }),
            SampleFormat::ComplexInt | SampleFormat::ComplexIeeeFp => Err(TiffError::Unsupported(
                UnsupportedError::SampleFormat(format.to_u16()),
            )),
            // Uint, Void and unknown formats.
            _ => Ok(match bits {
                1..=8 => Self::U8,
                9..=16 => Self::U16,
                17..=32 => Self::U32,
                _ => Self::U64,
            }),
        }
    }
}

/// A decoded image or chunk, in one of the eleven native slot types.
///
/// `F16` carries the raw IEEE-754 binary16 bit patterns rather than a
/// `half::f16`, so the native API needs no external dependency.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Samples {
    /// 8-bit unsigned samples.
    U8(Vec<u8>),
    /// 16-bit unsigned samples.
    U16(Vec<u16>),
    /// 32-bit unsigned samples.
    U32(Vec<u32>),
    /// 64-bit unsigned samples.
    U64(Vec<u64>),
    /// 8-bit signed samples.
    I8(Vec<i8>),
    /// 16-bit signed samples.
    I16(Vec<i16>),
    /// 32-bit signed samples.
    I32(Vec<i32>),
    /// 64-bit signed samples.
    I64(Vec<i64>),
    /// Raw IEEE-754 binary16 bit patterns.
    F16(Vec<u16>),
    /// 32-bit floating-point samples.
    F32(Vec<f32>),
    /// 64-bit floating-point samples.
    F64(Vec<f64>),
}

macro_rules! samples_accessor {
    ($name:ident, $variant:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        #[must_use]
        pub fn $name(&self) -> Option<&[$ty]> {
            match self {
                Self::$variant(v) => Some(v),
                _ => None,
            }
        }
    };
}

impl Samples {
    /// The slot type of this buffer.
    #[must_use]
    pub const fn sample_type(&self) -> SampleType {
        match self {
            Self::U8(_) => SampleType::U8,
            Self::U16(_) => SampleType::U16,
            Self::U32(_) => SampleType::U32,
            Self::U64(_) => SampleType::U64,
            Self::I8(_) => SampleType::I8,
            Self::I16(_) => SampleType::I16,
            Self::I32(_) => SampleType::I32,
            Self::I64(_) => SampleType::I64,
            Self::F16(_) => SampleType::F16,
            Self::F32(_) => SampleType::F32,
            Self::F64(_) => SampleType::F64,
        }
    }

    /// Number of samples held.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::U16(v) | Self::F16(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    /// `true` when no samples are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Size in bytes of the equivalent native-endian byte buffer.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.len() * self.sample_type().byte_width()
    }

    samples_accessor!(as_u8, U8, u8, "The samples, if this is a `U8` buffer.");
    samples_accessor!(as_u16, U16, u16, "The samples, if this is a `U16` buffer.");
    samples_accessor!(as_u32, U32, u32, "The samples, if this is a `U32` buffer.");
    samples_accessor!(as_u64, U64, u64, "The samples, if this is a `U64` buffer.");
    samples_accessor!(as_i8, I8, i8, "The samples, if this is an `I8` buffer.");
    samples_accessor!(as_i16, I16, i16, "The samples, if this is an `I16` buffer.");
    samples_accessor!(as_i32, I32, i32, "The samples, if this is an `I32` buffer.");
    samples_accessor!(as_i64, I64, i64, "The samples, if this is an `I64` buffer.");
    samples_accessor!(
        as_f16_bits,
        F16,
        u16,
        "The raw binary16 bit patterns, if this is an `F16` buffer."
    );
    samples_accessor!(as_f32, F32, f32, "The samples, if this is an `F32` buffer.");
    samples_accessor!(as_f64, F64, f64, "The samples, if this is an `F64` buffer.");

    /// Allocates `len` zeroed samples, charged against `limits`.
    ///
    /// # Errors
    /// [`crate::LimitError::ImageSize`] when the buffer would be too large.
    pub fn new_zeroed(ty: SampleType, len: usize, limits: &Limits) -> Result<Self> {
        let bytes = (len as u64)
            .checked_mul(ty.byte_width() as u64)
            .ok_or(TiffError::IntOverflow)?;
        limits.check_image_bytes(bytes)?;
        Ok(match ty {
            SampleType::U8 => Self::U8(vec![0; len]),
            SampleType::U16 => Self::U16(vec![0; len]),
            SampleType::U32 => Self::U32(vec![0; len]),
            SampleType::U64 => Self::U64(vec![0; len]),
            SampleType::I8 => Self::I8(vec![0; len]),
            SampleType::I16 => Self::I16(vec![0; len]),
            SampleType::I32 => Self::I32(vec![0; len]),
            SampleType::I64 => Self::I64(vec![0; len]),
            SampleType::F16 => Self::F16(vec![0; len]),
            SampleType::F32 => Self::F32(vec![0.0; len]),
            SampleType::F64 => Self::F64(vec![0.0; len]),
        })
    }

    /// Reinterprets a native-endian byte buffer as typed samples.
    ///
    /// This is the single conversion pass the crate pays instead of a
    /// `bytecast` module. `bytes.len()` must be a whole multiple of the slot
    /// width.
    ///
    /// # Errors
    /// [`UsageError::BufferTooSmall`] when the length is not a multiple of the
    /// slot width.
    pub fn from_native_bytes(ty: SampleType, bytes: &[u8]) -> Result<Self> {
        let width = ty.byte_width();
        if bytes.len() % width != 0 {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: bytes.len().next_multiple_of(width),
                got: bytes.len(),
            }));
        }

        macro_rules! decode {
            ($variant:ident, $ty:ty, $n:literal) => {
                Self::$variant(
                    bytes
                        .chunks_exact($n)
                        .map(|c| {
                            let mut buf = [0u8; $n];
                            buf.copy_from_slice(c);
                            <$ty>::from_ne_bytes(buf)
                        })
                        .collect(),
                )
            };
        }

        Ok(match ty {
            SampleType::U8 => Self::U8(bytes.to_vec()),
            SampleType::I8 => Self::I8(bytes.iter().map(|b| *b as i8).collect()),
            SampleType::U16 => decode!(U16, u16, 2),
            SampleType::F16 => decode!(F16, u16, 2),
            SampleType::I16 => decode!(I16, i16, 2),
            SampleType::U32 => decode!(U32, u32, 4),
            SampleType::I32 => decode!(I32, i32, 4),
            SampleType::F32 => decode!(F32, f32, 4),
            SampleType::U64 => decode!(U64, u64, 8),
            SampleType::I64 => decode!(I64, i64, 8),
            SampleType::F64 => decode!(F64, f64, 8),
        })
    }

    /// Writes the samples into `dst` as native-endian bytes.
    ///
    /// # Errors
    /// [`UsageError::BufferTooSmall`] when `dst` is shorter than
    /// [`Self::byte_len`].
    pub fn write_native_bytes(&self, dst: &mut [u8]) -> Result<()> {
        let need = self.byte_len();
        if dst.len() < need {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: need,
                got: dst.len(),
            }));
        }

        macro_rules! encode {
            ($values:expr, $n:literal) => {{
                for (slot, value) in dst.chunks_exact_mut($n).zip($values.iter()) {
                    slot.copy_from_slice(&value.to_ne_bytes());
                }
            }};
        }

        match self {
            Self::U8(v) => {
                if let Some(head) = dst.get_mut(..v.len()) {
                    head.copy_from_slice(v);
                }
            }
            Self::I8(v) => {
                for (slot, value) in dst.iter_mut().zip(v.iter()) {
                    *slot = *value as u8;
                }
            }
            Self::U16(v) | Self::F16(v) => encode!(v, 2),
            Self::I16(v) => encode!(v, 2),
            Self::U32(v) => encode!(v, 4),
            Self::I32(v) => encode!(v, 4),
            Self::F32(v) => encode!(v, 4),
            Self::U64(v) => encode!(v, 8),
            Self::I64(v) => encode!(v, 8),
            Self::F64(v) => encode!(v, 8),
        }
        Ok(())
    }

    /// The samples as a fresh native-endian byte buffer.
    #[must_use]
    pub fn to_native_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.byte_len()];
        // `out` is exactly `byte_len()` long, so this cannot fail.
        let _ = self.write_native_bytes(&mut out);
        out
    }

    /// Expands a binary16 buffer to `f32`.
    #[must_use]
    pub fn f16_to_f32(&self) -> Option<Vec<f32>> {
        match self {
            Self::F16(v) => Some(v.iter().map(|b| f16_bits_to_f32(*b)).collect()),
            _ => None,
        }
    }
}

/// Expands an IEEE-754 binary16 bit pattern to `f32`, exactly.
#[must_use]
pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exp = (bits >> 10) & 0x1F;
    let mant = u32::from(bits & 0x03FF);
    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign);
        }
        // Subnormal: shift the mantissa left until bit 10 is set.
        let mut m = mant;
        let mut shifts = 0u32;
        while m & 0x0400 == 0 {
            m <<= 1;
            shifts += 1;
        }
        let exp32 = 113u32.saturating_sub(shifts);
        return f32::from_bits(sign | (exp32 << 23) | ((m & 0x03FF) << 13));
    }
    if exp == 0x1F {
        return f32::from_bits(sign | (0xFFu32 << 23) | (mant << 13));
    }
    f32::from_bits(sign | ((u32::from(exp) + 112) << 23) | (mant << 13))
}

/// Rounds an `f32` to the nearest IEEE-754 binary16 bit pattern (ties to even).
#[must_use]
pub fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x007F_FFFF;

    if exp == 0xFF {
        // Infinity or NaN: keep NaN payload non-zero.
        let payload = (mant >> 13) as u16;
        let payload = if mant != 0 && payload == 0 {
            1
        } else {
            payload
        };
        return sign | 0x7C00 | payload;
    }

    let half_exp = exp - 127 + 15;
    if half_exp >= 0x1F {
        return sign | 0x7C00;
    }
    if half_exp <= 0 {
        if half_exp < -10 {
            return sign;
        }
        let implicit = mant | 0x0080_0000;
        let shift = (14 - half_exp) as u32;
        let mut half = (implicit >> shift) as u16;
        let round_bit = 1u32 << (shift - 1);
        let sticky = implicit & (round_bit - 1);
        if implicit & round_bit != 0 && (sticky != 0 || half & 1 != 0) {
            half = half.wrapping_add(1);
        }
        return sign | half;
    }
    let mut half = ((half_exp as u16) << 10) | ((mant >> 13) as u16);
    if mant & 0x1000 != 0 && (mant & 0x0FFF != 0 || half & 1 != 0) {
        half = half.wrapping_add(1);
    }
    sign | half
}

/// `true` when a uniform bit depth can be copied byte-wise instead of unpacked.
#[must_use]
pub const fn is_byte_aligned_depth(bits: u16) -> bool {
    matches!(bits, 8 | 16 | 32 | 64)
}

/// Bits needed for one packed row of `count` samples of the given widths.
///
/// The widths cycle across the row (so `[5, 6, 5]` describes an RGB565 pixel).
#[must_use]
pub fn packed_row_bits(bits_per_sample: &[u16], count: usize) -> u64 {
    if bits_per_sample.is_empty() {
        return 0;
    }
    if let Some(first) = bits_per_sample.first() {
        if bits_per_sample.iter().all(|b| b == first) {
            return u64::from(*first) * count as u64;
        }
    }
    let mut total = 0u64;
    for i in 0..count {
        let idx = i % bits_per_sample.len();
        total += u64::from(bits_per_sample.get(idx).copied().unwrap_or(0));
    }
    total
}

/// Bytes needed for one packed row, including the row's byte padding.
#[must_use]
pub fn packed_row_bytes(bits_per_sample: &[u16], count: usize) -> u64 {
    packed_row_bits(bits_per_sample, count).div_ceil(8)
}

/// Reverses the bit order of every byte in `buf` (`FillOrder` 2).
pub fn reverse_bits_in_place(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = byte.reverse_bits();
    }
}

/// Unpacks one packed row into native-endian sample slots.
///
/// * `src` is one row of packed data, already in `FillOrder::Msb2Lsb` order.
/// * `bits_per_sample` cycles across the row.
/// * `dst` receives `count` slots of `slot_width` bytes, native-endian.
///
/// Bit-packed data is always read most-significant-bit first, as libtiff does
/// for every depth that is not 8/16/32/64. Byte-aligned depths keep the file's
/// byte order and are handled by the caller's endian swap instead.
///
/// # Errors
/// [`UsageError::BufferTooSmall`] when `dst` cannot hold `count` slots, and
/// [`UnsupportedError::BitsPerSample`] for a zero or over-64 width.
pub fn unpack_row(
    src: &[u8],
    bits_per_sample: &[u16],
    count: usize,
    slot: SampleType,
    dst: &mut [u8],
) -> Result<()> {
    let width = slot.byte_width();
    let need = count.checked_mul(width).ok_or(TiffError::IntOverflow)?;
    if dst.len() < need {
        return Err(TiffError::Usage(UsageError::BufferTooSmall {
            needed: need,
            got: dst.len(),
        }));
    }
    if bits_per_sample.is_empty() {
        return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
            Vec::new(),
        )));
    }
    for bits in bits_per_sample {
        if *bits == 0 || *bits > 64 {
            return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
                bits_per_sample.to_vec(),
            )));
        }
    }

    let mut bit_pos = 0u64;
    let total_bits = (src.len() as u64) * 8;
    for i in 0..count {
        let bits = u64::from(
            bits_per_sample
                .get(i % bits_per_sample.len())
                .copied()
                .unwrap_or(1),
        );
        let raw = if bit_pos + bits <= total_bits {
            read_bits_msb(src, bit_pos, bits)
        } else {
            0
        };
        bit_pos += bits;
        let Some(out) = dst.get_mut(i * width..i * width + width) else {
            break;
        };
        store_sample(out, raw, bits as u16, slot);
    }
    Ok(())
}

/// Packs native-endian sample slots into one bit-packed row.
///
/// The exact inverse of [`unpack_row`]. `dst` must be at least
/// `packed_row_bytes(bits_per_sample, count)` long and is zeroed first.
///
/// # Errors
/// [`UsageError::BufferTooSmall`] when `dst` or `src` is too short.
pub fn pack_row(
    src: &[u8],
    bits_per_sample: &[u16],
    count: usize,
    slot: SampleType,
    dst: &mut [u8],
) -> Result<()> {
    let width = slot.byte_width();
    let need_src = count.checked_mul(width).ok_or(TiffError::IntOverflow)?;
    if src.len() < need_src {
        return Err(TiffError::Usage(UsageError::BufferTooSmall {
            needed: need_src,
            got: src.len(),
        }));
    }
    let need_dst = packed_row_bytes(bits_per_sample, count) as usize;
    if dst.len() < need_dst {
        return Err(TiffError::Usage(UsageError::BufferTooSmall {
            needed: need_dst,
            got: dst.len(),
        }));
    }
    if bits_per_sample.is_empty() {
        return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
            Vec::new(),
        )));
    }
    for byte in dst.iter_mut().take(need_dst) {
        *byte = 0;
    }

    let mut bit_pos = 0u64;
    for i in 0..count {
        let bits = u64::from(
            bits_per_sample
                .get(i % bits_per_sample.len())
                .copied()
                .unwrap_or(1),
        );
        let Some(input) = src.get(i * width..i * width + width) else {
            break;
        };
        let raw = load_sample(input, bits as u16, slot);
        write_bits_msb(dst, bit_pos, bits, raw);
        bit_pos += bits;
    }
    Ok(())
}

/// Reads `bits` bits most-significant-first starting at bit `pos`.
fn read_bits_msb(src: &[u8], pos: u64, bits: u64) -> u64 {
    let mut value = 0u64;
    for i in 0..bits {
        let bit_index = pos + i;
        let byte = src.get((bit_index / 8) as usize).copied().unwrap_or(0);
        let shift = 7 - (bit_index % 8) as u32;
        value = (value << 1) | u64::from((byte >> shift) & 1);
    }
    value
}

/// Writes `bits` bits of `value` most-significant-first starting at bit `pos`.
fn write_bits_msb(dst: &mut [u8], pos: u64, bits: u64, value: u64) {
    for i in 0..bits {
        let bit = (value >> (bits - 1 - i)) & 1;
        let bit_index = pos + i;
        let Some(byte) = dst.get_mut((bit_index / 8) as usize) else {
            return;
        };
        let shift = 7 - (bit_index % 8) as u32;
        if bit == 1 {
            *byte |= 1 << shift;
        } else {
            *byte &= !(1u8 << shift);
        }
    }
}

/// Places one raw packed value into a native-endian slot.
fn store_sample(out: &mut [u8], raw: u64, bits: u16, slot: SampleType) {
    match slot {
        SampleType::U8 => {
            if let Some(b) = out.first_mut() {
                *b = raw as u8;
            }
        }
        SampleType::I8 => {
            if let Some(b) = out.first_mut() {
                *b = sign_extend(raw, bits) as u8;
            }
        }
        SampleType::U16 => copy_ne(out, &(raw as u16).to_ne_bytes()),
        SampleType::I16 => copy_ne(out, &(sign_extend(raw, bits) as i16).to_ne_bytes()),
        SampleType::U32 => copy_ne(out, &(raw as u32).to_ne_bytes()),
        SampleType::I32 => copy_ne(out, &(sign_extend(raw, bits) as i32).to_ne_bytes()),
        SampleType::U64 => copy_ne(out, &raw.to_ne_bytes()),
        SampleType::I64 => copy_ne(out, &sign_extend(raw, bits).to_ne_bytes()),
        SampleType::F16 => copy_ne(out, &(raw as u16).to_ne_bytes()),
        SampleType::F32 => {
            let value = if bits == 24 {
                // A 24-bit float is a binary32 with the low eight mantissa
                // bits truncated; re-attach them as zeros.
                f32::from_bits((raw as u32) << 8)
            } else {
                f32::from_bits(raw as u32)
            };
            copy_ne(out, &value.to_ne_bytes());
        }
        SampleType::F64 => copy_ne(out, &f64::from_bits(raw).to_ne_bytes()),
    }
}

/// Extracts one raw packed value out of a native-endian slot.
fn load_sample(input: &[u8], bits: u16, slot: SampleType) -> u64 {
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let raw = match slot {
        SampleType::U8 | SampleType::I8 => u64::from(input.first().copied().unwrap_or(0)),
        SampleType::U16 | SampleType::I16 | SampleType::F16 => u64::from(read_ne_u16(input)),
        SampleType::U32 | SampleType::I32 => u64::from(read_ne_u32(input)),
        SampleType::F32 => {
            let value = f32::from_bits(read_ne_u32(input));
            if bits == 24 {
                u64::from(value.to_bits() >> 8)
            } else {
                u64::from(value.to_bits())
            }
        }
        SampleType::U64 | SampleType::I64 => read_ne_u64(input),
        SampleType::F64 => f64::from_bits(read_ne_u64(input)).to_bits(),
    };
    raw & mask
}

fn copy_ne(out: &mut [u8], bytes: &[u8]) {
    let take = out.len().min(bytes.len());
    if let Some(dst) = out.get_mut(..take) {
        if let Some(src) = bytes.get(..take) {
            dst.copy_from_slice(src);
        }
    }
}

fn read_ne_u16(input: &[u8]) -> u16 {
    let mut buf = [0u8; 2];
    let take = input.len().min(2);
    if let Some(dst) = buf.get_mut(..take) {
        if let Some(src) = input.get(..take) {
            dst.copy_from_slice(src);
        }
    }
    u16::from_ne_bytes(buf)
}

fn read_ne_u32(input: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    let take = input.len().min(4);
    if let Some(dst) = buf.get_mut(..take) {
        if let Some(src) = input.get(..take) {
            dst.copy_from_slice(src);
        }
    }
    u32::from_ne_bytes(buf)
}

fn read_ne_u64(input: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let take = input.len().min(8);
    if let Some(dst) = buf.get_mut(..take) {
        if let Some(src) = input.get(..take) {
            dst.copy_from_slice(src);
        }
    }
    u64::from_ne_bytes(buf)
}

/// Sign-extends a `bits`-wide two's-complement value to `i64`.
fn sign_extend(raw: u64, bits: u16) -> i64 {
    if bits == 0 || bits >= 64 {
        return raw as i64;
    }
    let shift = 64 - u32::from(bits);
    ((raw << shift) as i64) >> shift
}

/// Applies `FillOrder` to a chunk that is still **compressed**, in place.
///
/// Returns `true` when the buffer was modified.
///
/// # Where in the pipeline this belongs
///
/// libtiff reverses the bits of every byte of the *raw* strip or tile — before
/// the codec runs on read (`TIFFFillStrip`) and after it runs on write
/// (`TIFFFlushData1`) — whenever `FillOrder` is 2, and it does so for **every**
/// bit depth, not only for sub-byte ones.
///
/// This was verified against libtiff 4.7.1: `tiffcp -c packbits -f lsb2msb`
/// produces a strip whose PackBits control bytes are themselves reversed
/// (decoding it without reversing first yields the wrong *length*, not merely
/// the wrong bits), and `tiffcp -c none -f lsb2msb` on an 8-bit image reverses
/// every sample byte. Reversing after decompression, or skipping the reversal
/// for byte-aligned depths, therefore produces files no other TIFF reader
/// agrees with.
///
/// The CCITT codecs are the one exception: libtiff sets `TIFF_NOBITREV` for
/// them and the fax decoder consumes the tag itself, so the caller must skip
/// this step for them — see [`crate::compression::handles_fill_order`].
///
/// ```
/// use oxiarc_tiff::sample::apply_fill_order;
/// use oxiarc_tiff::FillOrder;
///
/// let mut buf = [0b1010_0000u8];
/// assert!(apply_fill_order(&mut buf, FillOrder::Lsb2Msb));
/// assert_eq!(buf, [0b0000_0101]);
/// ```
pub fn apply_fill_order(buf: &mut [u8], fill_order: FillOrder) -> bool {
    if fill_order != FillOrder::Lsb2Msb {
        return false;
    }
    reverse_bits_in_place(buf);
    true
}

/// Swaps a decoded chunk from the file's byte order into the host's.
///
/// Only meaningful for uniform byte-aligned depths; sub-byte and non-power-of-two
/// depths are bit-packed and are handled by [`unpack_row`] instead.
pub fn to_native_endian(buf: &mut [u8], endian: Endian, bits_per_sample: &[u16]) {
    let Some(first) = bits_per_sample.first() else {
        return;
    };
    if !bits_per_sample.iter().all(|b| b == first) {
        return;
    }
    if !is_byte_aligned_depth(*first) {
        return;
    }
    endian.to_native_in_place(buf, (*first / 8) as usize);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_maps_every_documented_case() {
        let cases = [
            (1u16, SampleFormat::Uint, SampleType::U8),
            (8, SampleFormat::Uint, SampleType::U8),
            (12, SampleFormat::Uint, SampleType::U16),
            (16, SampleFormat::Uint, SampleType::U16),
            (24, SampleFormat::Uint, SampleType::U32),
            (32, SampleFormat::Uint, SampleType::U32),
            (64, SampleFormat::Uint, SampleType::U64),
            (8, SampleFormat::Int, SampleType::I8),
            (16, SampleFormat::Int, SampleType::I16),
            (32, SampleFormat::Int, SampleType::I32),
            (64, SampleFormat::Int, SampleType::I64),
            (16, SampleFormat::IeeeFp, SampleType::F16),
            (24, SampleFormat::IeeeFp, SampleType::F32),
            (32, SampleFormat::IeeeFp, SampleType::F32),
            (64, SampleFormat::IeeeFp, SampleType::F64),
            (8, SampleFormat::Void, SampleType::U8),
            (8, SampleFormat::Unknown(77), SampleType::U8),
        ];
        for (bits, format, expected) in cases {
            assert_eq!(
                SampleType::resolve(bits, format).expect("resolvable"),
                expected,
                "{bits} bits, {format}"
            );
        }
    }

    #[test]
    fn resolve_rejects_impossible_widths_and_complex_formats() {
        assert!(SampleType::resolve(0, SampleFormat::Uint).is_err());
        assert!(SampleType::resolve(65, SampleFormat::Uint).is_err());
        assert!(SampleType::resolve(8, SampleFormat::IeeeFp).is_err());
        assert!(SampleType::resolve(32, SampleFormat::ComplexInt).is_err());
        assert!(SampleType::resolve(64, SampleFormat::ComplexIeeeFp).is_err());
    }

    #[test]
    fn sample_type_geometry() {
        assert_eq!(SampleType::U8.byte_width(), 1);
        assert_eq!(SampleType::F16.byte_width(), 2);
        assert_eq!(SampleType::F64.byte_width(), 8);
        assert!(SampleType::F32.is_float());
        assert!(!SampleType::U32.is_float());
        assert!(SampleType::I16.is_signed_int());
        assert!(!SampleType::U16.is_signed_int());
    }

    #[test]
    fn f16_round_trips_through_f32_for_representable_values() {
        let cases: [(u16, f32); 8] = [
            (0x0000, 0.0),
            (0x8000, -0.0),
            (0x3C00, 1.0),
            (0xC000, -2.0),
            (0x7BFF, 65504.0),
            (0x0400, 6.103_515_6e-5),
            (0x0001, 5.960_464_5e-8),
            (0x3555, 0.333_251_95),
        ];
        for (bits, expected) in cases {
            let value = f16_bits_to_f32(bits);
            assert!(
                (value - expected).abs() <= expected.abs() * 1e-6 + f32::MIN_POSITIVE,
                "0x{bits:04X} -> {value} (expected {expected})"
            );
            assert_eq!(f32_to_f16_bits(value), bits, "round trip of 0x{bits:04X}");
        }
    }

    #[test]
    fn f16_handles_infinities_nans_and_overflow() {
        assert!(f16_bits_to_f32(0x7C00).is_infinite());
        assert!(f16_bits_to_f32(0xFC00).is_infinite());
        assert!(f16_bits_to_f32(0x7E00).is_nan());
        assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7C00);
        assert_eq!(f32_to_f16_bits(f32::NEG_INFINITY), 0xFC00);
        assert_ne!(f32_to_f16_bits(f32::NAN) & 0x03FF, 0);
        // 1e5 overflows binary16.
        assert_eq!(f32_to_f16_bits(1.0e5), 0x7C00);
        // 1e-9 underflows to zero.
        assert_eq!(f32_to_f16_bits(1.0e-9), 0x0000);
        assert_eq!(f32_to_f16_bits(-1.0e-9), 0x8000);
    }

    #[test]
    fn every_f16_bit_pattern_survives_the_f32_round_trip() {
        for bits in 0u16..=u16::MAX {
            let exp = (bits >> 10) & 0x1F;
            let mant = bits & 0x03FF;
            if exp == 0x1F && mant != 0 {
                continue; // NaN payloads are not required to be preserved bit-exactly.
            }
            let value = f16_bits_to_f32(bits);
            assert_eq!(f32_to_f16_bits(value), bits, "0x{bits:04X}");
        }
    }

    #[test]
    fn samples_buffers_convert_both_ways() {
        let samples = Samples::U16(vec![0x0102, 0x0304]);
        assert_eq!(samples.sample_type(), SampleType::U16);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples.byte_len(), 4);
        assert!(!samples.is_empty());
        let bytes = samples.to_native_bytes();
        assert_eq!(
            Samples::from_native_bytes(SampleType::U16, &bytes).expect("round trip"),
            samples
        );
        assert_eq!(samples.as_u16(), Some(&[0x0102u16, 0x0304][..]));
        assert_eq!(samples.as_u8(), None);
    }

    #[test]
    fn every_sample_variant_round_trips_through_bytes() {
        let all = [
            Samples::U8(vec![1, 2]),
            Samples::U16(vec![1, 2]),
            Samples::U32(vec![1, 2]),
            Samples::U64(vec![1, 2]),
            Samples::I8(vec![-1, 2]),
            Samples::I16(vec![-1, 2]),
            Samples::I32(vec![-1, 2]),
            Samples::I64(vec![-1, 2]),
            Samples::F16(vec![0x3C00, 0x4000]),
            Samples::F32(vec![1.5, -2.5]),
            Samples::F64(vec![1.5, -2.5]),
        ];
        for samples in all {
            let bytes = samples.to_native_bytes();
            let back =
                Samples::from_native_bytes(samples.sample_type(), &bytes).expect("round trip");
            assert_eq!(back, samples, "{:?}", samples.sample_type());
        }
    }

    #[test]
    fn from_native_bytes_rejects_a_ragged_length() {
        let err = Samples::from_native_bytes(SampleType::U32, &[1, 2, 3])
            .expect_err("3 bytes is not a whole number of u32s");
        assert!(matches!(
            err,
            TiffError::Usage(UsageError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn write_native_bytes_checks_the_destination() {
        let samples = Samples::U32(vec![1, 2]);
        let mut small = [0u8; 4];
        assert!(samples.write_native_bytes(&mut small).is_err());
        let mut big = [0u8; 16];
        samples.write_native_bytes(&mut big).expect("fits");
    }

    #[test]
    fn zeroed_buffers_honour_limits() {
        let limits = Limits {
            max_image_bytes: 8,
            ..Limits::default()
        };
        assert_eq!(
            Samples::new_zeroed(SampleType::U16, 4, &limits)
                .expect("fits")
                .len(),
            4
        );
        assert!(Samples::new_zeroed(SampleType::U16, 5, &limits).is_err());
        for ty in [
            SampleType::U8,
            SampleType::U16,
            SampleType::U32,
            SampleType::U64,
            SampleType::I8,
            SampleType::I16,
            SampleType::I32,
            SampleType::I64,
            SampleType::F16,
            SampleType::F32,
            SampleType::F64,
        ] {
            let s = Samples::new_zeroed(ty, 2, &Limits::default()).expect("alloc");
            assert_eq!(s.sample_type(), ty);
            assert_eq!(s.len(), 2);
        }
    }

    #[test]
    fn f16_buffer_expands_to_f32() {
        let s = Samples::F16(vec![0x3C00, 0xC000]);
        let expanded = s.f16_to_f32().expect("expand");
        assert_eq!(expanded, vec![1.0, -2.0]);
        assert!(Samples::U8(vec![0]).f16_to_f32().is_none());
    }

    #[test]
    fn row_geometry_matches_the_spec_padding_rule() {
        assert_eq!(packed_row_bits(&[1], 9), 9);
        assert_eq!(packed_row_bytes(&[1], 9), 2);
        assert_eq!(packed_row_bytes(&[4], 3), 2);
        assert_eq!(packed_row_bytes(&[12], 5), 8);
        assert_eq!(packed_row_bytes(&[8, 8, 8], 3), 3);
        assert_eq!(packed_row_bits(&[5, 6, 5], 3), 16);
        assert_eq!(packed_row_bytes(&[5, 6, 5], 6), 4);
        assert_eq!(packed_row_bits(&[], 4), 0);
    }

    #[test]
    fn one_bit_data_unpacks_msb_first() {
        let src = [0b1010_1100u8];
        let mut dst = [0u8; 8];
        unpack_row(&src, &[1], 8, SampleType::U8, &mut dst).expect("unpack");
        assert_eq!(dst, [1, 0, 1, 0, 1, 1, 0, 0]);
        let mut packed = [0u8; 1];
        pack_row(&dst, &[1], 8, SampleType::U8, &mut packed).expect("pack");
        assert_eq!(packed, src);
    }

    #[test]
    fn two_and_four_bit_data_round_trip() {
        let values2 = [3u8, 1, 0, 2, 3];
        let mut packed = vec![0u8; packed_row_bytes(&[2], 5) as usize];
        pack_row(&values2, &[2], 5, SampleType::U8, &mut packed).expect("pack");
        let mut back = [0u8; 5];
        unpack_row(&packed, &[2], 5, SampleType::U8, &mut back).expect("unpack");
        assert_eq!(back, values2);

        let values4 = [0xFu8, 0, 7, 8, 1];
        let mut packed = vec![0u8; packed_row_bytes(&[4], 5) as usize];
        pack_row(&values4, &[4], 5, SampleType::U8, &mut packed).expect("pack");
        let mut back = [0u8; 5];
        unpack_row(&packed, &[4], 5, SampleType::U8, &mut back).expect("unpack");
        assert_eq!(back, values4);
    }

    #[test]
    fn twelve_bit_data_round_trips_into_u16_slots() {
        let values: [u16; 5] = [0, 4095, 2048, 1, 3000];
        let mut src = Vec::new();
        for v in values {
            src.extend_from_slice(&v.to_ne_bytes());
        }
        let mut packed = vec![0u8; packed_row_bytes(&[12], 5) as usize];
        assert_eq!(packed.len(), 8);
        pack_row(&src, &[12], 5, SampleType::U16, &mut packed).expect("pack");
        let mut back = vec![0u8; 10];
        unpack_row(&packed, &[12], 5, SampleType::U16, &mut back).expect("unpack");
        let decoded = Samples::from_native_bytes(SampleType::U16, &back).expect("typed");
        assert_eq!(decoded.as_u16(), Some(&values[..]));
    }

    #[test]
    fn twenty_four_bit_integers_round_trip_into_u32_slots() {
        let values: [u32; 3] = [0, 0xFF_FFFF, 0x12_3456];
        let mut src = Vec::new();
        for v in values {
            src.extend_from_slice(&v.to_ne_bytes());
        }
        let mut packed = vec![0u8; packed_row_bytes(&[24], 3) as usize];
        assert_eq!(packed.len(), 9);
        pack_row(&src, &[24], 3, SampleType::U32, &mut packed).expect("pack");
        let mut back = vec![0u8; 12];
        unpack_row(&packed, &[24], 3, SampleType::U32, &mut back).expect("unpack");
        let decoded = Samples::from_native_bytes(SampleType::U32, &back).expect("typed");
        assert_eq!(decoded.as_u32(), Some(&values[..]));
    }

    #[test]
    fn twenty_four_bit_floats_expand_to_f32() {
        let original = [1.0f32, -2.5, 0.0];
        let mut src = Vec::new();
        for v in original {
            src.extend_from_slice(&v.to_ne_bytes());
        }
        let mut packed = vec![0u8; packed_row_bytes(&[24], 3) as usize];
        pack_row(&src, &[24], 3, SampleType::F32, &mut packed).expect("pack");
        let mut back = vec![0u8; 12];
        unpack_row(&packed, &[24], 3, SampleType::F32, &mut back).expect("unpack");
        let decoded = Samples::from_native_bytes(SampleType::F32, &back).expect("typed");
        assert_eq!(decoded.as_f32(), Some(&original[..]));
    }

    #[test]
    fn signed_sub_byte_samples_are_sign_extended() {
        // Two 4-bit two's-complement values: 0b1111 (-1) and 0b0111 (7).
        let src = [0b1111_0111u8];
        let mut dst = vec![0u8; 4];
        unpack_row(&src, &[4], 2, SampleType::I16, &mut dst).expect("unpack");
        let decoded = Samples::from_native_bytes(SampleType::I16, &dst).expect("typed");
        assert_eq!(decoded.as_i16(), Some(&[-1i16, 7][..]));
    }

    #[test]
    fn heterogeneous_bit_depths_are_supported() {
        // RGB565 packed into 16 bits per pixel.
        let values: [u16; 6] = [31, 63, 31, 0, 32, 1];
        let mut src = Vec::new();
        for v in values {
            src.extend_from_slice(&v.to_ne_bytes());
        }
        let bits = [5u16, 6, 5];
        let mut packed = vec![0u8; packed_row_bytes(&bits, 6) as usize];
        assert_eq!(packed.len(), 4);
        pack_row(&src, &bits, 6, SampleType::U16, &mut packed).expect("pack");
        let mut back = vec![0u8; 12];
        unpack_row(&packed, &bits, 6, SampleType::U16, &mut back).expect("unpack");
        let decoded = Samples::from_native_bytes(SampleType::U16, &back).expect("typed");
        assert_eq!(decoded.as_u16(), Some(&values[..]));
    }

    #[test]
    fn unpack_refuses_impossible_widths_and_short_destinations() {
        let mut dst = [0u8; 2];
        assert!(unpack_row(&[0], &[0], 2, SampleType::U8, &mut dst).is_err());
        assert!(unpack_row(&[0], &[65], 2, SampleType::U8, &mut dst).is_err());
        assert!(unpack_row(&[0], &[], 2, SampleType::U8, &mut dst).is_err());
        let mut tiny = [0u8; 1];
        assert!(unpack_row(&[0], &[8], 2, SampleType::U8, &mut tiny).is_err());
    }

    #[test]
    fn unpack_pads_a_short_source_with_zeros_instead_of_panicking() {
        let mut dst = [0u8; 4];
        unpack_row(&[0xFF], &[8], 4, SampleType::U8, &mut dst).expect("short source must not fail");
        assert_eq!(dst, [0xFF, 0, 0, 0]);
    }

    #[test]
    fn fill_order_applies_at_every_bit_depth() {
        // libtiff reverses the raw strip for every depth, not just sub-byte
        // ones; see the doc comment on `apply_fill_order`.
        let mut buf = [0b1000_0001u8];
        assert!(apply_fill_order(&mut buf, FillOrder::Lsb2Msb));
        assert_eq!(buf, [0b1000_0001]);
        let mut buf = [0b1010_0000u8];
        assert!(apply_fill_order(&mut buf, FillOrder::Lsb2Msb));
        assert_eq!(buf, [0b0000_0101]);
        // 8-bit data is reversed too: 0x02 -> 0x40, as `tiffcp -f lsb2msb`
        // writes it.
        let mut buf = [0x02u8, 0x0d];
        assert!(apply_fill_order(&mut buf, FillOrder::Lsb2Msb));
        assert_eq!(buf, [0x40, 0xb0]);
        // Msb2Lsb is a no-op.
        assert!(!apply_fill_order(&mut buf, FillOrder::Msb2Lsb));
        assert_eq!(buf, [0x40, 0xb0]);
    }

    #[test]
    fn native_endian_conversion_only_touches_uniform_byte_widths() {
        let foreign = if Endian::native() == Endian::Little {
            Endian::Big
        } else {
            Endian::Little
        };
        let mut buf = [1u8, 2, 3, 4];
        to_native_endian(&mut buf, foreign, &[16]);
        assert_eq!(buf, [2, 1, 4, 3]);
        let mut buf = [1u8, 2, 3, 4];
        to_native_endian(&mut buf, foreign, &[12]);
        assert_eq!(buf, [1, 2, 3, 4]);
        let mut buf = [1u8, 2, 3, 4];
        to_native_endian(&mut buf, foreign, &[8, 16]);
        assert_eq!(buf, [1, 2, 3, 4]);
        let mut buf = [1u8, 2];
        to_native_endian(&mut buf, foreign, &[]);
        assert_eq!(buf, [1, 2]);
    }

    #[test]
    fn byte_alignment_predicate() {
        for bits in [8u16, 16, 32, 64] {
            assert!(is_byte_aligned_depth(bits));
        }
        for bits in [1u16, 2, 4, 12, 24, 48] {
            assert!(!is_byte_aligned_depth(bits));
        }
    }
}
