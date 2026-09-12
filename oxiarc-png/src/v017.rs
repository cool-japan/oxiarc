//! The `png` **0.17**-shaped surface, for codebases that have not moved to
//! 0.18 yet: `use oxiarc_png::v017 as png;`.
//!
//! The crate root is 0.18-shaped (see [`crate`]'s "Migrating from the `png`
//! crate"). This module exists because four names changed shape between
//! 0.17.16 and 0.18.1, so a single root cannot be both:
//!
//! | Item | 0.17.16 | 0.18.1 (the crate root) |
//! |---|---|---|
//! | `Reader::output_buffer_size` | `-> usize` | `-> Option<usize>` |
//! | Filter selection | `FilterType` **plus** `AdaptiveFilterType`, set by `set_filter` and `set_adaptive_filter` | one [`crate::Filter`] enum, `set_filter` only |
//! | `Decoder` bound | `R: Read` | `R: BufRead + Seek` |
//! | `lib.rs` exports | no `Adam7Variant`/`UnfilterBuf`/`UnfilterRegion`/`splat_interlaced_row` | present |
//!
//! It is a **module rather than a Cargo feature** on purpose: a feature that
//! changed a public signature would be unified across a dependency graph, so
//! one crate turning it on would silently reshape the API every other crate
//! in the build sees. A module cannot do that — a 0.17 caller and a 0.18
//! caller can share one build of this crate.
//!
//! The `Decoder` bound needs no shim: this crate's [`crate::Decoder`] is
//! already `R: Read`, which is what 0.17 required and is strictly wider than
//! 0.18's `R: BufRead + Seek`. The extra 0.18-only exports are simply absent
//! here, which is what a 0.17 codebase expects.
//!
//! ```
//! use oxiarc_png::v017 as png;
//!
//! let mut buf = Vec::new();
//! let mut encoder = png::Encoder::new(&mut buf, 2, 2);
//! encoder.set_color(png::ColorType::Grayscale);
//! encoder.set_depth(png::BitDepth::Eight);
//! encoder.set_filter(png::FilterType::Paeth);
//! encoder.set_adaptive_filter(png::AdaptiveFilterType::NonAdaptive);
//! let mut writer = encoder.write_header()?;
//! writer.write_image_data(&[1, 2, 3, 4])?;
//! writer.finish()?;
//!
//! let decoder = png::Decoder::new(&buf[..]);
//! let mut reader = decoder.read_info()?;
//! // 0.17's `output_buffer_size` returns a plain `usize`, not an `Option`.
//! let mut out = vec![0; reader.output_buffer_size()];
//! let info = reader.next_frame(&mut out)?;
//! assert_eq!((info.width, info.height), (2, 2));
//! assert_eq!(out, [1, 2, 3, 4]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io::{Read, Write};

pub use crate::chunk;
pub use crate::text_metadata;
pub use crate::{
    Adam7Info, AnimationControl, BitDepth, BlendOp, ColorType, Compression, DecodeOptions, Decoded,
    DecodingError, DisposeOp, EncodingError, FrameControl, Info, InterlaceInfo, InterlacedRow,
    Limits, OutputInfo, PixelDimensions, ScaledFloat, SourceChromaticities, SrgbRenderingIntent,
    StreamingDecoder, Transformations, Unit, expand_interlaced_row,
};

/// A fixed per-row filter, 0.17's spelling of the fixed half of
/// [`crate::Filter`].
///
/// Same discriminants as 0.17's `FilterType` (`NoFilter = 0` … `Paeth = 4`),
/// and the same `Sub` default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum FilterType {
    /// Store the row unfiltered.
    NoFilter = 0,
    /// Subtract the pixel to the left. 0.17's default.
    #[default]
    Sub = 1,
    /// Subtract the pixel above.
    Up = 2,
    /// Subtract the average of left and above.
    Avg = 3,
    /// Subtract the Paeth predictor.
    Paeth = 4,
}

