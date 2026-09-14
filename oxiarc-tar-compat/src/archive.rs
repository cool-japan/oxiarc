//! Reading tar archives: [`Archive`], [`Entries`], [`Entry`].

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use oxiarc_archive::tar::TarHeader;

use crate::header::{BLOCK_SIZE, Header, other};

struct ArchiveInner<R: ?Sized> {
    pos: Cell<u64>,
    ignore_zeros: bool,
    preserve_permissions: bool,
    preserve_mtime: bool,
    overwrite: bool,
    obj: RefCell<R>,
}

/// A top-level representation of an archive file.
///
/// This archive can have an entry added to it and it can be iterated over.
pub struct Archive<R: ?Sized + Read> {
    inner: ArchiveInner<R>,
}

impl<R: ?Sized + Read> std::fmt::Debug for Archive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("pos", &self.inner.pos.get())
            .finish()
    }
}

impl<R: ?Sized + Read> ArchiveInner<R> {
    /// Read up to `buf.len()` bytes at the current position.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.obj.borrow_mut().read(buf)?;
        self.pos.set(self.pos.get() + n as u64);
        Ok(n)
    }

    /// Fill `buf` completely; `Ok(false)` on EOF before the first byte,
    /// `UnexpectedEof` on EOF in the middle.
    fn read_full(&self, buf: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]) {
                Ok(0) if filled == 0 => return Ok(false),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to read entire block",
                    ));
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Advance to absolute offset `target` by reading and discarding.
    fn skip_to(&self, target: u64) -> io::Result<()> {
        let pos = self.pos.get();
        if target < pos {
            return Err(other("tar entries were read out of order"));
        }
        let mut left = target - pos;
        let mut scratch = [0u8; 8192];
        while left > 0 {
            let want = (left as usize).min(scratch.len());
            let n = self.read(&mut scratch[..want])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF while skipping tar entry data",
                ));
            }
            left -= n as u64;
        }
        Ok(())
    }
}

impl<R: Read> Archive<R> {
    /// Create a new archive with the underlying object as the reader.
    pub fn new(obj: R) -> Archive<R> {
        Archive {
            inner: ArchiveInner {
                pos: Cell::new(0),
                ignore_zeros: false,
                preserve_permissions: false,
                preserve_mtime: true,
                overwrite: true,
                obj: RefCell::new(obj),
            },
        }
    }

    /// Unwrap this archive, returning the underlying object.
    pub fn into_inner(self) -> R {
        self.inner.obj.into_inner()
    }
}

impl<R: ?Sized + Read> Archive<R> {
    /// Construct an iterator over the entries in this archive.
    ///
    /// Note that care must be taken to consider each entry within an
    /// archive in sequence. If entries are processed out of sequence (from
    /// what the iterator returns), then the contents read for each entry
    /// may be corrupted.
    ///
    /// # Errors
    ///
    /// When the archive has already been (partially) iterated.
    pub fn entries(&mut self) -> io::Result<Entries<'_, R>> {
        if self.inner.pos.get() != 0 {
            return Err(other("cannot call entries unless archive is at position 0"));
        }
        Ok(Entries {
            archive: &self.inner,
            next: 0,
            done: false,
            raw: false,
            pax_global: HashMap::new(),
        })
    }

    /// Unpacks the contents tarball into the specified `dst`.
    ///
    /// Entries whose path would escape `dst` are skipped.
    ///
    /// # Errors
    ///
    /// Any read or filesystem error.
    pub fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<()> {
        let dst = dst.as_ref();
        fs::create_dir_all(dst)?;
        for entry in self.entries()? {
            let mut entry = entry?;
            entry.unpack_in(dst)?;
        }
        Ok(())
    }

    /// Indicate whether extended file attributes (xattrs on Unix) are
    /// preserved when unpacking (accepted; xattrs are not restored).
    pub fn set_unpack_xattrs(&mut self, _unpack_xattrs: bool) {}

    /// Indicate whether extended permissions (like suid on Unix) are
    /// preserved when unpacking this entry.
    pub fn set_preserve_permissions(&mut self, preserve: bool) {
        self.inner.preserve_permissions = preserve;
    }

    /// Indicate whether files and symlinks should be overwritten on
    /// extraction.
    pub fn set_overwrite(&mut self, overwrite: bool) {
        self.inner.overwrite = overwrite;
    }

    /// Indicate whether access time information is preserved when unpacking
    /// this entry.
    pub fn set_preserve_mtime(&mut self, preserve: bool) {
        self.inner.preserve_mtime = preserve;
    }

    /// Ignore zeroed headers, which would otherwise indicate to the archive
    /// that it has no more entries.
    pub fn set_ignore_zeros(&mut self, ignore_zeros: bool) {
        self.inner.ignore_zeros = ignore_zeros;
    }
}

