//! Core traits for compression and archive operations.
//!
//! This module defines the fundamental traits that all compression algorithms
//! and archive handlers must implement.

use crate::entry::Entry;
use crate::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// Status of a streaming decompression operation.
///
/// Marked `#[non_exhaustive]` so new statuses can be added in a minor release
/// without breaking downstream `match` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecompressStatus {
    /// More input is needed to continue decompression.
    NeedsInput,
    /// More output buffer space is needed.
    NeedsOutput,
    /// Decompression is complete.
    Done,
    /// A block boundary was reached (caller may want to check CRC, etc.).
    BlockEnd,
}

/// Status of a streaming compression operation.
///
/// Marked `#[non_exhaustive]` so new statuses can be added in a minor release
/// without breaking downstream `match` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressStatus {
    /// More input data can be accepted.
    NeedsInput,
    /// More output buffer space is needed.
    NeedsOutput,
    /// Compression is complete.
    Done,
}

/// Flush mode for compression.
///
/// Marked `#[non_exhaustive]` so new flush modes can be added in a minor
/// release without breaking downstream `match` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FlushMode {
    /// No flush - buffer data for best compression.
    #[default]
    None,
    /// Sync flush - emit all pending output.
    Sync,
    /// Full flush - emit and reset encoder state.
    Full,
    /// Partial flush - emit compressed block without sync marker.
    Partial,
    /// Finish - complete the stream.
    Finish,
}

/// A streaming decompressor (decoder).
///
/// This trait models the DEFLATE-family streaming contract (consume input,
/// produce output, report status) and is implemented by the Deflate/LZH/LZ4
/// family of codecs in this workspace. It is an *optional* convenience
/// abstraction, not a requirement placed on every codec crate: algorithms
/// whose native streaming shape does not map cleanly onto this
/// consume/produce/status contract (e.g. block-oriented codecs such as
/// bzip2, or codecs built around a different streaming API) are free to
/// expose their own encoder/decoder types instead of implementing this
/// trait.
///
/// # Contract: `decompress` receives a whole-remaining-input, not a genuine
/// chunk
///
/// `decompress` has no `flush`/`finish` parameter, unlike [`Compressor`]'s
/// [`compress`][Compressor::compress]. That is deliberate, not an oversight:
/// **every call must be handed all of the compressed input still
/// available**, as if it were the final, complete tail of the stream. An
/// implementation is entitled to treat a slice that ends mid-symbol as
/// truncated input and return an error, rather than requesting more input
/// via [`DecompressStatus::NeedsInput`] — a conforming caller has, by
/// definition, nothing more to offer this call. [`decompress_all`] (below)
/// upholds this by construction: it always passes the *entire* unconsumed
/// remainder of `input` on every call, never a deliberately-truncated
/// prefix.
///
/// **This trait is the wrong tool for feeding genuine chunks** — a few
/// kilobytes of an HTTP response body at a time, say, where more bytes are
/// truly still in flight and have not arrived yet. Reaching for
/// `decompress`/`decompress_all` in that situation, then re-driving the same
/// decompressor with each new chunk appended, does not recover the intended
/// semantics: an implementation that honors the contract above is free to
/// error out on the first incomplete chunk instead of waiting. Callers with
/// genuine chunks must use a codec's own push-decoder type instead, where an
/// explicit flush parameter distinguishes "more is coming" from "this is
/// everything" (for example `oxiarc_lz4`'s frame streaming decoder, or
/// `StreamingLzhDecoder`; `oxiarc_deflate` is gaining a resumable
/// `InflateStream`/`WrappedInflate` push API for exactly this purpose).
///
/// # Contract: no silent, unbounded spinning
///
/// A conforming implementation must make *some* forward progress — consume
/// input, produce output, or transition to
/// [`Done`][DecompressStatus::Done] — on every call that is not already
/// finished; it must never return the same `(0, 0, status)` indefinitely
/// while claiming to still need input or output it can never receive from a
/// whole-remaining-input caller. [`decompress_all`]'s default
/// implementation enforces this from the caller's side as a safety net: two
/// consecutive calls that both consume zero bytes and produce zero bytes
/// (without reaching `Done`) are treated as a stalled decoder and reported
/// as an error, instead of looping forever.
///
/// [`decompress_all`]: Decompressor::decompress_all
pub trait Decompressor {
    /// Decompress data from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Input compressed data
    /// * `output` - Output buffer for decompressed data
    ///
    /// # Returns
    ///
    /// A tuple of (bytes consumed from input, bytes written to output, status)
    ///
    /// # Contract
    ///
    /// `input` is the whole remainder of the compressed stream, not an
    /// arbitrary prefix a caller expects more data to eventually follow —
    /// see the trait-level documentation above.
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)>;

    /// Reset the decompressor to its initial state.
    fn reset(&mut self);

    /// Check if the decompressor has finished.
    fn is_finished(&self) -> bool;

    /// Decompress all data at once (convenience method).
    ///
    /// Repeatedly calls [`decompress`](Decompressor::decompress), each time
    /// passing the entire not-yet-consumed remainder of `input` — honoring
    /// the whole-remaining-input contract documented on this trait — until
    /// the decompressor reports [`Done`](DecompressStatus::Done) or (for a
    /// decoder that has no explicit end marker) input runs out while it
    /// reports [`NeedsInput`](DecompressStatus::NeedsInput).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying [`decompress`](Decompressor::decompress)
    /// call does, or if the decompressor makes **no progress** (consumes
    /// zero bytes and produces zero bytes) on two consecutive calls without
    /// reaching `Done` — a defensive guard against a non-conforming or
    /// buggy implementation spinning this loop forever instead of erroring
    /// or completing.
    fn decompress_all(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut input_pos = 0;
        let mut buffer = vec![0u8; 32768];
        let mut stalled_once = false;

        loop {
            let (consumed, produced, status) = self.decompress(&input[input_pos..], &mut buffer)?;

            input_pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            let made_progress = consumed > 0 || produced > 0 || status == DecompressStatus::Done;
            if made_progress {
                stalled_once = false;
            } else if stalled_once {
                return Err(OxiArcError::corrupted(
                    input_pos as u64,
                    "decompress_all: decoder made no progress on two consecutive calls",
                ));
            } else {
                stalled_once = true;
            }

            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput if input_pos >= input.len() => break,
                DecompressStatus::NeedsOutput | DecompressStatus::NeedsInput => continue,
                DecompressStatus::BlockEnd => continue,
            }
        }

        Ok(output)
    }
}

