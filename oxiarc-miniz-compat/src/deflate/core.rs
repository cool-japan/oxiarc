//! miniz's low-level `tdefl` interface: [`compress`] and
//! [`compress_to_output`] over a [`CompressorOxide`].

use oxiarc_flate2_compat::{Compress, Compression, FlushCompress, Status};

use super::CompressionLevel;
use crate::{DataFormat, MZError, MZFlush};

/// Flags for [`CompressorOxide::new`] and
/// [`create_comp_flags_from_zip_params`].
pub mod deflate_flags {
    /// Whether to use a zlib wrapper.
    pub const TDEFL_WRITE_ZLIB_HEADER: u32 = 0x0000_1000;
    /// Should we compute the adler32 checksum.
    pub const TDEFL_COMPUTE_ADLER32: u32 = 0x0000_2000;
    /// Should we use greedy parsing (as opposed to lazy parsing).
    pub const TDEFL_GREEDY_PARSING_FLAG: u32 = 0x0000_4000;
    /// Used in miniz to skip zero-initializing hash and dict (no-op here).
    pub const TDEFL_NONDETERMINISTIC_PARSING_FLAG: u32 = 0x0000_8000;
    /// Only look for matches with a distance of 0.
    pub const TDEFL_RLE_MATCHES: u32 = 0x0001_0000;
    /// Only use matches that are at least 6 bytes long.
    pub const TDEFL_FILTER_MATCHES: u32 = 0x0002_0000;
    /// Force the compressor to only output static blocks.
    pub const TDEFL_FORCE_ALL_STATIC_BLOCKS: u32 = 0x0004_0000;
    /// Force the compressor to only output raw/uncompressed blocks.
    pub const TDEFL_FORCE_ALL_RAW_BLOCKS: u32 = 0x0008_0000;
}

use deflate_flags::*;

/// miniz's per-level hash-chain probe counts, which it stores in the low
/// 12 bits of the flags.
const NUM_PROBES: [u16; 11] = [0, 1, 6, 32, 16, 32, 128, 256, 512, 768, 1500];
const MAX_PROBES_MASK: u32 = 0xFFF;

/// Strategy setting for compression.
#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum CompressionStrategy {
    /// Don't use any of the special strategies.
    Default = 0,
    /// Only use matches that are at least 5 bytes long.
    Filtered = 1,
    /// Don't look for matches, only huffman encode the literals.
    HuffmanOnly = 2,
    /// Only look for matches with a distance of 1, i.e do run-length
    /// encoding only.
    RLE = 3,
    /// Only use static/fixed blocks.
    Fixed = 4,
}

impl From<CompressionStrategy> for i32 {
    #[inline(always)]
    fn from(value: CompressionStrategy) -> Self {
        value as i32
    }
}

/// A list of deflate flush types.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum TDEFLFlush {
    /// Normal operation.
    None = 0,
    /// Try to flush all the current data and output an empty raw block.
    Sync = 2,
    /// Same as [`TDEFLFlush::Sync`], but also forget the history.
    Full = 3,
    /// Try to flush everything and end the deflate stream.
    Finish = 4,
}

impl From<MZFlush> for TDEFLFlush {
    fn from(flush: MZFlush) -> Self {
        match flush {
            MZFlush::None => TDEFLFlush::None,
            MZFlush::Sync | MZFlush::Partial => TDEFLFlush::Sync,
            MZFlush::Full => TDEFLFlush::Full,
            MZFlush::Finish => TDEFLFlush::Finish,
            MZFlush::Block => TDEFLFlush::None,
        }
    }
}

impl TDEFLFlush {
    /// Create a `TDEFLFlush` from an integer.
    ///
    /// # Errors
    ///
    /// [`MZError::Param`] for an unknown value.
    pub const fn new(flush: i32) -> Result<Self, MZError> {
        match flush {
            0 => Ok(TDEFLFlush::None),
            2 => Ok(TDEFLFlush::Sync),
            3 => Ok(TDEFLFlush::Full),
            4 => Ok(TDEFLFlush::Finish),
            _ => Err(MZError::Param),
        }
    }
}

/// Return status of compression.
#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum TDEFLStatus {
    /// Usage error.
    BadParam = -2,
    /// Error putting data into output buffer (callback returned `false`).
    PutBufFailed = -1,
    /// Compression succeeded normally.
    Okay = 0,
    /// Compression is done.
    Done = 1,
}

