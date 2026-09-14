//! CRC-32 (the gzip polynomial) with a running byte count, plus `Read` /
//! `Write` adapters that checksum everything passing through them.

use std::io::{self, BufRead, Read, Write};

use oxiarc_core::Crc32;

/// The CRC calculated by a [`CrcReader`].
#[derive(Debug, Default, Clone)]
pub struct Crc {
    amt: u32,
    hasher: Crc32,
}

impl Crc {
    /// Create a new CRC.
    pub fn new() -> Self {
        Crc::default()
    }

    /// Returns the current crc32 checksum.
    pub fn sum(&self) -> u32 {
        self.hasher.value()
    }

    /// The number of bytes that have been used to calculate the CRC.
    /// This value is only accurate if the amount is lower than 2^32.
    pub fn amount(&self) -> u32 {
        self.amt
    }

    /// Update the CRC with the bytes in `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.amt = self.amt.wrapping_add(data.len() as u32);
        self.hasher.update(data);
    }

    /// Reset the CRC.
    pub fn reset(&mut self) {
        self.amt = 0;
        self.hasher.reset();
    }

    /// Combine the CRC with the CRC for the subsequent block of bytes, as
    /// zlib's `crc32_combine` does.
    pub fn combine(&mut self, additional_crc: &Self) {
        let combined = Crc32::combine(
            self.sum(),
            additional_crc.sum(),
            u64::from(additional_crc.amt),
        );
        self.amt = self.amt.wrapping_add(additional_crc.amt);
        self.hasher = Crc32::from_value(combined);
    }
}

/// A wrapper around a [`Read`] that calculates the CRC.
#[derive(Debug)]
pub struct CrcReader<R> {
    inner: R,
    crc: Crc,
}

impl<R: Read> CrcReader<R> {
    /// Create a new `CrcReader`.
    pub fn new(r: R) -> CrcReader<R> {
        CrcReader {
            inner: r,
            crc: Crc::new(),
        }
    }
}

impl<R> CrcReader<R> {
    /// Get the Crc for this `CrcReader`.
    pub fn crc(&self) -> &Crc {
        &self.crc
    }

    /// Get the reader that is wrapped by this `CrcReader`.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Get the reader that is wrapped by this `CrcReader` by reference.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Get a mutable reference to the reader that is wrapped by this
    /// `CrcReader`.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Reset the Crc in this `CrcReader`.
    pub fn reset(&mut self) {
        self.crc.reset();
    }
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let amt = self.inner.read(into)?;
        self.crc.update(&into[..amt]);
        Ok(amt)
    }
}

impl<R: BufRead> BufRead for CrcReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        if let Ok(data) = self.inner.fill_buf() {
            let n = amt.min(data.len());
            self.crc.update(&data[..n]);
        }
        self.inner.consume(amt);
    }
}

/// A wrapper around a [`Write`] that calculates the CRC.
#[derive(Debug)]
pub struct CrcWriter<W> {
    inner: W,
    crc: Crc,
}

impl<W> CrcWriter<W> {
    /// Get the Crc for this `CrcWriter`.
    pub fn crc(&self) -> &Crc {
        &self.crc
    }

    /// Get the writer that is wrapped by this `CrcWriter`.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Get the writer that is wrapped by this `CrcWriter` by reference.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Get a mutable reference to the writer that is wrapped by this
    /// `CrcWriter`.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Reset the Crc in this `CrcWriter`.
    pub fn reset(&mut self) {
        self.crc.reset();
    }
}

impl<W: Write> CrcWriter<W> {
    /// Create a new `CrcWriter`.
    pub fn new(w: W) -> CrcWriter<W> {
        CrcWriter {
            inner: w,
            crc: Crc::new(),
        }
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let amt = self.inner.write(buf)?;
        self.crc.update(&buf[..amt]);
        Ok(amt)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        let mut crc = Crc::new();
        crc.update(b"123456789");
        assert_eq!(crc.sum(), 0xcbf4_3926);
        assert_eq!(crc.amount(), 9);
    }

    #[test]
    fn combine_matches_concatenation() {
        let a = b"The quick brown fox ";
        let b = b"jumps over the lazy dog";
        let mut ca = Crc::new();
        ca.update(a);
        let mut cb = Crc::new();
        cb.update(b);
        ca.combine(&cb);
        let mut whole = Crc::new();
        whole.update(a);
        whole.update(b);
        assert_eq!(ca.sum(), whole.sum());
        assert_eq!(ca.amount(), whole.amount());
        // The combined state keeps running correctly.
        ca.update(b"!");
        whole.update(b"!");
        assert_eq!(ca.sum(), whole.sum());
    }
}
