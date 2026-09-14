//! miniz's low-level `tinfl` interface: [`decompress`] over a
//! [`DecompressorOxide`].

use oxiarc_flate2_compat::{Decompress, FlushDecompress, Status};

use super::TINFLStatus;

/// Flags to [`decompress`].
pub mod inflate_flags {
    /// Should we try to parse a zlib header?
    pub const TINFL_FLAG_PARSE_ZLIB_HEADER: u32 = 1;
    /// There is more input that hasn't been given to the decompressor yet.
    pub const TINFL_FLAG_HAS_MORE_INPUT: u32 = 2;
    /// The output buffer should not wrap around.
    pub const TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF: u32 = 4;
    /// Calculate the Adler-32 checksum of the output (always done for zlib
    /// streams here).
    pub const TINFL_FLAG_COMPUTE_ADLER32: u32 = 8;
    /// Ignore the Adler-32 checksum of a zlib stream.
    pub const TINFL_FLAG_IGNORE_ADLER32: u32 = 64;
}

use inflate_flags::*;

/// Size of the history window miniz uses; exported for callers that size
/// wrapping buffers from it.
pub const TINFL_LZ_DICT_SIZE: usize = 32_768;

/// Main decompression struct.
#[derive(Debug)]
pub struct DecompressorOxide {
    inner: Option<Decompress>,
    /// Adler-32 of the decoded output, once a zlib stream completed.
    adler: Option<u32>,
    /// Running Adler-32 of everything decoded so far.
    running: u32,
    finished: bool,
}

impl Default for DecompressorOxide {
    fn default() -> Self {
        DecompressorOxide {
            inner: None,
            adler: None,
            running: crate::MZ_ADLER32_INIT,
            finished: false,
        }
    }
}

impl DecompressorOxide {
    /// Create a new tinfl decompressor.
    pub fn new() -> DecompressorOxide {
        DecompressorOxide::default()
    }

    /// Set the current state to `Start`, as for a new stream.
    pub fn init(&mut self) {
        *self = DecompressorOxide::default();
    }

    /// Returns the adler32 checksum of the currently decompressed data.
    /// Only available for completed zlib streams.
    pub fn adler32(&self) -> Option<u32> {
        self.adler
    }

    /// Returns the adler32 that was read from the zlib header, if it
    /// exists (completed zlib streams only).
    pub fn adler32_header(&self) -> Option<u32> {
        self.adler
    }
}

