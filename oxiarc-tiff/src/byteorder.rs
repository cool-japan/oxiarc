//! Byte-order primitives — no `byteorder` crate, no `unsafe`.
//!
//! A TIFF file declares its own byte order in the first two bytes (`II` or
//! `MM`) and *every* multi-byte field in the file, including the pixel data,
//! is stored in that order. [`Endian`] carries it around; [`EndianReader`] and
//! [`EndianWriter`] add positioned reads and writes on top of `Read + Seek` /
//! `Write + Seek`.
//!
//! ```
//! use oxiarc_tiff::Endian;
//!
//! assert_eq!(Endian::Little.u16([0x2A, 0x00]), 42);
//! assert_eq!(Endian::Big.u16([0x00, 0x2A]), 42);
//! assert_eq!(Endian::Little.magic(), *b"II");
//! ```

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Result, TiffError};

/// The byte order a TIFF file declares in its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endian {
    /// `II` — Intel, least significant byte first.
    Little,
    /// `MM` — Motorola, most significant byte first.
    Big,
}

macro_rules! endian_scalar {
    ($read:ident, $put:ident, $ty:ty, $n:literal) => {
        #[doc = concat!("Decodes a `", stringify!($ty), "` in this byte order.")]
        #[must_use]
        pub const fn $read(self, bytes: [u8; $n]) -> $ty {
            match self {
                Self::Little => <$ty>::from_le_bytes(bytes),
                Self::Big => <$ty>::from_be_bytes(bytes),
            }
        }

        #[doc = concat!("Encodes a `", stringify!($ty), "` in this byte order.")]
        #[must_use]
        pub const fn $put(self, value: $ty) -> [u8; $n] {
            match self {
                Self::Little => value.to_le_bytes(),
                Self::Big => value.to_be_bytes(),
            }
        }
    };
}

impl Endian {
    /// The host's byte order.
    #[must_use]
    pub const fn native() -> Self {
        if cfg!(target_endian = "little") {
            Self::Little
        } else {
            Self::Big
        }
    }

    /// The two magic bytes that introduce a file in this byte order.
    #[must_use]
    pub const fn magic(self) -> [u8; 2] {
        match self {
            Self::Little => [b'I', b'I'],
            Self::Big => [b'M', b'M'],
        }
    }

    /// Recognises the two leading magic bytes.
    #[must_use]
    pub const fn from_magic(magic: [u8; 2]) -> Option<Self> {
        match magic {
            [b'I', b'I'] => Some(Self::Little),
            [b'M', b'M'] => Some(Self::Big),
            _ => None,
        }
    }

    /// `true` when this order matches the host's.
    #[must_use]
    pub const fn is_native(self) -> bool {
        matches!(
            (self, Self::native()),
            (Self::Little, Self::Little) | (Self::Big, Self::Big)
        )
    }

    endian_scalar!(u16, put_u16, u16, 2);
    endian_scalar!(i16, put_i16, i16, 2);
    endian_scalar!(u32, put_u32, u32, 4);
    endian_scalar!(i32, put_i32, i32, 4);
    endian_scalar!(u64, put_u64, u64, 8);
    endian_scalar!(i64, put_i64, i64, 8);
    endian_scalar!(f32, put_f32, f32, 4);
    endian_scalar!(f64, put_f64, f64, 8);

    /// Decodes a `u16` from the first two bytes of a slice, or `None` if short.
    #[must_use]
    pub fn u16_at(self, bytes: &[u8], offset: usize) -> Option<u16> {
        let end = offset.checked_add(2)?;
        let slice = bytes.get(offset..end)?;
        Some(self.u16([*slice.first()?, *slice.get(1)?]))
    }

    /// Decodes a `u32` from four bytes of a slice, or `None` if short.
    #[must_use]
    pub fn u32_at(self, bytes: &[u8], offset: usize) -> Option<u32> {
        let end = offset.checked_add(4)?;
        let slice = bytes.get(offset..end)?;
        let mut buf = [0u8; 4];
        buf.copy_from_slice(slice);
        Some(self.u32(buf))
    }

    /// Decodes a `u64` from eight bytes of a slice, or `None` if short.
    #[must_use]
    pub fn u64_at(self, bytes: &[u8], offset: usize) -> Option<u64> {
        let end = offset.checked_add(8)?;
        let slice = bytes.get(offset..end)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(slice);
        Some(self.u64(buf))
    }

