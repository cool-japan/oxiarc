//! [`StreamWriter`]: a [`Write`] adapter over one frame's pixel data, for
//! images too large to hold in memory at once.
//!
//! Bytes accumulate into a one-scanline buffer; once a whole row has
//! arrived it is filtered and fed into the same continuous zlib stream
//! (the private `zlib::FrameEncoder`) [`super::Writer::write_image_data`]
//! uses, so the two paths compress identically and produce byte-identical
//! `IDAT`/`fdAT` **payload bytes** for the same pixels and options.
//!
//! **Chunk *boundaries* are a separate matter and, by default, do not
//! match.** The private `StreamWriter::new`'s internal flush granularity is
//! `min(idat_chunk_size, buf_len)`, where `buf_len` is 4 KiB by default —
//! [`DEFAULT_BUFFER_LENGTH`], matching `png` 0.18's own `StreamWriter`
//! default — while [`super::Writer::write_image_data`] flushes at
//! `idat_chunk_size` alone, 64 KiB by default. This is deliberate, not an
//! oversight: a caller who explicitly picks a small `buf_len` (via
//! [`super::Writer::stream_writer_with_size`]) is choosing a bounded
//! memory footprint for *this* frame, and `FrameEncoder` honors that bound
//! by never accumulating more than `buf_len` compressed bytes before
//! writing a chunk — even when compression is effective enough that
//! `idat_chunk_size` would otherwise let more pile up. Concatenated, the
//! two paths' `IDAT`/`fdAT` *stream contents* still decode to the same
//! pixels either way (chunk boundaries carry no meaning to a decoder); they
//! are only byte-for-byte identical file output when `buf_len ==
//! idat_chunk_size`, which the default 4 KiB vs. 64 KiB is not. Call
//! [`super::Writer::stream_writer_with_size`] with the same value passed to
//! [`super::Encoder::set_idat_chunk_size`] (before `write_header`) to get
//! identical bytes.
//!
//! **Interlaced images are out of scope here** — not a shortcut, an actual
//! mismatch: Adam7 pass 1 needs pixels from every eighth row before pass 2
//! needs anything at all, so producing it from a single forward pass over
//! row-major bytes would require buffering the whole image, defeating the
//! bounded-memory point of a stream writer. `png` 0.18 has no interlaced
//! writer at all, streaming or otherwise, for the same reason; use
//! [`super::Writer::write_image_data`] (which buffers internally) for an
//! interlaced file instead.

use std::io::{self, Write};

use crate::error::{EncodingError, EncodingFormatErrorKind};
use crate::filter::AdaptiveScratch;

use super::Writer;
use super::image_data::{
    FrameDestination, advance_after_frame, check_current_frame_rect, current_frame_size,
    plan_next_frame, validate_new_image, write_fctl,
};
use super::zlib::{ChunkKind, FrameEncoder, use_optimal_parsing};

/// Default internal buffer size, matching `png` 0.18.
pub const DEFAULT_BUFFER_LENGTH: usize = 4 * 1024;

/// Where a [`StreamWriter`] sends its finished chunks: a borrowed
/// [`Writer`] (so the caller can append more chunks afterward) or one it
/// owns (so the stream writer can outlive the call that made it).
pub(crate) enum Target<'a, W: Write> {
    Borrowed(&'a mut Writer<W>),
    Owned(Writer<W>),
}

impl<'a, W: Write> Target<'a, W> {
    fn get(&self) -> &Writer<W> {
        match self {
            Target::Borrowed(w) => w,
            Target::Owned(w) => w,
        }
    }

    fn get_mut(&mut self) -> &mut Writer<W> {
        match self {
            Target::Borrowed(w) => w,
            Target::Owned(w) => w,
        }
    }
}

/// A [`Write`] adapter that filters and compresses one frame's pixel data as
/// it arrives, rather than requiring the whole frame in memory at once.
///
/// Write exactly `row_stride * height` bytes (raw samples, no filter
/// bytes — the same shape [`super::Writer::write_image_data`] expects), then
/// call [`StreamWriter::finish`]. Dropping without finishing silently
/// discards any partial row and swallows write errors, exactly like
/// [`super::Writer`]'s own `Drop` — prefer `finish()`.
pub struct StreamWriter<'a, W: Write> {
    target: Target<'a, W>,
    encoder: Option<FrameEncoder>,
    scratch: AdaptiveScratch,
    row_buf: Vec<u8>,
    prev_row: Vec<u8>,
    row_stride: usize,
    total_rows: usize,
    rows_written: usize,
    level: u8,
    destination: FrameDestination,
    counts_as_animation_frame: bool,
    seq: u32,
    finished: bool,
}