/// Main decompression function. Keeps decompressing data from `in_buf`
/// until `in_buf` is empty, `out` is full, the end of the deflate stream is
/// hit, or there is an error in the deflate stream.
///
/// Returns `(status, bytes_read_from_in_buf, bytes_written_to_out)`. Output
/// is written at `out[out_pos..]`.
///
/// With `TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF` unset, `out.len()` must
/// be a power of two (as in miniz) and the caller wraps `out_pos` to zero
/// when the buffer is full.
pub fn decompress(
    r: &mut DecompressorOxide,
    in_buf: &[u8],
    out: &mut [u8],
    out_pos: usize,
    flags: u32,
) -> (TINFLStatus, usize, usize) {
    let non_wrapping = flags & TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF != 0;
    if out_pos > out.len() || (!non_wrapping && !out.len().is_power_of_two()) {
        return (TINFLStatus::BadParam, 0, 0);
    }
    if r.finished {
        return (TINFLStatus::Done, 0, 0);
    }
    let zlib = flags & TINFL_FLAG_PARSE_ZLIB_HEADER != 0;
    let inner = r.inner.get_or_insert_with(|| {
        let mut d = Decompress::new(zlib);
        d.set_checksum_verification(flags & TINFL_FLAG_IGNORE_ADLER32 == 0);
        d
    });
    let more_input = flags & TINFL_FLAG_HAS_MORE_INPUT != 0;
    let before_in = inner.total_in();
    let before_out = inner.total_out();
    let flush = if more_input {
        FlushDecompress::None
    } else {
        FlushDecompress::Finish
    };
    let result = inner.decompress(in_buf, &mut out[out_pos..], flush);
    let read = (inner.total_in() - before_in) as usize;
    let written = (inner.total_out() - before_out) as usize;
    r.running = crate::mz_adler32_oxide(r.running, &out[out_pos..out_pos + written]);
    match result {
        Ok(Status::StreamEnd) => {
            r.finished = true;
            if zlib {
                r.adler = Some(r.running);
            }
            (TINFLStatus::Done, read, written)
        }
        Ok(_) => {
            if out_pos + written == out.len() {
                (TINFLStatus::HasMoreOutput, read, written)
            } else if more_input {
                (TINFLStatus::NeedsMoreInput, read, written)
            } else {
                (TINFLStatus::FailedCannotMakeProgress, read, written)
            }
        }
        Err(e) => {
            let status = if e.message() == Some("invalid adler32 checksum") {
                TINFLStatus::Adler32Mismatch
            } else {
                TINFLStatus::Failed
            };
            (status, read, written)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtrace_style_one_shot() {
        // backtrace's decompress_zlib: whole input, exact-size output.
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let packed = crate::deflate::compress_to_vec_zlib(&data, 6);
        let mut out = vec![0u8; data.len()];
        let (status, read, written) = decompress(
            &mut DecompressorOxide::new(),
            &packed,
            &mut out,
            0,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_PARSE_ZLIB_HEADER,
        );
        assert_eq!(status, TINFLStatus::Done);
        assert_eq!(read, packed.len());
        assert_eq!(written, data.len());
        assert_eq!(out, data);
    }

    #[test]
    fn wrapping_buffer_streaming() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 253) as u8).collect();
        let packed = crate::deflate::compress_to_vec(&data, 9);
        let mut dict = vec![0u8; TINFL_LZ_DICT_SIZE];
        let mut r = DecompressorOxide::new();
        let mut decoded = Vec::new();
        let mut in_pos = 0;
        let mut out_pos = 0;
        loop {
            let end = (in_pos + 1000).min(packed.len());
            let flags = if end < packed.len() {
                TINFL_FLAG_HAS_MORE_INPUT
            } else {
                0
            };
            let (status, read, written) =
                decompress(&mut r, &packed[in_pos..end], &mut dict, out_pos, flags);
            decoded.extend_from_slice(&dict[out_pos..out_pos + written]);
            in_pos += read;
            out_pos = (out_pos + written) & (dict.len() - 1);
            match status {
                TINFLStatus::Done => break,
                TINFLStatus::NeedsMoreInput | TINFLStatus::HasMoreOutput => {}
                other => panic!("unexpected status {other:?}"),
            }
        }
        assert_eq!(decoded, data);
    }

    #[test]
    fn truncated_without_more_input_cannot_progress() {
        let packed = crate::deflate::compress_to_vec_zlib(&[7u8; 1000], 6);
        let mut out = vec![0u8; 2000];
        let (status, _, _) = decompress(
            &mut DecompressorOxide::new(),
            &packed[..packed.len() - 3],
            &mut out,
            0,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_PARSE_ZLIB_HEADER,
        );
        assert_eq!(status, TINFLStatus::FailedCannotMakeProgress);
    }

    #[test]
    fn bad_adler_reported() {
        let mut packed = crate::deflate::compress_to_vec_zlib(b"abcdefabcdef", 6);
        let n = packed.len();
        packed[n - 1] ^= 1;
        let mut out = vec![0u8; 64];
        let (status, _, _) = decompress(
            &mut DecompressorOxide::new(),
            &packed,
            &mut out,
            0,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_PARSE_ZLIB_HEADER,
        );
        assert_eq!(status, TINFLStatus::Adler32Mismatch);
        let (status, _, _) = decompress(
            &mut DecompressorOxide::new(),
            &packed,
            &mut out,
            0,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
                | TINFL_FLAG_PARSE_ZLIB_HEADER
                | TINFL_FLAG_IGNORE_ADLER32,
        );
        assert_eq!(status, TINFLStatus::Done);
    }
}
