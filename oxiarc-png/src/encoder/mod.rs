//! The `png`-0.18-shaped encoder: [`Encoder`] builds header metadata,
//! [`Encoder::write_header`] commits it and returns a [`Writer`], and
//! [`Writer::write_image_data`] (or [`Writer::stream_writer`]) supplies the
//! pixels.
//!
//! # Design
//!
//! Every scanline (filtered, filter byte included) is fed straight to
//! [`oxiarc_deflate::Deflater::deflate`] with `finish == false`, one
//! continuous zlib stream per **frame** (not per file — an APNG's `fdAT`
//! frames are each their own independent stream, exactly as the decoder
//! requires). See `zlib::FrameEncoder` (private), which owns this. Filter selection
//! reuses [`crate::filter::select_filter`] verbatim; nothing here
//! re-implements filtering. **Never** replace this with
//! `oxiarc_deflate::ZlibStreamEncoder` — it auto-`sync_flush`es every
//! 128 KiB, inserting an empty stored block and resetting the entropy coder,
//! a measurable ratio loss on filtered PNG rows for no benefit.
//!
//! Interlaced encoding (`Encoder::set_interlaced`, an extension `png` 0.18
//! does not have at all) does not change what the caller passes to
//! [`Writer::write_image_data`]: it is always the full image in plain
//! row-major order, the same shape [`crate::Image::data`] uses. The encoder
//! splits it into the seven Adam7 passes internally
//! ([`crate::interlace::extract_pass_row`]) and filters/compresses each pass
//! in transmission order, resetting the filter's "previous row" — but *not*
//! the zlib stream — at the start of every pass.

mod header;
pub(crate) mod image_data;
#[cfg(feature = "parallel")]
pub(crate) mod parallel;
pub mod stream;
pub(crate) mod zlib;

pub use stream::StreamWriter;

use std::borrow::Cow;
use std::io::Write;

use crate::common::{
    AnimationControl, BlendOp, Compression, DeflateCompression, DisposeOp, FrameControl,
    PixelDimensions, ScaledFloat, SourceChromaticities, SrgbRenderingIntent,
};
use crate::error::{EncodingError, EncodingFormatErrorKind, ParameterErrorKind};
use crate::filter::Filter;
use crate::header::{BitDepth, ColorType, Ihdr, Interlace};
use crate::info::Info;
use crate::text_metadata::{EncodableTextChunk, ITXtChunk, TEXtChunk, ZTXtChunk};

pub(crate) use zlib::DEFAULT_CHUNK_SIZE;

/// Options carried from [`Encoder`] into [`Writer`], deliberately mirroring
/// `png` 0.18's private `Options` (filter, compression, `sep_def_img`,
/// `validate_sequence`) plus this crate's `idat_chunk_size` extension.
#[derive(Debug, Clone)]
pub(crate) struct EncOptions {
    filter: Filter,
    compression: DeflateCompression,
    sep_def_img: bool,
    validate_sequence: bool,
    idat_chunk_size: usize,
}