    /// Byte-swaps `buf` in place, viewing it as `width`-byte elements, so the
    /// result is in the host's byte order.
    ///
    /// A `width` of 0 or 1, or a byte order that already matches the host's, is
    /// a no-op. A trailing partial element is left untouched (callers validate
    /// the length; corrupt chunks must not panic here).
    pub fn to_native_in_place(self, buf: &mut [u8], width: usize) {
        if width <= 1 || self.is_native() {
            return;
        }
        for element in buf.chunks_exact_mut(width) {
            element.reverse();
        }
    }

    /// Byte-swaps `buf` in place from host order into this byte order.
    ///
    /// The transform is its own inverse, so this simply forwards to
    /// [`Self::to_native_in_place`]; it exists so writer code reads correctly.
    pub fn from_native_in_place(self, buf: &mut [u8], width: usize) {
        self.to_native_in_place(buf, width);
    }
}

/// A `Read + Seek` source that knows the file's byte order and its length.
///
/// The length is captured once at construction with a `SeekFrom::End(0)` probe
/// (`Seek::stream_len` is newer than this crate's MSRV) and is used to bound
/// every offset that comes out of the file.
#[derive(Debug)]
pub struct EndianReader<R> {
    inner: R,
    endian: Endian,
    len: u64,
}

impl<R: Read + Seek> EndianReader<R> {
    /// Wraps `inner`, recording its length and rewinding it to the start.
    ///
    /// # Errors
    /// Propagates any seek failure.
    pub fn new(mut inner: R, endian: Endian) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self { inner, endian, len })
    }

    /// The byte order every scalar is read in.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// Total length of the source in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` when the source is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrows the wrapped reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Unwraps the reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Seeks to an absolute offset.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn seek_to(&mut self, offset: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Start(offset))?;
        Ok(())
    }

    /// Current position.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn position(&mut self) -> Result<u64> {
        Ok(self.inner.stream_position()?)
    }

    /// Reads `buf.len()` bytes from `offset`.
    ///
    /// # Errors
    /// [`crate::FormatError::ValueOffsetOutOfBounds`] is *not* raised here —
    /// callers do range checking with [`Self::range_in_bounds`]; this simply
    /// propagates the I/O error of a short read.
    pub fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.seek_to(offset)?;
        self.inner.read_exact(buf)?;
        Ok(())
    }

    /// Reads exactly `buf.len()` bytes from the current position.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf)?;
        Ok(())
    }

    /// `true` when `offset .. offset + len` lies inside the source.
    #[must_use]
    pub fn range_in_bounds(&self, offset: u64, len: u64) -> bool {
        match offset.checked_add(len) {
            Some(end) => end <= self.len,
            None => false,
        }
    }

    /// Reads a `u16` at the current position.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn read_u16(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.inner.read_exact(&mut buf)?;
        Ok(self.endian.u16(buf))
    }

    /// Reads a `u32` at the current position.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn read_u32(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.inner.read_exact(&mut buf)?;
        Ok(self.endian.u32(buf))
    }

    /// Reads a `u64` at the current position.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn read_u64(&mut self) -> Result<u64> {
        let mut buf = [0u8; 8];
        self.inner.read_exact(&mut buf)?;
        Ok(self.endian.u64(buf))
    }

    /// Reads a 4-byte (classic) or 8-byte (BigTIFF) file offset.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn read_offset(&mut self, big: bool) -> Result<u64> {
        if big {
            self.read_u64()
        } else {
            Ok(u64::from(self.read_u32()?))
        }
    }
}

/// A `Write + Seek` sink that tracks its own position and byte order.
#[derive(Debug)]
pub struct EndianWriter<W> {
    inner: W,
    endian: Endian,
    pos: u64,
}

impl<W: Write + Seek> EndianWriter<W> {
    /// Wraps `inner` and records its current position as the origin.
    ///
    /// # Errors
    /// Propagates any seek failure.
    pub fn new(mut inner: W, endian: Endian) -> Result<Self> {
        let pos = inner.stream_position()?;
        Ok(Self { inner, endian, pos })
    }

    /// The byte order every scalar is written in.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// Changes the byte order.
    ///
    /// Only meaningful before anything has been written: TIFF declares one
    /// byte order for the whole file.
    pub fn set_endian(&mut self, endian: Endian) {
        self.endian = endian;
    }

