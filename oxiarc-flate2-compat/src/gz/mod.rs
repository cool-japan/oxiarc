//! gzip (RFC 1952) streams: the header type, the builder, and the
//! `bufread` / `read` / `write` encoders and decoders.

use std::ffi::CString;
use std::io::{self, BufRead, Error, ErrorKind};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oxiarc_core::Crc32;

use crate::Compression;

pub mod bufread;
pub mod read;
pub mod write;

const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;
const FRESERVED: u8 = (1 << 5) | (1 << 6) | (1 << 7);

/// Longest `FNAME` / `FCOMMENT` accepted, as in flate2.
const MAX_HEADER_BUF: usize = 65535;

/// A structure representing the header of a gzip stream.
///
/// The header can contain metadata about the file that was compressed, if
/// present.
#[derive(PartialEq, Eq, Clone, Debug, Default)]
pub struct GzHeader {
    extra: Option<Vec<u8>>,
    filename: Option<Vec<u8>>,
    comment: Option<Vec<u8>>,
    operating_system: u8,
    mtime: u32,
}

impl GzHeader {
    /// Returns the `filename` field of this gzip stream's header, if present.
    pub fn filename(&self) -> Option<&[u8]> {
        self.filename.as_deref()
    }

    /// Returns the `extra` field of this gzip stream's header, if present.
    pub fn extra(&self) -> Option<&[u8]> {
        self.extra.as_deref()
    }

    /// Returns the `comment` field of this gzip stream's header, if present.
    pub fn comment(&self) -> Option<&[u8]> {
        self.comment.as_deref()
    }

    /// Returns the `operating_system` field of this gzip stream's header.
    pub fn operating_system(&self) -> u8 {
        self.operating_system
    }

    /// This gives the most recent modification time of the original file
    /// being compressed, in seconds since the Unix epoch; `0` means "not
    /// available".
    pub fn mtime(&self) -> u32 {
        self.mtime
    }

    /// Returns the most recent modification time represented by a date-time
    /// type, or `None` when the header carries no time.
    pub fn mtime_as_datetime(&self) -> Option<SystemTime> {
        if self.mtime == 0 {
            None
        } else {
            UNIX_EPOCH.checked_add(Duration::from_secs(u64::from(self.mtime)))
        }
    }
}

fn bad_header() -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid gzip header")
}

pub(crate) fn corrupt() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "corrupt gzip stream does not have a matching checksum",
    )
}

/// Parse a complete gzip member header from the front of `data`.
///
/// `Ok(None)` means `data` is a valid but incomplete prefix of a header;
/// `Ok(Some((header, len)))` means the header is `data[..len]`.
pub(crate) fn parse_header(data: &[u8]) -> io::Result<Option<(GzHeader, usize)>> {
    if data.len() < 10 {
        // Validate what we have so a non-gzip stream fails early.
        if (!data.is_empty() && data[0] != 0x1f)
            || (data.len() > 1 && data[1] != 0x8b)
            || (data.len() > 2 && data[2] != 8)
            || (data.len() > 3 && data[3] & FRESERVED != 0)
        {
            return Err(bad_header());
        }
        return Ok(None);
    }
    if data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err(bad_header());
    }
    let flags = data[3];
    if flags & FRESERVED != 0 {
        return Err(bad_header());
    }
    let mut header = GzHeader {
        mtime: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        operating_system: data[9],
        ..GzHeader::default()
    };
    let mut pos = 10usize;
    if flags & FEXTRA != 0 {
        let Some(len_bytes) = data.get(pos..pos + 2) else {
            return Ok(None);
        };
        let xlen = usize::from(u16::from_le_bytes([len_bytes[0], len_bytes[1]]));
        pos += 2;
        let Some(extra) = data.get(pos..pos + xlen) else {
            return Ok(None);
        };
        header.extra = Some(extra.to_vec());
        pos += xlen;
    }
    for (flag, is_name) in [(FNAME, true), (FCOMMENT, false)] {
        if flags & flag == 0 {
            continue;
        }
        let rest = &data[pos..];
        match rest.iter().position(|&b| b == 0) {
            Some(nul) => {
                if nul > MAX_HEADER_BUF {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "gzip header field too long",
                    ));
                }
                let field = rest[..nul].to_vec();
                if is_name {
                    header.filename = Some(field);
                } else {
                    header.comment = Some(field);
                }
                pos += nul + 1;
            }
            None => {
                if rest.len() > MAX_HEADER_BUF {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "gzip header field too long",
                    ));
                }
                return Ok(None);
            }
        }
    }
    if flags & FHCRC != 0 {
        let Some(crc_bytes) = data.get(pos..pos + 2) else {
            return Ok(None);
        };
        let stored = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        let computed = Crc32::compute(&data[..pos]) as u16;
        if stored != computed {
            return Err(corrupt());
        }
        pos += 2;
    }
    Ok(Some((header, pos)))
}

/// Resumable gzip header reader over a `BufRead`.
///
/// Bytes are only consumed from the reader once they are known to belong to
/// the header, so the DEFLATE payload that follows is left in place. A
/// `WouldBlock` from the reader leaves the partial header buffered here, and
/// the next call resumes where this one stopped.
#[derive(Debug, Default)]
pub(crate) struct HeaderReader {
    seen: Vec<u8>,
}

impl HeaderReader {
    pub(crate) fn new() -> Self {
        HeaderReader { seen: Vec::new() }
    }

