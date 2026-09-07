//! Image File Directories: entries, values and chain/tree traversal.
//!
//! A [`Directory`] stores only the fixed-size part of every entry (24 bytes of
//! `Copy` data in a `BTreeMap`); values are materialised on demand through a
//! [`ValueSource`]. A 60 000-entry IFD therefore costs one allocation, not
//! 60 000 heap vectors.
//!
//! ```
//! use oxiarc_tiff::ifd::{Rational, Value};
//!
//! let v = Value::Short(vec![1, 2, 3]);
//! assert_eq!(v.len(), 3);
//! assert_eq!(v.as_u64_vec(), Some(vec![1, 2, 3]));
//! assert_eq!(v.first_u64(), Some(1));
//! assert_eq!(Rational { num: 72, den: 1 }.as_f64(), Some(72.0));
//! ```

use std::collections::{BTreeMap, HashSet};

use crate::byteorder::{Endian, EndianReader};
use crate::error::{FormatError, LimitError, Result, TiffError};
use crate::header::Variant;
use crate::limits::{Leniency, Limits, Warning, Warnings};
use crate::tags::{Tag, Type};

use std::io::{Read, Seek};

/// An unsigned TIFF `RATIONAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rational {
    /// Numerator.
    pub num: u32,
    /// Denominator.
    pub den: u32,
}

impl Rational {
    /// The quotient, or `None` when the denominator is zero.
    ///
    /// TIFF files with a zero denominator exist; returning `None` rather than
    /// an infinity keeps the arithmetic honest.
    #[must_use]
    pub fn as_f64(self) -> Option<f64> {
        if self.den == 0 {
            None
        } else {
            Some(f64::from(self.num) / f64::from(self.den))
        }
    }
}

/// A signed TIFF `SRATIONAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SRational {
    /// Numerator.
    pub num: i32,
    /// Denominator.
    pub den: i32,
}

impl SRational {
    /// The quotient, or `None` when the denominator is zero.
    #[must_use]
    pub fn as_f64(self) -> Option<f64> {
        if self.den == 0 {
            None
        } else {
            Some(f64::from(self.num) / f64::from(self.den))
        }
    }
}

/// A pointer to another IFD in the same file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IfdPointer(pub u64);

impl IfdPointer {
    /// The raw file offset.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A materialised tag value.
///
/// The 16 assigned field types each get an arm; unassigned type codes are kept
/// verbatim in [`Value::Unknown`] so a reader/writer round-trip preserves them
/// (upstream `tiff` fails the whole directory instead).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// `BYTE` (type 1).
    Byte(Vec<u8>),
    /// `ASCII` (type 2), with the single trailing NUL removed.
    Ascii(String),
    /// `SHORT` (type 3).
    Short(Vec<u16>),
    /// `LONG` (type 4).
    Long(Vec<u32>),
    /// `RATIONAL` (type 5).
    Rational(Vec<Rational>),
    /// `SBYTE` (type 6).
    SByte(Vec<i8>),
    /// `UNDEFINED` (type 7).
    Undefined(Vec<u8>),
    /// `SSHORT` (type 8).
    SShort(Vec<i16>),
    /// `SLONG` (type 9).
    SLong(Vec<i32>),
    /// `SRATIONAL` (type 10).
    SRational(Vec<SRational>),
    /// `FLOAT` (type 11).
    Float(Vec<f32>),
    /// `DOUBLE` (type 12).
    Double(Vec<f64>),
    /// `IFD` (type 13).
    Ifd(Vec<u32>),
    /// `LONG8` (type 16).
    Long8(Vec<u64>),
    /// `SLONG8` (type 17).
    SLong8(Vec<i64>),
    /// `IFD8` (type 18).
    Ifd8(Vec<u64>),
    /// A type code this crate does not know, or ASCII that is not UTF-8.
    Unknown {
        /// The raw type code from the file.
        ty_raw: u16,
        /// The value bytes exactly as they appear in the file.
        bytes: Vec<u8>,
    },
}

impl Value {
    /// The field type this value was decoded as, when it is an assigned one.
    #[must_use]
    pub const fn ty(&self) -> Option<Type> {
        match self {
            Self::Byte(_) => Some(Type::Byte),
            Self::Ascii(_) => Some(Type::Ascii),
            Self::Short(_) => Some(Type::Short),
            Self::Long(_) => Some(Type::Long),
            Self::Rational(_) => Some(Type::Rational),
            Self::SByte(_) => Some(Type::SByte),
            Self::Undefined(_) => Some(Type::Undefined),
            Self::SShort(_) => Some(Type::SShort),
            Self::SLong(_) => Some(Type::SLong),
            Self::SRational(_) => Some(Type::SRational),
            Self::Float(_) => Some(Type::Float),
            Self::Double(_) => Some(Type::Double),
            Self::Ifd(_) => Some(Type::Ifd),
            Self::Long8(_) => Some(Type::Long8),
            Self::SLong8(_) => Some(Type::SLong8),
            Self::Ifd8(_) => Some(Type::Ifd8),
            Self::Unknown { .. } => None,
        }
    }

