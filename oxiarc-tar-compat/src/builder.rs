//! Writing tar archives.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::EntryType;
use crate::header::{BLOCK_SIZE, Header, other};

/// How much metadata `append_path`-style methods copy from the filesystem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum HeaderMode {
    /// All supported metadata, including mod/access times and ownership will
    /// be included.
    Complete,
    /// Only metadata that is directly relevant to the identity of a file
    /// will be included. In particular, ownership and mod/access times are
    /// excluded.
    Deterministic,
}

/// A structure for building archives.
///
/// This structure has methods for building up an archive from scratch into
/// any arbitrary writer. The archive is terminated (two zero blocks) by
/// [`Builder::finish`], [`Builder::into_inner`], or on drop.
pub struct Builder<W: Write> {
    mode: HeaderMode,
    follow: bool,
    finished: bool,
    obj: Option<W>,
}

impl<W: Write> std::fmt::Debug for Builder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("mode", &self.mode)
            .field("finished", &self.finished)
            .finish()
    }
}

/// Write `header` then `data`, padded to a whole block.
fn append(dst: &mut dyn Write, header: &Header, mut data: &mut dyn Read) -> io::Result<()> {
    dst.write_all(header.as_bytes())?;
    let len = io::copy(&mut data, dst)?;
    let rem = (len % BLOCK_SIZE as u64) as usize;
    if rem != 0 {
        dst.write_all(&[0u8; BLOCK_SIZE][..BLOCK_SIZE - rem])?;
    }
    Ok(())
}

/// The GNU `././@LongLink` header the tar crate writes before an entry whose
/// name does not fit.
fn prepare_long_header(size: u64, entry_type: u8) -> Header {
    let mut header = Header::new_gnu();
    let name = b"././@LongLink";
    header.as_mut_bytes()[..name.len()].copy_from_slice(name);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(size + 1);
    header.set_entry_type(EntryType::new(entry_type));
    header.set_cksum();
    header
}

/// Set `path` on `header`, emitting a GNU long-name record first when it
/// does not fit (the tar crate's `prepare_header_path`).
fn prepare_header_path(dst: &mut dyn Write, header: &mut Header, path: &Path) -> io::Result<()> {
    if let Err(e) = header.set_path(path) {
        let data = crate::header::normalize_path(path, false, false, false)?;
        let max = 100;
        if data.len() < max {
            return Err(e);
        }
        let long = prepare_long_header(data.len() as u64, b'L');
        let mut with_nul = data.clone();
        with_nul.push(0);
        append(dst, &long, &mut &with_nul[..])?;
        // Store a truncated name in the real header, as GNU tar does.
        let truncated = match std::str::from_utf8(&data[..max]) {
            Ok(s) => s,
            Err(e) => std::str::from_utf8(&data[..e.valid_up_to()]).unwrap_or_default(),
        };
        header.set_truncated_path_for_gnu_header(Path::new(truncated), false)?;
    }
    Ok(())
}

impl<W: Write> Builder<W> {
    /// Create a new archive builder with the underlying object as the
    /// destination of all data written. The builder will use
    /// `HeaderMode::Complete` by default.
    pub fn new(obj: W) -> Builder<W> {
        Builder {
            mode: HeaderMode::Complete,
            follow: true,
            finished: false,
            obj: Some(obj),
        }
    }

    /// Changes the HeaderMode that will be used when reading fs Metadata
    /// for methods that implicitly read metadata for an input Path.
    pub fn mode(&mut self, mode: HeaderMode) {
        self.mode = mode;
    }

    /// Follow symlinks, archiving the contents of the file they point to
    /// rather than adding a symlink to the archive. Defaults to true.
    pub fn follow_symlinks(&mut self, follow: bool) {
        self.follow = follow;
    }

    /// Gets shared reference to the underlying object.
    pub fn get_ref(&self) -> &W {
        match self.obj.as_ref() {
            Some(w) => w,
            None => unreachable!("Builder used after into_inner"),
        }
    }

    /// Gets mutable reference to the underlying object.
    ///
    /// Note that care must be taken while writing to the underlying object.
    pub fn get_mut(&mut self) -> &mut W {
        match self.obj.as_mut() {
            Some(w) => w,
            None => unreachable!("Builder used after into_inner"),
        }
    }

    fn obj(&mut self) -> io::Result<&mut W> {
        self.obj
            .as_mut()
            .ok_or_else(|| other("builder already consumed"))
    }

    /// Unwrap this archive, returning the underlying object.
    ///
    /// This function will finish writing the archive if the `finish`
    /// function hasn't yet been called, returning any I/O error which
    /// happens during that operation.
    ///
    /// # Errors
    ///
    /// Any I/O error finishing the archive.
    pub fn into_inner(mut self) -> io::Result<W> {
        if !self.finished {
            self.finish()?;
        }
        self.obj
            .take()
            .ok_or_else(|| other("builder already consumed"))
    }

