//! A reusable `.xz` decoder context.
//!
//! [`decompress_into`](super::decompress_into) is a thin one-shot wrapper:
//! every call parses a fresh [`XzReader`], which starts with an empty LZMA2
//! decoder cache and therefore builds its dictionary (window) buffer from
//! nothing. That is the right default for decoding a handful of streams,
//! but some callers decode many streams in a tight loop that all share the
//! same LZMA2 dictionary size — the motivating case is TIFF
//! `Compression = 34925`, where every strip or tile of one image is its own
//! complete `.xz` stream, and a large image can have thousands of them.
//! Rebuilding a multi-megabyte dictionary buffer (growing it one
//! `Vec::push` at a time from empty) thousands of times over is pure waste
//! when the buffer could simply be kept and overwritten in place.
//!
//! [`XzDecoder`] is that reusable context: construct one, call
//! [`XzDecoder::decompress_into`] once per stream, and the LZMA2 decoder
//! (dictionary buffer, probability model, coder state) it built for the
//! first stream is kept and reused for the next one, as long as that next
//! stream declares the same dictionary size — which repeated calls from one
//! caller almost always do, since it comes from the encoder's settings, not
//! the data. A stream that declares a different dictionary size still
//! decodes correctly; it just pays for a fresh allocation, exactly like
//! today's one-shot [`decompress_into`](super::decompress_into) always does.

use super::header::XzReader;
use oxiarc_core::error::{OxiArcError, Result};

/// A reusable `.xz` decoder context.
///
/// See the [module documentation](self) for why this exists and when it
/// helps. Each [`XzDecoder::decompress_into`] call decodes one complete,
/// independent `.xz` stream — the same contract
/// [`decompress_into`](super::decompress_into) has — but keeps its LZMA2
/// decoder (dictionary buffer, probability model, coder state) allocated
/// between calls instead of rebuilding it every time.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzma::xz::{self, XzDecoder};
///
/// let original = b"one TIFF strip's worth of pixel data";
/// let stream = xz::compress(original, 6)?;
///
/// // Decode the same shape of stream many times through one context, the
/// // way a TIFF reader decodes one strip after another.
/// let mut decoder = XzDecoder::new();
/// let mut strip = vec![0u8; original.len()];
/// for _ in 0..1000 {
///     let written = decoder.decompress_into(&stream, &mut strip)?;
///     assert_eq!(&strip[..written], original);
/// }
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
pub struct XzDecoder {
    /// The reusable LZMA2 decoder, keyed by the dictionary size it was
    /// built with. Threaded into a fresh, lightweight [`XzReader`] on every
    /// [`Self::decompress_into`] call and retrieved again afterwards —
    /// `XzReader` itself owns no heap buffers worth keeping (it just wraps
    /// a `Cursor` over the caller's `src` slice), so there is nothing to
    /// gain from keeping the reader itself alive, only the decoder inside
    /// it.
    cache: Option<(u32, crate::Lzma2Decoder)>,
    /// Optional cap on a single stream's total uncompressed size, enforced
    /// *during* decoding. See [`Self::with_max_output`].
    max_output: Option<u64>,
    /// How many blocks the most recent [`Self::decompress_into`] call
    /// decoded with a reused LZMA2 decoder.
    ///
    /// Test-only observability: reuse is deliberately unobservable from
    /// output and errors, so a broken reuse predicate would silently
    /// disable this whole type without failing a single correctness test.
    #[cfg(test)]
    last_reuses: u32,
}

impl XzDecoder {
    /// Create a new, empty decoder context.
    ///
    /// The first [`Self::decompress_into`] call allocates a fresh LZMA2
    /// decoder, exactly like [`decompress_into`](super::decompress_into)
    /// does every time; the benefit of this type only appears from the
    /// second call onward.
    pub fn new() -> Self {
        Self {
            cache: None,
            max_output: None,
            #[cfg(test)]
            last_reuses: 0,
        }
    }

    /// Cap the total uncompressed size of *each* stream decoded through
    /// this context.
    ///
    /// This is independent of, and in addition to, the `dst` bound every
    /// [`Self::decompress_into`] call already enforces: the effective cap
    /// for a given call is `min(max_output, dst.len())`. Setting it lower
    /// than `dst.len()` is only useful when `dst` is a large, reusable
    /// scratch buffer and the caller wants a tighter, earlier bomb-detection
    /// threshold than the buffer's own size would give.
    ///
    /// The cap is checked after every LZMA2 chunk, not after a stream has
    /// been fully expanded, matching [`XzReader::with_max_output`].
    #[must_use]
    pub fn with_max_output(mut self, max_output: u64) -> Self {
        self.max_output = Some(max_output);
        self
    }

