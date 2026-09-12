//! The image-data compressor.
//!
//! Drives [`oxiarc_deflate::Deflater`] **directly** across one continuous
//! zlib stream: every scanline (filtered, filter byte included) is fed to
//! the same `Deflater` with `finish == false`, so LZ77 back-references and
//! the entropy coder's state carry across rows, and only the very last row
//! of the frame closes the stream. This is deliberate and load-bearing —
//! `oxiarc_deflate::ZlibStreamEncoder` auto-`sync_flush`es every 128 KiB,
//! which inserts an empty stored block and resets the entropy coder every
//! time it fires, a measurable ratio loss on filtered PNG rows for no
//! benefit. **Never swap this for `ZlibStreamEncoder`.**
//!
//! The Adler-32 trailer is computed over exactly the bytes handed to
//! `Deflater`, i.e. the *filtered* rows — that is what a zlib stream's
//! checksum always covers, and it is what a decoder's own Adler-32 (when it
//! chooses to verify) checks against.
//!
//! `IDAT`/`fdAT` chunk splitting is handled here too: bytes accumulate in a
//! `pending` buffer and are drained in `chunk_size`-sized pieces as they
//! become available, so a large image never holds its whole compressed form
//! in memory at once.
//!
//! **Filtered rows are batched before each `Deflater::deflate` call, not
//! handed over one scanline at a time** — measured (in an external scratch
//! project, per policy; see `TODO.md`/the handoff notes for the numbers) to
//! matter far more than it looks like it should: calling `deflate` once per
//! ~4 KiB scanline on a 1024-row frame cost roughly 3x a single call over
//! the same bytes, with the cliff between "clearly slow" and "at parity
//! with one big call" sitting right around 32 KiB per call — suspiciously
//! close to DEFLATE's own 32 KiB window, though the exact mechanism inside
//! `Deflater` was not chased further since fixing it, if there is anything
//! to fix, belongs in `oxiarc-deflate`. Batching the *input* this way is
//! unrelated to `pending`'s batching of the *output*: this crate's own
//! `IDAT`/`fdAT` chunk boundaries stay exactly where `chunk_size` puts
//! them, and the resulting zlib stream's bytes are identical either way —
//! `deflate(a, out, false); deflate(b, out, false)` and
//! `deflate(concat(a, b), out, false)` compress the same continuous stream,
//! just with a different number of internal calls.

use std::io::Write;

use oxiarc_deflate::{Adler32, Deflater};

use crate::chunk;
use crate::error::EncodingError;

/// Filtered-row bytes are accumulated up to this many bytes before being
/// handed to `Deflater::deflate` in one call, rather than one call per
/// scanline. See the module doc above for the measurement behind this
/// number. Unrelated to [`DEFAULT_CHUNK_SIZE`] below, which sizes the
/// *output* `IDAT`/`fdAT` chunks, not this *input* batching.
const DEFLATE_BATCH_SIZE: usize = 32 * 1024;

/// The default `IDAT`/`fdAT` payload size.
///
/// **Not** a `png` 0.18 parity claim, despite an earlier version of this
/// comment saying so: real `png` 0.18's `Writer::write_zlib_encoded_idat`
/// chunks at `MAX_IDAT_CHUNK_LEN = u32::MAX >> 1` — one ~2 GiB `IDAT` chunk
/// for any image whose compressed form is smaller than that, which is
/// every image in practice — confirmed by reading that method's source
/// directly, not assumed. This crate instead defaults to a much smaller
/// 64 KiB, a deliberate choice for streaming-friendliness (a reader that
/// processes chunks as they arrive need not buffer gigabytes), matching
/// the conventional IDAT chunk size many other PNG encoders use (libpng's
/// own default among them) rather than this specific Rust crate's choice.
pub const DEFAULT_CHUNK_SIZE: usize = 65_536;