impl FilterType {
    /// The filter with this wire value, or `None` for 5..=255.
    ///
    /// ```
    /// use oxiarc_png::v017::FilterType;
    /// assert_eq!(FilterType::from_u8(4), Some(FilterType::Paeth));
    /// assert_eq!(FilterType::from_u8(5), None);
    /// ```
    #[must_use]
    pub fn from_u8(n: u8) -> Option<FilterType> {
        match n {
            0 => Some(FilterType::NoFilter),
            1 => Some(FilterType::Sub),
            2 => Some(FilterType::Up),
            3 => Some(FilterType::Avg),
            4 => Some(FilterType::Paeth),
            _ => None,
        }
    }
}

/// Whether the encoder picks a filter per row or uses the fixed
/// [`FilterType`], 0.17's spelling of the adaptive half of
/// [`crate::Filter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AdaptiveFilterType {
    /// Choose per row (this crate's [`crate::Filter::Adaptive`]).
    Adaptive,
    /// Always use the configured [`FilterType`]. 0.17's default.
    #[default]
    NonAdaptive,
}

/// Fold 0.17's two filter knobs into this crate's single [`crate::Filter`],
/// exactly as 0.17 itself resolved them: `Adaptive` selects per row and
/// ignores `filter`; `NonAdaptive` applies `filter` to every row.
fn resolve_filter(filter: FilterType, adaptive: AdaptiveFilterType) -> crate::Filter {
    match adaptive {
        AdaptiveFilterType::Adaptive => crate::Filter::Adaptive,
        AdaptiveFilterType::NonAdaptive => match filter {
            FilterType::NoFilter => crate::Filter::NoFilter,
            FilterType::Sub => crate::Filter::Sub,
            FilterType::Up => crate::Filter::Up,
            FilterType::Avg => crate::Filter::Avg,
            FilterType::Paeth => crate::Filter::Paeth,
        },
    }
}

/// 0.17's `Decoder`: identical to [`crate::Decoder`] except that
/// [`Decoder::read_info`] hands back this module's [`Reader`].
#[derive(Debug)]
pub struct Decoder<R: Read> {
    inner: crate::Decoder<R>,
}

impl<R: Read> Decoder<R> {
    /// A decoder with the default options and limits.
    #[must_use]
    pub fn new(r: R) -> Decoder<R> {
        Decoder {
            inner: crate::Decoder::new(r),
        }
    }

    /// A decoder with an explicit memory budget.
    #[must_use]
    pub fn new_with_limits(r: R, limits: Limits) -> Decoder<R> {
        Decoder {
            inner: crate::Decoder::new_with_limits(r, limits),
        }
    }

    /// A decoder with explicit options.
    #[must_use]
    pub fn new_with_options(r: R, options: DecodeOptions) -> Decoder<R> {
        Decoder {
            inner: crate::Decoder::new_with_options(r, options),
        }
    }

    /// Replace the memory budget.
    pub fn set_limits(&mut self, limits: Limits) {
        self.inner.set_limits(limits);
    }

    /// Request output transformations.
    pub fn set_transformations(&mut self, transform: Transformations) {
        self.inner.set_transformations(transform);
    }

    /// Do not parse the text chunks.
    pub fn set_ignore_text_chunk(&mut self, ignore: bool) {
        self.inner.set_ignore_text_chunk(ignore);
    }

    /// Do not parse `iCCP`.
    pub fn set_ignore_iccp_chunk(&mut self, ignore: bool) {
        self.inner.set_ignore_iccp_chunk(ignore);
    }

    /// Skip both the chunk CRCs and the zlib Adler-32.
    pub fn ignore_checksums(&mut self, ignore: bool) {
        self.inner.ignore_checksums(ignore);
    }

    /// Read up to the first `IDAT`.
    ///
    /// # Errors
    ///
    /// Any structural problem before the image data.
    pub fn read_header_info(&mut self) -> Result<&Info<'static>, DecodingError> {
        self.inner.read_header_info()
    }

    /// Read the header and return a [`Reader`] for the pixels.
    ///
    /// # Errors
    ///
    /// Any structural problem before the image data.
    pub fn read_info(self) -> Result<Reader<R>, DecodingError> {
        Ok(Reader {
            inner: self.inner.read_info()?,
        })
    }
}

/// 0.17's `Reader`: identical to [`crate::Reader`] except that
/// [`Reader::output_buffer_size`] returns a plain `usize`.
#[derive(Debug)]
pub struct Reader<R: Read> {
    inner: crate::Reader<R>,
}