    /// Current write offset.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.pos
    }

    /// Borrows the wrapped writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Flushes and unwraps the writer.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn into_inner(mut self) -> Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }

    /// Appends raw bytes.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.write_all(bytes)?;
        self.pos = self
            .pos
            .checked_add(bytes.len() as u64)
            .ok_or(TiffError::IntOverflow)?;
        Ok(())
    }

    /// Appends a `u16`.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn write_u16(&mut self, value: u16) -> Result<()> {
        self.write_bytes(&self.endian.put_u16(value))
    }

    /// Appends a `u32`.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn write_u32(&mut self, value: u32) -> Result<()> {
        self.write_bytes(&self.endian.put_u32(value))
    }

    /// Appends a `u64`.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn write_u64(&mut self, value: u64) -> Result<()> {
        self.write_bytes(&self.endian.put_u64(value))
    }

    /// Appends a 4-byte (classic) or 8-byte (BigTIFF) offset.
    ///
    /// # Errors
    /// [`crate::UsageError::ClassicTiffOverflow`] when a classic file would
    /// need more than 32 bits, plus I/O failures.
    pub fn write_offset(&mut self, value: u64, big: bool) -> Result<()> {
        if big {
            self.write_u64(value)
        } else {
            let narrow = u32::try_from(value).map_err(|_| {
                TiffError::Usage(crate::error::UsageError::ClassicTiffOverflow { offset: value })
            })?;
            self.write_u32(narrow)
        }
    }

    /// Pads with zero bytes until the offset is a multiple of `align`.
    ///
    /// Returns the new offset. TIFF recommends word alignment for values and
    /// IFDs; this crate always honours it on write.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn align_to(&mut self, align: u64) -> Result<u64> {
        if align <= 1 {
            return Ok(self.pos);
        }
        let rem = self.pos % align;
        if rem != 0 {
            let pad = align - rem;
            let zeros = [0u8; 8];
            let mut left = pad;
            while left > 0 {
                let take = left.min(zeros.len() as u64) as usize;
                self.write_bytes(&zeros[..take])?;
                left -= take as u64;
            }
        }
        Ok(self.pos)
    }

    /// Overwrites four bytes at `offset` and returns to the end of the stream.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn patch_u32_at(&mut self, offset: u64, value: u32) -> Result<()> {
        let end = self.pos;
        self.inner.seek(SeekFrom::Start(offset))?;
        self.inner.write_all(&self.endian.put_u32(value))?;
        self.inner.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Overwrites eight bytes at `offset` and returns to the end of the stream.
    ///
    /// # Errors
    /// Propagates I/O failures.
    pub fn patch_u64_at(&mut self, offset: u64, value: u64) -> Result<()> {
        let end = self.pos;
        self.inner.seek(SeekFrom::Start(offset))?;
        self.inner.write_all(&self.endian.put_u64(value))?;
        self.inner.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Overwrites a 4- or 8-byte offset field at `offset`.
    ///
    /// # Errors
    /// [`crate::UsageError::ClassicTiffOverflow`] when a classic file would
    /// need more than 32 bits, plus I/O failures.
    pub fn patch_offset_at(&mut self, offset: u64, value: u64, big: bool) -> Result<()> {
        if big {
            self.patch_u64_at(offset, value)
        } else {
            let narrow = u32::try_from(value).map_err(|_| {
                TiffError::Usage(crate::error::UsageError::ClassicTiffOverflow { offset: value })
            })?;
            self.patch_u32_at(offset, narrow)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn scalars_round_trip_in_both_orders() {
        for endian in [Endian::Little, Endian::Big] {
            assert_eq!(endian.u16(endian.put_u16(0xBEEF)), 0xBEEF);
            assert_eq!(endian.i16(endian.put_i16(-2)), -2);
            assert_eq!(endian.u32(endian.put_u32(0xDEAD_BEEF)), 0xDEAD_BEEF);
            assert_eq!(endian.i32(endian.put_i32(-70000)), -70000);
            assert_eq!(endian.u64(endian.put_u64(u64::MAX - 5)), u64::MAX - 5);
            assert_eq!(endian.i64(endian.put_i64(i64::MIN)), i64::MIN);
            assert!((endian.f32(endian.put_f32(1.5)) - 1.5).abs() < f32::EPSILON);
            assert!((endian.f64(endian.put_f64(-0.25)) + 0.25).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn magic_bytes_round_trip() {
        assert_eq!(Endian::from_magic(*b"II"), Some(Endian::Little));
        assert_eq!(Endian::from_magic(*b"MM"), Some(Endian::Big));
        assert_eq!(Endian::from_magic(*b"XX"), None);
        assert_eq!(Endian::Big.magic(), *b"MM");
    }

    #[test]
    fn slice_accessors_are_bounds_checked() {
        let bytes = [1u8, 2, 3];
        assert_eq!(Endian::Little.u16_at(&bytes, 0), Some(0x0201));
        assert_eq!(Endian::Little.u16_at(&bytes, 2), None);
        assert_eq!(Endian::Little.u32_at(&bytes, 0), None);
        assert_eq!(Endian::Little.u64_at(&bytes, 0), None);
        assert_eq!(Endian::Big.u16_at(&bytes, 1), Some(0x0203));
    }

    #[test]
    fn in_place_swap_ignores_trailing_partial_elements() {
        let foreign = if Endian::native() == Endian::Little {
            Endian::Big
        } else {
            Endian::Little
        };
        let mut buf = [1u8, 2, 3, 4, 5];
        foreign.to_native_in_place(&mut buf, 2);
        assert_eq!(buf, [2, 1, 4, 3, 5]);
        // Native order and width <= 1 are no-ops.
        let mut same = [1u8, 2, 3, 4];
        Endian::native().to_native_in_place(&mut same, 2);
        assert_eq!(same, [1, 2, 3, 4]);
        foreign.to_native_in_place(&mut same, 1);
        assert_eq!(same, [1, 2, 3, 4]);
    }

    #[test]
    fn reader_records_length_and_reads_positionally() {
        let data: Vec<u8> = (0u8..=31).collect();
        let mut reader = EndianReader::new(Cursor::new(data), Endian::Little).expect("wrap cursor");
        assert_eq!(reader.len(), 32);
        assert!(!reader.is_empty());
        assert!(reader.range_in_bounds(24, 8));
        assert!(!reader.range_in_bounds(25, 8));
        assert!(!reader.range_in_bounds(u64::MAX, 1));
        let mut buf = [0u8; 4];
        reader.read_exact_at(4, &mut buf).expect("positional read");
        assert_eq!(buf, [4, 5, 6, 7]);
        reader.seek_to(0).expect("rewind");
        assert_eq!(reader.read_u16().expect("u16"), 0x0100);
        assert_eq!(reader.read_u32().expect("u32"), 0x0504_0302);
        assert_eq!(reader.position().expect("pos"), 6);
        reader.seek_to(0).expect("rewind");
        assert_eq!(reader.read_offset(false).expect("offset"), 0x0302_0100);
    }

    #[test]
    fn writer_tracks_offsets_aligns_and_patches() {
        let mut writer =
            EndianWriter::new(Cursor::new(Vec::new()), Endian::Little).expect("wrap cursor");
        writer.write_u16(0x1234).expect("u16");
        assert_eq!(writer.offset(), 2);
        writer.write_bytes(&[9]).expect("byte");
        writer.align_to(4).expect("align");
        assert_eq!(writer.offset(), 4);
        writer.write_u32(0).expect("placeholder");
        writer.write_u64(7).expect("u64");
        writer.patch_u32_at(4, 0xAABB_CCDD).expect("patch");
        assert_eq!(writer.offset(), 16);
        let out = writer.into_inner().expect("finish").into_inner();
        assert_eq!(&out[0..2], &[0x34, 0x12]);
        assert_eq!(&out[4..8], &[0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn classic_offsets_reject_values_above_u32_max() {
        let mut writer =
            EndianWriter::new(Cursor::new(Vec::new()), Endian::Big).expect("wrap cursor");
        let err = writer
            .write_offset(u64::from(u32::MAX) + 1, false)
            .expect_err("must not truncate");
        assert!(matches!(
            err,
            TiffError::Usage(crate::error::UsageError::ClassicTiffOverflow { .. })
        ));
        writer
            .write_offset(u64::from(u32::MAX) + 1, true)
            .expect("BigTIFF holds it");
    }
}
