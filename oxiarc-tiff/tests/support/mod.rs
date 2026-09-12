//! A deliberately dumb, spec-literal TIFF writer for the test suites.
//!
//! It shares no code with `oxiarc_tiff::writer`, so a bug in the encoder
//! cannot mask a bug in the decoder. It is also the only way to build the
//! fixtures no tool on this machine produces: a missing `StripByteCounts`, a
//! `SHORT`-typed `StripOffsets`, an IFD loop, an unassigned field type, a
//! `count` that overflows, and so on.

#![allow(dead_code)]

use std::collections::BTreeMap;

/// One IFD entry: `(type code, count, value bytes in file order)`.
type RawEntry = (u16, u64, Vec<u8>);

/// A hand-built TIFF file.
pub struct RawTiff {
    little_endian: bool,
    big: bool,
    entries: BTreeMap<u16, RawEntry>,
    payload: Vec<u8>,
    next_ifd: NextIfd,
    force_entry_count: Option<u64>,
}

/// What to write in the IFD's next-directory pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextIfd {
    /// End of chain.
    None,
    /// Point at this very IFD (an infinite loop).
    SelfLoop,
    /// A literal offset.
    At(u64),
}

impl RawTiff {
    /// A little-endian classic TIFF.
    pub fn new() -> Self {
        Self {
            little_endian: true,
            big: false,
            entries: BTreeMap::new(),
            payload: Vec::new(),
            next_ifd: NextIfd::None,
            force_entry_count: None,
        }
    }

    /// Switches to big-endian (`MM`).
    pub fn big_endian(mut self) -> Self {
        self.little_endian = false;
        self
    }

    /// Switches to the BigTIFF container.
    pub fn bigtiff(mut self) -> Self {
        self.big = true;
        self
    }

    /// Writes a different entry count than the number of entries present.
    pub fn force_entry_count(mut self, count: u64) -> Self {
        self.force_entry_count = Some(count);
        self
    }

    /// Chooses what the next-IFD pointer holds.
    pub fn next_ifd(mut self, next: NextIfd) -> Self {
        self.next_ifd = next;
        self
    }

    fn u16(&self, v: u16) -> [u8; 2] {
        if self.little_endian {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }

    fn u32(&self, v: u32) -> [u8; 4] {
        if self.little_endian {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }

    fn u64(&self, v: u64) -> [u8; 8] {
        if self.little_endian {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }

    fn header_size(&self) -> usize {
        if self.big { 16 } else { 8 }
    }

    fn inline_bytes(&self) -> usize {
        if self.big { 8 } else { 4 }
    }

    /// Appends a block of image data, returning its file offset.
    pub fn add_data(&mut self, bytes: &[u8]) -> u64 {
        if self.payload.len() % 2 == 1 {
            self.payload.push(0);
        }
        let offset = (self.header_size() + self.payload.len()) as u64;
        self.payload.extend_from_slice(bytes);
        offset
    }

    /// Appends a block of image data at an odd offset.
    pub fn add_data_odd(&mut self, bytes: &[u8]) -> u64 {
        if self.payload.len() % 2 == 0 {
            self.payload.push(0);
        }
        let offset = (self.header_size() + self.payload.len()) as u64;
        self.payload.extend_from_slice(bytes);
        offset
    }

    /// Sets a tag from raw parts, bypassing every consistency rule.
    pub fn raw(&mut self, tag: u16, ty: u16, count: u64, bytes: Vec<u8>) -> &mut Self {
        self.entries.insert(tag, (ty, count, bytes));
        self
    }

    /// Sets a `SHORT` tag.
    pub fn short(&mut self, tag: u16, values: &[u16]) -> &mut Self {
        let mut bytes = Vec::with_capacity(values.len() * 2);
        for v in values {
            bytes.extend_from_slice(&self.u16(*v));
        }
        self.raw(tag, 3, values.len() as u64, bytes)
    }

    /// Sets a `LONG` tag.
    pub fn long(&mut self, tag: u16, values: &[u32]) -> &mut Self {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for v in values {
            bytes.extend_from_slice(&self.u32(*v));
        }
        self.raw(tag, 4, values.len() as u64, bytes)
    }

    /// Sets a `LONG8` tag.
    pub fn long8(&mut self, tag: u16, values: &[u64]) -> &mut Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for v in values {
            bytes.extend_from_slice(&self.u64(*v));
        }
        self.raw(tag, 16, values.len() as u64, bytes)
    }

    /// Sets a `BYTE` tag.
    pub fn byte(&mut self, tag: u16, values: &[u8]) -> &mut Self {
        self.raw(tag, 1, values.len() as u64, values.to_vec())
    }

    /// Sets an `UNDEFINED` tag.
    pub fn undefined(&mut self, tag: u16, values: &[u8]) -> &mut Self {
        self.raw(tag, 7, values.len() as u64, values.to_vec())
    }

    /// Sets an `ASCII` tag (a single trailing NUL is appended).
    pub fn ascii(&mut self, tag: u16, text: &str) -> &mut Self {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let count = bytes.len() as u64;
        self.raw(tag, 2, count, bytes)
    }

    /// Sets a `RATIONAL` tag.
    pub fn rational(&mut self, tag: u16, values: &[(u32, u32)]) -> &mut Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for (num, den) in values {
            bytes.extend_from_slice(&self.u32(*num));
            bytes.extend_from_slice(&self.u32(*den));
        }
        self.raw(tag, 5, values.len() as u64, bytes)
    }

    /// Sets a `DOUBLE` tag.
    pub fn double(&mut self, tag: u16, values: &[f64]) -> &mut Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for v in values {
            let raw = v.to_bits();
            bytes.extend_from_slice(&self.u64(raw));
        }
        self.raw(tag, 12, values.len() as u64, bytes)
    }