impl Default for EncOptions {
    fn default() -> EncOptions {
        EncOptions {
            filter: Filter::default(),
            compression: DeflateCompression::default(),
            sep_def_img: false,
            validate_sequence: false,
            idat_chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

/// Map a [`Compression`] preset onto the filter strategy `png` 0.18 pairs it
/// with (`filter/mod.rs`'s private `Filter::from_simple`): `NoCompression`
/// gets `NoFilter` (filtering incompressible-by-design output would only
/// cost time), everything else gets `Adaptive`.
fn filter_from_compression(c: Compression) -> Filter {
    match c {
        Compression::NoCompression => Filter::NoFilter,
        _ => Filter::Adaptive,
    }
}

/// Builds a PNG's header metadata, then hands off to a [`Writer`] for the
/// pixel data.
///
/// `use oxiarc_png as png;` compiles every real-world call site this crate
/// was designed against unchanged: `Encoder::new`, `set_color`, `set_depth`,
/// `set_palette`, `set_compression`, `write_header`.
pub struct Encoder<'a, W: Write> {
    w: W,
    info: Info<'a>,
    options: EncOptions,
}

impl<'a, W: Write> Encoder<'a, W> {
    /// A new encoder for a `width` x `height` image, 8-bit grayscale by
    /// default (call [`Encoder::set_color`]/[`Encoder::set_depth`] to
    /// change that).
    #[must_use]
    pub fn new(w: W, width: u32, height: u32) -> Encoder<'static, W> {
        Encoder {
            w,
            info: Info::with_size(width, height),
            options: EncOptions::default(),
        }
    }

    /// An encoder pre-populated from a full [`Info`] — the natural shape of
    /// a decode-then-re-encode round trip. Every ancillary field `Info`
    /// carries (palette, `tRNS`, gamma, chromaticities, ICC profile, text,
    /// retained unknown chunks, …) is written by
    /// [`Encoder::write_header`] without needing a setter for each one.
    ///
    /// # Errors
    ///
    /// `animation_control` and `frame_control` must be either both present
    /// or both absent, a present `animation_control` must not declare zero
    /// frames, and a present `frame_control` must describe a non-empty
    /// rectangle that fits inside the canvas — the same rule
    /// [`Writer::set_frame_dimension`]/[`Writer::set_frame_position`]
    /// enforce for the setter route. Without that last check an
    /// `Info` assembled by hand (or copied from a hostile file and edited)
    /// could reach [`Writer::write_image_data`] with a zero-width frame,
    /// where the row stride is zero and the row loop's `chunks_exact(0)`
    /// panics, or with an out-of-canvas rectangle, which writes an `fcTL`
    /// this crate's own decoder then rejects with `BadSubFrameBounds`.
    pub fn with_info(w: W, info: Info<'a>) -> Result<Encoder<'a, W>, EncodingError> {
        if info.animation_control.is_some() != info.frame_control.is_some() {
            return Err(EncodingFormatErrorKind::NotAnimated.into());
        }
        if let Some(actl) = info.animation_control {
            if actl.num_frames == 0 {
                return Err(EncodingFormatErrorKind::ZeroFrames.into());
            }
        }
        if let Some(fctl) = info.frame_control {
            // A zero-sized canvas is `write_header`'s error to report, with
            // a more useful `ZeroWidth`/`ZeroHeight` than the `OutOfBounds`
            // a bounds check against it would produce, so only the frame's
            // own emptiness is checked here in that case.
            check_frame_rect(
                fctl.x_offset,
                fctl.y_offset,
                fctl.width,
                fctl.height,
                info.width,
                info.height,
                info.width != 0 && info.height != 0,
            )?;
        }
        Ok(Encoder {
            w,
            info,
            options: EncOptions::default(),
        })
    }

    /// Declare the image animated: `num_frames` frames, looping `num_plays`
    /// times (`0` means forever).
    ///
    /// Every [`Writer::write_image_data`] call after
    /// [`Encoder::write_header`] then writes one animation frame — the first
    /// as `IDAT` (unless [`Encoder::set_sep_def_img`] says otherwise), the
    /// rest as `fdAT`.
    ///
    /// # Errors
    ///
    /// `num_frames == 0`.
    pub fn set_animated(&mut self, num_frames: u32, num_plays: u32) -> Result<(), EncodingError> {
        if num_frames == 0 {
            return Err(EncodingFormatErrorKind::ZeroFrames.into());
        }
        self.info.animation_control = Some(AnimationControl {
            num_frames,
            num_plays,
        });
        self.info.frame_control = Some(FrameControl {
            sequence_number: 0,
            width: self.info.width,
            height: self.info.height,
            ..FrameControl::default()
        });
        Ok(())
    }

    /// Mark the first frame written as a default image outside the
    /// animation (no `fcTL`; `num_frames` still counts only the frames
    /// written afterward).
    ///
    /// # Errors
    ///
    /// The encoder is not animated ([`Encoder::set_animated`] first).
    pub fn set_sep_def_img(&mut self, sep: bool) -> Result<(), EncodingError> {
        if self.info.animation_control.is_none() {
            return Err(EncodingFormatErrorKind::NotAnimated.into());
        }
        self.options.sep_def_img = sep;
        Ok(())
    }

    /// Set the raw `PLTE` payload.
    pub fn set_palette<T: Into<Cow<'a, [u8]>>>(&mut self, palette: T) {
        self.info.palette = Some(palette.into());
    }

    /// Set the raw `tRNS` payload, written verbatim (the `trns_original`
    /// bypass in the private `ancillary::emit::trns_payload`).
    pub fn set_trns<T: Into<Cow<'a, [u8]>>>(&mut self, trns: T) {
        let bytes = trns.into();
        self.info.trns_original = Some(bytes.clone().into_owned());
        self.info.trns = Some(bytes);
    }

