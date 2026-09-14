//! The 512-byte tar header block, with the tar crate's field encodings
//! (zero-padded octal terminated by NUL, base-256 for large values).

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::EntryType;

/// Size of a tar block.
pub(crate) const BLOCK_SIZE: usize = 512;

const NAME: std::ops::Range<usize> = 0..100;
const MODE: std::ops::Range<usize> = 100..108;
const UID: std::ops::Range<usize> = 108..116;
const GID: std::ops::Range<usize> = 116..124;
const SIZE: std::ops::Range<usize> = 124..136;
const MTIME: std::ops::Range<usize> = 136..148;
const CKSUM: std::ops::Range<usize> = 148..156;
const TYPEFLAG: usize = 156;
const LINKNAME: std::ops::Range<usize> = 157..257;
const MAGIC: std::ops::Range<usize> = 257..263;
const VERSION: std::ops::Range<usize> = 263..265;
const UNAME: std::ops::Range<usize> = 265..297;
const GNAME: std::ops::Range<usize> = 297..329;
const DEVMAJOR: std::ops::Range<usize> = 329..337;
const DEVMINOR: std::ops::Range<usize> = 337..345;
const PREFIX: std::ops::Range<usize> = 345..500;

pub(crate) fn other(msg: &str) -> io::Error {
    io::Error::other(msg.to_string())
}

/// Representation of the header of an entry in an archive.
#[derive(Clone)]
#[repr(C)]
pub struct Header {
    bytes: [u8; BLOCK_SIZE],
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Header")
            .field("path", &String::from_utf8_lossy(&self.path_bytes()))
            .field("size", &self.entry_size().ok())
            .field("mode", &self.mode().ok())
            .field("entry_type", &self.entry_type())
            .field("cksum", &self.cksum().ok())
            .finish()
    }
}

impl Default for Header {
    fn default() -> Header {
        Header::new_gnu()
    }
}

/// Zero-padded octal with a trailing NUL, exactly as the tar crate writes
/// numeric fields.
fn octal_into(dst: &mut [u8], val: u64) {
    let digits = format!("{val:o}");
    let digits = digits.as_bytes();
    let len = dst.len();
    dst[len - 1] = 0;
    let room = len - 1;
    for (i, slot) in dst[..room].iter_mut().enumerate() {
        let from_right = room - 1 - i;
        *slot = if from_right < digits.len() {
            digits[digits.len() - 1 - from_right]
        } else {
            b'0'
        };
    }
}

/// Base-256 ("GNU numeric extension") for values that do not fit octal.
fn numeric_extended_into(dst: &mut [u8], src: u64) {
    let len = dst.len();
    for slot in dst[..len - 8].iter_mut() {
        *slot = 0;
    }
    for (i, slot) in dst[len - 8..].iter_mut().enumerate() {
        *slot = (src >> (8 * (7 - i))) as u8;
    }
    dst[0] |= 0x80;
}

fn num_field_wrapper_into(dst: &mut [u8], src: u64) {
    if src >= 8_589_934_592 || (src >= 2_097_152 && dst.len() == 8) {
        numeric_extended_into(dst, src);
    } else {
        octal_into(dst, src);
    }
}

fn truncate(slice: &[u8]) -> &[u8] {
    match slice.iter().position(|b| *b == 0) {
        Some(i) => &slice[..i],
        None => slice,
    }
}

fn octal_from(slice: &[u8]) -> io::Result<u64> {
    let trun = truncate(slice);
    let num = std::str::from_utf8(trun).map_err(|_| {
        other(&format!(
            "numeric field did not have utf-8 text: {}",
            String::from_utf8_lossy(trun)
        ))
    })?;
    let num = num.trim();
    if num.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(num, 8)
        .map_err(|_| other(&format!("numeric field was not a number: {num}")))
}

fn num_field_wrapper_from(src: &[u8]) -> io::Result<u64> {
    if src[0] & 0x80 != 0 {
        let skip = src.len() - 8;
        let mut dst: u64 = if src.len() == 8 {
            u64::from(src[0] ^ 0x80)
        } else {
            0
        };
        let start = if src.len() == 8 { 1 } else { skip };
        for byte in &src[start..] {
            dst = (dst << 8) | u64::from(*byte);
        }
        Ok(dst)
    } else {
        octal_from(src)
    }
}