    /// Removes a tag.
    pub fn remove(&mut self, tag: u16) -> &mut Self {
        self.entries.remove(&tag);
        self
    }

    /// Serialises the file.
    pub fn build(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.header_size()];
        if self.little_endian {
            out[0] = b'I';
            out[1] = b'I';
        } else {
            out[0] = b'M';
            out[1] = b'M';
        }
        let version = if self.big { 43u16 } else { 42u16 };
        out[2..4].copy_from_slice(&self.u16(version));
        if self.big {
            out[4..6].copy_from_slice(&self.u16(8));
            out[6..8].copy_from_slice(&self.u16(0));
        }
        out.extend_from_slice(&self.payload);

        // Out-of-line values.
        let inline = self.inline_bytes();
        let mut placed: BTreeMap<u16, u64> = BTreeMap::new();
        for (tag, (_, _, bytes)) in &self.entries {
            if bytes.len() > inline {
                if out.len() % 2 == 1 {
                    out.push(0);
                }
                placed.insert(*tag, out.len() as u64);
                out.extend_from_slice(bytes);
            }
        }

        if out.len() % 2 == 1 {
            out.push(0);
        }
        let ifd_offset = out.len() as u64;
        let count = self.force_entry_count.unwrap_or(self.entries.len() as u64);
        if self.big {
            out.extend_from_slice(&self.u64(count));
        } else {
            out.extend_from_slice(&self.u16(count as u16));
        }
        for (tag, (ty, entry_count, bytes)) in &self.entries {
            out.extend_from_slice(&self.u16(*tag));
            out.extend_from_slice(&self.u16(*ty));
            if self.big {
                out.extend_from_slice(&self.u64(*entry_count));
            } else {
                out.extend_from_slice(&self.u32(*entry_count as u32));
            }
            match placed.get(tag) {
                Some(offset) => {
                    if self.big {
                        out.extend_from_slice(&self.u64(*offset));
                    } else {
                        out.extend_from_slice(&self.u32(*offset as u32));
                    }
                }
                None => {
                    let mut field = vec![0u8; inline];
                    let take = bytes.len().min(inline);
                    field[..take].copy_from_slice(&bytes[..take]);
                    out.extend_from_slice(&field);
                }
            }
        }
        let next = match self.next_ifd {
            NextIfd::None => 0,
            NextIfd::SelfLoop => ifd_offset,
            NextIfd::At(offset) => offset,
        };
        if self.big {
            out.extend_from_slice(&self.u64(next));
        } else {
            out.extend_from_slice(&self.u32(next as u32));
        }

        // Patch the first-IFD pointer.
        if self.big {
            let bytes = self.u64(ifd_offset);
            out[8..16].copy_from_slice(&bytes);
        } else {
            let bytes = self.u32(ifd_offset as u32);
            out[4..8].copy_from_slice(&bytes);
        }
        out
    }
}

impl Default for RawTiff {
    fn default() -> Self {
        Self::new()
    }
}

/// A minimal, valid, uncompressed 8-bit greyscale single-strip TIFF.
pub fn gray8(width: u32, height: u32, pixels: &[u8]) -> RawTiff {
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(pixels);
    tiff.long(256, &[width]);
    tiff.long(257, &[height]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[height]);
    tiff.long(279, &[pixels.len() as u32]);
    tiff
}

/// A ramp of `n` bytes, useful as deterministic pixel data.
pub fn ramp(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}