    /// Number of values (characters, for ASCII including the trailing NUL).
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Byte(v) | Self::Undefined(v) => v.len(),
            Self::Ascii(s) => s.len() + 1,
            Self::Short(v) => v.len(),
            Self::Long(v) | Self::Ifd(v) => v.len(),
            Self::Rational(v) => v.len(),
            Self::SByte(v) => v.len(),
            Self::SShort(v) => v.len(),
            Self::SLong(v) => v.len(),
            Self::SRational(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Double(v) => v.len(),
            Self::Long8(v) | Self::Ifd8(v) => v.len(),
            Self::SLong8(v) => v.len(),
            Self::Unknown { bytes, .. } => bytes.len(),
        }
    }

    /// `true` when the value carries nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Widens every unsigned-integer-shaped value to `u64`.
    ///
    /// Returns `None` for signed, floating-point, ASCII and unknown values.
    #[must_use]
    pub fn as_u64_vec(&self) -> Option<Vec<u64>> {
        match self {
            Self::Byte(v) | Self::Undefined(v) => Some(v.iter().map(|x| u64::from(*x)).collect()),
            Self::Short(v) => Some(v.iter().map(|x| u64::from(*x)).collect()),
            Self::Long(v) | Self::Ifd(v) => Some(v.iter().map(|x| u64::from(*x)).collect()),
            Self::Long8(v) | Self::Ifd8(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Widens every integer-shaped value to `i64`.
    #[must_use]
    pub fn as_i64_vec(&self) -> Option<Vec<i64>> {
        match self {
            Self::SByte(v) => Some(v.iter().map(|x| i64::from(*x)).collect()),
            Self::SShort(v) => Some(v.iter().map(|x| i64::from(*x)).collect()),
            Self::SLong(v) => Some(v.iter().map(|x| i64::from(*x)).collect()),
            Self::SLong8(v) => Some(v.clone()),
            other => other
                .as_u64_vec()
                .map(|v| v.into_iter().map(|x| x as i64).collect()),
        }
    }

    /// Converts any numeric value to `f64`, resolving rationals.
    ///
    /// A rational with a zero denominator yields `f64::NAN` rather than
    /// panicking; use [`Value::as_rational_vec`] to see the raw pair.
    #[must_use]
    pub fn as_f64_vec(&self) -> Option<Vec<f64>> {
        match self {
            Self::Float(v) => Some(v.iter().map(|x| f64::from(*x)).collect()),
            Self::Double(v) => Some(v.clone()),
            Self::Rational(v) => Some(v.iter().map(|r| r.as_f64().unwrap_or(f64::NAN)).collect()),
            Self::SRational(v) => Some(v.iter().map(|r| r.as_f64().unwrap_or(f64::NAN)).collect()),
            Self::SByte(_) | Self::SShort(_) | Self::SLong(_) | Self::SLong8(_) => self
                .as_i64_vec()
                .map(|v| v.into_iter().map(|x| x as f64).collect()),
            other => other
                .as_u64_vec()
                .map(|v| v.into_iter().map(|x| x as f64).collect()),
        }
    }

    /// The rational pairs, if this is a `RATIONAL` value.
    #[must_use]
    pub fn as_rational_vec(&self) -> Option<&[Rational]> {
        match self {
            Self::Rational(v) => Some(v),
            _ => None,
        }
    }

    /// The first value widened to `u64`, if any.
    #[must_use]
    pub fn first_u64(&self) -> Option<u64> {
        self.as_u64_vec().and_then(|v| v.first().copied())
    }

    /// The first value narrowed to `u16`, if it fits.
    #[must_use]
    pub fn first_u16(&self) -> Option<u16> {
        self.first_u64().and_then(|v| u16::try_from(v).ok())
    }

    /// The first value narrowed to `u32`, if it fits.
    #[must_use]
    pub fn first_u32(&self) -> Option<u32> {
        self.first_u64().and_then(|v| u32::try_from(v).ok())
    }

    /// The text of an `ASCII` value.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Ascii(s) => Some(s),
            _ => None,
        }
    }

    /// The raw bytes of a `BYTE`, `UNDEFINED` or unknown-typed value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Byte(v) | Self::Undefined(v) => Some(v),
            Self::Unknown { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    /// Serialises the value into the file's byte order.
    ///
    /// Returns the bytes and the value `count` that the entry must declare.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the count does not fit in a `u64`.
    pub fn to_bytes(&self, endian: Endian) -> Result<(Vec<u8>, u64)> {
        fn extend<const N: usize, T: Copy>(
            out: &mut Vec<u8>,
            values: &[T],
            f: impl Fn(T) -> [u8; N],
        ) {
            out.reserve(values.len() * N);
            for value in values {
                out.extend_from_slice(&f(*value));
            }
        }

        let mut out = Vec::new();
        let count = match self {
            Self::Byte(v) | Self::Undefined(v) => {
                out.extend_from_slice(v);
                v.len()
            }
            Self::Ascii(s) => {
                out.extend_from_slice(s.as_bytes());
                out.push(0);
                out.len()
            }
            Self::Short(v) => {
                extend(&mut out, v, |x| endian.put_u16(x));
                v.len()
            }
            Self::Long(v) | Self::Ifd(v) => {
                extend(&mut out, v, |x| endian.put_u32(x));
                v.len()
            }
            Self::Rational(v) => {
                for r in v {
                    out.extend_from_slice(&endian.put_u32(r.num));
                    out.extend_from_slice(&endian.put_u32(r.den));
                }
                v.len()
            }
            Self::SByte(v) => {
                extend(&mut out, v, |x: i8| [x as u8]);
                v.len()
            }
            Self::SShort(v) => {
                extend(&mut out, v, |x| endian.put_i16(x));
                v.len()
            }
            Self::SLong(v) => {
                extend(&mut out, v, |x| endian.put_i32(x));
                v.len()
            }
            Self::SRational(v) => {
                for r in v {
                    out.extend_from_slice(&endian.put_i32(r.num));
                    out.extend_from_slice(&endian.put_i32(r.den));
                }
                v.len()
            }
            Self::Float(v) => {
                extend(&mut out, v, |x| endian.put_f32(x));
                v.len()
            }
            Self::Double(v) => {
                extend(&mut out, v, |x| endian.put_f64(x));
                v.len()
            }
            Self::Long8(v) | Self::Ifd8(v) => {
                extend(&mut out, v, |x| endian.put_u64(x));
                v.len()
            }
            Self::SLong8(v) => {
                extend(&mut out, v, |x| endian.put_i64(x));
                v.len()
            }
            Self::Unknown { bytes, .. } => {
                out.extend_from_slice(bytes);
                bytes.len()
            }
        };
        Ok((out, count as u64))
    }

    /// The on-disk type code this value writes as.
    #[must_use]
    pub fn ty_raw(&self) -> u16 {
        match self {
            Self::Unknown { ty_raw, .. } => *ty_raw,
            other => other.ty().map_or(7, Type::to_u16),
        }
    }

    /// Decodes `bytes` (already in file order) as `count` values of `ty`.
    ///
    /// An unassigned type code, or ASCII that is not valid UTF-8, becomes
    /// [`Value::Unknown`].
    #[must_use]
    pub fn decode(ty_raw: u16, count: usize, bytes: &[u8], endian: Endian) -> Self {
        let Some(ty) = Type::from_u16(ty_raw) else {
            // An unassigned type code is charged one byte per value, which is
            // what `Entry::value_bytes` assumes, so the round-trip preserves
            // exactly the declared count.
            let take = count.min(bytes.len());
            return Self::Unknown {
                ty_raw,
                bytes: bytes.get(..take).unwrap_or(bytes).to_vec(),
            };
        };
        let width = ty.byte_len() as usize;
        let available = bytes.len() / width.max(1);
        let n = count.min(available);
        let take = n * width;
        let body = bytes.get(..take).unwrap_or(bytes);

        fn map_chunks<const N: usize, T>(body: &[u8], f: impl Fn([u8; N]) -> T) -> Vec<T> {
            body.chunks_exact(N)
                .map(|c| {
                    let mut buf = [0u8; N];
                    buf.copy_from_slice(c);
                    f(buf)
                })
                .collect()
        }

        match ty {
            Type::Byte => Self::Byte(body.to_vec()),
            Type::Undefined => Self::Undefined(body.to_vec()),
            Type::SByte => Self::SByte(body.iter().map(|b| *b as i8).collect()),
            Type::Ascii => {
                // Strip exactly one trailing NUL: interior NULs separate the
                // multiple strings some tags carry, and keeping them is what
                // makes a read/write round-trip byte-identical.
                let trimmed = match body.last() {
                    Some(0) => body.get(..body.len() - 1).unwrap_or(&[]),
                    _ => body,
                };
                match core::str::from_utf8(trimmed) {
                    Ok(text) => Self::Ascii(text.to_string()),
                    Err(_) => Self::Unknown {
                        ty_raw,
                        bytes: body.to_vec(),
                    },
                }
            }
            Type::Short => Self::Short(map_chunks(body, |b| endian.u16(b))),
            Type::SShort => Self::SShort(map_chunks(body, |b| endian.i16(b))),
            Type::Long => Self::Long(map_chunks(body, |b| endian.u32(b))),
            Type::SLong => Self::SLong(map_chunks(body, |b| endian.i32(b))),
            Type::Ifd => Self::Ifd(map_chunks(body, |b| endian.u32(b))),
            Type::Float => Self::Float(map_chunks(body, |b| endian.f32(b))),
            Type::Double => Self::Double(map_chunks(body, |b| endian.f64(b))),
            Type::Long8 => Self::Long8(map_chunks(body, |b| endian.u64(b))),
            Type::Ifd8 => Self::Ifd8(map_chunks(body, |b| endian.u64(b))),
            Type::SLong8 => Self::SLong8(map_chunks(body, |b| endian.i64(b))),
            Type::Rational => Self::Rational(
                body.chunks_exact(8)
                    .filter_map(|c| {
                        Some(Rational {
                            num: endian.u32_at(c, 0)?,
                            den: endian.u32_at(c, 4)?,
                        })
                    })
                    .collect(),
            ),
            Type::SRational => Self::SRational(
                body.chunks_exact(8)
                    .filter_map(|c| {
                        Some(SRational {
                            num: endian.u32_at(c, 0)? as i32,
                            den: endian.u32_at(c, 4)? as i32,
                        })
                    })
                    .collect(),
            ),
        }
    }
}

/// The fixed-size part of one IFD entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Raw tag number.
    pub tag: u16,
    /// Raw type code, retained even when unassigned.
    pub ty_raw: u16,
    /// Declared number of values.
    pub count: u64,
    /// The 4 (classic) or 8 (BigTIFF) value/offset bytes, in file order.
    raw: [u8; 8],
    /// Which container shape the entry came from.
    variant: Variant,
}