    /// Decompress one complete `.xz` stream directly into a caller-supplied
    /// buffer, reusing this context's LZMA2 decoder when possible.
    ///
    /// Same contract as the free function
    /// [`decompress_into`](super::decompress_into): `src` must be a
    /// complete `.xz` stream, the return value is the number of bytes
    /// written to `dst` (which may be fewer than `dst.len()`), and a stream
    /// that would expand past `dst` (or past a configured
    /// [`Self::with_max_output`] cap, if smaller) is rejected *during*
    /// decoding with [`OxiArcError::BufferTooSmall`] rather than expanded
    /// first. The only difference is that repeated calls on the same
    /// `XzDecoder` reuse the LZMA2 dictionary buffer, probability model and
    /// coder state whenever the stream's declared dictionary size matches
    /// the one already cached — see the [module documentation](self).
    ///
    /// # Errors
    ///
    /// - [`OxiArcError::BufferTooSmall`] if the stream expands past `dst`
    ///   (or the configured cap).
    /// - The usual framing/CRC errors for a corrupt or truncated stream.
    ///
    /// A failed call still keeps whatever LZMA2 decoder this context had
    /// cached (or had just allocated), so a single bad stream in a longer
    /// sequence does not force every later call to pay for a fresh
    /// allocation again.
    pub fn decompress_into(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize> {
        let cap = match self.max_output {
            Some(configured) => configured.min(dst.len() as u64),
            None => dst.len() as u64,
        };

        let mut reader = XzReader::new(std::io::Cursor::new(src))?.with_max_output(cap);
        reader.install_lzma2_cache(self.cache.take());
        let outcome = reader.decompress();
        self.cache = reader.take_lzma2_cache();
        #[cfg(test)]
        {
            self.last_reuses = reader.lzma2_reuses();
        }

        // Same reporting as the free function `decompress_into`: a budget
        // trip becomes `BufferTooSmall` (the cap here always derives from
        // `dst`, whether or not `with_max_output` narrowed it further), and
        // an oversized-but-otherwise-valid result is still checked against
        // `dst` explicitly since the running check inside decode only fires
        // once the *running total* exceeds the cap, not once the final
        // size is known to.
        let data = outcome.map_err(|err| match err {
            OxiArcError::MemoryBudgetExceeded { requested, .. } => OxiArcError::BufferTooSmall {
                needed: requested,
                available: dst.len(),
            },
            other => other,
        })?;
        if data.len() > dst.len() {
            return Err(OxiArcError::BufferTooSmall {
                needed: data.len(),
                available: dst.len(),
            });
        }
        dst[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    /// Discard this context's cached LZMA2 decoder.
    ///
    /// The next [`Self::decompress_into`] call allocates a fresh one, as if
    /// this were a brand-new `XzDecoder`. Any [`Self::with_max_output`]
    /// configuration is unaffected — that is a setting, not decode state.
    ///
    /// There is no correctness reason to call this between ordinary
    /// streams (a stream that declares a different dictionary size already
    /// triggers a fresh allocation on its own, and one that matches is
    /// exactly the case this type exists to make cheap); it exists for a
    /// caller that wants to release the cached buffer's memory before a
    /// long pause, or that wants a hard guarantee of no shared state across
    /// two batches of decoding.
    pub fn reset(&mut self) {
        self.cache = None;
    }

    /// How many blocks the most recent [`Self::decompress_into`] call
    /// decoded with a reused LZMA2 decoder rather than a fresh one.
    ///
    /// See [`Self::last_reuses`] for why this exists.
    #[cfg(test)]
    fn last_reuses(&self) -> u32 {
        self.last_reuses
    }
}

impl Default for XzDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xz;

    /// Deterministic pseudo-random bytes, mirroring `header`'s test helper
    /// (kept local rather than made `pub(super)` across two files just for
    /// this one use).
    fn xorshift_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn new_then_default_are_equivalent() {
        // Both start with an empty cache and no output cap; exercised via
        // one decode each rather than by inspecting private fields.
        let payload = b"equivalence check";
        let stream = xz::compress(payload, 3).expect("compress");
        let mut a = XzDecoder::new();
        let mut b = XzDecoder::default();
        let mut out_a = vec![0u8; payload.len()];
        let mut out_b = vec![0u8; payload.len()];
        let written_a = a.decompress_into(&stream, &mut out_a).expect("decode a");
        let written_b = b.decompress_into(&stream, &mut out_b).expect("decode b");
        assert_eq!(&out_a[..written_a], &out_b[..written_b]);
        assert_eq!(&out_a[..written_a], payload);
    }

    #[test]
    fn single_stream_matches_the_free_function() {
        let payload = xorshift_bytes(0x5EED, 40_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut via_free_function = vec![0u8; payload.len()];
        let free_written = xz::decompress_into(&stream, &mut via_free_function)
            .expect("free-function decompress_into");

        let mut decoder = XzDecoder::new();
        let mut via_decoder = vec![0u8; payload.len()];
        let decoder_written = decoder
            .decompress_into(&stream, &mut via_decoder)
            .expect("XzDecoder::decompress_into");

        assert_eq!(free_written, decoder_written);
        assert_eq!(via_free_function, via_decoder);
        assert_eq!(via_decoder, payload);
    }

    #[test]
    fn reused_decoder_matches_a_fresh_decoder_every_call() {
        // The correctness gate: decode the same set of streams once through
        // a single, reused `XzDecoder`, and once more with a brand-new
        // `XzDecoder` per stream (the "always fresh" baseline), and assert
        // byte-identical output at every step. Includes a dictionary-size
        // mismatch alternating within the reused run (level 1 -> 256 KiB
        // vs level 6 -> 8 MiB).
        //
        // That alternation alone does **not** prove the cache's
        // `dict_size` key is load-bearing, and this comment used to claim
        // it did: every payload here is far smaller than the *smallest*
        // dictionary in the sequence, so no match distance ever reaches
        // past the cached ring's capacity and reusing a wrongly-sized
        // decoder would still produce the right bytes. Verified by
        // mutation: deleting the `*cached_dict_size == dict_size` term
        // left this test (and the whole suite) green.
        // `a_bigger_dictionary_is_never_decoded_against_a_smaller_cached_ring`
        // below is the test that actually pins it.
        let payloads: Vec<(Vec<u8>, u8)> = vec![
            (xorshift_bytes(1, 3_000), 6),
            (xorshift_bytes(2, 70_000), 6),
            (
                b"small and highly compressible AAAAAAAAAAAAAAAAAAAA".to_vec(),
                1,
            ),
            (xorshift_bytes(3, 70_000), 6),
            (vec![], 6),
            (xorshift_bytes(4, 500), 1),
            (xorshift_bytes(5, 200_000), 9),
            (xorshift_bytes(6, 1), 6),
            (xorshift_bytes(7, 70_000), 6),
        ];

        let streams: Vec<Vec<u8>> = payloads
            .iter()
            .map(|(data, level)| xz::compress(data, *level).expect("compress"))
            .collect();

        let mut reused = XzDecoder::new();
        for (index, stream) in streams.iter().enumerate() {
            let (payload, _level) = &payloads[index];
            let mut reused_out = vec![0u8; payload.len()];
            let reused_written = reused
                .decompress_into(stream, &mut reused_out)
                .unwrap_or_else(|err| panic!("reused decode of stream {index} failed: {err}"));

            let mut fresh = XzDecoder::new();
            let mut fresh_out = vec![0u8; payload.len()];
            let fresh_written = fresh
                .decompress_into(stream, &mut fresh_out)
                .unwrap_or_else(|err| panic!("fresh decode of stream {index} failed: {err}"));

            assert_eq!(
                reused_written, fresh_written,
                "stream {index}: reused vs fresh length mismatch"
            );
            assert_eq!(
                reused_out, fresh_out,
                "stream {index}: reused vs fresh bytes mismatch"
            );
            assert_eq!(
                &reused_out[..reused_written],
                payload.as_slice(),
                "stream {index}: reused output does not match the original payload"
            );
        }
    }

    #[test]
    fn thousand_strip_shaped_reuse_is_byte_identical() {
        // The named target shape from the design brief: many same-size
        // ("strip"-like) payloads through one context. Kept well under
        // 1000 real streams for test runtime (the criterion bench covers
        // the full 1000 x 64 KiB shape); this proves correctness at a
        // representative count and a representative size.
        let strip = xorshift_bytes(0x5713, 64 * 1024);
        let stream = xz::compress(&strip, 6).expect("compress a 64 KiB strip");

        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; strip.len()];
        for iteration in 0..200 {
            let written = decoder
                .decompress_into(&stream, &mut buf)
                .unwrap_or_else(|err| panic!("iteration {iteration} failed: {err}"));
            assert_eq!(written, strip.len());
            assert_eq!(buf, strip, "iteration {iteration} produced different bytes");
        }
    }

    #[test]
    fn reset_forces_a_fresh_decoder_without_changing_output() {
        let payload = xorshift_bytes(0xA5A5, 50_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; payload.len()];
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("first decode");
        assert_eq!(buf, payload);

        decoder.reset();

        // Still correct after a reset -- it only discards the cache, not
        // any configuration, and the next call must allocate fresh.
        buf.fill(0xEE);
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("decode after reset");
        assert_eq!(buf, payload);
    }

    #[test]
    fn with_max_output_survives_reset() {
        // `reset()`'s rustdoc claims a configured `with_max_output` cap is
        // unaffected -- it is a setting, not decode state. Make that a
        // tested contract, not just an aspiration: a cap configured before
        // `reset()` must still be enforced after it.
        let payload = xorshift_bytes(0x7E5E7, 100_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut decoder = XzDecoder::new().with_max_output(1024);
        let mut buf = vec![0u8; payload.len()];

        let before = decoder
            .decompress_into(&stream, &mut buf)
            .expect_err("a 1 KiB cap must reject a ~100 KB stream before reset");
        assert!(matches!(before, OxiArcError::BufferTooSmall { .. }));

        decoder.reset();

        let after = decoder
            .decompress_into(&stream, &mut buf)
            .expect_err("the same 1 KiB cap must still apply after reset");
        assert!(matches!(after, OxiArcError::BufferTooSmall { .. }));
    }

    #[test]
    fn with_max_output_narrower_than_dst_still_caps() {
        let payload = xorshift_bytes(0x9, 100_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut decoder = XzDecoder::new().with_max_output(1024);
        let mut buf = vec![0u8; payload.len()]; // dst is generously sized...
        let err = decoder
            .decompress_into(&stream, &mut buf)
            .expect_err("a 1 KiB cap must reject a ~100 KB stream even with a large dst");
        assert!(
            matches!(err, OxiArcError::BufferTooSmall { .. }),
            "expected BufferTooSmall, got {err:?}"
        );
    }

    #[test]
    fn dst_too_small_is_rejected_and_decoder_stays_usable() {
        let payload = xorshift_bytes(0x10, 10_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut decoder = XzDecoder::new();
        let mut tiny = vec![0u8; 4];
        let err = decoder
            .decompress_into(&stream, &mut tiny)
            .expect_err("4-byte dst must reject a 10000-byte stream");
        assert!(matches!(err, OxiArcError::BufferTooSmall { .. }));

        // The context must still work for a subsequent, correctly-sized
        // call -- a failed call must not have poisoned or dropped the
        // cache in a way that breaks later use.
        let mut buf = vec![0u8; payload.len()];
        let written = decoder
            .decompress_into(&stream, &mut buf)
            .expect("decode after a prior BufferTooSmall failure");
        assert_eq!(&buf[..written], payload.as_slice());
    }

    #[test]
    fn a_second_stream_missing_its_dictionary_reset_is_rejected_the_same_way_reused_or_fresh() {
        // The safety property the reuse cache depends on: a block that
        // does not open with a dictionary-reset chunk must be rejected
        // identically whether the decoder that decodes it is freshly
        // allocated or was reused from a previous, unrelated stream. This
        // is the cross-call path `XzDecoder` introduces (as opposed to a
        // hand-built multi-block stream, which only exercises reuse
        // *within* one `XzReader::decompress()` call).
        let good_payload = xorshift_bytes(0x2222, 5_000);
        let good_stream = xz::compress(&good_payload, 6).expect("compress good stream");

        // Hand-build a malformed second "stream": a valid XZ stream header
        // plus a block whose LZMA2 payload starts with an uncompressed
        // chunk (control 0x02) that does *not* reset the dictionary,
        // followed immediately by the end-of-stream marker (0x00). This
        // constructs the framing directly so the chunk shape itself is
        // otherwise perfectly legal LZMA2 -- only "opens without a
        // dictionary reset" is wrong, which is exactly the shape a stale
        // reused decoder would otherwise decode instead of reject.
        let malformed = build_single_block_xz_with_raw_lzma2(&[
            0x02, 0x00, 0x01, b'h', b'i', // uncompressed chunk, no reset, payload "hi"
            0x00, // end of LZMA2 stream
        ]);

        let mut decoder = XzDecoder::new();

        // Prime the cache with a real, valid stream first.
        let mut good_out = vec![0u8; good_payload.len()];
        decoder
            .decompress_into(&good_stream, &mut good_out)
            .expect("priming decode");
        assert_eq!(good_out, good_payload);

        // Now decode the malformed stream through the *reused* decoder.
        let mut scratch = vec![0u8; 16];
        let reused_err = decoder
            .decompress_into(&malformed, &mut scratch)
            .expect_err("a non-resetting first chunk must be rejected (reused decoder)");

        // And once more through a brand-new decoder that never cached
        // anything, so its `Lzma2Decoder` is fresh.
        let mut fresh = XzDecoder::new();
        let fresh_err = fresh
            .decompress_into(&malformed, &mut scratch)
            .expect_err("a non-resetting first chunk must be rejected (fresh decoder)");

        assert_eq!(
            reused_err.to_string(),
            fresh_err.to_string(),
            "reused and fresh decoders must reject the same malformed block identically"
        );
        assert!(
            matches!(reused_err, OxiArcError::InvalidHeader { .. }),
            "expected InvalidHeader, got {reused_err:?}"
        );
    }

    /// Hand-assemble a `CheckType::None` `.xz` stream whose blocks carry
    /// exactly the given raw LZMA2 payloads (not run through the real LZMA2
    /// encoder), so a specific, otherwise-illegal chunk shape can be placed
    /// at the very start of any block. Self-contained (does not reach into
    /// `XzWriter`'s private helpers from this sibling module): every field
    /// is built from the format's own public constants, mirroring what
    /// `XzWriter::write_block` / `write_index` / `write_stream_footer` do
    /// internally.
    ///
    /// Every block header declares neither optional size field (flags byte
    /// `0x00`), so the reader takes the self-describing `decompress_block`
    /// path -- the shape real `xz`-produced streams use. The index records
    /// carry a real unpadded size but a placeholder uncompressed size,
    /// which the reader deliberately does not cross-check (only the record
    /// *count* and the index CRC-32 are validated).
    ///
    /// Every block declares the smallest LZMA2 dictionary (properties byte
    /// `0x00`, 4 KiB) -- the shape every other test in this module needs.
    /// [`build_xz_with_raw_lzma2_blocks_and_dict_props`] is the general form
    /// for a test that needs to declare a specific (e.g. oversized)
    /// dictionary-size properties byte instead.
    fn build_xz_with_raw_lzma2_blocks(blocks: &[&[u8]]) -> Vec<u8> {
        build_xz_with_raw_lzma2_blocks_and_dict_props(blocks, 0x00)
    }

    /// As [`build_xz_with_raw_lzma2_blocks`], but with an explicit LZMA2
    /// dictionary-size properties byte for every block instead of the
    /// hardcoded smallest dictionary.
    ///
    /// Used by
    /// [`a_huge_declared_dictionary_size_is_rejected_identically_reused_or_fresh`]
    /// to build a block that declares an oversized dictionary (properties
    /// byte 40, `dict_size_from_props(40) == 0xFFFF_FFFF`) without needing a
    /// real LZMA2 stream at that dictionary size -- the block must be
    /// rejected while parsing the header, before any dictionary is
    /// allocated, so what the LZMA2 payload actually contains is
    /// irrelevant.
    fn build_xz_with_raw_lzma2_blocks_and_dict_props(blocks: &[&[u8]], dict_props: u8) -> Vec<u8> {
        use crate::xz::header::{FILTER_LZMA2, XZ_FOOTER_MAGIC, XZ_MAGIC};
        use oxiarc_core::crc::Crc32;

        fn write_multibyte_int(output: &mut Vec<u8>, mut value: u64) {
            loop {
                let byte = (value & 0x7F) as u8;
                value >>= 7;
                if value == 0 {
                    output.push(byte);
                    break;
                }
                output.push(byte | 0x80);
            }
        }

        let mut output = Vec::new();

        // Stream header: magic + flags (reserved 0x00, CheckType::None is
        // check-id 0x00) + CRC32 over the flags.
        let flags_bytes = [0x00u8, 0x00u8];
        output.extend_from_slice(&XZ_MAGIC);
        output.extend_from_slice(&flags_bytes);
        output.extend_from_slice(&Crc32::compute(&flags_bytes).to_le_bytes());

        let mut unpadded_sizes = Vec::with_capacity(blocks.len());
        for raw_lzma2 in blocks {
            // Block header: 1 filter (LZMA2), no optional size fields.
            let mut block_header = vec![0x00u8]; // flags
            block_header.push(FILTER_LZMA2 as u8); // filter id
            block_header.push(0x01); // properties size
            block_header.push(dict_props); // dict-size properties byte
            // Same derivation `XzWriter::write_block` uses: the smallest
            // size_byte such that (size_byte + 1) * 4 >= content.len() + 1 + 4.
            let header_size_byte = ((block_header.len() + 4) / 4) as u8;
            let total_header_size = (header_size_byte as usize + 1) * 4;
            let header_padding = total_header_size - 1 - block_header.len() - 4;
            block_header.resize(block_header.len() + header_padding, 0x00);
            let mut header_crc_input = Vec::with_capacity(1 + block_header.len());
            header_crc_input.push(header_size_byte);
            header_crc_input.extend_from_slice(&block_header);
            let header_crc = Crc32::compute(&header_crc_input);

            output.push(header_size_byte);
            output.extend_from_slice(&block_header);
            output.extend_from_slice(&header_crc.to_le_bytes());

            // Compressed data: the caller's raw LZMA2 bytes, padded to a
            // 4-byte boundary. `CheckType::None` means no check bytes follow.
            output.extend_from_slice(raw_lzma2);
            let payload_padding = (4 - (raw_lzma2.len() % 4)) % 4;
            output.extend(std::iter::repeat_n(0u8, payload_padding));

            unpadded_sizes.push((total_header_size + raw_lzma2.len()) as u64);
        }

        // Index: indicator + one record per block (unpadded size,
        // uncompressed size) + padding + CRC32.
        let mut index = vec![0x00u8]; // index indicator
        write_multibyte_int(&mut index, blocks.len() as u64);
        for unpadded_size in &unpadded_sizes {
            write_multibyte_int(&mut index, *unpadded_size);
            write_multibyte_int(&mut index, 0); // uncompressed size (not cross-checked)
        }
        while (index.len() + 4) % 4 != 0 {
            index.push(0x00);
        }
        index.extend_from_slice(&Crc32::compute(&index).to_le_bytes());
        let index_len = index.len();
        output.extend_from_slice(&index);

        // Stream footer: CRC32 + backward size + flags + magic.
        let backward_size = ((index_len / 4) - 1) as u32;
        let mut footer_data = Vec::new();
        footer_data.extend_from_slice(&backward_size.to_le_bytes());
        footer_data.extend_from_slice(&flags_bytes);
        output.extend_from_slice(&Crc32::compute(&footer_data).to_le_bytes());
        output.extend_from_slice(&backward_size.to_le_bytes());
        output.extend_from_slice(&flags_bytes);
        output.extend_from_slice(&XZ_FOOTER_MAGIC);

        output
    }

    /// Single-block convenience wrapper over
    /// [`build_xz_with_raw_lzma2_blocks`].
    fn build_single_block_xz_with_raw_lzma2(raw_lzma2: &[u8]) -> Vec<u8> {
        build_xz_with_raw_lzma2_blocks(&[raw_lzma2])
    }

    /// One valid single-LZMA-chunk payload plus the same chunk rewritten to
    /// re-use properties it never declares.
    ///
    /// Returns `(payload, valid_lzma2, stale_props_lzma2)`:
    /// * `valid_lzma2` is a complete LZMA2 stream for `payload` -- one LZMA
    ///   chunk with reset field 3 (state + new properties + dictionary
    ///   reset) followed by the end-of-stream marker.
    /// * `stale_props_lzma2` opens with an *uncompressed* chunk that resets
    ///   the dictionary (control `0x01`, so it is a legal block opener and
    ///   the "first chunk must reset the dictionary" rule is satisfied) and
    ///   then repeats the same LZMA chunk with its reset field lowered to 1
    ///   (state reset only) and its properties byte removed. Nothing in that
    ///   payload ever declares LZMA properties, so a decoder that has none
    ///   cached must reject it -- and a decoder that reuses another stream's
    ///   properties must not silently accept it.
    fn stale_props_payloads() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let payload: Vec<u8> = (0..3000u32).map(|index| (index % 251) as u8).collect();
        let valid = crate::lzma2::encode_lzma2(&payload, crate::LzmaLevel::new(6))
            .expect("encode a single-chunk LZMA2 stream");

        assert!(
            valid[0] >= 0x80,
            "expected an LZMA chunk opener, got 0x{:02X}",
            valid[0]
        );
        assert_eq!(
            (valid[0] >> 5) & 0x03,
            3,
            "expected the opener to reset state + properties + dictionary"
        );
        let compressed_size = u16::from_be_bytes([valid[3], valid[4]]) as usize + 1;
        assert_eq!(
            valid.len(),
            // control + 2 size bytes + 2 size bytes + props + data + 0x00
            6 + compressed_size + 1,
            "expected exactly one LZMA chunk plus the end-of-stream marker"
        );

        let mut stale = vec![
            0x01, 0x00, 0x03, 0x00, 0x00, 0x00,
            0x00, // uncompressed chunk, dict reset, 4 zeros
        ];
        stale.push(0xA0 | (valid[0] & 0x1F)); // same chunk, reset field 1 (state only)
        stale.extend_from_slice(&valid[1..5]); // unchanged size fields
        stale.extend_from_slice(&valid[6..6 + compressed_size]); // payload, props byte dropped
        stale.push(0x00); // end of LZMA2 stream

        (payload, valid, stale)
    }

    #[test]
    fn a_later_block_cannot_borrow_an_earlier_block_s_lzma_properties() {
        // Regression: reusing one `Lzma2Decoder` across the blocks of one
        // stream must not let a later block decode with LZMA properties it
        // never declared. Block 2 below opens with an uncompressed chunk
        // that *does* reset the dictionary (control 0x01, so the "first
        // chunk must reset the dictionary" rule is satisfied) and then uses
        // an LZMA chunk with reset field 1 -- state reset, no new
        // properties. Nothing in block 2 ever declares `lc`/`lp`/`pb`, so
        // it must be rejected exactly as it is when it stands alone; a
        // decoder that carried block 1's properties over would instead
        // accept it and produce ~3000 bytes of plausible-looking garbage.
        let (payload, valid, stale) = stale_props_payloads();
        let two_block = build_xz_with_raw_lzma2_blocks(&[&valid, &stale]);
        let solo = build_single_block_xz_with_raw_lzma2(&stale);

        let two_block_err = xz::decompress(&mut std::io::Cursor::new(&two_block))
            .expect_err("a block that never declares LZMA properties must be rejected");
        let solo_err = xz::decompress(&mut std::io::Cursor::new(&solo))
            .expect_err("the same block standing alone must be rejected");

        assert_eq!(
            two_block_err.to_string(),
            solo_err.to_string(),
            "a block must be judged the same whether or not an earlier block \
             primed the reader's decoder cache"
        );
        assert!(
            matches!(two_block_err, OxiArcError::InvalidHeader { .. }),
            "expected InvalidHeader, got {two_block_err:?}"
        );

        // The valid block on its own still decodes, so the test really is
        // about block 2 and not about the hand-built framing being broken.
        let good = xz::decompress(&mut std::io::Cursor::new(
            &build_single_block_xz_with_raw_lzma2(&valid),
        ))
        .expect("the valid block alone must still decode");
        assert_eq!(good, payload);
    }

    #[test]
    fn a_later_stream_cannot_borrow_an_earlier_stream_s_lzma_properties() {
        // The same property one level up: across two separate
        // `XzDecoder::decompress_into` calls.
        let (payload, valid, stale) = stale_props_payloads();
        let priming = build_single_block_xz_with_raw_lzma2(&valid);
        let malformed = build_single_block_xz_with_raw_lzma2(&stale);

        let mut primed = XzDecoder::new();
        let mut buf = vec![0u8; payload.len() + 16];
        let written = primed
            .decompress_into(&priming, &mut buf)
            .expect("priming decode");
        assert_eq!(&buf[..written], payload.as_slice());

        let primed_err = primed
            .decompress_into(&malformed, &mut buf)
            .expect_err("a stream that never declares LZMA properties must be rejected");

        let mut fresh = XzDecoder::new();
        let fresh_err = fresh
            .decompress_into(&malformed, &mut buf)
            .expect_err("...and identically so on a decoder that cached nothing");

        assert_eq!(
            primed_err.to_string(),
            fresh_err.to_string(),
            "priming a decoder must not change which streams it accepts"
        );
        assert!(
            matches!(primed_err, OxiArcError::InvalidHeader { .. }),
            "expected InvalidHeader, got {primed_err:?}"
        );
    }

    /// Deterministic, highly repetitive bytes.
    ///
    /// Unlike [`xorshift_bytes`], this pattern is compressible enough that
    /// the real single-chunk LZMA2 encoder (`Lzma2Encoder::encode`, the
    /// path every stream at or under 64 KiB and under the 2 MiB chunk limit
    /// takes) picks an LZMA-coded chunk over falling back to an
    /// uncompressed "stored" chunk (`lzma2.rs`'s `compressed.len() >=
    /// data.len()` check) -- which matters here because only an LZMA-coded
    /// chunk's reset field can be `3` (state + new properties + dictionary),
    /// the shape [`block_opener_permits_decoder_reuse`] requires before it
    /// will let a block reuse a cached decoder. A stored chunk always fails
    /// that check by construction (see the dedicated `stored_only` /
    /// `matching_dict` case a few lines below), so a test that wants to
    /// prove reuse actually happens for a stream built through the public
    /// [`xz::compress`] / [`crate::xz::XzWriter`] API -- not just through
    /// this module's hand-built raw-LZMA2 blocks -- needs data the real
    /// encoder chooses to LZMA-code, not store.
    fn compressible_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn reuse_actually_happens_for_real_xz_streams() {
        // Reuse is unobservable from output and errors by design, so
        // without this test a broken reuse predicate would turn the cache
        // into dead weight and every other test here would still pass.
        let payload = compressible_bytes(40_000);
        let stream = xz::compress(&payload, 6).expect("compress");

        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; payload.len()];
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("first decode");
        assert_eq!(
            decoder.last_reuses(),
            0,
            "the first call has nothing cached to reuse"
        );
        for call in 1..4 {
            decoder
                .decompress_into(&stream, &mut buf)
                .unwrap_or_else(|err| panic!("call {call} failed: {err}"));
            assert_eq!(
                decoder.last_reuses(),
                1,
                "call {call} must have reused the cached LZMA2 decoder"
            );
        }

        // `reset()` really drops the cache, so the next call cannot reuse.
        decoder.reset();
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("decode after reset");
        assert_eq!(decoder.last_reuses(), 0, "reset() must clear the cache");

        // A stream whose block opens with an uncompressed chunk resets the
        // dictionary but not the LZMA properties, so it must never reuse a
        // cached decoder even though its declared dictionary size matches.
        let stored_only = build_single_block_xz_with_raw_lzma2(&[
            0x01, 0x00, 0x03, b'a', b'b', b'c', b'd', // uncompressed chunk, dict reset
            0x00, // end of LZMA2 stream
        ]);
        let matching_dict =
            build_single_block_xz_with_raw_lzma2(&[0x01, 0x00, 0x03, b'w', b'x', b'y', b'z', 0x00]);
        let mut stored_decoder = XzDecoder::new();
        let mut small = [0u8; 8];
        assert_eq!(
            stored_decoder
                .decompress_into(&matching_dict, &mut small)
                .expect("stored-chunk stream decodes"),
            4
        );
        assert_eq!(
            stored_decoder
                .decompress_into(&stored_only, &mut small)
                .expect("second stored-chunk stream decodes"),
            4
        );
        assert_eq!(&small[..4], b"abcd");
        assert_eq!(
            stored_decoder.last_reuses(),
            0,
            "an uncompressed-chunk opener must not reuse a cached decoder: it \
             resets the dictionary but leaves the LZMA properties behind"
        );

        // Inside one multi-block stream the same rule applies per block,
        // and every block a real encoder writes does open with a full
        // reset -- so an N-block stream reuses N-1 times. Compressible data
        // again, and for the same reason: `write_block` LZMA2-encodes each
        // 16 KiB block independently, and a 16 KiB block of incompressible
        // bytes would itself become a stored chunk, which never reuses by
        // design.
        let big = compressible_bytes(60_000);
        let multi = crate::xz::XzWriter::new(crate::LzmaLevel::new(6))
            .with_block_size(16 * 1024)
            .compress(&big)
            .expect("multi-block compress");
        let mut reader =
            crate::xz::XzReader::new(std::io::Cursor::new(&multi)).expect("read multi-block");
        let decoded = reader.decompress().expect("decode multi-block");
        assert_eq!(decoded, big);
        assert!(
            reader.lzma2_reuses() >= 3,
            "a 60 KB payload at a 16 KiB block size must reuse the decoder for \
             every block after the first, got {}",
            reader.lzma2_reuses()
        );
    }

    /// A payload whose tail repeats its head across an LZMA2 chunk
    /// boundary, so decoding it needs a *dictionary* back-reference longer
    /// than `beyond` bytes (a match that far back cannot be served from the
    /// current chunk's own output).
    ///
    /// The head is compressible (nibble values, ~2:1) on purpose: an
    /// incompressible head would make the encoder open the block with a
    /// stored chunk, which [`block_opener_permits_decoder_reuse`] refuses
    /// to reuse after — and a test of the *dictionary-size* condition must
    /// not be masked by the *opener* condition.
    fn long_back_reference_payload(beyond: usize) -> Vec<u8> {
        let head: Vec<u8> = xorshift_bytes(0xBEEF, 96 * 1024)
            .iter()
            .map(|byte| byte & 0x0F)
            .collect();
        let mut payload = Vec::with_capacity(head.len() * 2 + beyond);
        payload.extend_from_slice(&head);
        payload.extend(std::iter::repeat_n(0u8, beyond));
        payload.extend_from_slice(&head);
        payload
    }

    #[test]
    fn a_bigger_dictionary_is_never_decoded_against_a_smaller_cached_ring() {
        // The cache is keyed by dictionary size, and that key is not
        // decoration: a `Lzma2Decoder` built for a 64 KiB dictionary wraps
        // its ring buffer at 64 KiB and caps `dict_len` there, so a stream
        // that legitimately matches ~300 KiB back would be *rejected*
        // ("match distance exceeds dictionary contents") if it were decoded
        // by that cached decoder instead of a correctly-sized fresh one.
        //
        // Proven by mutation, not assumed: with the `*cached_dict_size ==
        // dict_size` term removed, this exact sequence fails with
        // `Corrupted data ...: match distance exceeds dictionary contents`
        // at offset 298304. With the term in place it decodes byte-for-byte.
        let payload = long_back_reference_payload(200_000);

        // Level 0 -> 64 KiB dictionary; level 1 -> 256 KiB. The tail match
        // reaches ~296 KiB back: past the 64 KiB ring, inside the 256 KiB one.
        let small_dict_stream = xz::compress(
            &xorshift_bytes(3, 3_000)
                .iter()
                .map(|byte| byte & 0x0F)
                .collect::<Vec<u8>>(),
            0,
        )
        .expect("compress a 64 KiB-dictionary stream");
        let big_dict_stream =
            xz::compress(&payload, 1).expect("compress a 256 KiB-dictionary stream");

        // Baseline: a decoder that cached nothing decodes it correctly.
        let mut fresh = XzDecoder::new();
        let mut fresh_out = vec![0u8; payload.len()];
        let fresh_written = fresh
            .decompress_into(&big_dict_stream, &mut fresh_out)
            .expect("a fresh decoder must decode the long-back-reference stream");
        assert_eq!(&fresh_out[..fresh_written], payload.as_slice());

        // Now prime the same context with a *smaller*-dictionary stream and
        // decode the big one again. It must still be byte-identical...
        let mut primed = XzDecoder::new();
        let mut scratch = vec![0u8; 3_000];
        primed
            .decompress_into(&small_dict_stream, &mut scratch)
            .expect("priming decode");
        let mut primed_out = vec![0u8; payload.len()];
        let primed_written = primed
            .decompress_into(&big_dict_stream, &mut primed_out)
            .expect(
                "a 256 KiB-dictionary stream must not be decoded against a cached \
                 64 KiB ring",
            );
        assert_eq!(primed_written, fresh_written);
        assert_eq!(primed_out, fresh_out);

        // ...and it must have got there by *refusing* the cache, not by
        // luck: the dictionary sizes differ, so no block may have reused.
        assert_eq!(
            primed.last_reuses(),
            0,
            "a dictionary-size mismatch must force a fresh decoder"
        );

        // The other direction (large cached, small declared) is safe on its
        // own — a bigger ring can serve any distance a smaller one can —
        // but the key refuses it too, so assert the observable half:
        // correct bytes, no reuse.
        let small_payload = xorshift_bytes(0x5A5A, 20_000);
        let small_again = xz::compress(&small_payload, 0).expect("compress small again");
        let mut small_out = vec![0u8; small_payload.len()];
        let small_written = primed
            .decompress_into(&small_again, &mut small_out)
            .expect("a smaller-dictionary stream after a bigger one must decode");
        assert_eq!(&small_out[..small_written], small_payload.as_slice());
        assert_eq!(
            primed.last_reuses(),
            0,
            "a dictionary-size mismatch must force a fresh decoder in this direction too"
        );

        // And a second stream at the *same* dictionary size does reuse, so
        // the assertions above are about the size key and not about reuse
        // being switched off wholesale.
        let same_again = xz::compress(&payload, 1).expect("compress the same shape again");
        let mut same_out = vec![0u8; payload.len()];
        primed
            .decompress_into(&big_dict_stream, &mut same_out)
            .expect("re-prime at 256 KiB");
        primed
            .decompress_into(&same_again, &mut same_out)
            .expect("a matching dictionary size must decode");
        assert_eq!(same_out, fresh_out);
        assert_eq!(
            primed.last_reuses(),
            1,
            "a matching dictionary size must reuse the cached decoder"
        );
    }

    #[test]
    fn truncation_at_every_offset_through_a_reused_decoder_never_panics() {
        // Every prefix of a valid stream must fail cleanly (or, for the
        // rare prefix that is itself a complete stream, succeed) without
        // panicking, hanging, or poisoning the shared decoder cache.
        let payload = xorshift_bytes(0x7A11, 20_000);
        let stream = xz::compress(&payload, 6).expect("compress");
        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; payload.len()];
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("prime the cache");

        // Bounded call count: sample ~256 offsets across the stream plus
        // every offset in the first 64 bytes (all the framing lives there).
        let step = (stream.len() / 256).max(1);
        let offsets = (0..64.min(stream.len())).chain((0..stream.len()).step_by(step));
        for end in offsets {
            let _ = decoder.decompress_into(&stream[..end], &mut buf);
        }

        let written = decoder
            .decompress_into(&stream, &mut buf)
            .expect("the decoder must still work after every truncated input");
        assert_eq!(&buf[..written], payload.as_slice());
    }

    #[test]
    fn byte_flips_drops_and_inserts_through_a_reused_decoder_never_panic() {
        let payload = xorshift_bytes(0x517E, 8_000);
        let stream = xz::compress(&payload, 6).expect("compress");
        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; payload.len()];
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("prime the cache");

        let step = (stream.len() / 96).max(1);
        for offset in (0..stream.len()).step_by(step) {
            let mut flipped = stream.clone();
            flipped[offset] ^= 0xFF;
            let _ = decoder.decompress_into(&flipped, &mut buf);

            let mut dropped = stream.clone();
            dropped.remove(offset);
            let _ = decoder.decompress_into(&dropped, &mut buf);

            let mut inserted = stream.clone();
            inserted.insert(offset, 0x5A);
            let _ = decoder.decompress_into(&inserted, &mut buf);
        }

        let written = decoder
            .decompress_into(&stream, &mut buf)
            .expect("the decoder must still work after every corrupted input");
        assert_eq!(&buf[..written], payload.as_slice());
    }

    #[test]
    fn a_primed_context_and_a_fresh_one_agree_on_every_corrupted_stream() {
        // The invariant the whole design rests on: reuse must be
        // *unobservable*. Three tests above assert that for specific
        // hand-built malformed shapes; this generalises it to a systematic
        // differential -- every truncation, bit flip, byte drop and byte
        // insert at a sampled offset, decoded once by a context that cached
        // nothing and once by a context primed with an unrelated stream.
        // Both must agree on success-or-failure, on the bytes produced, and
        // on the exact error text (which carries stream offsets, so a
        // decoder carrying counters across streams would show up here even
        // though it produces the right bytes).
        fn outcome(decoder: &mut XzDecoder, src: &[u8], capacity: usize) -> String {
            let mut buffer = vec![0u8; capacity];
            match decoder.decompress_into(src, &mut buffer) {
                Ok(written) => format!(
                    "ok:{written}:{:08x}",
                    oxiarc_core::crc::Crc32::compute(&buffer[..written])
                ),
                Err(err) => format!("err:{err}"),
            }
        }

        let payload = compressible_bytes(30_000);
        let priming = xz::compress(&compressible_bytes(9_000), 6).expect("priming stream");

        for check in [
            crate::xz::CheckType::None,
            crate::xz::CheckType::Crc32,
            crate::xz::CheckType::Sha256,
        ] {
            for block_size in [u64::MAX, 4096] {
                let stream = crate::xz::XzWriter::new(crate::LzmaLevel::new(6))
                    .with_check_type(check)
                    .with_block_size(block_size)
                    .compress(&payload)
                    .expect("compress");

                let step = (stream.len() / 40).max(1);
                for cut in (0..stream.len()).step_by(step) {
                    let mut variants: Vec<Vec<u8>> = Vec::with_capacity(4);
                    variants.push(stream[..cut].to_vec());
                    let mut flipped = stream.clone();
                    flipped[cut] ^= 0xFF;
                    variants.push(flipped);
                    let mut dropped = stream.clone();
                    dropped.remove(cut);
                    variants.push(dropped);
                    let mut inserted = stream.clone();
                    inserted.insert(cut, 0x5A);
                    variants.push(inserted);

                    for (index, variant) in variants.iter().enumerate() {
                        let mut fresh = XzDecoder::new();
                        let fresh_outcome = outcome(&mut fresh, variant, payload.len() + 64);

                        let mut primed = XzDecoder::new();
                        let mut warm = vec![0u8; 9_000];
                        primed
                            .decompress_into(&priming, &mut warm)
                            .expect("priming decode");
                        let primed_outcome = outcome(&mut primed, variant, payload.len() + 64);

                        assert_eq!(
                            fresh_outcome, primed_outcome,
                            "{check:?}/block_size {block_size}: variant {index} at offset \
                             {cut} was judged differently by a primed context"
                        );
                    }
                }
            }
        }

        // Second phase, and the one that makes this differential able to
        // fail rather than merely able to pass: a payload with a
        // back-reference reaching ~296 KiB back, primed by a stream that
        // declares a *64 KiB* dictionary. If the cache ever stopped keying
        // on dictionary size, the primed context would decode this against
        // a 64 KiB ring and reject a perfectly valid stream, while the
        // fresh one accepts it -- a divergence in exactly the shape this
        // test is looking for. (Verified: with that key removed, this phase
        // fails; the first phase does not, because none of its payloads is
        // larger than the smallest dictionary involved.)
        let long_payload = long_back_reference_payload(200_000);
        let long_stream =
            xz::compress(&long_payload, 1).expect("compress the long-reference payload");
        let small_dict_priming = xz::compress(&compressible_bytes(3_000), 0)
            .expect("compress a 64 KiB-dictionary priming stream");

        let step = (long_stream.len() / 10).max(1);
        for cut in (0..long_stream.len()).step_by(step) {
            let mut variants: Vec<Vec<u8>> = Vec::with_capacity(3);
            variants.push(long_stream.clone());
            variants.push(long_stream[..cut].to_vec());
            let mut flipped = long_stream.clone();
            flipped[cut] ^= 0xFF;
            variants.push(flipped);

            for (index, variant) in variants.iter().enumerate() {
                let mut fresh = XzDecoder::new();
                let fresh_outcome = outcome(&mut fresh, variant, long_payload.len() + 64);

                let mut primed = XzDecoder::new();
                let mut warm = vec![0u8; 3_000];
                primed
                    .decompress_into(&small_dict_priming, &mut warm)
                    .expect("small-dictionary priming decode");
                let primed_outcome = outcome(&mut primed, variant, long_payload.len() + 64);

                assert_eq!(
                    fresh_outcome, primed_outcome,
                    "long-reference variant {index} at offset {cut} was judged \
                     differently by a context primed at a smaller dictionary size"
                );
            }
        }
    }

    #[test]
    fn combined_random_mutations_never_panic_hang_or_wedge_the_context() {
        // The systematic adversarial tests above each apply exactly one
        // mutation of one kind to one stream shape. This one is the
        // combination sweep they do not cover: 1-4 mutations of mixed kinds
        // (bit flip / byte drop / byte insert / truncate) at random offsets,
        // across every check type, single- and multi-block streams, and a
        // two-stream concatenation, with the destination buffer itself
        // varying between 0, 1, 1024 bytes and generously large.
        //
        // Deterministic (fixed seed, own xorshift): it is a bounded sweep,
        // not a fuzzer, so it can live in the normal suite without ever
        // being flaky. The invariants are: no panic, no hang, an `Ok` is
        // never silent truncation, and — checked *during* the sweep, not
        // only after it — an intact stream fed to the same context still
        // decodes byte-for-byte, so a poisoned cache cannot hide until the
        // end.
        fn next(state: &mut u64) -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        }

        let payload = compressible_bytes(50_000);
        let mut seeds: Vec<Vec<u8>> = Vec::new();
        for check in [
            crate::xz::CheckType::None,
            crate::xz::CheckType::Crc32,
            crate::xz::CheckType::Crc64,
            crate::xz::CheckType::Sha256,
        ] {
            for block_size in [u64::MAX, 4096] {
                seeds.push(
                    crate::xz::XzWriter::new(crate::LzmaLevel::new(6))
                        .with_check_type(check)
                        .with_block_size(block_size)
                        .compress(&payload)
                        .expect("compress a seed stream"),
                );
            }
        }
        let mut concatenated = seeds[0].clone();
        concatenated.extend_from_slice(&seeds[1]);
        seeds.push(concatenated);

        let mut state = 0xDEAD_BEEF_CAFE_1234u64;
        let mut decoder = XzDecoder::new();
        let mut scratch = vec![0u8; payload.len() * 2 + 64];

        for iteration in 0..8_000u32 {
            let seed = &seeds[(next(&mut state) as usize) % seeds.len()];
            let mut mutated = seed.clone();
            let mutations = 1 + (next(&mut state) % 4);
            for _ in 0..mutations {
                if mutated.is_empty() {
                    break;
                }
                let position = (next(&mut state) as usize) % mutated.len();
                match next(&mut state) % 4 {
                    0 => mutated[position] ^= (next(&mut state) % 256) as u8,
                    1 => {
                        mutated.remove(position);
                    }
                    2 => mutated.insert(position, (next(&mut state) % 256) as u8),
                    _ => mutated.truncate(position),
                }
            }
            let capacity = match next(&mut state) % 4 {
                0 => 0,
                1 => 1,
                2 => 1024,
                _ => scratch.len(),
            };
            if let Ok(written) = decoder.decompress_into(&mutated, &mut scratch[..capacity]) {
                // A mutated stream is allowed to still decode (the mutation
                // may have landed somewhere inert), but only to something
                // that fits what it claimed -- never a silently short read
                // past the end of the destination.
                assert!(
                    written <= capacity,
                    "iteration {iteration}: reported {written} bytes into a \
                     {capacity}-byte destination"
                );
            }

            // Every so often, prove the context is still healthy rather
            // than waiting until after 8000 hostile inputs to find out.
            if iteration % 512 == 0 {
                let mut intact = vec![0u8; payload.len()];
                let written = decoder
                    .decompress_into(&seeds[0], &mut intact)
                    .unwrap_or_else(|err| {
                        panic!("iteration {iteration}: intact stream stopped decoding: {err}")
                    });
                assert_eq!(
                    &intact[..written],
                    payload.as_slice(),
                    "iteration {iteration}: intact stream decoded to different bytes"
                );
            }
        }
    }