/// Recover the 0..=10 level from miniz flags.
fn level_from_flags(flags: u32) -> u8 {
    if flags & TDEFL_FORCE_ALL_RAW_BLOCKS != 0 {
        return 0;
    }
    let probes = (flags & MAX_PROBES_MASK) as u16;
    let greedy = flags & TDEFL_GREEDY_PARSING_FLAG != 0;
    if probes == 32 {
        return if greedy { 3 } else { 5 };
    }
    if let Some(level) = NUM_PROBES.iter().position(|&p| p == probes) {
        return level as u8;
    }
    // Custom probe count: the level whose probe count is nearest.
    let mut best = 6usize;
    let mut best_dist = u16::MAX;
    for (level, &p) in NUM_PROBES.iter().enumerate().skip(1) {
        let dist = p.abs_diff(probes);
        if dist < best_dist {
            best_dist = dist;
            best = level;
        }
    }
    best as u8
}

/// Main compression struct.
#[derive(Debug)]
pub struct CompressorOxide {
    inner: Compress,
    flags: u32,
    level: u8,
    format: DataFormat,
    adler: u32,
    prev_status: TDEFLStatus,
}

impl Default for CompressorOxide {
    /// A compressor with miniz's default flags: level 4 probes, zlib
    /// wrapper.
    fn default() -> Self {
        CompressorOxide::new(u32::from(NUM_PROBES[4]) | TDEFL_WRITE_ZLIB_HEADER)
    }
}

impl CompressorOxide {
    /// Create a new `CompressorOxide` with the given flags.
    pub fn new(flags: u32) -> Self {
        let level = level_from_flags(flags);
        let format = if flags & TDEFL_WRITE_ZLIB_HEADER != 0 {
            DataFormat::Zlib
        } else {
            DataFormat::Raw
        };
        CompressorOxide {
            inner: Compress::new(
                Compression::new(u32::from(level)),
                format == DataFormat::Zlib,
            ),
            flags,
            level,
            format,
            adler: crate::MZ_ADLER32_INIT,
            prev_status: TDEFLStatus::Okay,
        }
    }

    /// Get the adler32 checksum of the currently encoded data.
    pub const fn adler32(&self) -> u32 {
        self.adler
    }

    /// Get the return status of the previous
    /// [`compress`](fn.compress.html) call with this compressor.
    pub const fn prev_return_status(&self) -> TDEFLStatus {
        self.prev_status
    }

    /// Return the compression flags of the compressor.
    pub const fn flags(&self) -> i32 {
        self.flags as i32
    }

    /// Returns whether the compressor is wrapping the data in a zlib format
    /// or not.
    pub const fn data_format(&self) -> DataFormat {
        self.format
    }

    /// Reset the state of the compressor, keeping the same parameters.
    pub fn reset(&mut self) {
        *self = CompressorOxide::new(self.flags);
    }

    /// Set the compression level of the compressor.
    ///
    /// Using this to change level after compression has started is
    /// supported and carries the 32 KiB history across.
    pub fn set_compression_level(&mut self, level: CompressionLevel) {
        let raw = match level {
            CompressionLevel::DefaultCompression => 6,
            other => other as i32,
        };
        self.set_compression_level_raw(raw.clamp(0, 10) as u8);
    }

    /// Set the compression level of the compressor using an integer value.
    pub fn set_compression_level_raw(&mut self, level: u8) {
        let level = level.min(10);
        self.level = level;
        let zlib = self.flags & TDEFL_WRITE_ZLIB_HEADER;
        self.flags = create_comp_flags_from_zip_params(i32::from(level), 1, 0)
            & !TDEFL_WRITE_ZLIB_HEADER
            | zlib;
        let _ = self.inner.set_level(Compression::new(u32::from(level)));
    }

    /// Update the compression settings of the compressor.
    ///
    /// Changing the `DataFormat` after compression has started will result
    /// in a restarted stream, as in miniz where it is undefined.
    pub fn set_format_and_level(&mut self, data_format: DataFormat, level: u8) {
        let window_bits = if data_format == DataFormat::Raw {
            -15
        } else {
            15
        };
        *self = CompressorOxide::new(create_comp_flags_from_zip_params(
            i32::from(level),
            window_bits,
            0,
        ));
    }

    fn run(
        &mut self,
        in_buf: &[u8],
        out_buf: &mut [u8],
        flush: TDEFLFlush,
    ) -> (TDEFLStatus, usize, usize) {
        if self.prev_status == TDEFLStatus::Done {
            return (TDEFLStatus::Done, 0, 0);
        }
        let flush = match flush {
            TDEFLFlush::None => FlushCompress::None,
            TDEFLFlush::Sync => FlushCompress::Sync,
            TDEFLFlush::Full => FlushCompress::Full,
            TDEFLFlush::Finish => FlushCompress::Finish,
        };
        let before_in = self.inner.total_in();
        let before_out = self.inner.total_out();
        let result = self.inner.compress(in_buf, out_buf, flush);
        let read = (self.inner.total_in() - before_in) as usize;
        let written = (self.inner.total_out() - before_out) as usize;
        self.adler = crate::mz_adler32_oxide(self.adler, &in_buf[..read]);
        let status = match result {
            Ok(Status::StreamEnd) => TDEFLStatus::Done,
            Ok(_) => TDEFLStatus::Okay,
            Err(_) => TDEFLStatus::BadParam,
        };
        self.prev_status = status;
        (status, read, written)
    }
}