impl Entry {
    /// Builds an entry from its parsed fields.
    #[must_use]
    pub const fn new(tag: u16, ty_raw: u16, count: u64, raw: [u8; 8], variant: Variant) -> Self {
        Self {
            tag,
            ty_raw,
            count,
            raw,
            variant,
        }
    }

    /// The named tag.
    #[must_use]
    pub const fn tag(&self) -> Tag {
        Tag::from_u16(self.tag)
    }

    /// The assigned field type, if the code is one.
    #[must_use]
    pub const fn ty(&self) -> Option<Type> {
        Type::from_u16(self.ty_raw)
    }

    /// The raw value/offset bytes.
    #[must_use]
    pub const fn raw_field(&self) -> &[u8; 8] {
        &self.raw
    }

    /// `count * type width`, guarded against overflow.
    ///
    /// An unassigned type code is charged one byte per value (the only safe
    /// assumption) so the length still has an upper bound.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] on overflow.
    pub fn value_bytes(&self) -> Result<u64> {
        let width = self.ty().map_or(1, |t| t.byte_len());
        self.count.checked_mul(width).ok_or(TiffError::IntOverflow)
    }

    /// `true` when the value fits inside the entry itself.
    ///
    /// Recomputed from the byte length rather than trusting the writer.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] on overflow.
    pub fn is_inline(&self) -> Result<bool> {
        Ok(self.value_bytes()? <= self.variant.inline_value_bytes() as u64)
    }