/// A streaming compressor (encoder).
///
/// This trait models the DEFLATE-family streaming contract (consume input,
/// produce output, report status) and is implemented by the Deflate/LZH/LZ4
/// family of codecs in this workspace. It is an *optional* convenience
/// abstraction, not a requirement placed on every codec crate: algorithms
/// whose native streaming shape does not map cleanly onto this
/// consume/produce/status contract (e.g. block-oriented codecs such as
/// bzip2, or codecs built around a different streaming API) are free to
/// expose their own encoder/decoder types instead of implementing this
/// trait.
///
/// # Contrast with [`Decompressor`]
///
/// Unlike [`Decompressor::decompress`], `compress` takes an explicit
/// `flush: FlushMode` parameter. Accepting a genuine prefix of the input
/// across multiple calls is therefore a supported, ordinary pattern here:
/// pass [`FlushMode::None`] while more input is still coming and
/// [`FlushMode::Finish`] only on the last call (see
/// [`compress_all`](Compressor::compress_all)'s default implementation for a
/// worked example). This is a real, deliberate asymmetry between the two
/// traits, not an inconsistency to "fix" — the decompression side, by
/// contrast, is a whole-remaining-input contract with no way to say "more is
/// coming" (see `Decompressor`'s own documentation).
pub trait Compressor {
    /// Compress data from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Input data to compress
    /// * `output` - Output buffer for compressed data
    /// * `flush` - Flush mode
    ///
    /// # Returns
    ///
    /// A tuple of (bytes consumed from input, bytes written to output, status)
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)>;

    /// Reset the compressor to its initial state.
    fn reset(&mut self);

    /// Check if the compressor has finished.
    fn is_finished(&self) -> bool;

    /// Compress all data at once (convenience method).
    ///
    /// Drives [`compress`](Compressor::compress) with
    /// [`FlushMode::None`] while input remains and [`FlushMode::Finish`]
    /// once it is exhausted, until the compressor reports
    /// [`Done`](CompressStatus::Done).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying [`compress`](Compressor::compress)
    /// call does, or if the compressor makes **no progress** (consumes zero
    /// bytes and emits zero bytes, counting the final-flush call) on two
    /// consecutive iterations without reaching `Done` — the same defensive
    /// guard [`Decompressor::decompress_all`] applies, so a non-conforming
    /// implementation errors instead of spinning this loop forever.
    fn compress_all(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut input_pos = 0;
        let mut buffer = vec![0u8; 32768];
        let mut stalled_once = false;

        // Compress data
        loop {
            let flush = if input_pos >= input.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };

            let (consumed, produced, status) =
                self.compress(&input[input_pos..], &mut buffer, flush)?;

            input_pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            // Progress made by this iteration, including anything the
            // final-flush call below emits (that call is real progress even
            // when the outer call reported none, so it must be counted here
            // or a legitimately draining compressor would be misjudged).
            let mut progressed = consumed > 0 || produced > 0 || status == CompressStatus::Done;

            match status {
                CompressStatus::Done => break,
                CompressStatus::NeedsInput if input_pos >= input.len() => {
                    // Final flush
                    let (_, produced, status) =
                        self.compress(&[], &mut buffer, FlushMode::Finish)?;
                    output.extend_from_slice(&buffer[..produced]);
                    if produced > 0 || status == CompressStatus::Done {
                        progressed = true;
                    }
                    if status == CompressStatus::Done {
                        break;
                    }
                }
                _ => {}
            }

            if progressed {
                stalled_once = false;
            } else if stalled_once {
                return Err(OxiArcError::corrupted(
                    input_pos as u64,
                    "compress_all: compressor made no progress on two consecutive calls",
                ));
            } else {
                stalled_once = true;
            }
        }

        Ok(output)
    }
}

