//! IFD emission: value placement, ascending tag order and offset patching.
//!
//! TIFF requires the entries of a directory to appear in **ascending numeric
//! tag order** — libtiff enforces it on read — so entries live in a
//! `BTreeMap` and the ordering falls out for free.
//!
//! ```
//! use oxiarc_tiff::writer::DirectoryWriter;
//! use oxiarc_tiff::{Tag, Value};
//!
//! let mut dir = DirectoryWriter::new();
//! dir.set(Tag::ImageLength, Value::Long(vec![2]));
//! dir.set(Tag::ImageWidth, Value::Long(vec![4]));
//! assert_eq!(dir.tags(), vec![256, 257]);
//! ```

use std::collections::BTreeMap;
use std::io::{Seek, Write};

use crate::byteorder::EndianWriter;
use crate::error::{Result, TiffError, UsageError};
use crate::header::Variant;
use crate::ifd::Value;
use crate::tags::Tag;

/// A directory under construction.
#[derive(Clone, Debug, Default)]
pub struct DirectoryWriter {
    entries: BTreeMap<u16, Value>,
}

/// Where a written directory lives and which field links to the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrittenDirectory {
    /// Offset of the directory itself.
    pub offset: u64,
    /// Offset of its next-IFD pointer field, for patching.
    pub next_field: u64,
}

impl DirectoryWriter {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a named tag, replacing any previous value.
    pub fn set(&mut self, tag: Tag, value: Value) {
        self.entries.insert(tag.to_u16(), value);
    }

    /// Sets a raw tag number, replacing any previous value.
    pub fn set_raw(&mut self, tag: u16, value: Value) {
        self.entries.insert(tag, value);
    }

    /// Sets a tag only when it is not already present.
    pub fn set_default(&mut self, tag: Tag, value: Value) {
        self.entries.entry(tag.to_u16()).or_insert(value);
    }

    /// Reads back a tag.
    #[must_use]
    pub fn get(&self, tag: Tag) -> Option<&Value> {
        self.entries.get(&tag.to_u16())
    }

    /// Removes a tag.
    pub fn remove(&mut self, tag: Tag) {
        self.entries.remove(&tag.to_u16());
    }