    /// The out-of-line value offset, or `None` when the value is inline.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] on overflow.
    pub fn offset(&self, endian: Endian) -> Result<Option<u64>> {
        if self.is_inline()? {
            return Ok(None);
        }
        Ok(Some(match self.variant {
            Variant::Classic => u64::from(endian.u32_at(&self.raw, 0).unwrap_or(0)),
            Variant::Big => endian.u64_at(&self.raw, 0).unwrap_or(0),
        }))
    }

    /// Decodes an inline value, or `None` when the value lives elsewhere.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] on overflow.
    pub fn inline_value(&self, endian: Endian) -> Result<Option<Value>> {
        if !self.is_inline()? {
            return Ok(None);
        }
        let inline = self.variant.inline_value_bytes();
        let bytes = self.raw.get(..inline).unwrap_or(&self.raw);
        let count = usize::try_from(self.count).map_err(|_| TiffError::IntOverflow)?;
        Ok(Some(Value::decode(self.ty_raw, count, bytes, endian)))
    }
}

/// Anything that can materialise an out-of-line entry value.
///
/// Implemented by the decoder over its own reader; the geometry layer takes it
/// as a generic parameter so [`crate::ImageInfo`] can be built without knowing
/// about I/O.
pub trait ValueSource {
    /// Materialises the value of `entry`.
    ///
    /// # Errors
    /// Any read, bounds or limits failure.
    fn load(&mut self, entry: &Entry) -> Result<Value>;

    /// Reads `len` raw bytes at `offset`, when the source is backed by a file.
    ///
    /// Only old-style JPEG (compression 6) needs this: TIFF 6.0 §22's tags
    /// 519/520/521 hold *file offsets* to the quantisation and Huffman tables
    /// rather than the tables themselves, so they cannot be resolved from a
    /// directory entry alone.
    ///
    /// The default answers `Ok(None)` — "this source has no file behind it" —
    /// which is what the in-memory sources used by tests and by the writer
    /// want. A source that can read returns `Ok(None)` too when the range does
    /// not fit inside the file, so a bogus offset degrades to "no tables"
    /// rather than to an error.
    ///
    /// # Errors
    /// Any read or limits failure.
    fn read_raw(&mut self, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let _ = (offset, len);
        Ok(None)
    }
}