impl<'a, W: Write> StreamWriter<'a, W> {
    pub(crate) fn new(
        target: Target<'a, W>,
        buf_len: usize,
    ) -> Result<StreamWriter<'a, W>, EncodingError> {
        {
            let w = target.get();
            if w.geometry.interlaced {
                return Err(EncodingFormatErrorKind::Unrecoverable.into());
            }
            if w.geometry.color_type == crate::ColorType::Indexed && !w.geometry.has_palette {
                return Err(EncodingFormatErrorKind::NoPalette.into());
            }
        }
        let mut target = target;
        validate_new_image(target.get())?;
        let plan = plan_next_frame(target.get());
        check_current_frame_rect(target.get(), plan)?;
        if plan.emit_fctl {
            write_fctl(target.get_mut())?;
        }
        let w = target.get();
        let (width, height) = current_frame_size(w);
        let row_stride = w.geometry.row_stride_for_width(width);
        let level = w.options.compression.level();
        let optimal = use_optimal_parsing(level);
        let chunk_size = w.options.idat_chunk_size.min(buf_len.max(1)).max(1);
        let seq = w.frame_control.map_or(0, |f| f.sequence_number);
        Ok(StreamWriter {
            target,
            encoder: Some(FrameEncoder::new(level, optimal, chunk_size)),
            scratch: AdaptiveScratch::new(),
            row_buf: Vec::with_capacity(row_stride),
            prev_row: Vec::new(),
            row_stride,
            total_rows: height as usize,
            rows_written: 0,
            level,
            destination: plan.destination,
            counts_as_animation_frame: plan.counts_as_animation_frame,
            seq,
            finished: false,
        })
    }

    fn push_complete_row(&mut self) -> io::Result<()> {
        let Some(encoder) = self.encoder.as_mut() else {
            return Ok(());
        };
        let bpp = self.target.get().geometry.bpp();
        let filter = crate::filter::select_filter(
            self.target.get().options.filter,
            bpp,
            &self.prev_row,
            &self.row_buf,
            &mut self.scratch,
        );
        let mut kind = match self.destination {
            FrameDestination::Idat => ChunkKind::Idat,
            FrameDestination::Fdat => ChunkKind::Fdat(&mut self.seq),
        };
        encoder
            .push_row(
                self.level,
                filter.into_u8(),
                self.scratch.filtered(),
                &mut self.target.get_mut().w,
                &mut kind,
            )
            .map_err(io::Error::from)?;
        self.prev_row.clear();
        self.prev_row.extend_from_slice(&self.row_buf);
        self.row_buf.clear();
        self.rows_written += 1;
        Ok(())
    }

    /// Bytes still needed to complete the current frame.
    #[must_use]
    pub fn remaining(&self) -> usize {
        let rows_left = self.total_rows.saturating_sub(self.rows_written);
        rows_left
            .saturating_mul(self.row_stride)
            .saturating_sub(self.row_buf.len().min(self.row_stride))
    }

    fn finish_mut(&mut self) -> Result<(), EncodingError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if self.rows_written < self.total_rows || !self.row_buf.is_empty() {
            return Err(EncodingFormatErrorKind::MissingData(self.remaining()).into());
        }
        let Some(encoder) = self.encoder.take() else {
            return Ok(());
        };
        let kind = match self.destination {
            FrameDestination::Idat => ChunkKind::Idat,
            FrameDestination::Fdat => ChunkKind::Fdat(&mut self.seq),
        };
        encoder.finish(self.level, &mut self.target.get_mut().w, kind)?;
        if self.destination == FrameDestination::Fdat {
            if let Some(fctl) = self.target.get_mut().frame_control.as_mut() {
                fctl.sequence_number = self.seq;
            }
        }
        let plan = super::image_data::FramePlan {
            emit_fctl: false,
            destination: self.destination,
            counts_as_animation_frame: self.counts_as_animation_frame,
        };
        advance_after_frame(self.target.get_mut(), plan);
        Ok(())
    }

    /// Finish this frame: reject a short write, close the zlib stream, and
    /// write the final chunk(s). This is the **checked** path — prefer it
    /// over letting the writer drop.
    ///
    /// # Errors
    ///
    /// Fewer than `row_stride * height` bytes were written, or the
    /// underlying writer fails.
    pub fn finish(mut self) -> Result<(), EncodingError> {
        self.finish_mut()
    }
}

