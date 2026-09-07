//! Parallel strip/tile decode and encode through `rayon`.
//!
//! Off by default, so the crate stays wasm32-buildable (wasm32 has no
//! threads) -- see [`crate::Decoder::read_image_parallel`] and
//! [`crate::Encoder::write_image_parallel`].
//!
//! # Design
//!
//! Both directions follow the same shape (tiff-design.md P4): the one thing
//! that cannot be parallelised is I/O through a single `Read + Seek` /
//! `Write + Seek` handle, so that step stays serial and cheap; the CPU-bound
//! step -- decompress/predictor/unpack on decode, pack/predictor/compress on
//! encode -- is spread across a `rayon` thread pool, each chunk getting its
//! own scratch buffers so nothing is shared but read-only state.
//!
//! * **Decode**: fetch a *bounded batch* of chunks' compressed bytes serially
//!   (one `Vec<u8>` each), decode the batch in parallel, then place each
//!   decoded chunk into the destination serially (a cheap `memcpy`, and the
//!   only step that touches the shared `dst` buffer); repeat until every
//!   chunk is done.
//! * **Encode**: gather and encode every chunk in parallel into a batch of
//!   compressed payloads, then stream the batch through the writer serially
//!   in order. Unlike the streaming [`crate::ImageWriter`], this holds every
//!   chunk's compressed payload in memory at once (O(image) instead of
//!   O(chunk)) -- the price of being able to compress out of order.
//!
//! # Why the decode batch is bounded
//!
//! The serial decode pipeline holds exactly one chunk's compressed bytes at a
//! time, in a buffer whose size [`crate::Limits::intermediate_buffer_size`]
//! caps. A parallel driver that fetched *every* chunk before decoding any of
//! them would hold `chunk_count` of those at once -- and nothing in a TIFF
//! forces distinct strips to occupy distinct bytes: a file whose
//! `StripOffsets` all point at the same region and whose `StripByteCounts`
//! all declare that region's full length costs `chunk_count x count` resident
//! bytes while the file itself stays small. The per-chunk
//! `check_intermediate` guard cannot see that, because it only ever looks at
//! one chunk. [`batch_end`] therefore closes a batch as soon as the *declared*
//! compressed bytes in it would exceed `intermediate_buffer_size`, so the
//! resident compressed bytes never exceed that cap (a single chunk larger
//! than the cap is already rejected by `check_intermediate`, so the
//! "always take at least one" rule cannot breach it either). Planning from
//! the declared count rather than the fetched one is deliberate: it is the
//! upper bound, and it is known before the read that would allocate.
//!
//! The decoded side needs no separate cap of its own: the whole image's
//! decoded output is precharged against the caller's [`OutputBudget`] before
//! any chunk is fetched, and the destination buffer the chunks are placed
//! into was already sized and checked against
//! [`crate::Limits::max_image_bytes`]. A batch's aggregate *native* bytes can
//! exceed its precharged packed total -- sub-byte depths expand up to 8x on
//! unpacking, and edge tiles are decoded padded -- so the true bound there is
//! a small multiple of `max_image_bytes`, which is the inherent cost of
//! holding a batch of decoded chunks at once rather than one.
//!
//! Both drivers reuse the exact functions the serial path uses for every
//! step that is not "which thread runs this" -- [`crate::decode::decode_fetched_chunk`]
//! and [`crate::decode::chunk_output_len`] on read, and the (crate-private)
//! `writer::image::{encode_chunk_pure, gather_chunk_into,
//! build_shared_jpeg_tables_tag}` on write -- so there is exactly one
//! implementation of what a chunk decodes or encodes to, never a second copy
//! that can drift from the serial one.
//!
//! [`crate::compression::CodecState`] (shared, read-only, across every
//! worker of one call) serialises the handful of codecs whose scratch is a
//! `Mutex` (the Deflate window, the CCITT changing-element buffers); the LZW
//! code-width rule is a lock-free `AtomicU8`. A decode or encode that is
//! dominated by one of those codecs will not gain much from this feature;
//! LZW, PackBits, uncompressed and the CPU-bound part of JPEG do.

use std::io::{Read, Seek, Write};

use ::rayon::prelude::*;