impl<R: Read> Reader<R> {
    /// The parsed header metadata.
    #[must_use]
    pub fn info(&self) -> &Info<'static> {
        self.inner.info()
    }

    /// The colour type and bit depth [`Reader::next_frame`] will produce.
    #[must_use]
    pub fn output_color_type(&self) -> (ColorType, BitDepth) {
        self.inner.output_color_type()
    }

    /// Bytes in one output row of `width` pixels.
    ///
    /// 0.17 returned a bare `usize` here and computed it with unchecked
    /// arithmetic; this crate's [`crate::Reader::output_line_size`] returns
    /// an `Option`. The shim **saturates** rather than wrapping, so a size
    /// that does not fit becomes `usize::MAX` — a caller's `vec![0; n]`
    /// then fails loudly instead of silently under-allocating, which is
    /// what a wrapped value would have caused.
    #[must_use]
    pub fn output_line_size(&self, width: u32) -> usize {
        self.inner.output_line_size(width).unwrap_or(usize::MAX)
    }

    /// Bytes needed for a whole decoded frame.
    ///
    /// Saturates instead of wrapping — see [`Reader::output_line_size`].
    #[must_use]
    pub fn output_buffer_size(&self) -> usize {
        self.inner.output_buffer_size().unwrap_or(usize::MAX)
    }

    /// Decode one whole frame into `buf`.
    ///
    /// # Errors
    ///
    /// A short buffer, a malformed stream, or an exhausted animation.
    pub fn next_frame(&mut self, buf: &mut [u8]) -> Result<OutputInfo, DecodingError> {
        self.inner.next_frame(buf)
    }

    /// Read the next `fcTL` without decoding its pixels.
    ///
    /// # Errors
    ///
    /// A malformed stream, or no further frame.
    pub fn next_frame_info(&mut self) -> Result<&FrameControl, DecodingError> {
        self.inner.next_frame_info()
    }

    /// Decode one row into `out`.
    ///
    /// # Errors
    ///
    /// A short buffer or a malformed stream.
    pub fn read_row(&mut self, out: &mut [u8]) -> Result<Option<InterlaceInfo>, DecodingError> {
        self.inner.read_row(out)
    }

    /// The next de-interlaced row.
    ///
    /// # Errors
    ///
    /// A malformed stream.
    pub fn next_row(&mut self) -> Result<Option<crate::Row<'_>>, DecodingError> {
        self.inner.next_row()
    }

    /// The next row, still in its Adam7 pass.
    ///
    /// # Errors
    ///
    /// A malformed stream.
    pub fn next_interlaced_row(&mut self) -> Result<Option<InterlacedRow<'_>>, DecodingError> {
        self.inner.next_interlaced_row()
    }

    /// Consume the rest of the file, checking trailing chunks.
    ///
    /// # Errors
    ///
    /// A malformed trailer.
    pub fn finish(&mut self) -> Result<(), DecodingError> {
        self.inner.finish()
    }
}

/// 0.17's `Encoder`: [`crate::Encoder`] with the two separate filter knobs
/// 0.17 had.
pub struct Encoder<'a, W: Write> {
    inner: crate::Encoder<'a, W>,
    filter: FilterType,
    adaptive: AdaptiveFilterType,
}

