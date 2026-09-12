//! The image-data decompressor.
//!
//! Wraps [`oxiarc_deflate::WrappedInflate`] with the three things PNG needs on
//! top of RFC 1950:
//!
//! 1. the extra header conformance rules of clause 10.3 (`CM`, `CINFO`,
//!    `FDICT`), checked on the first two bytes of the **concatenated** image
//!    data before they reach the inflater;
//! 2. Adler-32 verification off by default, matching `png` 0.18's
//!    `ignore_adler32 = true`, while still consuming the trailer so framing
//!    and byte counts stay exact;
//! 3. raw-DEFLATE mode for Apple `CgBI` files, which carry no zlib header at
//!    all.

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateWrapper, TrailingPolicy, WrappedInflate};

use crate::error::{DecodingError, FormatErrorKind};

/// What one `decompress` call achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZlibProgress {
    /// Bytes taken from the input slice.
    pub consumed: usize,
    /// Bytes written into the output slice.
    pub produced: usize,
    /// Whether the compressed stream reached its end.
    pub finished: bool,
}

/// The image-data decompressor for one zlib stream.
#[derive(Debug)]
pub(crate) struct ZlibStream {
    inflate: WrappedInflate,
    verify_adler32: bool,
    raw_deflate: bool,
    header: [u8; 2],
    header_len: u8,
    header_fed: bool,
    finished: bool,
    started: bool,
}

impl ZlibStream {
    /// A decompressor for a conformant zlib-framed image data stream.
    pub(crate) fn new(verify_adler32: bool, raw_deflate: bool) -> ZlibStream {
        ZlibStream {
            inflate: Self::build(verify_adler32, raw_deflate),
            verify_adler32,
            raw_deflate,
            header: [0; 2],
            header_len: 0,
            header_fed: false,
            finished: false,
            started: false,
        }
    }

    fn build(verify_adler32: bool, raw_deflate: bool) -> WrappedInflate {
        let wrapper = if raw_deflate {
            InflateWrapper::Raw
        } else {
            InflateWrapper::Zlib
        };
        WrappedInflate::new(wrapper)
            .multi_member(false)
            .trailing_policy(TrailingPolicy::Stop)
            .strict_first_member(true)
            .verify_checksum(verify_adler32)
    }

    /// Turn Adler-32 verification on or off. Off is the default, matching the
    /// `png` crate.
    pub(crate) fn set_verify_adler32(&mut self, yes: bool) {
        if self.verify_adler32 != yes {
            self.verify_adler32 = yes;
            self.reset();
        }
    }

    /// Switch to raw DEFLATE, as Apple `CgBI` files require.
    pub(crate) fn set_raw_deflate(&mut self, yes: bool) {
        if self.raw_deflate != yes {
            self.raw_deflate = yes;
            self.reset();
        }
    }

    /// Start a fresh stream. APNG calls this at every frame boundary; the
    /// inflater's own output cap resets with it, which is why the file-level
    /// budget lives in [`crate::limits::FrameBudget`].
    pub(crate) fn reset(&mut self) {
        self.inflate = Self::build(self.verify_adler32, self.raw_deflate);
        self.header = [0; 2];
        self.header_len = 0;
        self.header_fed = false;
        self.finished = false;
        self.started = false;
    }

    /// Whether the compressed stream has ended.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Whether any input has been handed to this stream yet.
    ///
    /// Flushing a stream that never started would be an unexpected-EOF error
    /// rather than a no-op, so callers that skip image data check this first.
    pub(crate) fn has_started(&self) -> bool {
        self.started
    }

    /// Total decompressed bytes produced by the current stream.
    pub(crate) fn total_out(&self) -> u64 {
        self.inflate.total_out()
    }

