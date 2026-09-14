//! A `BufReader` whose constructors carry no `Read` bound, so the `read`
//! adapters can offer flate2's unbounded `reset(r)` signatures.

use std::io::{self, BufRead, Read};

/// Default buffer size, matching flate2's.
pub(crate) const DEFAULT_BUF_SIZE: usize = 32 * 1024;

#[derive(Debug)]
pub(crate) struct BufReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R> BufReader<R> {
    pub(crate) fn new(inner: R) -> BufReader<R> {
        BufReader::with_buf(vec![0; DEFAULT_BUF_SIZE], inner)
    }

    pub(crate) fn with_buf(buf: Vec<u8>, inner: R) -> BufReader<R> {
        let buf = if buf.is_empty() { vec![0; 1] } else { buf };
        BufReader {
            inner,
            buf: buf.into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    pub(crate) fn get_ref(&self) -> &R {
        &self.inner
    }

    pub(crate) fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    pub(crate) fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for BufReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.cap && buf.len() >= self.buf.len() {
            return self.inner.read(buf);
        }
        let nread = {
            let mut rem = self.fill_buf()?;
            rem.read(buf)?
        };
        self.consume(nread);
        Ok(nread)
    }
}

impl<R: Read> BufRead for BufReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.cap {
            self.cap = self.inner.read(&mut self.buf)?;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.cap);
    }
}
