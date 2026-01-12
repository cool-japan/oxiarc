//! ZIP archive format support.
//!
//! This module provides reading and writing of ZIP archives as specified
//! in the PKWARE APPNOTE.

mod header;

pub use header::{CompressionMethod, LocalFileHeader, ZipCompressionLevel, ZipReader, ZipWriter};

use oxiarc_core::error::Result;
use std::io::{Read, Seek, Write};

/// Read a ZIP archive.
pub fn read_zip<R: Read + Seek>(reader: R) -> Result<ZipReader<R>> {
    ZipReader::new(reader)
}

/// Create a new ZIP archive writer.
pub fn write_zip<W: Write>(writer: W) -> ZipWriter<W> {
    ZipWriter::new(writer)
}