/// An archive reader that can list and extract entries.
///
/// This trait is implemented by archive format handlers (ZIP, TAR, LZH, etc.).
pub trait ArchiveReader {
    /// Get the list of entries in the archive.
    fn entries(&mut self) -> Result<Vec<Entry>>;

    /// Extract a specific entry by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name/path of the entry to extract
    /// * `writer` - Where to write the extracted data
    ///
    /// # Returns
    ///
    /// The number of bytes written.
    fn extract_by_name<W: Write>(&mut self, name: &str, writer: &mut W) -> Result<u64>;

    /// Extract a specific entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to extract
    /// * `writer` - Where to write the extracted data
    ///
    /// # Returns
    ///
    /// The number of bytes written.
    fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64>;

    /// Get an entry by name.
    fn entry_by_name(&mut self, name: &str) -> Result<Option<Entry>> {
        let entries = self.entries()?;
        Ok(entries.into_iter().find(|e| e.name == name))
    }
}

/// An archive writer that can create archives.
pub trait ArchiveWriter {
    /// Add an entry to the archive.
    ///
    /// # Arguments
    ///
    /// * `entry` - Metadata for the entry
    /// * `data` - The data to write
    fn add_entry<R: Read>(&mut self, entry: &Entry, data: &mut R) -> Result<()>;

    /// Add a file from disk.
    ///
    /// # Arguments
    ///
    /// * `name` - Name/path in the archive
    /// * `path` - Path to the file on disk
    fn add_file(&mut self, name: &str, path: &std::path::Path) -> Result<()>;

    /// Finalize the archive.
    fn finish(&mut self) -> Result<()>;
}