    #[test]
    fn a_one_byte_destination_is_rejected_and_the_decoder_stays_usable() {
        let payload = xorshift_bytes(0x1B17, 5_000);
        let stream = xz::compress(&payload, 6).expect("compress");
        let mut decoder = XzDecoder::new();
        let mut buf = vec![0u8; payload.len()];
        decoder
            .decompress_into(&stream, &mut buf)
            .expect("prime the cache");

        let mut one = [0u8; 1];
        let err = decoder
            .decompress_into(&stream, &mut one)
            .expect_err("a one-byte destination cannot hold 5000 bytes");
        assert!(
            matches!(err, OxiArcError::BufferTooSmall { .. }),
            "expected BufferTooSmall, got {err:?}"
        );
        // An empty destination is the degenerate case of the same bound.
        let mut none: [u8; 0] = [];
        let err = decoder
            .decompress_into(&stream, &mut none)
            .expect_err("a zero-byte destination cannot hold 5000 bytes");
        assert!(
            matches!(err, OxiArcError::BufferTooSmall { .. }),
            "expected BufferTooSmall, got {err:?}"
        );

        let written = decoder
            .decompress_into(&stream, &mut buf)
            .expect("the decoder must still work afterwards");
        assert_eq!(&buf[..written], payload.as_slice());
    }