/// One parsed Image File Directory.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    entries: BTreeMap<u16, Entry>,
    next: Option<u64>,
    offset: u64,
}

impl Directory {
    /// An empty directory that will be written at `offset`.
    #[must_use]
    pub fn new(offset: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            next: None,
            offset,
        }
    }

    /// The file offset this directory was read from.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Looks up a named tag.
    #[must_use]
    pub fn get(&self, tag: Tag) -> Option<&Entry> {
        self.entries.get(&tag.to_u16())
    }

    /// Looks up a raw tag number.
    #[must_use]
    pub fn get_raw(&self, tag: u16) -> Option<&Entry> {
        self.entries.get(&tag)
    }

    /// `true` when the tag is present.
    #[must_use]
    pub fn contains(&self, tag: Tag) -> bool {
        self.entries.contains_key(&tag.to_u16())
    }

    /// Every entry, in ascending tag order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &Entry)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the directory has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The next IFD in the chain, if any.
    #[must_use]
    pub fn next_ifd(&self) -> Option<IfdPointer> {
        self.next.filter(|v| *v != 0).map(IfdPointer)
    }

    /// Inserts an entry, keeping the first when a tag repeats.
    ///
    /// Returns `true` when the entry was stored.
    pub fn insert(&mut self, entry: Entry) -> bool {
        if self.entries.contains_key(&entry.tag) {
            return false;
        }
        self.entries.insert(entry.tag, entry);
        true
    }

    /// Reads one directory at `offset`.
    ///
    /// Every count is checked against `limits` before anything is allocated,
    /// and the entry block is bounds-checked against the file length.
    ///
    /// # Errors
    /// [`FormatError::IfdTruncated`] when the entries do not fit in the file,
    /// [`LimitError::IfdEntries`] when there are too many, plus I/O failures.
    pub fn read<R: Read + Seek>(
        reader: &mut EndianReader<R>,
        offset: u64,
        variant: Variant,
        limits: &Limits,
        warnings: &mut Warnings,
    ) -> Result<Self> {
        let endian = reader.endian();
        let count_size = variant.count_size() as u64;
        if !reader.range_in_bounds(offset, count_size) {
            return Err(TiffError::Format(FormatError::IfdTruncated {
                offset,
                entries: 0,
            }));
        }
        reader.seek_to(offset)?;
        let entry_count = if variant.is_big() {
            reader.read_u64()?
        } else {
            u64::from(reader.read_u16()?)
        };
        let entry_count_usize = limits.check_ifd_entries(entry_count)?;

        let entry_size = variant.entry_size() as u64;
        let block = entry_count
            .checked_mul(entry_size)
            .ok_or(TiffError::IntOverflow)?;
        let body_start = offset
            .checked_add(count_size)
            .ok_or(TiffError::IntOverflow)?;
        let need = block
            .checked_add(variant.offset_size() as u64)
            .ok_or(TiffError::IntOverflow)?;
        if !reader.range_in_bounds(body_start, need) {
            return Err(TiffError::Format(FormatError::IfdTruncated {
                offset,
                entries: entry_count,
            }));
        }

        let mut directory = Self {
            entries: BTreeMap::new(),
            next: None,
            offset,
        };
        let mut buf = vec![0u8; variant.entry_size()];
        for _ in 0..entry_count_usize {
            reader.read_exact(&mut buf)?;
            let tag = endian.u16_at(&buf, 0).unwrap_or(0);
            let ty_raw = endian.u16_at(&buf, 2).unwrap_or(0);
            let count = if variant.is_big() {
                endian.u64_at(&buf, 4).unwrap_or(0)
            } else {
                u64::from(endian.u32_at(&buf, 4).unwrap_or(0))
            };
            let mut raw = [0u8; 8];
            let field_start = 4 + variant.offset_size();
            if let Some(field) = buf.get(field_start..) {
                let take = field.len().min(8);
                if let Some(dst) = raw.get_mut(..take) {
                    if let Some(src) = field.get(..take) {
                        dst.copy_from_slice(src);
                    }
                }
            }
            let entry = Entry::new(tag, ty_raw, count, raw, variant);
            if !directory.insert(entry) {
                warnings.push(Warning::DuplicateTag { tag });
            }
        }
        directory.next = Some(reader.read_offset(variant.is_big())?);
        Ok(directory)
    }
}

/// Walks an IFD chain or SubIFD tree with cycle detection and a hard cap.
#[derive(Debug, Default)]
pub struct IfdWalker {
    visited: HashSet<u64>,
    count: usize,
}