    pub(crate) fn parse<R: BufRead>(&mut self, r: &mut R) -> io::Result<GzHeader> {
        loop {
            let buf = match r.fill_buf() {
                Ok(buf) => buf,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if buf.is_empty() {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            let before = self.seen.len();
            self.seen.extend_from_slice(buf);
            match parse_header(&self.seen)? {
                Some((header, used)) => {
                    r.consume(used - before);
                    self.seen.clear();
                    return Ok(header);
                }
                None => {
                    let n = self.seen.len() - before;
                    r.consume(n);
                }
            }
        }
    }
}

/// A builder structure to create a new gzip Encoder.
///
/// This structure controls header configuration options such as the
/// filename.
#[derive(Debug, Default)]
pub struct GzBuilder {
    extra: Option<Vec<u8>>,
    filename: Option<CString>,
    comment: Option<CString>,
    operating_system: Option<u8>,
    mtime: u32,
}

impl GzBuilder {
    /// Create a new blank builder with no header by default.
    pub fn new() -> GzBuilder {
        GzBuilder::default()
    }

    /// Configure the `mtime` field in the gzip header.
    pub fn mtime(mut self, mtime: u32) -> GzBuilder {
        self.mtime = mtime;
        self
    }

    /// Configure the `operating_system` field in the gzip header.
    pub fn operating_system(mut self, os: u8) -> GzBuilder {
        self.operating_system = Some(os);
        self
    }

    /// Configure the `extra` field in the gzip header.
    pub fn extra<T: Into<Vec<u8>>>(mut self, extra: T) -> GzBuilder {
        self.extra = Some(extra.into());
        self
    }

    /// Configure the `filename` field in the gzip header.
    ///
    /// # Panics
    ///
    /// Panics if the `filename` slice contains a zero, as flate2 does.
    pub fn filename<T: Into<Vec<u8>>>(mut self, filename: T) -> GzBuilder {
        self.filename = CString::new(filename.into()).ok();
        assert!(self.filename.is_some(), "filename contains a NUL byte");
        self
    }

    /// Configure the `comment` field in the gzip header.
    ///
    /// # Panics
    ///
    /// Panics if the `comment` slice contains a zero, as flate2 does.
    pub fn comment<T: Into<Vec<u8>>>(mut self, comment: T) -> GzBuilder {
        self.comment = CString::new(comment.into()).ok();
        assert!(self.comment.is_some(), "comment contains a NUL byte");
        self
    }

    /// Consume this builder, creating a writer encoder in the process.
    pub fn write<W: io::Write>(self, w: W, lvl: Compression) -> write::GzEncoder<W> {
        write::gz_encoder(self.into_header(lvl), w, lvl)
    }

    /// Consume this builder, creating a reader encoder in the process.
    pub fn read<R: io::Read>(self, r: R, lvl: Compression) -> read::GzEncoder<R> {
        read::gz_encoder(self.buf_read(crate::bufreader::BufReader::new(r), lvl))
    }

    /// Consume this builder, creating a buffered reader encoder in the
    /// process.
    pub fn buf_read<R>(self, r: R, lvl: Compression) -> bufread::GzEncoder<R>
    where
        R: BufRead,
    {
        bufread::gz_encoder(self.into_header(lvl), r, lvl)
    }

    /// Serialise the header bytes, byte-for-byte what flate2 writes.
    pub(crate) fn into_header(self, lvl: Compression) -> Vec<u8> {
        let GzBuilder {
            extra,
            filename,
            comment,
            operating_system,
            mtime,
        } = self;
        let mut flg = 0u8;
        let mut header = vec![0u8; 10];
        if let Some(v) = extra {
            flg |= FEXTRA;
            header.extend_from_slice(&(v.len() as u16).to_le_bytes());
            header.extend_from_slice(&v);
        }
        if let Some(filename) = filename {
            flg |= FNAME;
            header.extend_from_slice(filename.as_bytes_with_nul());
        }
        if let Some(comment) = comment {
            flg |= FCOMMENT;
            header.extend_from_slice(comment.as_bytes_with_nul());
        }
        header[0] = 0x1f;
        header[1] = 0x8b;
        header[2] = 8;
        header[3] = flg;
        header[4..8].copy_from_slice(&mtime.to_le_bytes());
        header[8] = if lvl.level() >= Compression::best().level() {
            2
        } else if lvl.level() <= Compression::fast().level() {
            4
        } else {
            0
        };
        header[9] = operating_system.unwrap_or(255);
        header
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_all_fields() {
        let bytes = GzBuilder::new()
            .filename("name.txt")
            .comment("hello")
            .extra(vec![1, 2, 3])
            .mtime(1234)
            .operating_system(3)
            .into_header(Compression::default());
        let parsed = parse_header(&bytes).ok().flatten();
        let Some((header, used)) = parsed else {
            panic!("header did not parse");
        };
        assert_eq!(used, bytes.len());
        assert_eq!(header.filename(), Some(&b"name.txt"[..]));
        assert_eq!(header.comment(), Some(&b"hello"[..]));
        assert_eq!(header.extra(), Some(&[1u8, 2, 3][..]));
        assert_eq!(header.mtime(), 1234);
        assert_eq!(header.operating_system(), 3);
        for cut in 0..bytes.len() {
            assert!(matches!(parse_header(&bytes[..cut]), Ok(None)));
        }
    }

    #[test]
    fn rejects_non_gzip() {
        assert!(parse_header(b"PK\x03\x04").is_err());
        assert!(parse_header(&[0x1f, 0x8b, 7]).is_err());
    }

    #[test]
    fn header_crc_is_verified() {
        let mut bytes = vec![0x1f, 0x8b, 8, FHCRC, 0, 0, 0, 0, 0, 255];
        let crc = Crc32::compute(&bytes) as u16;
        bytes.extend_from_slice(&crc.to_le_bytes());
        assert!(matches!(parse_header(&bytes), Ok(Some((_, 12)))));
        bytes[10] ^= 1;
        assert!(parse_header(&bytes).is_err());
    }
}