fn copy_into(slot: &mut [u8], bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > slot.len() {
        Err(other("provided value is too long"))
    } else if bytes.contains(&0) {
        Err(other("provided value contains a nul byte"))
    } else {
        for (dst, src) in slot.iter_mut().zip(bytes.iter().chain(Some(&0))) {
            *dst = *src;
        }
        for dst in slot.iter_mut().skip(bytes.len() + 1) {
            *dst = 0;
        }
        Ok(())
    }
}

fn path2bytes(p: &Path) -> io::Result<Vec<u8>> {
    let s = p
        .to_str()
        .ok_or_else(|| other(&format!("path {} was not valid Unicode", p.display())))?;
    Ok(s.replace('\\', "/").into_bytes())
}

/// Apply the tar crate's path rules and return the bytes to store: relative,
/// no `..` (unless `allow_trailing_parent` for a truncated GNU long name),
/// `/`-separated, a trailing `/` kept.
pub(crate) fn normalize_path(
    path: &Path,
    is_link_name: bool,
    allow_trailing_parent: bool,
    allow_absolute: bool,
) -> io::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut emitted = false;
    let mut needs_slash = false;
    let count = path.components().count();
    let mut iter = path.components().peekable();
    while let Some(component) = iter.next() {
        let bytes = path2bytes(Path::new(component.as_os_str()))?;
        match (component, is_link_name, allow_absolute) {
            (Component::Prefix(..), false, false) | (Component::RootDir, false, false) => {
                return Err(other("paths in archives must be relative"));
            }
            (Component::ParentDir, false, _) if !allow_trailing_parent || iter.peek().is_some() => {
                return Err(other("paths in archives must not have `..`"));
            }
            (Component::CurDir, false, _) if count == 1 => {}
            (Component::CurDir, false, _) => continue,
            _ => {}
        }
        if needs_slash {
            out.push(b'/');
        }
        if bytes.contains(&b'/') {
            if let Component::Normal(..) = component {
                return Err(other("path component in archive cannot contain `/`"));
            }
        }
        out.extend_from_slice(&bytes);
        if bytes != b"/" {
            needs_slash = true;
        }
        emitted = true;
    }
    if !emitted {
        return Err(other("paths in archives must have at least one component"));
    }
    if path.to_string_lossy().ends_with('/') {
        out.push(b'/');
    }
    Ok(out)
}