impl IfdWalker {
    /// A fresh walker with an empty visited set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            visited: HashSet::new(),
            count: 0,
        }
    }

    /// How many IFDs have been admitted so far.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Registers `offset` as about to be parsed.
    ///
    /// # Errors
    /// [`FormatError::IfdCycle`] when the offset was already visited, and
    /// [`LimitError::IfdCount`] once `limits.max_ifds` is reached.
    pub fn visit(&mut self, offset: u64, limits: &Limits) -> Result<()> {
        if !self.visited.insert(offset) {
            return Err(TiffError::Format(FormatError::IfdCycle { offset }));
        }
        self.count += 1;
        if self.count > limits.max_ifds {
            return Err(TiffError::Limits(LimitError::IfdCount {
                limit: limits.max_ifds,
            }));
        }
        Ok(())
    }

    /// `true` when `offset` has already been parsed.
    #[must_use]
    pub fn seen(&self, offset: u64) -> bool {
        self.visited.contains(&offset)
    }

    /// Forgets every visited offset.
    pub fn reset(&mut self) {
        self.visited.clear();
        self.count = 0;
    }
}

/// Reads the pointers stored in an IFD-pointer tag (330/34665/34853/40965).
///
/// # Errors
/// Propagates value-loading failures; a wrong field type is an error under
/// [`Leniency::Strict`] and an empty result otherwise.
pub fn read_ifd_pointers<S: ValueSource>(
    entry: &Entry,
    source: &mut S,
    leniency: Leniency,
) -> Result<Vec<IfdPointer>> {
    let value = source.load(entry)?;
    match value.as_u64_vec() {
        Some(offsets) => Ok(offsets
            .into_iter()
            .filter(|v| *v != 0)
            .map(IfdPointer)
            .collect()),
        None => {
            if leniency.is_strict() {
                Err(TiffError::Format(FormatError::InvalidTypeForTag {
                    tag: entry.tag(),
                    ty: entry.ty().unwrap_or(Type::Undefined),
                }))
            } else {
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn entry(ty: Type, count: u64, raw: [u8; 8], variant: Variant) -> Entry {
        Entry::new(256, ty.to_u16(), count, raw, variant)
    }

    #[test]
    fn rationals_refuse_to_divide_by_zero() {
        assert_eq!(Rational { num: 1, den: 0 }.as_f64(), None);
        assert_eq!(SRational { num: 1, den: 0 }.as_f64(), None);
        assert_eq!(Rational { num: 300, den: 2 }.as_f64(), Some(150.0));
        assert_eq!(SRational { num: -6, den: 3 }.as_f64(), Some(-2.0));
    }

    #[test]
    fn every_assigned_type_decodes_and_reencodes() {
        let endian = Endian::Little;
        let cases: Vec<(u16, usize, Vec<u8>)> = vec![
            (1, 2, vec![1, 2]),
            (2, 4, b"ab\0\0".to_vec()),
            (3, 2, vec![1, 0, 2, 0]),
            (4, 1, vec![1, 0, 0, 0]),
            (5, 1, vec![2, 0, 0, 0, 1, 0, 0, 0]),
            (6, 2, vec![0xFF, 1]),
            (7, 2, vec![9, 8]),
            (8, 1, vec![0xFF, 0xFF]),
            (9, 1, vec![0xFF, 0xFF, 0xFF, 0xFF]),
            (10, 1, vec![0xFF, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0]),
            (11, 1, vec![0, 0, 0x80, 0x3F]),
            (12, 1, vec![0, 0, 0, 0, 0, 0, 0xF0, 0x3F]),
            (13, 1, vec![8, 0, 0, 0]),
            (16, 1, vec![1, 0, 0, 0, 0, 0, 0, 0]),
            (17, 1, vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
            (18, 1, vec![16, 0, 0, 0, 0, 0, 0, 0]),
        ];
        for (ty_raw, count, bytes) in cases {
            let value = Value::decode(ty_raw, count, &bytes, endian);
            assert_eq!(value.ty_raw(), ty_raw, "type {ty_raw}");
            let (re, n) = value.to_bytes(endian).expect("re-encode");
            assert_eq!(re, bytes, "type {ty_raw} must round-trip");
            assert_eq!(n as usize, count, "type {ty_raw} count");
        }
    }

    #[test]
    fn unassigned_type_codes_are_retained() {
        let value = Value::decode(14, 3, &[1, 2, 3], Endian::Little);
        assert_eq!(
            value,
            Value::Unknown {
                ty_raw: 14,
                bytes: vec![1, 2, 3]
            }
        );
        assert_eq!(value.ty(), None);
        assert_eq!(value.as_bytes(), Some(&[1u8, 2, 3][..]));
        let (bytes, count) = value.to_bytes(Endian::Big).expect("round trip");
        assert_eq!(bytes, vec![1, 2, 3]);
        assert_eq!(count, 3);
    }

    #[test]
    fn non_utf8_ascii_becomes_unknown_so_it_round_trips() {
        let value = Value::decode(2, 3, &[0xFF, 0xFE, 0], Endian::Little);
        assert!(matches!(value, Value::Unknown { ty_raw: 2, .. }));
    }

    #[test]
    fn ascii_strips_exactly_the_trailing_nul() {
        let value = Value::decode(2, 8, b"WGS 84|\0", Endian::Little);
        assert_eq!(value.as_str(), Some("WGS 84|"));
        let (bytes, count) = value.to_bytes(Endian::Little).expect("encode");
        assert_eq!(bytes, b"WGS 84|\0");
        assert_eq!(count, 8);
        assert_eq!(value.len(), 8);
    }

    #[test]
    fn numeric_accessors_widen_consistently() {
        let short = Value::Short(vec![7, 8]);
        assert_eq!(short.as_u64_vec(), Some(vec![7, 8]));
        assert_eq!(short.as_i64_vec(), Some(vec![7, 8]));
        assert_eq!(short.first_u16(), Some(7));
        assert_eq!(short.first_u32(), Some(7));
        let signed = Value::SShort(vec![-3]);
        assert_eq!(signed.as_u64_vec(), None);
        assert_eq!(signed.as_i64_vec(), Some(vec![-3]));
        let rational = Value::Rational(vec![Rational { num: 3, den: 2 }]);
        assert_eq!(rational.as_f64_vec(), Some(vec![1.5]));
        assert_eq!(rational.as_rational_vec().map(<[_]>::len), Some(1));
        assert!(Value::Ascii(String::new()).as_u64_vec().is_none());
        assert!(!Value::Byte(vec![1]).is_empty());
        assert!(Value::Byte(Vec::new()).is_empty());
    }

    #[test]
    fn inline_versus_offset_is_recomputed_from_the_length() {
        // 2 SHORTs = 4 bytes: inline in classic.
        let e = entry(Type::Short, 2, [1, 0, 2, 0, 0, 0, 0, 0], Variant::Classic);
        assert!(e.is_inline().expect("inline"));
        assert_eq!(e.offset(Endian::Little).expect("offset"), None);
        assert_eq!(
            e.inline_value(Endian::Little).expect("value"),
            Some(Value::Short(vec![1, 2]))
        );

        // 3 SHORTs = 6 bytes: out of line in classic, inline in BigTIFF.
        let e = entry(Type::Short, 3, [8, 0, 0, 0, 0, 0, 0, 0], Variant::Classic);
        assert!(!e.is_inline().expect("not inline"));
        assert_eq!(e.offset(Endian::Little).expect("offset"), Some(8));
        assert_eq!(e.inline_value(Endian::Little).expect("value"), None);

        let e = entry(Type::Short, 3, [1, 0, 2, 0, 3, 0, 0, 0], Variant::Big);
        assert!(e.is_inline().expect("inline in bigtiff"));
        assert_eq!(
            e.inline_value(Endian::Little).expect("value"),
            Some(Value::Short(vec![1, 2, 3]))
        );
    }

    #[test]
    fn huge_counts_do_not_overflow() {
        let e = entry(Type::Double, u64::MAX, [0; 8], Variant::Big);
        assert!(matches!(e.value_bytes(), Err(TiffError::IntOverflow)));
        assert!(matches!(e.is_inline(), Err(TiffError::IntOverflow)));
        assert!(matches!(
            e.offset(Endian::Little),
            Err(TiffError::IntOverflow)
        ));
    }

    #[test]
    fn entry_metadata_is_exposed() {
        let e = entry(Type::Long, 1, [4, 0, 0, 0, 0, 0, 0, 0], Variant::Classic);
        assert_eq!(e.tag(), Tag::ImageWidth);
        assert_eq!(e.ty(), Some(Type::Long));
        assert_eq!(e.raw_field()[0], 4);
        let unknown = Entry::new(1, 99, 1, [0; 8], Variant::Classic);
        assert_eq!(unknown.ty(), None);
        assert_eq!(unknown.value_bytes().expect("one byte per value"), 1);
    }

    fn build_ifd(entries: &[(u16, u16, u32, [u8; 4])], next: u32) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        out[0] = b'I';
        out[1] = b'I';
        out[2] = 42;
        out[4] = 8;
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, ty, count, field) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(field);
        }
        out.extend_from_slice(&next.to_le_bytes());
        out
    }

    #[test]
    fn directory_reads_entries_in_tag_order() {
        let bytes = build_ifd(
            &[
                (257, 3, 1, [16, 0, 0, 0]),
                (256, 3, 1, [32, 0, 0, 0]),
                (259, 3, 1, [1, 0, 0, 0]),
            ],
            0,
        );
        let mut reader = EndianReader::new(Cursor::new(bytes), Endian::Little).expect("wrap");
        let mut warnings = Warnings::new();
        let dir = Directory::read(
            &mut reader,
            8,
            Variant::Classic,
            &Limits::default(),
            &mut warnings,
        )
        .expect("read ifd");
        assert_eq!(dir.len(), 3);
        assert!(!dir.is_empty());
        assert_eq!(dir.offset(), 8);
        assert_eq!(dir.next_ifd(), None);
        let tags: Vec<u16> = dir.iter().map(|(t, _)| t).collect();
        assert_eq!(tags, vec![256, 257, 259]);
        assert!(dir.contains(Tag::ImageWidth));
        assert_eq!(
            dir.get(Tag::ImageWidth)
                .and_then(|e| e.inline_value(Endian::Little).ok())
                .flatten(),
            Some(Value::Short(vec![32]))
        );
        assert!(dir.get_raw(999).is_none());
        assert!(warnings.is_empty());
    }

    #[test]
    fn duplicate_tags_keep_the_first_and_warn() {
        let bytes = build_ifd(&[(256, 3, 1, [1, 0, 0, 0]), (256, 3, 1, [2, 0, 0, 0])], 0);
        let mut reader = EndianReader::new(Cursor::new(bytes), Endian::Little).expect("wrap");
        let mut warnings = Warnings::new();
        let dir = Directory::read(
            &mut reader,
            8,
            Variant::Classic,
            &Limits::default(),
            &mut warnings,
        )
        .expect("read ifd");
        assert_eq!(dir.len(), 1);
        assert_eq!(
            dir.get(Tag::ImageWidth)
                .and_then(|e| e.inline_value(Endian::Little).ok())
                .flatten(),
            Some(Value::Short(vec![1]))
        );
        assert_eq!(warnings.as_slice(), &[Warning::DuplicateTag { tag: 256 }]);
    }

    #[test]
    fn an_entry_count_larger_than_the_file_is_rejected_before_allocating() {
        let mut bytes = vec![0u8; 8];
        bytes.extend_from_slice(&60000u16.to_le_bytes());
        let mut reader = EndianReader::new(Cursor::new(bytes), Endian::Little).expect("wrap");
        let mut warnings = Warnings::new();
        let err = Directory::read(
            &mut reader,
            8,
            Variant::Classic,
            &Limits::default(),
            &mut warnings,
        )
        .expect_err("must not allocate 60000 entries");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::IfdTruncated { .. })
        ));
    }

    #[test]
    fn the_entry_limit_is_enforced() {
        let bytes = build_ifd(&[(256, 3, 1, [1, 0, 0, 0]), (257, 3, 1, [1, 0, 0, 0])], 0);
        let mut reader = EndianReader::new(Cursor::new(bytes), Endian::Little).expect("wrap");
        let limits = Limits {
            max_ifd_entries: 1,
            ..Limits::default()
        };
        let mut warnings = Warnings::new();
        let err = Directory::read(&mut reader, 8, Variant::Classic, &limits, &mut warnings)
            .expect_err("entry limit");
        assert!(err.is_limits());
    }

    #[test]
    fn ifd_walker_detects_loops_and_caps_the_count() {
        let limits = Limits::default();
        let mut walker = IfdWalker::new();
        walker.visit(8, &limits).expect("first");
        assert!(walker.seen(8));
        assert_eq!(walker.count(), 1);
        let err = walker.visit(8, &limits).expect_err("cycle");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::IfdCycle { offset: 8 })
        ));
        walker.reset();
        assert!(!walker.seen(8));

        let tiny = Limits {
            max_ifds: 2,
            ..Limits::default()
        };
        let mut walker = IfdWalker::new();
        walker.visit(1, &tiny).expect("1");
        walker.visit(2, &tiny).expect("2");
        assert!(walker.visit(3, &tiny).expect_err("cap").is_limits());
    }

    struct MapSource(Value);
    impl ValueSource for MapSource {
        fn load(&mut self, _entry: &Entry) -> Result<Value> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn ifd_pointer_tags_drop_zero_offsets() {
        let e = Entry::new(330, 13, 3, [0; 8], Variant::Classic);
        let mut source = MapSource(Value::Ifd(vec![100, 0, 200]));
        let pointers = read_ifd_pointers(&e, &mut source, Leniency::Normal).expect("pointers");
        assert_eq!(pointers, vec![IfdPointer(100), IfdPointer(200)]);
        assert_eq!(pointers[0].get(), 100);
    }

    #[test]
    fn a_wrongly_typed_pointer_tag_is_strict_only() {
        let e = Entry::new(330, 11, 1, [0; 8], Variant::Classic);
        let mut source = MapSource(Value::Float(vec![1.0]));
        assert!(
            read_ifd_pointers(&e, &mut source, Leniency::Normal)
                .expect("lenient")
                .is_empty()
        );
        let err = read_ifd_pointers(&e, &mut source, Leniency::Strict).expect_err("strict");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::InvalidTypeForTag { .. })
        ));
    }

    #[test]
    fn directory_insert_keeps_the_first_entry() {
        let mut dir = Directory::new(0);
        assert!(dir.insert(Entry::new(
            256,
            3,
            1,
            [1, 0, 0, 0, 0, 0, 0, 0],
            Variant::Classic
        )));
        assert!(!dir.insert(Entry::new(
            256,
            3,
            1,
            [2, 0, 0, 0, 0, 0, 0, 0],
            Variant::Classic
        )));
        assert_eq!(dir.len(), 1);
    }
}