/// Whether a compression level should use [`Deflater::with_optimal_parsing`]
/// (Zopfli-style graph-based parsing) rather than the greedy/lazy parser.
///
/// Gated to `level >= 9` (the explicit "best compression" request) only —
/// **deliberately, based on a measurement, not the original untested
/// assumption**. `tests/pillow_oracle.rs`'s
/// `compression_ratio_does_not_regress_past_the_measured_gap_to_pythons_zlib_level6`
/// measured level 6 (the crate's own default, and Python zlib's
/// `Z_DEFAULT_COMPRESSION`) losing 36.7% to `python3 zlib.compress(level=6)`
/// on realistic (gradient + noise) filtered PNG rows, and the design
/// report's reaction to a >5% loss is "wire `OptimalParser` more
/// aggressively" — but trying exactly that (lowering this threshold to
/// `level >= 6`) was measured to make it *worse*, not better: 121% over
/// Python's output instead of 37%, both measured via one-shot
/// `compress_to_vec`/`zlib_compress` calls (so this specific comparison is
/// unaffected by the row-batching change documented in this module's top
/// doc). **Why is left as an open question, not a diagnosed root cause**:
/// `Deflater::with_optimal_parsing(level)` still uses
/// `Lz77Encoder::with_level(level)` for match-finding, so a plausible guess
/// is that level 6's search is too shallow to hand the optimal-parsing DP a
/// useful match graph — but that hypothesis was not verified by reading
/// `Lz77Encoder`'s implementation or by testing intermediate levels, so
/// treat it as a lead, not an established cause. Either way, fixing
/// whatever the real cause is belongs in `oxiarc-deflate`, out of
/// `oxiarc-png`'s ownership; this crate can only choose which existing
/// `Deflater` constructor to call, and the level-9 threshold measurably
/// remains the better choice at every level this crate can pick from
/// today. The unresolved gap at level 6 is tracked as an open issue, not
/// silently dropped.
#[must_use]
pub(crate) fn use_optimal_parsing(level: u8) -> bool {
    level >= 9
}

/// The largest a single compressed-data chunk payload may be. `IDAT`'s own
/// cap (`png`'s `MAX_IDAT_CHUNK_LEN`) is four bytes larger — `u32::MAX >>
/// 1` — but `FrameEncoder` uses this one bound for both chunk kinds so a
/// caller's `idat_chunk_size` clamps identically regardless of which
/// chunk type a frame ends up using.
pub const MAX_FDAT_CHUNK_LEN: u32 = (u32::MAX >> 1) - 4;

/// Map an `oxiarc-deflate` level to the zlib header's two-bit `FLEVEL`
/// indicator. Cosmetic only: PNG's own validity rules (`CM == 8`,
/// `CINFO <= 7`, `FDICT == 0`) are the only header bits a decoder checks;
/// `FLEVEL` is advisory, exactly as in `oxiarc_deflate::zlib::zlib_compress`,
/// which this mirrors so encoder output looks like any other zlib stream.
fn flevel(level: u8) -> u8 {
    match level {
        0..=2 => 0,
        3..=5 => 1,
        6 => 2,
        _ => 3,
    }
}

/// The two-byte zlib header: `CMF = 0x78` (`CM = 8`, `CINFO = 7`), `FLG` with
/// `FDICT = 0` and `FCHECK` chosen so `(CMF*256 + FLG) % 31 == 0`.
fn zlib_header(level: u8) -> [u8; 2] {
    let cmf: u8 = 0x78;
    let flg_hi = flevel(level) << 6;
    let base = u16::from(cmf) * 256 + u16::from(flg_hi);
    let rem = base % 31;
    let fcheck = if rem == 0 { 0 } else { 31 - rem };
    [cmf, flg_hi | (fcheck as u8)]
}

fn deflate_into(
    deflater: &mut Deflater,
    data: &[u8],
    out: &mut Vec<u8>,
    finish: bool,
) -> Result<(), EncodingError> {
    deflater
        .deflate(data, out, finish)
        .map_err(|err| EncodingError::IoError(std::io::Error::other(err.to_string())))
}

/// Which physical chunk type a [`FrameEncoder`] emits, and how.
#[derive(Debug)]
pub(crate) enum ChunkKind<'a> {
    /// Plain `IDAT` chunks: the default image, or an APNG's non-animated
    /// still image.
    Idat,
    /// `fdAT` chunks. Each physical chunk draws its own sequence number from
    /// the shared counter — the counter is threaded through so a frame that
    /// splits across several `fdAT` chunks still increments once per chunk,
    /// exactly as the APNG specification requires.
    Fdat(&'a mut u32),
}

/// Drives one continuous zlib stream, one scanline at a time, splitting the
/// output into `IDAT`/`fdAT` chunks as it goes.
#[derive(Debug)]
pub(crate) struct FrameEncoder {
    deflater: Deflater,
    adler: Adler32,
    /// Filtered-row bytes not yet handed to `deflater.deflate` — the
    /// *input*-side batch (see [`DEFLATE_BATCH_SIZE`]), distinct from
    /// `pending` below, which batches already-*compressed* output bytes.
    input_batch: Vec<u8>,
    pending: Vec<u8>,
    chunk_size: usize,
    header_written: bool,
}