    /// Set the source display gamma (`gAMA`).
    pub fn set_source_gamma(&mut self, gamma: ScaledFloat) {
        self.info.source_gamma = Some(gamma);
    }

    /// Set the source display chromaticities (`cHRM`).
    pub fn set_source_chromaticities(&mut self, chroma: SourceChromaticities) {
        self.info.source_chromaticities = Some(chroma);
    }

    /// Mark the image as sRGB with the given rendering intent, clearing any
    /// ICC profile (the two chunks are mutually exclusive).
    pub fn set_source_srgb(&mut self, intent: SrgbRenderingIntent) {
        self.info.srgb = Some(intent);
        self.info.icc_profile = None;
    }

    /// Set the `pHYs` physical pixel dimensions, or clear them.
    pub fn set_pixel_dims(&mut self, dims: Option<PixelDimensions>) {
        self.info.pixel_dims = dims;
    }

    /// Set the colour type.
    pub fn set_color(&mut self, color: ColorType) {
        self.info.color_type = color;
    }

    /// Set the bit depth.
    pub fn set_depth(&mut self, depth: BitDepth) {
        self.info.bit_depth = depth;
    }

    /// Write the image with Adam7 interlacing.
    ///
    /// An extension over `png` 0.18, which cannot write interlaced files at
    /// all. The pixels [`Writer::write_image_data`] expects are unchanged —
    /// always the full image in plain row-major order — the encoder does
    /// the Adam7 split.
    pub fn set_interlaced(&mut self, interlaced: bool) {
        self.info.interlaced = interlaced;
    }

    /// Choose compression via one of the five coarse presets, which also
    /// selects a matching filter strategy (`png` 0.18's own
    /// `Filter::from_simple` mapping: `NoCompression -> NoFilter`,
    /// everything else `-> Adaptive`).
    pub fn set_compression(&mut self, compression: Compression) {
        self.set_deflate_compression(DeflateCompression::from_compression(compression));
        self.set_filter(filter_from_compression(compression));
    }

    /// Choose the exact `oxiarc-deflate` level (or no compression at all).
    pub fn set_deflate_compression(&mut self, compression: DeflateCompression) {
        self.options.compression = compression;
    }

    /// Choose the per-row filter strategy. Default [`Filter::Adaptive`].
    pub fn set_filter(&mut self, filter: Filter) {
        self.options.filter = filter;
    }

    /// The `IDAT`/`fdAT` payload size to split compressed output at.
    ///
    /// Default 64 KiB. **Not** a `png` 0.18 parity value: that crate has no
    /// equivalent knob at all and chunks `IDAT` at `MAX_IDAT_CHUNK_LEN =
    /// u32::MAX >> 1` (one ~2 GiB chunk for any realistic image) — see
    /// `encoder::zlib::DEFAULT_CHUNK_SIZE`'s own doc for why this crate
    /// picks the smaller, streaming-friendlier conventional size instead.
    pub fn set_idat_chunk_size(&mut self, size: usize) {
        self.options.idat_chunk_size = size.max(1);
    }