impl<'a, W: Write> Write for StreamWriter<'a, W> {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.rows_written >= self.total_rows {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "wrote more image data than the frame holds",
            ));
        }
        let mut total = 0usize;
        while !data.is_empty() && self.rows_written < self.total_rows {
            let want = self.row_stride - self.row_buf.len();
            let take = want.min(data.len());
            self.row_buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            total += take;
            if self.row_buf.len() == self.row_stride {
                self.push_complete_row()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a, W: Write> Drop for StreamWriter<'a, W> {
    fn drop(&mut self) {
        // Best-effort, exactly like `Writer`'s own `Drop`: a partial frame
        // or a write failure here cannot be reported from a destructor.
        if !self.finished && self.rows_written == self.total_rows && self.row_buf.is_empty() {
            let _ = self.finish_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{BitDepth, ColorType};

    /// A pseudo-random RGBA8 buffer, large enough that its filtered,
    /// compressed form reliably exceeds both `StreamWriter`'s 4 KiB default
    /// flush granularity and `write_image_data`'s 64 KiB one — a
    /// multiplicative hash rather than a real PRNG so this file gains no
    /// new dependency, but noisy enough that DEFLATE cannot shrink it much,
    /// which is what actually pushes the compressed size past 64 KiB.
    fn noisy_rgba8(width: u32, height: u32) -> Vec<u8> {
        (0..(width as usize * height as usize * 4))
            .map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_shr(24) as u8)
            .collect()
    }

    /// Small image, both defaults happen to fit the whole frame in one
    /// chunk either way — see
    /// `default_buf_len_diverges_from_write_image_data_for_a_large_frame`
    /// and `matching_buf_len_to_idat_chunk_size_makes_them_identical` below
    /// for what happens once a frame is large enough that the two paths'
    /// *different* default flush granularities matter.
    #[test]
    fn stream_writer_matches_write_image_data_byte_for_byte() {
        let pixels: Vec<u8> = (0..(4 * 3)).map(|i| (i * 17) as u8).collect();

        let mut direct = Vec::new();
        let mut enc = super::super::Encoder::new(&mut direct, 4, 3);
        enc.set_color(ColorType::Grayscale);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc.write_header().expect("header");
        w.write_image_data(&pixels).expect("image data");
        w.finish().expect("finish");

        let mut streamed = Vec::new();
        let mut enc2 = super::super::Encoder::new(&mut streamed, 4, 3);
        enc2.set_color(ColorType::Grayscale);
        enc2.set_depth(BitDepth::Eight);
        let mut w2 = enc2.write_header().expect("header");
        {
            let mut sw = w2.stream_writer().expect("stream writer");
            for chunk in pixels.chunks(3) {
                sw.write_all(chunk).expect("write");
            }
            sw.finish().expect("finish stream");
        }
        w2.finish().expect("finish");

        assert_eq!(direct, streamed);
        let image = crate::decode(&streamed).expect("decode");
        assert_eq!(image.data, pixels);
    }

    /// For a frame whose compressed form crosses both default flush
    /// granularities, `stream_writer()`'s default 4 KiB `buf_len` and
    /// `write_image_data`'s default 64 KiB `idat_chunk_size` produce
    /// *different* `IDAT` chunk boundaries — exactly as the module doc
    /// says they will — even though both decode back to the same pixels.
    /// This is the case the byte-identity claim never covered; see
    /// `matching_buf_len_to_idat_chunk_size_makes_them_identical` below
    /// for the configuration that does make them identical.
    #[test]
    fn default_buf_len_diverges_from_write_image_data_for_a_large_frame() {
        let pixels = noisy_rgba8(300, 300);
        assert!(
            pixels.len() > 4 * 1024,
            "must exceed StreamWriter's default buf_len to be a meaningful test"
        );

        let mut direct = Vec::new();
        let mut enc = super::super::Encoder::new(&mut direct, 300, 300);
        enc.set_color(ColorType::Rgba);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc.write_header().expect("header");
        w.write_image_data(&pixels).expect("image data");
        w.finish().expect("finish");

        let mut streamed = Vec::new();
        let mut enc2 = super::super::Encoder::new(&mut streamed, 300, 300);
        enc2.set_color(ColorType::Rgba);
        enc2.set_depth(BitDepth::Eight);
        let mut w2 = enc2.write_header().expect("header");
        {
            let mut sw = w2.stream_writer().expect("stream writer");
            sw.write_all(&pixels).expect("write");
            sw.finish().expect("finish stream");
        }
        w2.finish().expect("finish");

        assert_ne!(
            direct, streamed,
            "4 KiB buf_len and 64 KiB idat_chunk_size intentionally give different chunk boundaries"
        );
        assert_eq!(crate::decode(&direct).expect("decode direct").data, pixels);
        assert_eq!(
            crate::decode(&streamed).expect("decode streamed").data,
            pixels
        );
    }

    /// Calling `stream_writer_with_size` with the same value as the
    /// (default) `idat_chunk_size` restores byte-for-byte identity, even
    /// for a frame large enough to need several chunks either way — the
    /// precise condition the module doc now states instead of an
    /// unconditional claim.
    #[test]
    fn matching_buf_len_to_idat_chunk_size_makes_them_identical() {
        let pixels = noisy_rgba8(300, 300);

        let mut direct = Vec::new();
        let mut enc = super::super::Encoder::new(&mut direct, 300, 300);
        enc.set_color(ColorType::Rgba);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc.write_header().expect("header");
        w.write_image_data(&pixels).expect("image data");
        w.finish().expect("finish");

        let mut streamed = Vec::new();
        let mut enc2 = super::super::Encoder::new(&mut streamed, 300, 300);
        enc2.set_color(ColorType::Rgba);
        enc2.set_depth(BitDepth::Eight);
        let mut w2 = enc2.write_header().expect("header");
        {
            // `super::DEFAULT_CHUNK_SIZE` is private to `encoder::zlib`, so
            // this spells out the number this crate's own default
            // `idat_chunk_size` equals — 64 KiB, chosen for
            // streaming-friendliness, *not* a `png` 0.18 parity claim: real
            // `png` 0.18 chunks `IDAT` at `u32::MAX >> 1` (one ~2 GiB chunk
            // for anything realistic), confirmed by reading
            // `Writer::write_zlib_encoded_idat` in its own source.
            let mut sw = w2
                .stream_writer_with_size(64 * 1024)
                .expect("stream writer");
            sw.write_all(&pixels).expect("write");
            sw.finish().expect("finish stream");
        }
        w2.finish().expect("finish");

        assert_eq!(direct, streamed);
        assert_eq!(crate::decode(&streamed).expect("decode").data, pixels);
    }

    #[test]
    fn short_write_is_reported_by_finish() {
        let mut buf = Vec::new();
        let mut enc = super::super::Encoder::new(&mut buf, 4, 3);
        enc.set_color(ColorType::Grayscale);
        let mut w = enc.write_header().expect("header");
        let mut sw = w.stream_writer().expect("stream writer");
        sw.write_all(&[1, 2, 3]).expect("write");
        assert!(sw.finish().is_err());
    }

    #[test]
    fn writing_past_the_frame_is_an_error() {
        let mut buf = Vec::new();
        let mut enc = super::super::Encoder::new(&mut buf, 2, 1);
        enc.set_color(ColorType::Grayscale);
        let mut w = enc.write_header().expect("header");
        let mut sw = w.stream_writer().expect("stream writer");
        sw.write_all(&[1, 2]).expect("first row");
        assert!(sw.write_all(&[3]).is_err());
    }

    #[test]
    fn into_stream_writer_owns_the_writer() {
        // `W = Vec<u8>` by value (not `&mut Vec<u8>`), so the returned
        // `StreamWriter<'static, _>` truly owns everything and does not
        // borrow from this function's stack frame.
        let mut enc = super::super::Encoder::new(Vec::new(), 2, 2);
        enc.set_color(ColorType::Grayscale);
        let w = enc.write_header().expect("header");
        let mut sw = w.into_stream_writer().expect("owned stream writer");
        sw.write_all(&[1, 2, 3, 4]).expect("write");
        sw.finish().expect("finish");
    }

    #[test]
    fn interlaced_stream_writer_is_rejected() {
        let mut buf = Vec::new();
        let mut enc = super::super::Encoder::new(&mut buf, 4, 4);
        enc.set_color(ColorType::Grayscale);
        enc.set_interlaced(true);
        let mut w = enc.write_header().expect("header");
        assert!(w.stream_writer().is_err());
    }
}