impl FrameEncoder {
    /// A fresh encoder. `optimal` selects `Deflater::with_optimal_parsing`,
    /// which the caller wires in for the best/9 compression path.
    pub(crate) fn new(level: u8, optimal: bool, chunk_size: usize) -> FrameEncoder {
        let deflater = if optimal {
            Deflater::with_optimal_parsing(level)
        } else {
            Deflater::new(level)
        };
        FrameEncoder {
            deflater,
            adler: Adler32::new(),
            input_batch: Vec::with_capacity(DEFLATE_BATCH_SIZE),
            pending: Vec::with_capacity(chunk_size.clamp(64, DEFAULT_CHUNK_SIZE)),
            chunk_size: chunk_size.clamp(1, MAX_FDAT_CHUNK_LEN as usize),
            header_written: false,
        }
    }

    fn ensure_header(&mut self, level: u8) {
        if !self.header_written {
            self.pending.extend_from_slice(&zlib_header(level));
            self.header_written = true;
        }
    }

    /// Hand `input_batch` to `Deflater::deflate` and clear it. `finish`
    /// mirrors `Deflater::deflate`'s own parameter: `false` for a
    /// mid-stream flush (only ever called once `input_batch` has grown
    /// past [`DEFLATE_BATCH_SIZE`]), `true` to close the stream (called
    /// exactly once, from [`FrameEncoder::finish`], on whatever is left —
    /// possibly empty, which is a valid and already-tested way to emit a
    /// zlib stream's terminating block).
    fn flush_input_batch(&mut self, finish: bool) -> Result<(), EncodingError> {
        deflate_into(
            &mut self.deflater,
            &self.input_batch,
            &mut self.pending,
            finish,
        )?;
        self.input_batch.clear();
        Ok(())
    }

    /// Compress one already-filtered scanline into the stream: its
    /// `filter_byte` (PNG's per-row filter-type prefix) followed by
    /// `filtered_samples`.
    ///
    /// The two are taken **separately** rather than as one pre-assembled
    /// row on purpose. `input_batch` already exists to concatenate rows, so
    /// having each caller build a throwaway `Vec` of `[filter_byte] +
    /// samples` first cost one heap allocation and one extra full-row copy
    /// per scanline — a whole extra pass over the image per encode, for a
    /// buffer whose only use was to be copied straight into `input_batch`
    /// and dropped.
    ///
    /// Bytes accumulate in `input_batch` and only reach
    /// `Deflater::deflate` once that batch is large enough (see the module
    /// doc); `pending` (already-compressed output) is drained into whole
    /// chunks as it fills, independently, but always keeping back a
    /// possibly-partial tail.
    pub(crate) fn push_row<W: Write>(
        &mut self,
        level: u8,
        filter_byte: u8,
        filtered_samples: &[u8],
        w: &mut W,
        kind: &mut ChunkKind<'_>,
    ) -> Result<(), EncodingError> {
        self.ensure_header(level);
        self.adler.update(&[filter_byte]);
        self.adler.update(filtered_samples);
        self.input_batch.push(filter_byte);
        self.input_batch.extend_from_slice(filtered_samples);
        if self.input_batch.len() >= DEFLATE_BATCH_SIZE {
            self.flush_input_batch(false)?;
        }
        self.drain_full_chunks(w, kind)
    }

    /// Close the stream: flush whatever is left of `input_batch` as the
    /// final `deflate` call, append the Adler-32 trailer, and write every
    /// remaining byte out as the last chunk(s).
    pub(crate) fn finish<W: Write>(
        mut self,
        level: u8,
        w: &mut W,
        mut kind: ChunkKind<'_>,
    ) -> Result<(), EncodingError> {
        self.ensure_header(level);
        self.flush_input_batch(true)?;
        let checksum = self.adler.finish();
        self.pending.extend_from_slice(&checksum.to_be_bytes());
        self.drain_full_chunks(w, &mut kind)?;
        if !self.pending.is_empty() {
            let piece = std::mem::take(&mut self.pending);
            emit_chunk(w, &mut kind, &piece)?;
        }
        Ok(())
    }

    fn drain_full_chunks<W: Write>(
        &mut self,
        w: &mut W,
        kind: &mut ChunkKind<'_>,
    ) -> Result<(), EncodingError> {
        while self.pending.len() >= self.chunk_size {
            let remainder = self.pending.split_off(self.chunk_size);
            let piece = std::mem::replace(&mut self.pending, remainder);
            emit_chunk(w, kind, &piece)?;
        }
        Ok(())
    }
}