use crate::byteorder::EndianReader;
use crate::compression::CodecRegistry;
use crate::decode::{self, ChunkBuffers, ChunkShape, strip, tile};
use crate::error::Result;
use crate::image::{ChunkType, ImageInfo, Rect};
use crate::limits::{Leniency, Limits, OutputBudget, Warnings};
use crate::writer::{Encoder, ImageSpec};

/// The exclusive end of the fetch batch that starts at `start`.
///
/// `declared` holds each chunk's `StripByteCounts`/`TileByteCounts` entry, in
/// the order the batch loop walks them. A batch grows while the declared
/// bytes in it stay within `cap`, and always holds at least one chunk, so the
/// loop that calls this always makes progress. See the module docs for why
/// the bound exists at all.
fn batch_end(declared: &[u64], start: usize, cap: u64) -> usize {
    let mut used = 0u64;
    let mut end = start;
    while let Some(&want) = declared.get(end) {
        if end > start {
            // `checked_add`, not `saturating_add`: when `cap` is `u64::MAX`
            // (`Limits::unlimited`), saturation would compare `MAX > MAX`,
            // find it false, and keep growing a batch whose declared bytes
            // have already overflowed.
            let Some(next) = used.checked_add(want) else {
                break;
            };
            if next > cap {
                break;
            }
            used = next;
        } else {
            used = want;
        }
        end += 1;
    }
    end
}

/// Decodes every chunk that intersects `rect` into `dst`, in parallel.
///
/// Called by [`crate::Decoder`]'s `*_parallel` methods, which handle bounds
/// checking and destination sizing exactly as their serial counterparts do;
/// this function only replaces the dispatch to [`strip::decode_into`] /
/// [`tile::decode_into`].
///
/// # Errors
/// Every failure the serial decode pipeline can produce, plus whatever a
/// worker's decode step returns. When more than one chunk of a batch fails,
/// which of those errors is reported is not specified: `rayon`'s
/// `collect::<Result<_>>` short-circuits on whichever failure its workers
/// reach first, which depends on how the batch was split across threads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_into_parallel<R: Read + Seek>(
    reader: &mut EndianReader<R>,
    info: &ImageInfo,
    rect: Rect,
    dst: &mut [u8],
    limits: &Limits,
    leniency: Leniency,
    budget: &mut OutputBudget,
    registry: Option<&CodecRegistry>,
    warnings: &mut Warnings,
) -> Result<()> {
    let indices = match info.chunk_type() {
        ChunkType::Strip => strip::chunk_indices_for_rect(info, rect),
        ChunkType::Tile => tile::chunk_indices_for_rect(info, rect),
    };
    if indices.is_empty() {
        return Ok(());
    }

    // Precharge the whole image against the caller's budget before decoding
    // any of it -- strictly safer than the serial per-chunk charge (this
    // fails before any work starts, rather than partway through a batch
    // that is already committed to running in parallel), and the total is
    // the same either way because `chunk_output_len` is the one place both
    // paths compute a chunk's decoded size. The declared compressed sizes
    // are collected in the same pass, for `batch_end`.
    let mut total = 0u64;
    let byte_counts = info.chunks.byte_counts();
    let mut declared: Vec<u64> = Vec::with_capacity(indices.len());
    for &index in &indices {
        total = total.saturating_add(decode::chunk_output_len(info, index, limits)? as u64);
        declared.push(
            usize::try_from(index)
                .ok()
                .and_then(|i| byte_counts.get(i).copied())
                .unwrap_or(0),
        );
    }
    budget.charge(total)?;

    let cap = limits.intermediate_buffer_size as u64;
    let mut scratch = ChunkBuffers::new();
    let mut start = 0usize;
    while start < indices.len() {
        let end = batch_end(&declared, start, cap);

        // Fetch is serial: `reader` is one `Read + Seek` handle, and only
        // this step touches it. A chunk's fetch warnings travel *with* the
        // chunk rather than going straight into the shared list, so the
        // merge below can interleave them with that same chunk's decode
        // warnings exactly as the serial pipeline would.
        let mut fetched: Vec<(u64, Vec<u8>, Warnings)> =
            Vec::with_capacity(end.saturating_sub(start));
        for &index in indices.get(start..end).unwrap_or_default() {
            let mut fetch_warnings = Warnings::new();
            decode::fetch_chunk(
                reader,
                info,
                index,
                limits,
                leniency,
                &mut fetch_warnings,
                &mut scratch,
            )?;
            fetched.push((index, scratch.compressed().to_vec(), fetch_warnings));
        }

        // Decode is parallel: each worker gets a fresh `ChunkBuffers`
        // (nothing shared but `info`/`limits`/`registry`, all read-only) and
        // a budget capped at `u64::MAX` -- the real cap was already charged
        // above, once, so this inner charge can never fail; it exists only
        // because `decode_fetched_chunk` always charges one.
        let decoded: Vec<(ChunkShape, Vec<u8>, Warnings, Warnings)> = fetched
            .into_par_iter()
            .map(|(index, compressed, fetch_warnings)| {
                let mut buffers = ChunkBuffers::new();
                buffers.load_compressed(compressed);
                let mut local_budget = OutputBudget::new(u64::MAX);
                let mut local_warnings = Warnings::new();
                let shape = decode::decode_fetched_chunk(
                    info,
                    index,
                    limits,
                    leniency,
                    &mut local_budget,
                    registry,
                    &mut local_warnings,
                    &mut buffers,
                )?;
                Ok((shape, buffers.take_native(), fetch_warnings, local_warnings))
            })
            .collect::<Result<Vec<_>>>()?;

        // Place is serial and cheap (a `memcpy` per chunk); order does not
        // matter for correctness since every chunk writes a disjoint region
        // of `dst`, but `Vec::into_par_iter().map(..).collect::<Vec<_>>()`
        // is index-preserving regardless, so `decoded` is already in
        // `indices` order -- and because each chunk carries both its fetch
        // and its decode warnings, appending them per chunk in that order
        // reproduces the serial path's warning sequence exactly *on a
        // successful decode*. When a chunk of a batch fails, the `?` above
        // drops that batch's still-unmerged fetch warnings, where the serial
        // path would already have pushed them; carrying them per chunk is
        // what makes the success order right, so this is the deliberate
        // trade, not an oversight.
        for (shape, native, mut fetch_warnings, mut decode_warnings) in decoded {
            decode::place_chunk_in_rect(
                &native,
                &shape,
                dst,
                rect,
                info.samples_per_pixel,
                info.planar,
            )?;
            warnings.append(&mut fetch_warnings);
            warnings.append(&mut decode_warnings);
        }

        start = end;
    }
    Ok(())
}