// NOTE: A `CompressionLevel(u8)` newtype previously lived here as a proposed
// shared compression-level abstraction. It has been removed as part of the
// pre-1.0 API freeze: no codec in this workspace ever adopted it (each
// format defines its own level enum — see `oxiarc_bzip2::CompressionLevel`,
// `ZipCompressionLevel`, `LzhCompressionLevel`, and the CLI's own
// `CompressionLevel` — and `oxiarc_bzip2::CompressionLevel` even collides on
// the name), so keeping an unused, never-implemented "shared" type in core
// would advertise an abstraction nothing honors. Per-codec level types are
// the deliberate, documented design: each compression format has a
// different natural level range/semantics (e.g. 0-9 vs 0-11), and forcing
// them through one shared type would either lose precision or require
// lossy conversions at every call site. If a genuinely shared level
// abstraction is wanted in the future, it should be reintroduced only once
// at least the majority of codecs are prepared to adopt it directly,
// tracked per-crate.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flush_mode_default() {
        assert_eq!(FlushMode::default(), FlushMode::None);
    }

    /// A decoder that never makes progress: every call reports
    /// `NeedsOutput` with zero bytes consumed and zero bytes produced.
    /// `decompress_all` must detect this and return an error instead of
    /// looping forever — this is the regression test for that guard; if the
    /// guard regresses, this test hangs rather than merely failing, so it
    /// doubles as the strongest possible check.
    struct StalledDecompressor;

    impl Decompressor for StalledDecompressor {
        fn decompress(
            &mut self,
            _input: &[u8],
            _output: &mut [u8],
        ) -> Result<(usize, usize, DecompressStatus)> {
            Ok((0, 0, DecompressStatus::NeedsOutput))
        }

        fn reset(&mut self) {}

        fn is_finished(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_decompress_all_detects_stalled_decoder() {
        let mut decoder = StalledDecompressor;
        let result = decoder.decompress_all(b"some compressed bytes");
        assert!(
            result.is_err(),
            "decompress_all must error on a decoder that never makes progress, not loop forever"
        );
    }

    /// A decoder that stalls exactly once (`(0, 0, NeedsOutput)`) before
    /// making real progress and finishing. A single stalled call must not
    /// itself be treated as an error — only two in a row are — so
    /// `decompress_all` must still complete correctly here.
    struct StallsOnceThenFinishes {
        calls: usize,
    }

    impl Decompressor for StallsOnceThenFinishes {
        fn decompress(
            &mut self,
            input: &[u8],
            output: &mut [u8],
        ) -> Result<(usize, usize, DecompressStatus)> {
            self.calls += 1;
            if self.calls == 1 {
                return Ok((0, 0, DecompressStatus::NeedsOutput));
            }
            let n = input.len().min(output.len());
            output[..n].copy_from_slice(&input[..n]);
            Ok((n, n, DecompressStatus::Done))
        }

        fn reset(&mut self) {
            self.calls = 0;
        }

        fn is_finished(&self) -> bool {
            self.calls > 1
        }
    }

    /// A compressor that never makes progress: every call reports
    /// `NeedsOutput` with zero bytes consumed and zero bytes emitted.
    /// `compress_all` must detect this and error instead of looping forever
    /// (a regression hangs this test rather than failing it).
    struct StalledCompressor;

    impl Compressor for StalledCompressor {
        fn compress(
            &mut self,
            _input: &[u8],
            _output: &mut [u8],
            _flush: FlushMode,
        ) -> Result<(usize, usize, CompressStatus)> {
            Ok((0, 0, CompressStatus::NeedsOutput))
        }

        fn reset(&mut self) {}

        fn is_finished(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_compress_all_detects_stalled_compressor() {
        let mut encoder = StalledCompressor;
        let result = encoder.compress_all(b"some bytes to compress");
        assert!(
            result.is_err(),
            "compress_all must error on a compressor that never makes progress"
        );
    }

    /// A compressor that emits nothing on its streaming calls and flushes
    /// everything from the final `FlushMode::Finish` call, one buffer at a
    /// time. The outer call reports no progress on those iterations, so the
    /// guard must credit the final-flush call's output — otherwise this
    /// perfectly legitimate shape would be misreported as a stall.
    struct FlushOnlyCompressor {
        pending: Vec<u8>,
        buffered: Vec<u8>,
        emitted: usize,
    }

    impl Compressor for FlushOnlyCompressor {
        fn compress(
            &mut self,
            input: &[u8],
            output: &mut [u8],
            flush: FlushMode,
        ) -> Result<(usize, usize, CompressStatus)> {
            if flush != FlushMode::Finish {
                self.buffered.extend_from_slice(input);
                return Ok((input.len(), 0, CompressStatus::NeedsInput));
            }
            if self.pending.is_empty() && self.emitted == 0 {
                self.pending = std::mem::take(&mut self.buffered);
            }
            // Emit at most 4 bytes per call, so several final-flush calls are
            // needed and the outer loop iterates with consumed == 0.
            let n = self.pending.len().min(output.len()).min(4);
            output[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            self.emitted += n;
            let status = if self.pending.is_empty() {
                CompressStatus::Done
            } else {
                CompressStatus::NeedsInput
            };
            Ok((0, n, status))
        }

        fn reset(&mut self) {
            self.pending.clear();
            self.buffered.clear();
            self.emitted = 0;
        }

        fn is_finished(&self) -> bool {
            self.emitted > 0 && self.pending.is_empty()
        }
    }

    #[test]
    fn test_compress_all_tolerates_flush_only_compressor() {
        let mut encoder = FlushOnlyCompressor {
            pending: Vec::new(),
            buffered: Vec::new(),
            emitted: 0,
        };
        let data = b"a flush-only compressor drains through Finish calls".to_vec();
        let out = encoder
            .compress_all(&data)
            .expect("a compressor that only emits on Finish must not look stalled");
        assert_eq!(out, data);
    }

    #[test]
    fn test_decompress_all_tolerates_single_stall() {
        let mut decoder = StallsOnceThenFinishes { calls: 0 };
        let data = b"round trips fine".to_vec();
        let result = decoder
            .decompress_all(&data)
            .expect("a single stalled call must not be treated as an error");
        assert_eq!(result, data);
    }
}