/// An iterator over the entries of an archive.
pub struct Entries<'a, R: 'a + ?Sized + Read> {
    archive: &'a ArchiveInner<R>,
    next: u64,
    done: bool,
    raw: bool,
    pax_global: HashMap<String, String>,
}

impl<'a, R: ?Sized + Read> Entries<'a, R> {
    /// Indicates whether this iterator will return raw entries or not.
    ///
    /// If the raw list of entries are returned, then no preprocessing
    /// happens on account of this library, for example taking into account
    /// GNU long name or long link archive members. Raw iteration is
    /// disabled by default.
    pub fn raw(self, raw: bool) -> Entries<'a, R> {
        Entries { raw, ..self }
    }

    fn read_extension(&mut self, size: u64, data_start: u64) -> io::Result<Vec<u8>> {
        if size > 1 << 24 {
            return Err(other("tar extension header is too large"));
        }
        self.archive.skip_to(data_start)?;
        let mut data = vec![0u8; size as usize];
        if !data.is_empty() && !self.archive.read_full(&mut data)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated tar extension header",
            ));
        }
        Ok(data)
    }

    fn next_entry(&mut self) -> io::Result<Option<Entry<'a, R>>> {
        let mut long_name: Option<Vec<u8>> = None;
        let mut long_link: Option<Vec<u8>> = None;
        let mut pax: Option<HashMap<String, String>> = None;
        loop {
            if self.done {
                return Ok(None);
            }
            self.archive.skip_to(self.next)?;
            let mut block = [0u8; BLOCK_SIZE];
            if !self.archive.read_full(&mut block)? {
                self.done = true;
                return Ok(None);
            }
            let header_pos = self.next;
            if block.iter().all(|b| *b == 0) {
                if self.archive.ignore_zeros {
                    self.next += BLOCK_SIZE as u64;
                    continue;
                }
                self.done = true;
                return Ok(None);
            }
            let header = Header::from_block(block);
            let stored = header.cksum()?;
            let computed = TarHeader::compute_checksum(&block);
            if stored != computed {
                self.done = true;
                return Err(other(&format!(
                    "archive header checksum mismatch (stored {stored}, computed {computed})"
                )));
            }
            let size = header.entry_size()?;
            let data_start = header_pos + BLOCK_SIZE as u64;
            let padded = size.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64;
            self.next = data_start
                .checked_add(padded)
                .ok_or_else(|| other("size overflow"))?;

            if !self.raw {
                let ty = header.entry_type();
                if ty.is_gnu_longname() || ty.is_gnu_longlink() {
                    let mut data = self.read_extension(size, data_start)?;
                    while data.last() == Some(&0) {
                        data.pop();
                    }
                    if ty.is_gnu_longname() {
                        long_name = Some(data);
                    } else {
                        long_link = Some(data);
                    }
                    continue;
                }
                if ty.is_pax_local_extensions() {
                    let data = self.read_extension(size, data_start)?;
                    pax = Some(TarHeader::parse_pax_data(&data));
                    continue;
                }
                if ty.is_pax_global_extensions() {
                    let data = self.read_extension(size, data_start)?;
                    self.pax_global.extend(TarHeader::parse_pax_data(&data));
                    continue;
                }
            }

            let mut attrs = self.pax_global.clone();
            if let Some(local) = pax.take() {
                attrs.extend(local);
            }
            let data_size = match attrs.get("size").and_then(|s| s.parse::<u64>().ok()) {
                Some(n) => {
                    // A PAX size override changes where the next header is.
                    let padded = n.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64;
                    self.next = data_start + padded;
                    n
                }
                None => size,
            };
            let path_override = long_name
                .take()
                .or_else(|| attrs.get("path").map(|p| p.as_bytes().to_vec()));
            let link_override = long_link
                .take()
                .or_else(|| attrs.get("linkpath").map(|p| p.as_bytes().to_vec()));
            return Ok(Some(Entry {
                header,
                header_pos,
                path_override,
                link_override,
                pax: attrs,
                size: data_size,
                pos: data_start,
                end: data_start + data_size,
                archive: self.archive,
            }));
        }
    }
}