    /// Feed `data` and write decompressed bytes into `out`.
    ///
    /// `finish` must be `true` only once every byte of the stream is in hand;
    /// the main `IDAT` chain therefore passes `false` until the run ends. A
    /// stream that ends mid-symbol under `finish` is an error, which is what
    /// turns a truncated file into a diagnosable failure instead of a short
    /// image.
    pub(crate) fn decompress(
        &mut self,
        data: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<ZlibProgress, DecodingError> {
        if self.finished {
            return Ok(ZlibProgress {
                consumed: 0,
                produced: 0,
                finished: true,
            });
        }
        let mut consumed = 0usize;
        let mut produced = 0usize;
        if !data.is_empty() {
            self.started = true;
        }

        if !self.raw_deflate && !self.header_fed {
            while self.header_len < 2 {
                let Some(byte) = data.get(consumed).copied() else {
                    return Ok(ZlibProgress {
                        consumed,
                        produced,
                        finished: false,
                    });
                };
                self.header[usize::from(self.header_len)] = byte;
                self.header_len += 1;
                consumed += 1;
            }
            crate::zlib::validate_zlib_header(self.header[0], self.header[1])?;
            // The two bytes were taken out of `data`, so hand them to the
            // inflater separately before the rest of the payload.
            let header = self.header;
            let progress = self.run(&header, out, false)?;
            produced += progress.produced;
            self.header_fed = true;
            if progress.consumed != 2 {
                return Err(FormatErrorKind::CorruptFlateStream {
                    err: "zlib header was not consumed".to_string(),
                }
                .into());
            }
        }

        let progress = self.run(&data[consumed..], &mut out[produced..], finish)?;
        Ok(ZlibProgress {
            consumed: consumed + progress.consumed,
            produced: produced + progress.produced,
            finished: self.finished,
        })
    }

    fn run(
        &mut self,
        data: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<ZlibProgress, DecodingError> {
        let flush = if finish {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = self.inflate.inflate(data, out, flush).map_err(|err| {
            DecodingError::from(FormatErrorKind::CorruptFlateStream {
                err: err.to_string(),
            })
        })?;
        if progress.status == InflateStatus::StreamEnd {
            self.finished = true;
        }
        Ok(ZlibProgress {
            consumed: progress.consumed,
            produced: progress.produced,
            finished: self.finished,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decompress_all(stream: &mut ZlibStream, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 64];
        let mut pos = 0usize;
        let mut guard = 0;
        while pos < data.len() || !stream.is_finished() {
            guard += 1;
            assert!(guard < 100_000, "no progress");
            let end = (pos + chunk).min(data.len());
            let finish = end == data.len();
            let progress = stream
                .decompress(&data[pos..end], &mut scratch, finish)
                .expect("decompress");
            pos += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.finished {
                break;
            }
            if progress.consumed == 0 && progress.produced == 0 && end == data.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn whole_and_byte_at_a_time_agree() {
        let payload: Vec<u8> = (0..5000u16).map(|i| (i % 251) as u8).collect();
        let compressed = oxiarc_deflate::zlib_compress(&payload, 6).expect("compress");
        for chunk in [1usize, 2, 7, 64, 4096] {
            let mut stream = ZlibStream::new(false, false);
            assert_eq!(
                decompress_all(&mut stream, &compressed, chunk),
                payload,
                "chunk size {chunk}"
            );
            assert!(stream.is_finished());
            assert_eq!(stream.total_out(), payload.len() as u64);
        }
    }

    #[test]
    fn png_header_rules_are_enforced_before_inflating() {
        // CINFO 8 (64 KiB window) with valid check bits: 0x88 0x1D.
        let mut stream = ZlibStream::new(false, false);
        let mut out = [0u8; 16];
        let err = stream
            .decompress(&[0x88, 0x1D, 0x00], &mut out, true)
            .unwrap_err();
        assert!(matches!(
            err.format_kind(),
            Some(FormatErrorKind::InvalidZlibWindowSize { cinfo: 8 })
        ));
    }

    #[test]
    fn header_validation_survives_a_split_between_the_two_bytes() {
        let mut stream = ZlibStream::new(false, false);
        let mut out = [0u8; 16];
        let progress = stream
            .decompress(&[0x88], &mut out, false)
            .expect("partial");
        assert_eq!(progress.consumed, 1);
        assert!(stream.decompress(&[0x1D], &mut out, false).is_err());
    }

    #[test]
    fn raw_deflate_mode_skips_the_header() {
        let payload = b"apple cgbi payload";
        let raw = oxiarc_deflate::deflate(payload, 6).expect("deflate");
        let mut stream = ZlibStream::new(false, true);
        assert_eq!(decompress_all(&mut stream, &raw, 3), payload);
    }

    #[test]
    fn adler32_is_ignored_by_default_but_can_be_checked() {
        let payload = b"checksum policy";
        let mut compressed = oxiarc_deflate::zlib_compress(payload, 6).expect("compress");
        let last = compressed.len() - 1;
        compressed[last] ^= 0xFF;

        let mut lenient = ZlibStream::new(false, false);
        assert_eq!(decompress_all(&mut lenient, &compressed, 4096), payload);

        let mut strict = ZlibStream::new(false, false);
        strict.set_verify_adler32(true);
        let mut out = [0u8; 64];
        assert!(strict.decompress(&compressed, &mut out, true).is_err());
    }

    #[test]
    fn truncation_under_finish_is_an_error() {
        let payload = vec![7u8; 4000];
        let compressed = oxiarc_deflate::zlib_compress(&payload, 6).expect("compress");
        let mut stream = ZlibStream::new(false, false);
        let mut out = vec![0u8; 4096];
        let cut = compressed.len() / 2;
        assert!(
            stream
                .decompress(&compressed[..cut], &mut out, true)
                .is_err()
        );
    }

    #[test]
    fn reset_starts_a_new_stream() {
        let payload = b"frame data";
        let compressed = oxiarc_deflate::zlib_compress(payload, 6).expect("compress");
        let mut stream = ZlibStream::new(false, false);
        assert_eq!(decompress_all(&mut stream, &compressed, 4096), payload);
        stream.reset();
        assert!(!stream.is_finished());
        assert_eq!(decompress_all(&mut stream, &compressed, 4096), payload);
    }
}