fn bytes2path(bytes: Cow<'_, [u8]>) -> io::Result<Cow<'_, Path>> {
    match bytes {
        Cow::Borrowed(b) => {
            let s = std::str::from_utf8(b).map_err(|_| other("path was not valid utf-8"))?;
            Ok(Cow::Borrowed(Path::new(s)))
        }
        Cow::Owned(b) => {
            let s = String::from_utf8(b).map_err(|_| other("path was not valid utf-8"))?;
            Ok(Cow::Owned(PathBuf::from(s)))
        }
    }
}

impl Header {
    /// Creates a new blank GNU header.
    ///
    /// The GNU style header is the default for this library and allows
    /// various extensions such as long path names, long link names, and
    /// setting the atime/ctime metadata attributes of files.
    pub fn new_gnu() -> Header {
        let mut header = Header {
            bytes: [0; BLOCK_SIZE],
        };
        header.bytes[MAGIC].copy_from_slice(b"ustar ");
        header.bytes[VERSION].copy_from_slice(b" \0");
        header.set_mtime(0);
        header
    }

    /// Creates a new blank UStar header.
    pub fn new_ustar() -> Header {
        let mut header = Header {
            bytes: [0; BLOCK_SIZE],
        };
        header.bytes[MAGIC].copy_from_slice(b"ustar\0");
        header.bytes[VERSION].copy_from_slice(b"00");
        header.set_mtime(0);
        header
    }

    /// Creates a new blank old header.
    pub fn new_old() -> Header {
        let mut header = Header {
            bytes: [0; BLOCK_SIZE],
        };
        header.set_mtime(0);
        header
    }

    /// Build a header from a raw block.
    pub(crate) fn from_block(bytes: [u8; BLOCK_SIZE]) -> Header {
        Header { bytes }
    }

    fn is_ustar(&self) -> bool {
        &self.bytes[MAGIC] == b"ustar\0" && &self.bytes[VERSION] == b"00"
    }

    fn is_gnu(&self) -> bool {
        &self.bytes[MAGIC] == b"ustar " && &self.bytes[VERSION] == b" \0"
    }

    /// Returns a view into this header as a byte array.
    pub fn as_bytes(&self) -> &[u8; 512] {
        &self.bytes
    }

    /// Returns a view into this header as a mutable byte array.
    pub fn as_mut_bytes(&mut self) -> &mut [u8; 512] {
        &mut self.bytes
    }

    /// Returns the size of entry's data this header represents.
    ///
    /// # Errors
    ///
    /// A malformed size field.
    pub fn entry_size(&self) -> io::Result<u64> {
        num_field_wrapper_from(&self.bytes[SIZE])
            .map_err(|err| io::Error::new(err.kind(), format!("{err} when getting size")))
    }

    /// Returns the file size this header represents.
    ///
    /// # Errors
    ///
    /// A malformed size field.
    pub fn size(&self) -> io::Result<u64> {
        self.entry_size()
    }

    /// Encodes the `size` argument into the size field of this header.
    pub fn set_size(&mut self, size: u64) {
        num_field_wrapper_into(&mut self.bytes[SIZE], size);
    }

    /// Returns the raw path name stored in this header.
    ///
    /// # Errors
    ///
    /// A path that is not valid UTF-8.
    pub fn path(&self) -> io::Result<Cow<'_, Path>> {
        bytes2path(self.path_bytes())
    }

    /// Returns the pathname stored in this header as a byte array.
    pub fn path_bytes(&self) -> Cow<'_, [u8]> {
        let name = truncate(&self.bytes[NAME]);
        if self.is_ustar() {
            let prefix = truncate(&self.bytes[PREFIX]);
            if !prefix.is_empty() {
                let mut bytes = prefix.to_vec();
                bytes.push(b'/');
                bytes.extend_from_slice(name);
                return Cow::Owned(bytes);
            }
        }
        Cow::Borrowed(name)
    }

    /// Sets the path name for this header.
    ///
    /// # Errors
    ///
    /// An absolute path, a `..` component, or a path too long for the
    /// header format (use `Builder::append_data` for GNU long names).
    pub fn set_path<P: AsRef<Path>>(&mut self, p: P) -> io::Result<()> {
        self.set_path_inner(p.as_ref(), false, false)
    }

    /// Like [`Header::set_path`] but allows absolute paths.
    ///
    /// # Errors
    ///
    /// See [`Header::set_path`].
    pub fn set_path_absolute<P: AsRef<Path>>(&mut self, p: P) -> io::Result<()> {
        self.set_path_inner(p.as_ref(), false, true)
    }

    pub(crate) fn set_truncated_path_for_gnu_header(
        &mut self,
        p: &Path,
        allow_absolute: bool,
    ) -> io::Result<()> {
        self.set_path_inner(p, true, allow_absolute)
    }

    fn set_path_inner(
        &mut self,
        path: &Path,
        is_truncated_gnu_long_path: bool,
        allow_absolute: bool,
    ) -> io::Result<()> {
        let bytes = normalize_path(path, false, is_truncated_gnu_long_path, allow_absolute)?;
        if self.is_ustar() {
            if bytes.len() <= NAME.len() {
                copy_into(&mut self.bytes[NAME], &bytes)?;
                copy_into(&mut self.bytes[PREFIX], &[])?;
                return Ok(());
            }
            // Split at a '/' so that prefix <= 155 and name <= 100.
            let split = bytes
                .iter()
                .enumerate()
                .filter(|(i, b)| **b == b'/' && *i <= PREFIX.len())
                .map(|(i, _)| i)
                .find(|&i| bytes.len() - i - 1 <= NAME.len() && i > 0)
                .ok_or_else(|| other("path cannot be split to be inserted into archive"))?;
            copy_into(&mut self.bytes[PREFIX], &bytes[..split])?;
            copy_into(&mut self.bytes[NAME], &bytes[split + 1..])?;
            Ok(())
        } else {
            copy_into(&mut self.bytes[NAME], &bytes).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("{e} when setting path for {}", path.display()),
                )
            })
        }
    }

    /// Returns the link name stored in this header, if any is found.
    ///
    /// # Errors
    ///
    /// A link name that is not valid UTF-8.
    pub fn link_name(&self) -> io::Result<Option<Cow<'_, Path>>> {
        match self.link_name_bytes() {
            Some(bytes) => bytes2path(bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Returns the link name stored in this header as a byte array, if any.
    pub fn link_name_bytes(&self) -> Option<Cow<'_, [u8]>> {
        let bytes = truncate(&self.bytes[LINKNAME]);
        if bytes.is_empty() {
            None
        } else {
            Some(Cow::Borrowed(bytes))
        }
    }

    /// Sets the link name for this header.
    ///
    /// # Errors
    ///
    /// A link name too long for the field.
    pub fn set_link_name<P: AsRef<Path>>(&mut self, p: P) -> io::Result<()> {
        let bytes = normalize_path(p.as_ref(), true, false, true)?;
        copy_into(&mut self.bytes[LINKNAME], &bytes)
    }

    /// Returns the mode bits for this file.
    ///
    /// # Errors
    ///
    /// A malformed mode field.
    pub fn mode(&self) -> io::Result<u32> {
        octal_from(&self.bytes[MODE]).map(|u| u as u32)
    }

    /// Encodes the `mode` provided into this header.
    pub fn set_mode(&mut self, mode: u32) {
        octal_into(&mut self.bytes[MODE], u64::from(mode));
    }

    /// Returns the value of the owner's user ID field.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn uid(&self) -> io::Result<u64> {
        num_field_wrapper_from(&self.bytes[UID])
    }

    /// Encodes the `uid` provided into this header.
    pub fn set_uid(&mut self, uid: u64) {
        num_field_wrapper_into(&mut self.bytes[UID], uid);
    }

    /// Returns the value of the group's user ID field.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn gid(&self) -> io::Result<u64> {
        num_field_wrapper_from(&self.bytes[GID])
    }

    /// Encodes the `gid` provided into this header.
    pub fn set_gid(&mut self, gid: u64) {
        num_field_wrapper_into(&mut self.bytes[GID], gid);
    }

    /// Returns the last modification time in Unix time format.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn mtime(&self) -> io::Result<u64> {
        num_field_wrapper_from(&self.bytes[MTIME])
    }

    /// Encodes the `mtime` provided into this header.
    pub fn set_mtime(&mut self, mtime: u64) {
        num_field_wrapper_into(&mut self.bytes[MTIME], mtime);
    }

    /// Return the user name of the owner of this file (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// A name that is not valid UTF-8.
    pub fn username(&self) -> Result<Option<&str>, std::str::Utf8Error> {
        match self.username_bytes() {
            Some(bytes) => std::str::from_utf8(bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Returns the user name of the owner of this file as bytes.
    pub fn username_bytes(&self) -> Option<&[u8]> {
        if self.is_ustar() || self.is_gnu() {
            Some(truncate(&self.bytes[UNAME]))
        } else {
            None
        }
    }

    /// Sets the username inside this header (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// An old-style header, or a name over 32 bytes.
    pub fn set_username(&mut self, name: &str) -> io::Result<()> {
        if self.is_ustar() || self.is_gnu() {
            copy_into(&mut self.bytes[UNAME], name.as_bytes())
        } else {
            Err(other("not a ustar or gnu archive, cannot set username"))
        }
    }

    /// Return the group name of the owner of this file (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// A name that is not valid UTF-8.
    pub fn groupname(&self) -> Result<Option<&str>, std::str::Utf8Error> {
        match self.groupname_bytes() {
            Some(bytes) => std::str::from_utf8(bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Returns the group name of the owner of this file as bytes.
    pub fn groupname_bytes(&self) -> Option<&[u8]> {
        if self.is_ustar() || self.is_gnu() {
            Some(truncate(&self.bytes[GNAME]))
        } else {
            None
        }
    }

    /// Sets the group name inside this header (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// An old-style header, or a name over 32 bytes.
    pub fn set_groupname(&mut self, name: &str) -> io::Result<()> {
        if self.is_ustar() || self.is_gnu() {
            copy_into(&mut self.bytes[GNAME], name.as_bytes())
        } else {
            Err(other("not a ustar or gnu archive, cannot set groupname"))
        }
    }

    /// Returns the device major number, if present.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn device_major(&self) -> io::Result<Option<u32>> {
        if self.is_ustar() || self.is_gnu() {
            octal_from(&self.bytes[DEVMAJOR]).map(|u| Some(u as u32))
        } else {
            Ok(None)
        }
    }

    /// Encodes the device major number (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// An old-style header.
    pub fn set_device_major(&mut self, major: u32) -> io::Result<()> {
        if self.is_ustar() || self.is_gnu() {
            octal_into(&mut self.bytes[DEVMAJOR], u64::from(major));
            Ok(())
        } else {
            Err(other("not a ustar or gnu archive, cannot set dev_major"))
        }
    }

    /// Returns the device minor number, if present.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn device_minor(&self) -> io::Result<Option<u32>> {
        if self.is_ustar() || self.is_gnu() {
            octal_from(&self.bytes[DEVMINOR]).map(|u| Some(u as u32))
        } else {
            Ok(None)
        }
    }

    /// Encodes the device minor number (UStar/GNU only).
    ///
    /// # Errors
    ///
    /// An old-style header.
    pub fn set_device_minor(&mut self, minor: u32) -> io::Result<()> {
        if self.is_ustar() || self.is_gnu() {
            octal_into(&mut self.bytes[DEVMINOR], u64::from(minor));
            Ok(())
        } else {
            Err(other("not a ustar or gnu archive, cannot set dev_minor"))
        }
    }

    /// Returns the type of file described by this header.
    pub fn entry_type(&self) -> EntryType {
        EntryType::new(self.bytes[TYPEFLAG])
    }

    /// Sets the type of file that will be described by this header.
    pub fn set_entry_type(&mut self, ty: EntryType) {
        self.bytes[TYPEFLAG] = ty.as_byte();
    }

    /// Returns the checksum field of this header.
    ///
    /// # Errors
    ///
    /// A malformed field.
    pub fn cksum(&self) -> io::Result<u32> {
        octal_from(&self.bytes[CKSUM]).map(|u| u as u32)
    }

    /// Sets the checksum field of this header based on the current fields
    /// in this header.
    pub fn set_cksum(&mut self) {
        let cksum = self.calculate_cksum();
        octal_into(&mut self.bytes[CKSUM], u64::from(cksum));
    }

    /// Sum of the header bytes with the checksum field read as spaces
    /// (POSIX); the same computation oxiarc-archive's reader verifies.
    pub(crate) fn calculate_cksum(&self) -> u32 {
        oxiarc_archive::tar::TarHeader::compute_checksum(&self.bytes)
    }

    /// Fill this header from the given metadata: size, mode, mtime, uid,
    /// gid and entry type (`HeaderMode::Complete` semantics).
    pub fn set_metadata(&mut self, meta: &std::fs::Metadata) {
        let size = if meta.is_dir() || meta.file_type().is_symlink() {
            0
        } else {
            meta.len()
        };
        self.set_size(size);
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.set_mtime(mtime);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.set_mode(meta.mode() & 0o7777);
            self.set_uid(u64::from(meta.uid()));
            self.set_gid(u64::from(meta.gid()));
        }
        #[cfg(not(unix))]
        {
            let mode = if meta.is_dir() {
                0o755
            } else if meta.permissions().readonly() {
                0o444
            } else {
                0o644
            };
            self.set_mode(mode);
        }
        let ty = if meta.is_dir() {
            EntryType::Directory
        } else if meta.file_type().is_symlink() {
            EntryType::Symlink
        } else {
            EntryType::Regular
        };
        self.set_entry_type(ty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_fields_match_tar_crate_layout() {
        let mut h = Header::new_gnu();
        h.set_size(1234);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        assert_eq!(&h.as_bytes()[SIZE], b"00000002322\0");
        assert_eq!(&h.as_bytes()[MODE], b"0000644\0");
        assert_eq!(&h.as_bytes()[MTIME], b"14524770400\0");
        assert_eq!(&h.as_bytes()[MAGIC], b"ustar ");
        h.set_cksum();
        let stored = h.cksum().ok();
        assert_eq!(stored, Some(h.calculate_cksum()));
        // Base-256 for sizes over 8 GiB.
        h.set_size(1 << 40);
        assert_eq!(h.size().ok(), Some(1 << 40));
        assert_eq!(h.as_bytes()[SIZE.start] & 0x80, 0x80);
    }

    #[test]
    fn path_rules() {
        let mut h = Header::new_gnu();
        assert!(h.set_path("a/b/c.txt").is_ok());
        assert_eq!(h.path_bytes().as_ref(), b"a/b/c.txt");
        assert!(h.set_path("/abs").is_err());
        assert!(h.set_path("a/../b").is_err());
        assert!(h.set_path("x".repeat(101)).is_err());
        assert!(h.set_path("./x").is_ok());
        assert_eq!(h.path_bytes().as_ref(), b"x");

        let mut u = Header::new_ustar();
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(90));
        assert!(u.set_path(&long).is_ok());
        assert_eq!(u.path_bytes().as_ref(), long.as_bytes());
    }
}