impl<'a, W: Write> Encoder<'a, W> {
    /// A new encoder for a `width` x `height` image.
    #[must_use]
    pub fn new(w: W, width: u32, height: u32) -> Encoder<'static, W> {
        Encoder {
            inner: crate::Encoder::new(w, width, height),
            filter: FilterType::default(),
            adaptive: AdaptiveFilterType::default(),
        }
    }

    /// Set the colour type.
    pub fn set_color(&mut self, color: ColorType) {
        self.inner.set_color(color);
    }

    /// Set the bit depth.
    pub fn set_depth(&mut self, depth: BitDepth) {
        self.inner.set_depth(depth);
    }

    /// Set the raw `PLTE` payload.
    pub fn set_palette<T: Into<std::borrow::Cow<'a, [u8]>>>(&mut self, palette: T) {
        self.inner.set_palette(palette);
    }

    /// Set the raw `tRNS` payload.
    pub fn set_trns<T: Into<std::borrow::Cow<'a, [u8]>>>(&mut self, trns: T) {
        self.inner.set_trns(trns);
    }

    /// Choose compression via one of the coarse presets.
    ///
    /// **Note:** 0.18 (and this crate) pair a preset with a filter strategy,
    /// so this overrides an earlier [`Encoder::set_filter`]. Call the filter
    /// setters *after* this one, exactly as 0.17's own docs advised.
    pub fn set_compression(&mut self, compression: Compression) {
        self.inner.set_compression(compression);
        // Restore 0.17's semantics: the preset does not own the filter.
        self.inner
            .set_filter(resolve_filter(self.filter, self.adaptive));
    }

    /// Set the fixed per-row filter (used when
    /// [`AdaptiveFilterType::NonAdaptive`] is in force).
    pub fn set_filter(&mut self, filter: FilterType) {
        self.filter = filter;
        self.inner
            .set_filter(resolve_filter(self.filter, self.adaptive));
    }

    /// Choose between per-row filter selection and the fixed
    /// [`FilterType`].
    pub fn set_adaptive_filter(&mut self, adaptive: AdaptiveFilterType) {
        self.adaptive = adaptive;
        self.inner
            .set_filter(resolve_filter(self.filter, self.adaptive));
    }

    /// Set the source display gamma (`gAMA`).
    pub fn set_source_gamma(&mut self, gamma: ScaledFloat) {
        self.inner.set_source_gamma(gamma);
    }

    /// Set the source display chromaticities (`cHRM`).
    pub fn set_source_chromaticities(&mut self, chroma: SourceChromaticities) {
        self.inner.set_source_chromaticities(chroma);
    }

    /// Mark the image as sRGB.
    pub fn set_source_srgb(&mut self, intent: SrgbRenderingIntent) {
        self.inner.set_source_srgb(intent);
    }

    /// Set the `pHYs` physical pixel dimensions, or clear them.
    pub fn set_pixel_dims(&mut self, dims: Option<PixelDimensions>) {
        self.inner.set_pixel_dims(dims);
    }

    /// Declare the image animated.
    ///
    /// # Errors
    ///
    /// `num_frames == 0`.
    pub fn set_animated(&mut self, num_frames: u32, num_plays: u32) -> Result<(), EncodingError> {
        self.inner.set_animated(num_frames, num_plays)
    }

    /// Mark the first written image a default image outside the animation.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_sep_def_img(&mut self, sep: bool) -> Result<(), EncodingError> {
        self.inner.set_sep_def_img(sep)
    }

    /// Set every following frame's display delay.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_frame_delay(&mut self, num: u16, den: u16) -> Result<(), EncodingError> {
        self.inner.set_frame_delay(num, den)
    }

    /// Set every following frame's blend operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_blend_op(&mut self, op: BlendOp) -> Result<(), EncodingError> {
        self.inner.set_blend_op(op)
    }

    /// Set every following frame's dispose operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_dispose_op(&mut self, op: DisposeOp) -> Result<(), EncodingError> {
        self.inner.set_dispose_op(op)
    }

    /// Append an uncompressed `tEXt` chunk.
    ///
    /// # Errors
    ///
    /// Never in practice; kept fallible for 0.17 signature parity.
    pub fn add_text_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.inner.add_text_chunk(keyword, text)
    }

    /// Append a zlib-compressed `zTXt` chunk.
    ///
    /// # Errors
    ///
    /// Never in practice; kept fallible for 0.17 signature parity.
    pub fn add_ztxt_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.inner.add_ztxt_chunk(keyword, text)
    }

    /// Append a UTF-8 `iTXt` chunk.
    ///
    /// # Errors
    ///
    /// Never in practice; kept fallible for 0.17 signature parity.
    pub fn add_itxt_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.inner.add_itxt_chunk(keyword, text)
    }

    /// Turn on sequencing checks in the returned [`Writer`].
    pub fn validate_sequence(&mut self, validate: bool) {
        self.inner.validate_sequence(validate);
    }

    /// Write the signature, `IHDR` and the header chunks.
    ///
    /// # Errors
    ///
    /// Zero width or height, or an invalid colour type / bit depth pair.
    pub fn write_header(self) -> Result<Writer<W>, EncodingError> {
        Ok(Writer {
            inner: self.inner.write_header()?,
            filter: self.filter,
            adaptive: self.adaptive,
        })
    }
}