/// Main compression function. Tries to compress as much as possible from
/// `in_buf` and puts compressed output into `out_buf`.
///
/// Returns `(status, bytes_read, bytes_written)`.
pub fn compress(
    d: &mut CompressorOxide,
    in_buf: &[u8],
    out_buf: &mut [u8],
    flush: TDEFLFlush,
) -> (TDEFLStatus, usize, usize) {
    d.run(in_buf, out_buf, flush)
}

/// Main compression function. Callbacks output.
///
/// Returns `(status, bytes_read)`. The callback receives compressed chunks
/// and returns `false` to abort (status [`TDEFLStatus::PutBufFailed`]).
pub fn compress_to_output(
    d: &mut CompressorOxide,
    in_buf: &[u8],
    flush: TDEFLFlush,
    mut callback_func: impl FnMut(&[u8]) -> bool,
) -> (TDEFLStatus, usize) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut consumed = 0usize;
    loop {
        let (status, read, written) = d.run(&in_buf[consumed..], &mut buf, flush);
        consumed += read;
        if written > 0 && !callback_func(&buf[..written]) {
            d.prev_status = TDEFLStatus::PutBufFailed;
            return (TDEFLStatus::PutBufFailed, consumed);
        }
        match status {
            TDEFLStatus::Done => return (TDEFLStatus::Done, consumed),
            TDEFLStatus::Okay => {
                // Continue while output keeps coming (buffer was filled) or
                // input remains.
                if written < buf.len() && consumed == in_buf.len() {
                    return (TDEFLStatus::Okay, consumed);
                }
            }
            other => return (other, consumed),
        }
    }
}

/// Create a set of compression flags using parameters used by zlib and
/// other compressors. Mainly intended for use with transition from c
/// libraries as it deals with raw integers.
///
/// # Parameters
/// `level` determines compression level. Clamped to maximum of 10.
/// Negative values result in `CompressionLevel::DefaultLevel`.
/// `window_bits`: Above 0, wraps the stream in a zlib wrapper, 0 or
/// negative for a raw deflate stream.
/// `strategy`: Sets the strategy if this conforms to any of the values in
/// `CompressionStrategy`.
pub fn create_comp_flags_from_zip_params(level: i32, window_bits: i32, strategy: i32) -> u32 {
    let num_probes = (if level >= 0 {
        level.min(10)
    } else {
        CompressionLevel::DefaultLevel as i32
    }) as usize;
    let greedy = if level <= 3 {
        TDEFL_GREEDY_PARSING_FLAG
    } else {
        0
    };
    let mut comp_flags = u32::from(NUM_PROBES[num_probes]) | greedy;
    if window_bits > 0 {
        comp_flags |= TDEFL_WRITE_ZLIB_HEADER;
    }
    if level == 0 {
        comp_flags |= TDEFL_FORCE_ALL_RAW_BLOCKS;
    } else if strategy == CompressionStrategy::Filtered as i32 {
        comp_flags |= TDEFL_FILTER_MATCHES;
    } else if strategy == CompressionStrategy::HuffmanOnly as i32 {
        comp_flags &= !MAX_PROBES_MASK;
    } else if strategy == CompressionStrategy::Fixed as i32 {
        comp_flags |= TDEFL_FORCE_ALL_STATIC_BLOCKS;
    } else if strategy == CompressionStrategy::RLE as i32 {
        comp_flags |= TDEFL_RLE_MATCHES;
    }
    comp_flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_roundtrip_through_flags() {
        for level in 0..=10i32 {
            let flags = create_comp_flags_from_zip_params(level, 15, 0);
            assert_eq!(i32::from(level_from_flags(flags)), level);
            assert_ne!(flags & TDEFL_WRITE_ZLIB_HEADER, 0);
        }
    }

    #[test]
    fn compress_to_output_collects_stream() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 211) as u8).collect();
        let mut d = CompressorOxide::new(create_comp_flags_from_zip_params(6, 15, 0));
        let mut packed = Vec::new();
        let (status, read) = compress_to_output(&mut d, &data, TDEFLFlush::Finish, |chunk| {
            packed.extend_from_slice(chunk);
            true
        });
        assert_eq!(status, TDEFLStatus::Done);
        assert_eq!(read, data.len());
        assert_eq!(d.adler32(), crate::mz_adler32_oxide(1, &data));
        assert_eq!(
            crate::inflate::decompress_to_vec_zlib(&packed).ok(),
            Some(data)
        );
    }
}