fn emit_chunk<W: Write>(
    w: &mut W,
    kind: &mut ChunkKind<'_>,
    payload: &[u8],
) -> Result<(), EncodingError> {
    match kind {
        ChunkKind::Idat => chunk::write_chunk(w, chunk::IDAT, payload)?,
        ChunkKind::Fdat(seq) => {
            let n = **seq;
            **seq = n.wrapping_add(1);
            chunk::write_chunk_parts(w, chunk::fdAT, &[&n.to_be_bytes(), payload])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compress_all(level: u8, rows: &[Vec<u8>], chunk_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = FrameEncoder::new(level, false, chunk_size);
        let mut seq = 0u32;
        for row in rows {
            let (filter_byte, samples) = row.split_first().unwrap_or((&0, &[]));
            enc.push_row(
                level,
                *filter_byte,
                samples,
                &mut out,
                &mut ChunkKind::Fdat(&mut seq),
            )
            .expect("push");
        }
        enc.finish(level, &mut out, ChunkKind::Fdat(&mut seq))
            .expect("finish");
        out
    }

    fn concat_fdat_payloads(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut seq_seen = Vec::new();
        for item in crate::chunk::ChunkIter::new(bytes) {
            let (kind, data) = item.expect("chunk");
            assert_eq!(kind, chunk::fdAT);
            let seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            seq_seen.push(seq);
            out.extend_from_slice(&data[4..]);
        }
        let expected: Vec<u32> = (0..seq_seen.len() as u32).collect();
        assert_eq!(seq_seen, expected, "sequence numbers must be consecutive");
        out
    }

    #[test]
    fn round_trips_through_the_real_zlib_decompressor() {
        let rows: Vec<Vec<u8>> = (0..40u8)
            .map(|y| {
                let mut row = vec![0u8; 21];
                row[0] = 0; // filter byte
                for (i, b) in row.iter_mut().enumerate().skip(1) {
                    *b = y.wrapping_mul(7).wrapping_add(i as u8);
                }
                row
            })
            .collect();
        let expected: Vec<u8> = rows.iter().flatten().copied().collect();
        let compressed = compress_all(6, &rows, 16);
        let zlib_stream = concat_fdat_payloads(&compressed);
        let decompressed = oxiarc_deflate::zlib_decompress(&zlib_stream).expect("decompress");
        assert_eq!(decompressed, expected);
    }

    /// `push_row` only calls `Deflater::deflate` once `input_batch` passes
    /// [`DEFLATE_BATCH_SIZE`] (32 KiB) — this frame's filtered rows total
    /// well past that, so `flush_input_batch(false)` fires more than once
    /// before `finish` closes the stream. The compressed result must still
    /// decompress back to exactly the concatenated input, proving the
    /// batching is transparent to stream *content*, only to how many
    /// `deflate` calls produce it.
    #[test]
    fn input_batching_across_many_rows_still_decompresses_exactly() {
        let rows: Vec<Vec<u8>> = (0..8000u32)
            .map(|y| {
                let mut row = vec![0u8; 20];
                row[0] = u8::try_from(y % 5).expect("filter byte fits in u8"); // filter byte
                for (i, b) in row.iter_mut().enumerate().skip(1) {
                    *b = (y.wrapping_mul(7).wrapping_add(i as u32) % 256) as u8;
                }
                row
            })
            .collect();
        let total_filtered: usize = rows.iter().map(Vec::len).sum();
        assert!(
            total_filtered > 3 * DEFLATE_BATCH_SIZE,
            "must exercise several mid-stream flushes, not just one"
        );
        let expected: Vec<u8> = rows.iter().flatten().copied().collect();
        let compressed = compress_all(6, &rows, DEFAULT_CHUNK_SIZE);
        let zlib_stream = concat_fdat_payloads(&compressed);
        let decompressed = oxiarc_deflate::zlib_decompress(&zlib_stream).expect("decompress");
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn a_single_chunk_covers_a_small_stream_when_chunk_size_is_large() {
        let rows = vec![vec![0u8, 1, 2, 3]];
        let out = compress_all(6, &rows, DEFAULT_CHUNK_SIZE);
        let chunks: Vec<_> = crate::chunk::ChunkIter::new(&out).collect();
        assert_eq!(chunks.len(), 1, "one fdAT chunk expected");
    }

    #[test]
    fn zlib_header_bytes_are_valid_for_every_level() {
        for level in 0..=9u8 {
            let [cmf, flg] = zlib_header(level);
            assert_eq!(cmf, 0x78);
            assert_eq!((u16::from(cmf) * 256 + u16::from(flg)) % 31, 0);
            assert_eq!(flg & 0x20, 0, "FDICT must be clear");
        }
    }

    #[test]
    fn empty_frame_still_produces_a_valid_zlib_stream() {
        let out = compress_all(6, &[], 16);
        let zlib_stream = concat_fdat_payloads(&out);
        let decompressed = oxiarc_deflate::zlib_decompress(&zlib_stream).expect("decompress");
        assert!(decompressed.is_empty());
    }
}