    /// `true` when the tag is present.
    #[must_use]
    pub fn contains(&self, tag: Tag) -> bool {
        self.entries.contains_key(&tag.to_u16())
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when nothing has been set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The tag numbers, in the order they will be written.
    #[must_use]
    pub fn tags(&self) -> Vec<u16> {
        self.entries.keys().copied().collect()
    }

    /// Every entry, in ascending tag order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &Value)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Writes the out-of-line values, then the directory itself.
    ///
    /// Values that fit in the entry's 4 (classic) or 8 (BigTIFF) value bytes
    /// are packed inline, left-justified and zero padded; everything else is
    /// written before the directory and referenced by offset. Every value and
    /// the directory itself start on an even offset, as TIFF recommends.
    ///
    /// # Errors
    /// [`UsageError::UnwritableTag`] for a value whose count does not fit the
    /// container, [`UsageError::ClassicTiffOverflow`] when a classic file would
    /// need a 64-bit offset, plus I/O failures.
    pub fn write<W: Write + Seek>(
        &self,
        writer: &mut EndianWriter<W>,
        variant: Variant,
    ) -> Result<WrittenDirectory> {
        let endian = writer.endian();
        let inline_bytes = variant.inline_value_bytes();
        let big = variant.is_big();

        // Pass 1: serialise every value and place the out-of-line ones.
        let mut encoded: Vec<(u16, u16, u64, Vec<u8>, Option<u64>)> =
            Vec::with_capacity(self.entries.len());
        for (tag, value) in &self.entries {
            let (bytes, count) = value.to_bytes(endian)?;
            if !big && count > u64::from(u32::MAX) {
                return Err(TiffError::Usage(UsageError::UnwritableTag {
                    tag: *tag,
                    message: format!("count {count} does not fit a classic TIFF entry"),
                }));
            }
            let ty_raw = value.ty_raw();
            let offset = if bytes.len() > inline_bytes {
                let at = writer.align_to(2)?;
                writer.write_bytes(&bytes)?;
                Some(at)
            } else {
                None
            };
            encoded.push((*tag, ty_raw, count, bytes, offset));
        }

        // Pass 2: the directory.
        let ifd_offset = writer.align_to(2)?;
        if big {
            writer.write_u64(encoded.len() as u64)?;
        } else {
            let count = u16::try_from(encoded.len()).map_err(|_| {
                TiffError::Usage(UsageError::InvalidSpec(format!(
                    "{} entries do not fit a classic TIFF directory",
                    encoded.len()
                )))
            })?;
            writer.write_u16(count)?;
        }
        for (tag, ty_raw, count, bytes, offset) in &encoded {
            writer.write_u16(*tag)?;
            writer.write_u16(*ty_raw)?;
            if big {
                writer.write_u64(*count)?;
            } else {
                writer.write_u32(*count as u32)?;
            }
            match offset {
                Some(at) => writer.write_offset(*at, big)?,
                None => {
                    let mut field = vec![0u8; inline_bytes];
                    let take = bytes.len().min(inline_bytes);
                    if let Some(dst) = field.get_mut(..take) {
                        if let Some(src) = bytes.get(..take) {
                            dst.copy_from_slice(src);
                        }
                    }
                    writer.write_bytes(&field)?;
                }
            }
        }
        let next_field = writer.offset();
        writer.write_offset(0, big)?;
        Ok(WrittenDirectory {
            offset: ifd_offset,
            next_field,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::header::Header;
    use crate::ifd::Directory;
    use crate::limits::{Limits, Warnings};
    use std::io::Cursor;

    fn round_trip(dir: &DirectoryWriter, variant: Variant, endian: Endian) -> Directory {
        let mut buffer = Cursor::new(Vec::new());
        let header = Header {
            endian,
            variant,
            first_ifd: 0,
        };
        let (bytes, len) = header.to_bytes();
        let mut writer = EndianWriter::new(&mut buffer, endian).expect("writer");
        writer
            .write_bytes(bytes.get(..len).unwrap_or(&[]))
            .expect("header");
        let written = dir.write(&mut writer, variant).expect("write ifd");
        writer
            .patch_offset_at(
                header.first_ifd_field_offset(),
                written.offset,
                variant.is_big(),
            )
            .expect("patch");
        drop(writer);
        let data = buffer.into_inner();
        let mut reader =
            crate::byteorder::EndianReader::new(Cursor::new(data), endian).expect("reader");
        let mut warnings = Warnings::new();
        Directory::read(
            &mut reader,
            written.offset,
            variant,
            &Limits::default(),
            &mut warnings,
        )
        .expect("read back")
    }

    #[test]
    fn entries_are_written_in_ascending_tag_order() {
        let mut dir = DirectoryWriter::new();
        dir.set(Tag::StripOffsets, Value::Long(vec![8]));
        dir.set(Tag::ImageWidth, Value::Long(vec![4]));
        dir.set(Tag::ImageLength, Value::Long(vec![2]));
        assert_eq!(dir.tags(), vec![256, 257, 273]);
        let read = round_trip(&dir, Variant::Classic, Endian::Little);
        let tags: Vec<u16> = read.iter().map(|(t, _)| t).collect();
        assert_eq!(tags, vec![256, 257, 273]);
    }

    #[test]
    fn inline_and_out_of_line_values_both_round_trip() {
        let mut dir = DirectoryWriter::new();
        dir.set(Tag::ImageWidth, Value::Short(vec![42]));
        dir.set(Tag::BitsPerSample, Value::Short(vec![8, 8, 8]));
        dir.set(Tag::Software, Value::Ascii("oxiarc-tiff".to_string()));
        dir.set(
            Tag::XResolution,
            Value::Rational(vec![crate::ifd::Rational { num: 72, den: 1 }]),
        );
        for variant in [Variant::Classic, Variant::Big] {
            for endian in [Endian::Little, Endian::Big] {
                let read = round_trip(&dir, variant, endian);
                assert_eq!(read.len(), 4);
                let width = read
                    .get(Tag::ImageWidth)
                    .and_then(|e| e.inline_value(endian).ok())
                    .flatten();
                assert_eq!(width, Some(Value::Short(vec![42])));
            }
        }
    }

    #[test]
    fn setters_and_accessors_behave() {
        let mut dir = DirectoryWriter::new();
        assert!(dir.is_empty());
        dir.set(Tag::Compression, Value::Short(vec![1]));
        dir.set_default(Tag::Compression, Value::Short(vec![5]));
        assert_eq!(dir.get(Tag::Compression), Some(&Value::Short(vec![1])));
        dir.set_raw(60000, Value::Byte(vec![1, 2]));
        assert_eq!(dir.len(), 2);
        assert!(dir.contains(Tag::Compression));
        dir.remove(Tag::Compression);
        assert!(!dir.contains(Tag::Compression));
        assert_eq!(dir.iter().count(), 1);
    }

    #[test]
    fn a_directory_with_too_many_entries_is_rejected_for_classic() {
        let mut dir = DirectoryWriter::new();
        // 65536 entries cannot be expressed by a classic u16 count. Building
        // them all is slow, so assert the guard through a direct construction.
        for tag in 0..=u16::MAX {
            dir.set_raw(tag, Value::Short(vec![tag]));
        }
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = EndianWriter::new(&mut buffer, Endian::Little).expect("writer");
        // 65536 entries: u16::MAX + 1.
        assert_eq!(dir.len(), 65536);
        let err = dir
            .write(&mut writer, Variant::Classic)
            .expect_err("too many entries");
        assert!(matches!(err, TiffError::Usage(UsageError::InvalidSpec(_))));
    }

    #[test]
    fn values_and_directories_start_on_even_offsets() {
        let mut dir = DirectoryWriter::new();
        dir.set(Tag::Software, Value::Ascii("abc".to_string()));
        dir.set(Tag::Artist, Value::Ascii("de".to_string()));
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = EndianWriter::new(&mut buffer, Endian::Little).expect("writer");
        writer.write_bytes(&[0u8; 9]).expect("odd prefix");
        let written = dir.write(&mut writer, Variant::Classic).expect("write");
        assert_eq!(written.offset % 2, 0);
    }
}