    /// Set every following frame's display delay, in seconds
    /// (`numerator / denominator`; `denominator == 0` means `100`).
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_frame_delay(&mut self, num: u16, den: u16) -> Result<(), EncodingError> {
        match &mut self.info.frame_control {
            Some(fctl) => {
                fctl.delay_num = num;
                fctl.delay_den = den;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set every following frame's blend operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_blend_op(&mut self, op: BlendOp) -> Result<(), EncodingError> {
        match &mut self.info.frame_control {
            Some(fctl) => {
                fctl.blend_op = op;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set every following frame's dispose operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_dispose_op(&mut self, op: DisposeOp) -> Result<(), EncodingError> {
        match &mut self.info.frame_control {
            Some(fctl) => {
                fctl.dispose_op = op;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Append an uncompressed `tEXt` chunk.
    pub fn add_text_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.info
            .uncompressed_latin1_text
            .push(TEXtChunk::new(keyword, text));
        Ok(())
    }

    /// Append a zlib-compressed `zTXt` chunk.
    pub fn add_ztxt_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.info
            .compressed_latin1_text
            .push(ZTXtChunk::new(keyword, text));
        Ok(())
    }

    /// Append a UTF-8 `iTXt` chunk.
    pub fn add_itxt_chunk(&mut self, keyword: String, text: String) -> Result<(), EncodingError> {
        self.info.utf8_text.push(ITXtChunk::new(keyword, text));
        Ok(())
    }

    /// Turn on sequencing checks in the returned [`Writer`]: writing more
    /// images than the animation declares, or finishing with frames
    /// missing, becomes an error instead of silently accepted. Off by
    /// default.
    pub fn validate_sequence(&mut self, validate: bool) {
        self.options.validate_sequence = validate;
    }

    /// Write the signature, `IHDR` and every ancillary chunk that precedes
    /// `IDAT`, and return a [`Writer`] for the pixel data.
    ///
    /// # Errors
    ///
    /// Zero width or height, or a colour type / bit depth combination Table
    /// 11.1 forbids.
    pub fn write_header(mut self) -> Result<Writer<W>, EncodingError> {
        if self.info.width == 0 {
            return Err(EncodingFormatErrorKind::ZeroWidth.into());
        }
        if self.info.height == 0 {
            return Err(EncodingFormatErrorKind::ZeroHeight.into());
        }
        if ColorType::is_combination_invalid(self.info.color_type, self.info.bit_depth) {
            return Err(EncodingFormatErrorKind::InvalidColorCombination.into());
        }
        let ihdr = self.info.ihdr();
        header::write_header(
            &mut self.w,
            ihdr,
            &self.info,
            self.options.compression.level(),
        )?;
        Ok(Writer::new(self.w, &self.info, self.options))
    }
}

/// The subset of [`Info`] [`Writer`] needs after the header has already been
/// written — deliberately lifetime-free, which is what lets `Writer<W>`
/// carry no `'a` parameter and match `png` 0.18's shape exactly.
#[derive(Debug, Clone)]
pub(crate) struct FrameGeometry {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bit_depth: BitDepth,
    pub(crate) color_type: ColorType,
    pub(crate) interlaced: bool,
    pub(crate) has_palette: bool,
}

impl FrameGeometry {
    pub(crate) fn row_stride_for_width(&self, width: u32) -> usize {
        Ihdr {
            width,
            height: 1,
            bit_depth: self.bit_depth,
            color_type: self.color_type,
            interlace: Interlace::None,
        }
        .row_stride()
    }

    pub(crate) fn bpp(&self) -> crate::header::BytesPerPixel {
        crate::header::BytesPerPixel::from_color_and_depth(self.color_type, self.bit_depth)
    }

    /// Bits occupied by one pixel: `samples * bit_depth`. Always fits in a
    /// `u8` (Table 11.1's widest legal pixel is Rgba16 at 64 bits).
    pub(crate) fn bits_per_pixel(&self) -> u8 {
        self.color_type.samples_u8() * (self.bit_depth as u8)
    }
}

/// Writes image data after [`Encoder::write_header`].
///
/// `Drop` writes `IEND` on a best-effort basis if [`Writer::finish`] was
/// never called, exactly as `png` 0.18 does — any I/O error at that point is
/// swallowed, because a destructor cannot propagate one. **Prefer
/// `finish()`**: it is the only path that reports a write failure or a
/// sequencing problem.
pub struct Writer<W: Write> {
    pub(crate) w: W,
    pub(crate) geometry: FrameGeometry,
    pub(crate) options: EncOptions,
    pub(crate) frame_control: Option<FrameControl>,
    pub(crate) animation_control: Option<AnimationControl>,
    pub(crate) images_written: u64,
    pub(crate) animation_written: u32,
    iend_written: bool,
}

impl<W: Write> Writer<W> {
    fn new(w: W, info: &Info<'_>, options: EncOptions) -> Writer<W> {
        Writer {
            w,
            geometry: FrameGeometry {
                width: info.width,
                height: info.height,
                bit_depth: info.bit_depth,
                color_type: info.color_type,
                interlaced: info.interlaced,
                has_palette: info.palette.is_some(),
            },
            options,
            frame_control: info.frame_control,
            animation_control: info.animation_control,
            images_written: 0,
            animation_written: 0,
            iend_written: false,
        }
    }

    /// Write one raw chunk (length, type, CRC computed here).
    ///
    /// # Errors
    ///
    /// The payload exceeds the 2^31-1 chunk-length cap, or the underlying
    /// writer fails.
    pub fn write_chunk(
        &mut self,
        kind: crate::chunk::ChunkType,
        data: &[u8],
    ) -> Result<(), EncodingError> {
        crate::chunk::write_chunk(&mut self.w, kind, data)?;
        Ok(())
    }

    /// Write one text chunk (`tEXt`, `zTXt` or `iTXt`).
    ///
    /// # Errors
    ///
    /// The keyword or text cannot be encoded, or the underlying writer
    /// fails.
    pub fn write_text_chunk<T: EncodableTextChunk>(
        &mut self,
        chunk: &T,
    ) -> Result<(), EncodingError> {
        chunk.encode(&mut self.w)
    }

    /// Change the per-row filter strategy for frames written from now on.
    pub fn set_filter(&mut self, filter: Filter) {
        self.options.filter = filter;
    }

    /// Set the following frame's display delay.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_frame_delay(&mut self, num: u16, den: u16) -> Result<(), EncodingError> {
        match &mut self.frame_control {
            Some(fctl) => {
                fctl.delay_num = num;
                fctl.delay_den = den;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set the following frame's rectangle dimensions.
    ///
    /// # Errors
    ///
    /// The encoder is not animated, either dimension is zero, or the
    /// rectangle (at its current position) leaves the canvas.
    pub fn set_frame_dimension(&mut self, width: u32, height: u32) -> Result<(), EncodingError> {
        let canvas = (self.geometry.width, self.geometry.height);
        match &mut self.frame_control {
            Some(fctl) => {
                if width == 0 {
                    return Err(EncodingFormatErrorKind::ZeroWidth.into());
                }
                if height == 0 {
                    return Err(EncodingFormatErrorKind::ZeroHeight.into());
                }
                if Some(width) > canvas.0.checked_sub(fctl.x_offset)
                    || Some(height) > canvas.1.checked_sub(fctl.y_offset)
                {
                    return Err(EncodingFormatErrorKind::OutOfBounds.into());
                }
                fctl.width = width;
                fctl.height = height;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set the following frame's rectangle position.
    ///
    /// # Errors
    ///
    /// The encoder is not animated, or the rectangle (at its current
    /// dimensions) leaves the canvas.
    pub fn set_frame_position(&mut self, x: u32, y: u32) -> Result<(), EncodingError> {
        let canvas = (self.geometry.width, self.geometry.height);
        match &mut self.frame_control {
            Some(fctl) => {
                if Some(x) > canvas.0.checked_sub(fctl.width)
                    || Some(y) > canvas.1.checked_sub(fctl.height)
                {
                    return Err(EncodingFormatErrorKind::OutOfBounds.into());
                }
                fctl.x_offset = x;
                fctl.y_offset = y;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Grow the following frame's rectangle to the whole canvas, from its
    /// current position.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn reset_frame_dimension(&mut self) -> Result<(), EncodingError> {
        let canvas = (self.geometry.width, self.geometry.height);
        match &mut self.frame_control {
            Some(fctl) => {
                fctl.width = canvas.0.saturating_sub(fctl.x_offset);
                fctl.height = canvas.1.saturating_sub(fctl.y_offset);
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Move the following frame's rectangle to `(0, 0)`.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn reset_frame_position(&mut self) -> Result<(), EncodingError> {
        match &mut self.frame_control {
            Some(fctl) => {
                fctl.x_offset = 0;
                fctl.y_offset = 0;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set the following frame's blend operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_blend_op(&mut self, op: BlendOp) -> Result<(), EncodingError> {
        match &mut self.frame_control {
            Some(fctl) => {
                fctl.blend_op = op;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Set the following frame's dispose operator.
    ///
    /// # Errors
    ///
    /// The encoder is not animated.
    pub fn set_dispose_op(&mut self, op: DisposeOp) -> Result<(), EncodingError> {
        match &mut self.frame_control {
            Some(fctl) => {
                fctl.dispose_op = op;
                Ok(())
            }
            None => Err(EncodingFormatErrorKind::NotAnimated.into()),
        }
    }

    /// Turn this writer into a [`StreamWriter`] with a 4 KiB internal
    /// buffer, borrowing `self` so raw chunks can still be appended
    /// afterward.
    ///
    /// # Errors
    ///
    /// The underlying writer fails while preparing the first chunk.
    pub fn stream_writer(&mut self) -> Result<StreamWriter<'_, W>, EncodingError> {
        self.stream_writer_with_size(stream::DEFAULT_BUFFER_LENGTH)
    }

    /// As [`Writer::stream_writer`], with a caller-chosen internal buffer
    /// size.
    ///
    /// # Errors
    ///
    /// The underlying writer fails while preparing the first chunk.
    pub fn stream_writer_with_size(
        &mut self,
        size: usize,
    ) -> Result<StreamWriter<'_, W>, EncodingError> {
        StreamWriter::new(stream::Target::Borrowed(self), size)
    }

    /// As [`Writer::stream_writer`], consuming `self` so the returned
    /// writer can outlive this call.
    ///
    /// # Errors
    ///
    /// The underlying writer fails while preparing the first chunk.
    pub fn into_stream_writer(self) -> Result<StreamWriter<'static, W>, EncodingError> {
        self.into_stream_writer_with_size(stream::DEFAULT_BUFFER_LENGTH)
    }

    /// As [`Writer::into_stream_writer`], with a caller-chosen internal
    /// buffer size.
    ///
    /// # Errors
    ///
    /// The underlying writer fails while preparing the first chunk.
    pub fn into_stream_writer_with_size(
        self,
        size: usize,
    ) -> Result<StreamWriter<'static, W>, EncodingError> {
        StreamWriter::new(stream::Target::Owned(self), size)
    }

    /// Write `data` as one complete frame: filtered scanlines fed into a
    /// single continuous zlib stream, split into `IDAT`/`fdAT` chunks.
    ///
    /// `data` is always the **full, non-interlaced** image — `width *
    /// height` (or the current frame's rectangle, for an animation)
    /// samples in row-major order, no filter bytes. When
    /// [`Encoder::set_interlaced`] was set, the Adam7 split happens here.
    ///
    /// # Writing more frames than the animation declares
    ///
    /// Once `acTL`'s `num_frames` frames have been written the internal
    /// `fcTL` is cleared and any **further** call falls back to writing a
    /// plain `IDAT` — which lands *after* the `fdAT` chunks and so produces
    /// a file whose `IDAT`s are not consecutive, i.e. one the PNG
    /// specification forbids and other decoders may reject. This is
    /// deliberate parity with `png` 0.18 (`Writer::increment_images_written`:
    /// *"If we've written all animation frames, all following will be normal
    /// image chunks"*), kept rather than diverged from, and it is why
    /// [`Encoder::validate_sequence`] exists: turn it on and the extra call
    /// is an `EndReached` error instead.
    ///
    /// # Errors
    ///
    /// `data.len()` does not match the expected size, the colour type is
    /// `Indexed` with no palette set, the current frame rectangle is empty
    /// or leaves the canvas, sequencing was requested and this call would
    /// exceed it, or the underlying writer fails.
    pub fn write_image_data(&mut self, data: &[u8]) -> Result<(), EncodingError> {
        image_data::write_image_data(self, data)
    }

    fn write_iend(&mut self) -> Result<(), EncodingError> {
        self.iend_written = true;
        self.write_chunk(crate::chunk::IEND, &[])
    }

    fn validate_sequence_done(&self) -> Result<(), EncodingError> {
        if !self.options.validate_sequence {
            return Ok(());
        }
        let animation_incomplete = self.animation_control.is_some() && self.frame_control.is_some();
        if animation_incomplete || self.images_written == 0 {
            let remaining = self
                .animation_control
                .map(|a| a.num_frames.saturating_sub(self.animation_written))
                .unwrap_or(1);
            return Err(EncodingFormatErrorKind::MissingFrames(remaining).into());
        }
        Ok(())
    }

    /// Finish the file: validate sequencing (if requested), write `IEND`,
    /// flush the underlying writer.
    ///
    /// This is the **checked** path — prefer it over letting `Writer` drop.
    ///
    /// # Errors
    ///
    /// A missing-frames sequencing violation, or an I/O failure.
    pub fn finish(mut self) -> Result<(), EncodingError> {
        self.validate_sequence_done()?;
        self.write_iend()?;
        self.w.flush()?;
        Ok(())
    }
}

impl<W: Write> Drop for Writer<W> {
    fn drop(&mut self) {
        if !self.iend_written {
            let _ = self.write_iend();
        }
    }
}

pub(crate) fn buffer_size_error(expected: usize, actual: usize) -> EncodingError {
    ParameterErrorKind::ImageBufferSize { expected, actual }.into()
}

/// The one frame-rectangle validity rule, shared by every route that can
/// install an `fcTL`: [`Encoder::with_info`], [`Writer::set_frame_dimension`]
/// and [`Writer::set_frame_position`] up front, plus
/// [`Writer::write_image_data`] and `StreamWriter::new` as a guard at the
/// point of use.
///
/// The point-of-use guard is not redundant belt-and-braces: `row_stride`
/// derives from the frame width, a zero width makes it zero, and the row
/// loops divide the caller's buffer with `chunks_exact(row_stride)`, which
/// **panics** on a zero chunk size. An empty rectangle must therefore never
/// reach them, whatever new constructor is added later.
///
/// `check_bounds` is `false` only when the canvas itself is still
/// unvalidated (zero width or height), where `write_header`'s own
/// `ZeroWidth`/`ZeroHeight` is the more useful error than the `OutOfBounds`
/// a bounds test against a zero canvas would produce.
pub(crate) fn check_frame_rect(
    x_offset: u32,
    y_offset: u32,
    width: u32,
    height: u32,
    canvas_width: u32,
    canvas_height: u32,
    check_bounds: bool,
) -> Result<(), EncodingError> {
    if width == 0 {
        return Err(EncodingFormatErrorKind::ZeroWidth.into());
    }
    if height == 0 {
        return Err(EncodingFormatErrorKind::ZeroHeight.into());
    }
    if !check_bounds {
        return Ok(());
    }
    let right = x_offset
        .checked_add(width)
        .ok_or(EncodingFormatErrorKind::OutOfBounds)?;
    let bottom = y_offset
        .checked_add(height)
        .ok_or(EncodingFormatErrorKind::OutOfBounds)?;
    if right > canvas_width || bottom > canvas_height {
        return Err(EncodingFormatErrorKind::OutOfBounds.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{BitDepth, ColorType};

    #[test]
    fn minimal_round_trip_through_our_own_decoder() {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 2, 2);
        enc.set_color(ColorType::Grayscale);
        enc.set_depth(BitDepth::Eight);
        let mut w = enc.write_header().expect("header");
        w.write_image_data(&[1, 2, 3, 4]).expect("image data");
        w.finish().expect("finish");

        let image = crate::decode(&buf).expect("decode");
        assert_eq!(image.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn zero_dimensions_are_rejected_at_write_header() {
        let mut buf = Vec::new();
        let enc = Encoder::new(&mut buf, 0, 2);
        assert!(enc.write_header().is_err());
    }

    #[test]
    fn invalid_color_depth_combination_is_rejected() {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 1, 1);
        enc.set_color(ColorType::Rgb);
        enc.set_depth(BitDepth::Four);
        assert!(enc.write_header().is_err());
    }

    #[test]
    fn drop_writes_iend_best_effort() {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf, 1, 1);
            enc.set_color(ColorType::Grayscale);
            let mut w = enc.write_header().expect("header");
            w.write_image_data(&[9]).expect("image data");
            // no finish() call: Drop must still write IEND.
        }
        let kinds: Vec<_> = crate::chunk::ChunkIter::new(&buf)
            .map(|c| c.expect("ok").0)
            .collect();
        assert_eq!(
            *kinds.last().expect("at least one chunk"),
            crate::chunk::IEND
        );
    }

    #[test]
    fn finish_reports_missing_frames_when_validation_is_on() {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 1, 1);
        enc.validate_sequence(true);
        let w = enc.write_header().expect("header");
        assert!(w.finish().is_err());
    }

    #[test]
    fn with_info_rejects_mismatched_animation_fields() {
        let mut info = Info::with_size(1, 1);
        info.animation_control = Some(AnimationControl {
            num_frames: 1,
            num_plays: 0,
        });
        let buf: Vec<u8> = Vec::new();
        assert!(Encoder::with_info(buf, info).is_err());
    }

    #[test]
    fn frame_setters_require_animation() {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, 4, 4);
        enc.set_color(ColorType::Grayscale);
        let mut w = enc.write_header().expect("header");
        assert!(w.set_frame_delay(1, 2).is_err());
        assert!(w.set_frame_dimension(1, 1).is_err());
        assert!(w.set_frame_position(0, 0).is_err());
        assert!(w.reset_frame_dimension().is_err());
        assert!(w.reset_frame_position().is_err());
        assert!(w.set_blend_op(BlendOp::Over).is_err());
        assert!(w.set_dispose_op(DisposeOp::Background).is_err());
        w.write_image_data(&[0; 16]).expect("image data");
    }
}
