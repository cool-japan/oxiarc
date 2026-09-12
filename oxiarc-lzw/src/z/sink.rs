//! Output destinations for the `.Z` decoder.
//!
//! Every sink enforces its bound *while* decoding rather than afterwards:
//! a `.Z` stream can expand by more than 1000:1 at 16 bits, so a decoder
//! that only checks the size once it has finished has already paid for the
//! bomb.

use crate::error::{LzwError, Result};

/// Somewhere decoded bytes go.
pub(crate) trait ZSink {
    /// Append `bytes`.
    ///
    /// # Errors
    ///
    /// The sink's own bound error once `bytes` would take it past its
    /// capacity. Nothing beyond the bound is ever written.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
}

/// Sink that appends to a `Vec<u8>`, refusing to pass `limit` bytes.
pub(crate) struct VecZSink<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl<'a> VecZSink<'a> {
    /// `limit` is the maximum number of bytes this decode may produce; pass
    /// `usize::MAX` for an unbounded decode.
    pub(crate) fn new(out: &'a mut Vec<u8>, limit: usize) -> Self {
        Self { out, limit }
    }
}

impl ZSink for VecZSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.out.len().saturating_add(bytes.len()) > self.limit {
            return Err(LzwError::OutputLimitExceeded { limit: self.limit });
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }
}

/// Sink that fills a caller-supplied slice and allocates nothing.
///
/// Overflowing the slice is an error rather than a silent truncation: a
/// `.Z` stream carries no uncompressed size, so a short buffer means the
/// caller's size estimate was wrong, not that the stream ended.
pub(crate) struct SliceZSink<'a> {
    dst: &'a mut [u8],
    written: usize,
}

impl<'a> SliceZSink<'a> {
    pub(crate) fn new(dst: &'a mut [u8]) -> Self {
        Self { dst, written: 0 }
    }

    /// Bytes written so far.
    pub(crate) fn written(&self) -> usize {
        self.written
    }
}

impl ZSink for SliceZSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.written.saturating_add(bytes.len());
        if end > self.dst.len() {
            return Err(LzwError::BufferTooSmall {
                available: self.dst.len(),
            });
        }
        self.dst[self.written..end].copy_from_slice(bytes);
        self.written = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vec_sink_stops_at_its_limit_without_writing_past_it() {
        let mut out = Vec::new();
        {
            let mut sink = VecZSink::new(&mut out, 4);
            sink.write(b"ab").expect("fits");
            sink.write(b"cd").expect("exactly fills");
            assert!(matches!(
                sink.write(b"e"),
                Err(LzwError::OutputLimitExceeded { limit: 4 })
            ));
        }
        assert_eq!(out, b"abcd");
    }

    #[test]
    fn the_vec_sink_rejects_an_oversized_chunk_atomically() {
        let mut out = Vec::new();
        {
            let mut sink = VecZSink::new(&mut out, 4);
            sink.write(b"ab").expect("fits");
            assert!(sink.write(b"cdef").is_err());
        }
        assert_eq!(out, b"ab", "a rejected chunk must not be partly written");
    }

    #[test]
    fn the_slice_sink_reports_a_short_buffer() {
        let mut dst = [0u8; 3];
        let mut sink = SliceZSink::new(&mut dst);
        sink.write(b"ab").expect("fits");
        assert!(matches!(
            sink.write(b"cd"),
            Err(LzwError::BufferTooSmall { available: 3 })
        ));
        assert_eq!(sink.written(), 2);
    }

    #[test]
    fn an_unbounded_vec_sink_never_errors() {
        let mut out = Vec::new();
        {
            let mut sink = VecZSink::new(&mut out, usize::MAX);
            for _ in 0..1000 {
                sink.write(&[7u8; 64]).expect("unbounded");
            }
        }
        assert_eq!(out.len(), 64_000);
    }
}