    #[test]
    fn a_huge_declared_dictionary_size_is_rejected_identically_reused_or_fresh() {
        // Properties byte 40 declares a 4 GiB - 1 dictionary, far past the
        // decoder's allocation cap. The block must be refused before
        // anything is allocated, and the refusal must not depend on
        // whether the decoder was primed by an earlier stream.
        let (payload, valid, _stale) = stale_props_payloads();
        let priming = build_single_block_xz_with_raw_lzma2(&valid);
        let huge = build_xz_with_raw_lzma2_blocks_and_dict_props(&[&valid], 40);

        let mut primed = XzDecoder::new();
        let mut buf = vec![0u8; payload.len() + 16];
        primed
            .decompress_into(&priming, &mut buf)
            .expect("priming decode");
        let primed_err = primed
            .decompress_into(&huge, &mut buf)
            .expect_err("a 4 GiB dictionary must be refused");

        let mut fresh = XzDecoder::new();
        let fresh_err = fresh
            .decompress_into(&huge, &mut buf)
            .expect_err("a 4 GiB dictionary must be refused on a fresh decoder too");

        assert_eq!(primed_err.to_string(), fresh_err.to_string());
        assert!(
            primed_err.to_string().contains("dictionary size"),
            "expected a dictionary-size refusal, got {primed_err}"
        );
    }
}