/// 0.17's `Writer`: [`crate::Writer`] with the two separate filter knobs.
pub struct Writer<W: Write> {
    inner: crate::Writer<W>,
    filter: FilterType,
    adaptive: AdaptiveFilterType,
}

impl<W: Write> Writer<W> {
    /// Set the fixed per-row filter for frames written from now on.
    pub fn set_filter(&mut self, filter: FilterType) {
        self.filter = filter;
        self.inner
            .set_filter(resolve_filter(self.filter, self.adaptive));
    }

    /// Choose between per-row selection and the fixed [`FilterType`] for
    /// frames written from now on.
    pub fn set_adaptive_filter(&mut self, adaptive: AdaptiveFilterType) {
        self.adaptive = adaptive;
        self.inner
            .set_filter(resolve_filter(self.filter, self.adaptive));
    }

    /// Set the following frame's display delay.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_frame_delay(&mut self, num: u16, den: u16) -> Result<(), EncodingError> {
        self.inner.set_frame_delay(num, den)
    }

    /// Set the following frame's rectangle dimensions.
    ///
    /// # Errors
    ///
    /// Not animated, a zero dimension, or the rectangle leaves the canvas.
    pub fn set_frame_dimension(&mut self, width: u32, height: u32) -> Result<(), EncodingError> {
        self.inner.set_frame_dimension(width, height)
    }

    /// Set the following frame's rectangle position.
    ///
    /// # Errors
    ///
    /// Not animated, or the rectangle leaves the canvas.
    pub fn set_frame_position(&mut self, x: u32, y: u32) -> Result<(), EncodingError> {
        self.inner.set_frame_position(x, y)
    }

    /// Grow the following frame's rectangle to the whole canvas.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn reset_frame_dimension(&mut self) -> Result<(), EncodingError> {
        self.inner.reset_frame_dimension()
    }

    /// Move the following frame's rectangle to `(0, 0)`.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn reset_frame_position(&mut self) -> Result<(), EncodingError> {
        self.inner.reset_frame_position()
    }

    /// Set the following frame's blend operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_blend_op(&mut self, op: BlendOp) -> Result<(), EncodingError> {
        self.inner.set_blend_op(op)
    }

    /// Set the following frame's dispose operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_dispose_op(&mut self, op: DisposeOp) -> Result<(), EncodingError> {
        self.inner.set_dispose_op(op)
    }

    /// Write one raw chunk.
    ///
    /// # Errors
    ///
    /// An oversized payload, or the underlying writer fails.
    pub fn write_chunk(
        &mut self,
        kind: chunk::ChunkType,
        data: &[u8],
    ) -> Result<(), EncodingError> {
        self.inner.write_chunk(kind, data)
    }

    /// Write one text chunk.
    ///
    /// # Errors
    ///
    /// The keyword or text cannot be encoded, or the writer fails.
    pub fn write_text_chunk<T: text_metadata::EncodableTextChunk>(
        &mut self,
        chunk: &T,
    ) -> Result<(), EncodingError> {
        self.inner.write_text_chunk(chunk)
    }

    /// Write one complete frame.
    ///
    /// # Errors
    ///
    /// A size mismatch, a missing palette, a sequencing violation, or an
    /// I/O failure.
    pub fn write_image_data(&mut self, data: &[u8]) -> Result<(), EncodingError> {
        self.inner.write_image_data(data)
    }

    /// A [`crate::StreamWriter`] over this writer.
    ///
    /// # Errors
    ///
    /// As [`crate::Writer::stream_writer`].
    pub fn stream_writer(&mut self) -> Result<crate::StreamWriter<'_, W>, EncodingError> {
        self.inner.stream_writer()
    }

    /// A [`crate::StreamWriter`] with a caller-chosen buffer size.
    ///
    /// # Errors
    ///
    /// As [`crate::Writer::stream_writer_with_size`].
    pub fn stream_writer_with_size(
        &mut self,
        size: usize,
    ) -> Result<crate::StreamWriter<'_, W>, EncodingError> {
        self.inner.stream_writer_with_size(size)
    }

    /// A [`crate::StreamWriter`] that owns this writer.
    ///
    /// # Errors
    ///
    /// As [`crate::Writer::into_stream_writer`].
    pub fn into_stream_writer(self) -> Result<crate::StreamWriter<'static, W>, EncodingError> {
        self.inner.into_stream_writer()
    }

    /// A [`crate::StreamWriter`] that owns this writer, with a chosen buffer
    /// size.
    ///
    /// # Errors
    ///
    /// As [`crate::Writer::into_stream_writer_with_size`].
    pub fn into_stream_writer_with_size(
        self,
        size: usize,
    ) -> Result<crate::StreamWriter<'static, W>, EncodingError> {
        self.inner.into_stream_writer_with_size(size)
    }

    /// Finish the file: validate sequencing, write `IEND`, flush.
    ///
    /// # Errors
    ///
    /// A sequencing violation or an I/O failure.
    pub fn finish(self) -> Result<(), EncodingError> {
        self.inner.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_filter_knobs_fold_into_one_filter_the_way_017_resolved_them() {
        for (ft, expected) in [
            (FilterType::NoFilter, crate::Filter::NoFilter),
            (FilterType::Sub, crate::Filter::Sub),
            (FilterType::Up, crate::Filter::Up),
            (FilterType::Avg, crate::Filter::Avg),
            (FilterType::Paeth, crate::Filter::Paeth),
        ] {
            assert_eq!(
                resolve_filter(ft, AdaptiveFilterType::NonAdaptive),
                expected
            );
            assert_eq!(
                resolve_filter(ft, AdaptiveFilterType::Adaptive),
                crate::Filter::Adaptive,
                "Adaptive ignores the fixed filter, as 0.17 did"
            );
        }
    }

    #[test]
    fn the_017_defaults_match_the_real_crates() {
        assert_eq!(FilterType::default(), FilterType::Sub);
        assert_eq!(
            AdaptiveFilterType::default(),
            AdaptiveFilterType::NonAdaptive
        );
        assert_eq!(FilterType::from_u8(0), Some(FilterType::NoFilter));
        assert_eq!(FilterType::from_u8(255), None);
        assert_eq!(FilterType::Paeth as u8, 4);
    }

    #[test]
    fn set_compression_does_not_clobber_an_earlier_filter_choice() {
        // 0.18 pairs a `Compression` preset with a filter strategy; 0.17 did
        // not. The shim has to keep the caller's filter.
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 4, 4);
        enc.set_color(ColorType::Grayscale);
        enc.set_depth(BitDepth::Eight);
        enc.set_filter(FilterType::Paeth);
        enc.set_compression(Compression::Fast);
        let mut w = enc.write_header().expect("header");
        w.write_image_data(&[7u8; 16]).expect("data");
        w.finish().expect("finish");

        let decoded = crate::decode(&buf).expect("decode");
        assert_eq!(decoded.data, vec![7u8; 16]);
        // Every row must carry filter byte 4 (Paeth), proving the preset did
        // not silently switch the encoder to Adaptive.
        let mut z = Vec::new();
        for item in chunk::ChunkIter::new(&buf) {
            let (kind, data) = item.expect("chunk");
            if kind == chunk::IDAT {
                z.extend_from_slice(data);
            }
        }
        let raw = oxiarc_deflate::zlib_decompress(&z).expect("inflate");
        for row in raw.chunks_exact(5) {
            assert_eq!(row[0], FilterType::Paeth as u8);
        }
    }

    #[test]
    fn output_buffer_size_is_a_plain_usize_and_round_trips() {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 3, 2);
        enc.set_color(ColorType::Rgb);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc.write_header().expect("header");
        let pixels: Vec<u8> = (0..18u8).collect();
        w.write_image_data(&pixels).expect("data");
        w.finish().expect("finish");

        let mut reader = Decoder::new(&buf[..]).read_info().expect("read_info");
        let size: usize = reader.output_buffer_size();
        assert_eq!(size, 18);
        assert_eq!(reader.output_line_size(3), 9);
        let mut out = vec![0u8; size];
        let info = reader.next_frame(&mut out).expect("frame");
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(out, pixels);
    }
}