    /// Adds a new entry to this archive.
    ///
    /// This function will append the header specified, followed by
    /// contents of the stream specified by `data`. To produce a valid
    /// archive the `size` field of `header` must be the same as the length
    /// of the stream that's being written. Additionally the checksum for
    /// the header should have been set via the `set_cksum` method.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn append<R: Read>(&mut self, header: &Header, mut data: R) -> io::Result<()> {
        let obj = self.obj()?;
        append(obj, header, &mut data)
    }

    /// Adds a new entry to this archive with the specified path.
    ///
    /// This function will set the specified path in the given header, which
    /// may require appending a GNU long-name extension entry to the archive
    /// first. The checksum for the header will be automatically updated via
    /// the `set_cksum` method after setting the path.
    ///
    /// # Errors
    ///
    /// An invalid path (absolute, `..`), or any I/O error.
    pub fn append_data<P: AsRef<Path>, R: Read>(
        &mut self,
        header: &mut Header,
        path: P,
        data: R,
    ) -> io::Result<()> {
        prepare_header_path(self.obj()?, header, path.as_ref())?;
        header.set_cksum();
        self.append(header, data)
    }

    /// Adds a new link (symbolic or hard) entry to this archive with the
    /// specified path and target.
    ///
    /// # Errors
    ///
    /// An invalid path, or any I/O error.
    pub fn append_link<P: AsRef<Path>, T: AsRef<Path>>(
        &mut self,
        header: &mut Header,
        path: P,
        target: T,
    ) -> io::Result<()> {
        prepare_header_path(self.obj()?, header, path.as_ref())?;
        let target = target.as_ref();
        if header.set_link_name(target).is_err() {
            let data = target
                .to_str()
                .ok_or_else(|| other("link target was not valid Unicode"))?
                .as_bytes()
                .to_vec();
            let long = prepare_long_header(data.len() as u64, b'K');
            let mut with_nul = data;
            with_nul.push(0);
            let obj = self.obj()?;
            append(obj, &long, &mut &with_nul[..])?;
        }
        header.set_cksum();
        self.append(header, io::empty())
    }

    /// Adds a file on the local filesystem to this archive under `name`.
    ///
    /// # Errors
    ///
    /// Filesystem or I/O errors.
    pub fn append_path_with_name<P: AsRef<Path>, N: AsRef<Path>>(
        &mut self,
        path: P,
        name: N,
    ) -> io::Result<()> {
        let path = path.as_ref();
        let meta = if self.follow {
            fs::metadata(path)?
        } else {
            fs::symlink_metadata(path)?
        };
        let mut header = Header::new_gnu();
        header.set_metadata(&meta);
        if self.mode == HeaderMode::Deterministic {
            header.set_mtime(1_153_704_088);
            header.set_uid(0);
            header.set_gid(0);
            let mode = if meta.is_dir() { 0o755 } else { 0o644 };
            header.set_mode(mode);
        }
        if meta.is_dir() {
            self.append_data(&mut header, name, io::empty())
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            self.append_link(&mut header, name, target)
        } else {
            let file = fs::File::open(path)?;
            self.append_data(&mut header, name, file)
        }
    }

    /// Adds a file on the local filesystem to this archive (the path is used
    /// as the archive name).
    ///
    /// # Errors
    ///
    /// Filesystem or I/O errors.
    pub fn append_path<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.append_path_with_name(path, path)
    }

    /// Adds an open file to this archive under `path`.
    ///
    /// # Errors
    ///
    /// Filesystem or I/O errors.
    pub fn append_file<P: AsRef<Path>>(&mut self, path: P, file: &mut fs::File) -> io::Result<()> {
        let meta = file.metadata()?;
        let mut header = Header::new_gnu();
        header.set_metadata(&meta);
        self.append_data(&mut header, path, file)
    }

    /// Adds a directory entry (not its contents) under `path`.
    ///
    /// # Errors
    ///
    /// Filesystem or I/O errors.
    pub fn append_dir<P: AsRef<Path>, Q: AsRef<Path>>(
        &mut self,
        path: P,
        src_path: Q,
    ) -> io::Result<()> {
        let meta = fs::metadata(src_path)?;
        let mut header = Header::new_gnu();
        header.set_metadata(&meta);
        self.append_data(&mut header, path, io::empty())
    }

    /// Adds a directory and all of its contents (recursively) under `path`.
    ///
    /// # Errors
    ///
    /// Filesystem or I/O errors.
    pub fn append_dir_all<P: AsRef<Path>, Q: AsRef<Path>>(
        &mut self,
        path: P,
        src_path: Q,
    ) -> io::Result<()> {
        let src = src_path.as_ref();
        let base = path.as_ref().to_path_buf();
        self.append_path_with_name(src, &base)?;
        let mut stack = vec![(src.to_path_buf(), base)];
        while let Some((dir, name)) = stack.pop() {
            let mut children: Vec<_> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let child_path = child.path();
                let child_name = name.join(child.file_name());
                let is_dir = if self.follow {
                    fs::metadata(&child_path)?.is_dir()
                } else {
                    child.file_type()?.is_dir()
                };
                self.append_path_with_name(&child_path, &child_name)?;
                if is_dir {
                    stack.push((child_path, child_name));
                }
            }
        }
        Ok(())
    }

    /// Finish writing this archive, emitting the termination sections.
    ///
    /// This function should only be called when the archive has been
    /// written entirely and if an I/O error happens the underlying object
    /// still needs to be acquired.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.obj()?.write_all(&[0; 1024])
    }
}

impl<W: Write> Drop for Builder<W> {
    fn drop(&mut self) {
        if self.obj.is_some() {
            let _ = self.finish();
        }
    }
}
