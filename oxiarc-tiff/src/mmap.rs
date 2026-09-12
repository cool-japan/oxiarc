//! A [`Decoder`] over a memory-mapped file.
//!
//! [`Decoder::from_path`] opens a file with
//! [`oxiarc_core::mmap::MappedFile`] and wraps it in a [`Cursor`], which
//! implements `Read + Seek` over any `T: AsRef<[u8]>` -- so no new reader type
//! is needed, only the wiring. For a COG-style access pattern (many small
//! [`Decoder::read_region`] calls scattered across a large tiled image) this
//! removes one `seek` + `read` syscall pair per tile; for one linear
//! [`Decoder::read_image`] pass it is roughly neutral, since the OS still has
//! to page every byte in from disk either way.
//!
//! ```no_run
//! use oxiarc_tiff::Decoder;
//!
//! let mut decoder = Decoder::from_path("scan.tif")?;
//! let (width, height) = decoder.dimensions()?;
//! println!("{width}x{height}");
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```
//!
//! # Safety note (inherited from `memmap2`)
//!
//! A memory-mapped file is undefined behaviour to read if another process
//! truncates or otherwise mutates it while the mapping is open (a SIGBUS on
//! most platforms, not a Rust-visible error). `oxiarc-core`'s mapping is
//! read-only, which rules out self-inflicted corruption, but not a concurrent
//! writer elsewhere on the system. Prefer [`Decoder::new`] over a `File` or a
//! `Vec` buffer when the source might be modified while open.

use std::io::Cursor;
use std::path::Path;

use oxiarc_core::mmap::MappedFile;

use crate::error::Result;
use crate::reader::Decoder;

/// A [`Decoder`] reading a memory-mapped file, as built by
/// [`Decoder::from_path`].
pub type MmapDecoder = Decoder<Cursor<MappedFile>>;

impl Decoder<Cursor<MappedFile>> {
    /// Opens `path` as a read-only memory-mapped file and builds a decoder
    /// over it.
    ///
    /// Equivalent to `Decoder::new(Cursor::new(MappedFile::open(path)?))`;
    /// exposed as a convenience because that is the whole implementation.
    ///
    /// # Errors
    /// [`crate::TiffError::Io`] when the file cannot be opened or mapped,
    /// plus the header-parsing failures [`Decoder::new`] can produce.
    ///
    /// ```
    /// use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec};
    /// use std::io::{Cursor, Write};
    ///
    /// let path = std::env::temp_dir().join(format!(
    ///     "oxiarc-tiff-mmap-doctest-{}.tif",
    ///     std::process::id()
    /// ));
    ///
    /// let mut encoder = Encoder::new(Cursor::new(Vec::new()))?;
    /// encoder.write_image(&ImageSpec::new(2, 2, ColorType::Gray(8)), &[9u8, 8, 7, 6])?;
    /// std::fs::File::create(&path)?.write_all(&encoder.finish()?.into_inner())?;
    ///
    /// let mut decoder = Decoder::from_path(&path)?;
    /// assert_eq!(decoder.dimensions()?, (2, 2));
    ///
    /// std::fs::remove_file(&path)?;
    /// # Ok::<(), oxiarc_tiff::TiffError>(())
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        Decoder::new(Cursor::new(mapped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorType, Encoder, ImageSpec};
    use std::io::Write;

    /// A unique path per test name, so parallel tests in this file (sharing
    /// one process ID) never collide.
    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "oxiarc-tiff-mmap-{name}-{}.tif",
            std::process::id()
        ))
    }

    fn write_fixture(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) {
        let mut encoder = Encoder::new(Cursor::new(Vec::new())).expect("encoder");
        encoder
            .write_image(&ImageSpec::new(width, height, ColorType::Gray(8)), pixels)
            .expect("write");
        let bytes = encoder.finish().expect("finish").into_inner();
        std::fs::File::create(path)
            .expect("create")
            .write_all(&bytes)
            .expect("write file");
    }

    #[test]
    fn from_path_decodes_a_written_file() {
        let path = temp_path("roundtrip");
        let pixels: Vec<u8> = (0..64).collect();
        write_fixture(&path, 8, 8, &pixels);

        let mut decoder = Decoder::from_path(&path).expect("mmap decoder");
        assert_eq!(decoder.dimensions().expect("dims"), (8, 8));
        let region = decoder.read_region(0, 0, 8, 8).expect("read");
        assert_eq!(region.as_u8(), Some(pixels.as_slice()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_path_reports_io_error_for_a_missing_file() {
        let path = temp_path("missing-dir").join("does-not-exist.tif");
        let err = Decoder::from_path(&path).expect_err("missing file");
        assert!(matches!(err, crate::TiffError::Io(_)));
    }

    #[test]
    fn mmap_decoder_type_alias_matches_from_path() {
        let path = temp_path("alias");
        write_fixture(&path, 1, 1, &[42u8]);

        let decoder: MmapDecoder = Decoder::from_path(&path).expect("mmap decoder");
        drop(decoder);
        let _ = std::fs::remove_file(&path);
    }
}