impl<'a, R: ?Sized + Read> Iterator for Entries<'a, R> {
    type Item = io::Result<Entry<'a, R>>;

    fn next(&mut self) -> Option<io::Result<Entry<'a, R>>> {
        match self.next_entry() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// The result of unpacking an entry.
#[derive(Debug)]
#[non_exhaustive]
pub enum Unpacked {
    /// A file was unpacked.
    File(fs::File),
    /// A directory, hardlink, symlink, or other node was unpacked.
    Other,
}

/// A read-only view into an entry of an archive.
///
/// This structure is a window into a portion of a borrowed archive which
/// can be inspected. It acts as a file handle by implementing the Reader
/// trait. An entry cannot be rewritten once inserted into an archive.
pub struct Entry<'a, R: 'a + ?Sized + Read> {
    header: Header,
    header_pos: u64,
    path_override: Option<Vec<u8>>,
    link_override: Option<Vec<u8>>,
    pax: HashMap<String, String>,
    size: u64,
    pos: u64,
    end: u64,
    archive: &'a ArchiveInner<R>,
}

impl<R: ?Sized + Read> std::fmt::Debug for Entry<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("path", &String::from_utf8_lossy(&self.path_bytes()))
            .field("size", &self.size)
            .finish()
    }
}

fn bytes_to_path(bytes: Cow<'_, [u8]>) -> io::Result<Cow<'_, Path>> {
    match bytes {
        Cow::Borrowed(b) => std::str::from_utf8(b)
            .map(|s| Cow::Borrowed(Path::new(s)))
            .map_err(|_| other("path was not valid utf-8")),
        Cow::Owned(b) => String::from_utf8(b)
            .map(|s| Cow::Owned(PathBuf::from(s)))
            .map_err(|_| other("path was not valid utf-8")),
    }
}

impl<R: ?Sized + Read> Entry<'_, R> {
    /// Returns the path name for this entry, honoring GNU long names and
    /// PAX `path` records.
    ///
    /// # Errors
    ///
    /// A path that is not valid UTF-8.
    pub fn path(&self) -> io::Result<Cow<'_, Path>> {
        bytes_to_path(self.path_bytes())
    }

    /// Returns the raw bytes listed for this entry's path.
    pub fn path_bytes(&self) -> Cow<'_, [u8]> {
        match &self.path_override {
            Some(p) => Cow::Borrowed(p),
            None => self.header.path_bytes(),
        }
    }

    /// Returns the link name for this entry, if any is found.
    ///
    /// # Errors
    ///
    /// A link name that is not valid UTF-8.
    pub fn link_name(&self) -> io::Result<Option<Cow<'_, Path>>> {
        match self.link_name_bytes() {
            Some(bytes) => bytes_to_path(bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Returns the link name for this entry, in bytes, if listed.
    pub fn link_name_bytes(&self) -> Option<Cow<'_, [u8]>> {
        match &self.link_override {
            Some(l) => Some(Cow::Borrowed(l)),
            None => self.header.link_name_bytes(),
        }
    }

    /// Returns the PAX extension records that applied to this entry.
    pub fn pax_records(&self) -> &HashMap<String, String> {
        &self.pax
    }

    /// Returns access to the header of this entry in the archive.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Returns the size of this entry's data.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the starting position, in bytes, of the header of this
    /// entry in the archive.
    pub fn raw_header_position(&self) -> u64 {
        self.header_pos
    }

    /// Returns the starting position, in bytes, of the file of this entry
    /// in the archive.
    pub fn raw_file_position(&self) -> u64 {
        self.end - self.size
    }

    /// Unpacks this entry into the specified destination path (which must
    /// not escape anything: no sanitization happens here, see
    /// [`Entry::unpack_in`]).
    ///
    /// # Errors
    ///
    /// Filesystem or read errors.
    pub fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<Unpacked> {
        let dst = dst.as_ref();
        let ty = self.header.entry_type();
        if ty.is_dir() {
            fs::create_dir_all(dst)?;
            return Ok(Unpacked::Other);
        }
        if ty.is_symlink() || ty.is_hard_link() {
            let target = self
                .link_name()?
                .ok_or_else(|| other("link entry without a target"))?
                .into_owned();
            if self.archive.overwrite && fs::symlink_metadata(dst).is_ok() {
                fs::remove_file(dst)?;
            }
            #[cfg(unix)]
            {
                if ty.is_symlink() {
                    std::os::unix::fs::symlink(&target, dst)?;
                } else {
                    fs::hard_link(&target, dst)?;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = target;
                return Err(other("links are not supported on this platform"));
            }
            return Ok(Unpacked::Other);
        }
        if !(ty.is_file() || ty.is_contiguous()) {
            return Ok(Unpacked::Other);
        }
        if !self.archive.overwrite && dst.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", dst.display()),
            ));
        }
        let mut file = fs::File::create(dst)?;
        io::copy(self, &mut file)?;
        #[cfg(unix)]
        if let Ok(mode) = self.header.mode() {
            use std::os::unix::fs::PermissionsExt;
            let mode = if self.archive.preserve_permissions {
                mode & 0o7777
            } else {
                mode & 0o777
            };
            fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
        }
        if self.archive.preserve_mtime {
            if let Ok(mtime) = self.header.mtime() {
                let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
                file.set_modified(when)?;
            }
        }
        Ok(Unpacked::File(file))
    }

    /// Extracts this file under the specified path, avoiding security
    /// issues: entries with absolute paths or `..` escaping `dst` are
    /// skipped (returns `Ok(false)`).
    ///
    /// # Errors
    ///
    /// Filesystem or read errors.
    pub fn unpack_in<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<bool> {
        let mut file_dst = dst.as_ref().to_path_buf();
        {
            let path = self.path()?;
            for part in path.components() {
                match part {
                    Component::Prefix(..) | Component::RootDir | Component::CurDir => continue,
                    Component::ParentDir => return Ok(false),
                    Component::Normal(part) => file_dst.push(part),
                }
            }
        }
        if file_dst == dst.as_ref() {
            return Ok(true);
        }
        if let Some(parent) = file_dst.parent() {
            fs::create_dir_all(parent)?;
        }
        self.unpack(&file_dst)?;
        Ok(true)
    }
}

impl<R: ?Sized + Read> Read for Entry<'_, R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.end || into.is_empty() {
            return Ok(0);
        }
        self.archive.skip_to(self.pos)?;
        let want = ((self.end - self.pos) as usize).min(into.len());
        let n = self.archive.read(&mut into[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF inside tar entry",
            ));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_zero_archives() -> io::Result<()> {
        let mut a = Archive::new(&[][..]);
        assert_eq!(a.entries()?.count(), 0);
        let zeros = [0u8; 1024];
        let mut a = Archive::new(&zeros[..]);
        assert_eq!(a.entries()?.count(), 0);
        Ok(())
    }
}