impl<W: Write + Seek> Encoder<W> {
    /// [`Encoder::write_image`], with per-chunk gather and encode spread
    /// across a `rayon` thread pool.
    ///
    /// Output is byte-identical to [`Encoder::write_image`] -- both call the
    /// same (crate-private) `writer::image::encode_chunk_pure` for every
    /// chunk, just in a different order. Unlike [`Encoder::write_image`], which streams
    /// each chunk's payload as soon as it is encoded (`O(chunk)` memory),
    /// this holds every chunk's compressed payload at once before writing
    /// any of them (`O(image)` memory), because encoding happens out of
    /// order and streaming still has to happen in order.
    ///
    /// # Errors
    /// Every failure [`Encoder::write_image`] can produce.
    ///
    /// ```
    /// use oxiarc_tiff::{ColorType, Compression, Encoder, ImageSpec};
    /// use std::io::Cursor;
    ///
    /// let spec = ImageSpec::new(4, 4, ColorType::Gray(8)).with_compression(Compression::PackBits);
    /// let pixels: Vec<u8> = (0..16).collect();
    ///
    /// let mut serial = Cursor::new(Vec::new());
    /// Encoder::new(&mut serial)?.write_image(&spec, &pixels)?;
    ///
    /// let mut parallel = Cursor::new(Vec::new());
    /// Encoder::new(&mut parallel)?.write_image_parallel(&spec, &pixels)?;
    ///
    /// assert_eq!(serial.into_inner(), parallel.into_inner());
    /// # Ok::<(), oxiarc_tiff::TiffError>(())
    /// ```
    pub fn write_image_parallel(&mut self, spec: &ImageSpec, data: &[u8]) -> Result<()> {
        let need = spec.image_native_len()?;
        if data.len() < need {
            return Err(crate::error::TiffError::Usage(
                crate::error::UsageError::BufferTooSmall {
                    needed: need,
                    got: data.len(),
                },
            ));
        }

        let endian = self.endian();
        let jpeg_tables = crate::writer::image::build_shared_jpeg_tables_tag(spec, endian)?;
        let codec_state = crate::compression::CodecState::new();

        // The registry borrow lives only for this block: it is read from
        // `self` before `new_image` takes `self` mutably below, and its last
        // use is inside `.collect()`, so the borrow checker sees it end
        // there (non-lexical lifetimes) rather than at the end of the
        // function.
        let payloads: Vec<Vec<u8>> = {
            let registry = self.registry();
            (0..spec.chunk_count())
                .into_par_iter()
                .map(|index| {
                    let mut native = Vec::new();
                    crate::writer::image::gather_chunk_into(spec, data, index, &mut native)?;
                    let mut packed = Vec::new();
                    crate::writer::image::encode_chunk_pure(
                        spec,
                        endian,
                        index,
                        &native,
                        jpeg_tables.as_deref(),
                        &codec_state,
                        registry,
                        &mut packed,
                    )
                })
                .collect::<Result<Vec<_>>>()?
        };

        let mut writer = self.new_image(spec)?;
        writer.set_jpeg_tables(jpeg_tables);
        for payload in &payloads {
            writer.write_encoded_chunk(payload)?;
        }
        writer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::batch_end;

    /// The cap is a real bound on a batch's resident compressed bytes, and
    /// the loop that calls `batch_end` always makes progress.
    #[test]
    fn a_batch_never_exceeds_the_cap_and_always_advances() {
        let declared = [10u64, 10, 10, 10, 10];
        let mut start = 0usize;
        let mut batches = Vec::new();
        while start < declared.len() {
            let end = batch_end(&declared, start, 25);
            assert!(end > start, "batch_end must advance past {start}");
            let used: u64 = declared.get(start..end).unwrap_or_default().iter().sum();
            assert!(used <= 25, "batch {start}..{end} holds {used} bytes");
            batches.push((start, end));
            start = end;
        }
        assert_eq!(batches, vec![(0, 2), (2, 4), (4, 5)]);
    }

    /// A single chunk larger than the cap still forms a batch of its own
    /// rather than stalling the loop. (`Limits::check_intermediate` rejects
    /// such a chunk before it is ever fetched, so this cannot actually
    /// breach the cap; it only has to terminate.)
    #[test]
    fn one_oversized_chunk_forms_its_own_batch() {
        let declared = [1u64, 1_000_000, 1];
        assert_eq!(batch_end(&declared, 0, 16), 1);
        assert_eq!(batch_end(&declared, 1, 16), 2);
        assert_eq!(batch_end(&declared, 2, 16), 3);
    }

    /// A zero cap degenerates to one chunk per batch, never to no progress.
    #[test]
    fn a_zero_cap_still_advances_one_chunk_at_a_time() {
        let declared = [1u64, 1, 1];
        assert_eq!(batch_end(&declared, 0, 0), 1);
        assert_eq!(batch_end(&declared, 1, 0), 2);
        // Chunks that declare no bytes cost nothing, so batching all of them
        // is right even at a zero cap -- what must never happen is a batch of
        // none.
        assert_eq!(batch_end(&[0u64, 0, 0], 0, 0), 3);
    }

    /// A cap wide enough for everything means exactly one batch -- the
    /// `Limits::unlimited()` shape, and the common case for ordinary strip
    /// sizes.
    #[test]
    fn a_wide_cap_keeps_the_whole_image_in_one_batch() {
        let declared = [1u64, 2, 3, 4];
        assert_eq!(batch_end(&declared, 0, u64::MAX), 4);
    }

    /// Declared counts that would overflow `u64` when summed must not wrap --
    /// nor saturate -- into a spuriously large batch. `u64::MAX` is also the
    /// cap `Limits::unlimited()` produces, which is exactly where a
    /// `saturating_add` would compare `MAX > MAX` and keep going.
    #[test]
    fn overflowing_declared_counts_do_not_widen_the_batch() {
        let declared = [u64::MAX, u64::MAX];
        assert_eq!(batch_end(&declared, 0, u64::MAX), 1);
        assert_eq!(batch_end(&declared, 1, u64::MAX), 2);
        // One below the overflow still fits in a single unlimited batch.
        assert_eq!(batch_end(&[1u64, u64::MAX - 1], 0, u64::MAX), 2);
    }

    #[test]
    fn an_empty_chunk_list_produces_no_batch() {
        assert_eq!(batch_end(&[], 0, 1024), 0);
    }
}
