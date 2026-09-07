//! APNG: control-chunk validation, and the [`ApngDecoder`] compositor.
//!
//! The structural half — the `acTL` frame count, the single sequence-number
//! counter shared by `fcTL` and `fdAT`, sub-frame bounds, and the
//! file-level byte budget that the codec's own per-stream cap cannot
//! provide (each frame is an independent zlib stream and resets that cap)
//! — is enforced by [`Decoder`]/[`Reader`] themselves via a private
//! tracker this module also owns. [`ApngDecoder`] is the compositor built on
//! top: it drives a plain [`Reader`] and turns its sequence of subframes
//! into full-canvas RGBA frames, honouring `dispose_op`/`blend_op`.

use std::io::{Read, Write};
use std::time::Duration;

use crate::common::{AnimationControl, BlendOp, DisposeOp, FrameControl, Transformations};
use crate::decoder::{Decoder, Reader};
use crate::encoder::{Encoder, Writer};
use crate::error::{
    DecodingError, EncodingError, EncodingFormatErrorKind, FormatErrorKind, ParameterErrorKind,
};
use crate::header::{BitDepth, ColorType};
use crate::image::Image;
use crate::limits::DecodeLimits;

/// Check that a sub-frame rectangle fits inside the canvas.
///
/// ```
/// use oxiarc_png::apng::validate_frame_bounds;
/// use oxiarc_png::FrameControl;
/// let fc = FrameControl { width: 4, height: 4, x_offset: 2, y_offset: 2, ..Default::default() };
/// assert!(validate_frame_bounds(&fc, 8, 8).is_ok());
/// assert!(validate_frame_bounds(&fc, 5, 8).is_err());
/// ```
pub fn validate_frame_bounds(
    frame: &FrameControl,
    canvas_width: u32,
    canvas_height: u32,
) -> Result<(), DecodingError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(FormatErrorKind::BadSubFrameBounds.into());
    }
    let right = frame
        .x_offset
        .checked_add(frame.width)
        .ok_or(FormatErrorKind::BadSubFrameBounds)?;
    let bottom = frame
        .y_offset
        .checked_add(frame.height)
        .ok_or(FormatErrorKind::BadSubFrameBounds)?;
    if right > canvas_width || bottom > canvas_height {
        return Err(FormatErrorKind::BadSubFrameBounds.into());
    }
    Ok(())
}

/// Tracks the animation state of one file while it is being read.
#[derive(Clone, Debug, Default)]
pub(crate) struct ApngTracker {
    /// The `acTL` chunk, once seen.
    pub(crate) control: Option<AnimationControl>,
    /// The next sequence number the file must present.
    pub(crate) next_sequence: u32,
    /// Whether an `fcTL` preceded `IDAT`, which makes the default image the
    /// animation's first frame.
    pub(crate) fctl_before_idat: bool,
    /// The `fcTL` currently in force.
    pub(crate) current: Option<FrameControl>,
    /// How many `fcTL` chunks have been accepted.
    pub(crate) frames_seen: u32,
}

impl ApngTracker {
    /// Record an `acTL` chunk.
    pub(crate) fn observe_actl(
        &mut self,
        control: AnimationControl,
        limits: &DecodeLimits,
    ) -> Result<(), DecodingError> {
        if control.num_frames == 0 {
            return Err(FormatErrorKind::ZeroFrames.into());
        }
        if control.num_frames > limits.max_frames {
            return Err(DecodingError::LimitsExceeded);
        }
        self.control = Some(control);
        Ok(())
    }

    /// Record an `fcTL` chunk, validating its sequence number and bounds.
    ///
    /// The first frame is special: when it precedes `IDAT` it describes the
    /// default image, and must therefore cover the whole canvas at the origin.
    pub(crate) fn observe_fctl(
        &mut self,
        frame: FrameControl,
        canvas: (u32, u32),
        have_idat: bool,
    ) -> Result<(), DecodingError> {
        self.check_sequence(frame.sequence_number)?;
        if !have_idat {
            if self.frames_seen > 0 {
                // A second fcTL before IDAT is not meaningful.
                return Err(FormatErrorKind::ApngOrder {
                    present: frame.sequence_number,
                    expected: self.next_sequence,
                }
                .into());
            }
            if frame.width != canvas.0
                || frame.height != canvas.1
                || frame.x_offset != 0
                || frame.y_offset != 0
            {
                return Err(FormatErrorKind::BadSubFrameBounds.into());
            }
            self.fctl_before_idat = true;
        } else {
            validate_frame_bounds(&frame, canvas.0, canvas.1)?;
        }
        self.frames_seen = self.frames_seen.saturating_add(1);
        self.current = Some(frame);
        Ok(())
    }

    /// Record an `fdAT` chunk's sequence number.
    pub(crate) fn observe_fdat(&mut self, sequence: u32) -> Result<(), DecodingError> {
        if self.current.is_none() {
            return Err(FormatErrorKind::MissingFctl.into());
        }
        self.check_sequence(sequence)
    }

    fn check_sequence(&mut self, sequence: u32) -> Result<(), DecodingError> {
        if sequence != self.next_sequence {
            return Err(FormatErrorKind::ApngOrder {
                present: sequence,
                expected: self.next_sequence,
            }
            .into());
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(())
    }

    /// The number of animation frames the caller should expect.
    ///
    /// When the default image is not part of the animation it is not counted,
    /// exactly as the APNG specification requires.
    pub(crate) fn frame_count(&self) -> u32 {
        self.control.map_or(0, |c| c.num_frames)
    }
}

/// The disposal a frame should be given, with the specification's first-frame
/// override applied: `Previous` on the very first frame has nothing to restore
/// and is treated as `Background`.
#[must_use]
pub fn effective_dispose_op(dispose: DisposeOp, is_first_frame: bool) -> DisposeOp {
    if is_first_frame && dispose == DisposeOp::Previous {
        DisposeOp::Background
    } else {
        dispose
    }
}

/// One sub-frame exactly as the file stored it: at its own `fcTL` dimensions
/// and offset, in the file's native colour type and bit depth, with **no**
/// composition applied. This is what an APNG-agnostic tool that only wants
/// "the frames, uncomposited" should read.
#[derive(Debug)]
pub struct Subframe<'a> {
    /// This subframe's `fcTL`.
    pub control: FrameControl,
    /// Native colour type (shared by every frame in the file).
    pub color_type: ColorType,
    /// Native bit depth (shared by every frame in the file).
    pub bit_depth: BitDepth,
    /// Exactly `control.width * control.height` pixels, packed the way
    /// [`crate::Image::data`] is: MSB-first for sub-byte depths, big-endian
    /// for 16-bit samples, no filter bytes.
    pub data: &'a [u8],
}

/// One fully composed animation frame: the whole canvas, after this frame's
/// `blend_op` was applied over whatever the previous frame's `dispose_op`
/// left behind.
#[derive(Debug)]
pub struct ComposedFrame<'a> {
    /// This frame's `fcTL` (position, size, delay, dispose/blend — dispose
    /// is the *stored* value; [`effective_dispose_op`] is already applied
    /// internally before this frame was disposed of).
    pub control: FrameControl,
    /// Canvas-sized RGBA8 (4 bytes/pixel) or RGBA16 big-endian (8
    /// bytes/pixel) when the file's native bit depth is
    /// [`BitDepth::Sixteen`], non-premultiplied.
    pub canvas: &'a [u8],
    /// Canvas width in pixels (the file's `IHDR` width, constant across
    /// every composed frame).
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// This frame's display duration.
    pub delay: Duration,
}

/// Composites an APNG's animation frames onto one RGBA canvas, applying
/// `dispose_op`/`blend_op` per PNG 3rd Edition §13.3.
///
/// Built on a plain [`Reader`] — no frame-iteration logic is duplicated
/// here. Each subframe is decoded in the file's native colour type (so
/// palette/`tRNS`/sub-byte scaling all go through the already-tested
/// [`crate::Image::to_rgba16`]) and then blended into an internal RGBA16
/// canvas; [`ComposedFrame::canvas`] is materialised from that canvas on
/// each call, truncated to RGBA8 unless the source is 16-bit.
///
/// A static default image (`IDAT` with no preceding `fcTL`) is decoded to
/// keep the stream in sync but is **not** emitted as a composed frame — it
/// is not part of the animation (PNG 3rd Edition §13.2).
pub struct ApngDecoder<R: Read> {
    reader: Reader<R>,
    scratch: Vec<u8>,
    canvas: Vec<u16>,
    saved: Option<Vec<u16>>,
    canvas_bytes: Vec<u8>,
    canvas_width: u32,
    canvas_height: u32,
    total_frames: u32,
    composed: u32,
    sixteen_bit: bool,
    consumed_default_image: bool,
}

impl<R: Read> ApngDecoder<R> {
    /// Read the header and prepare to composite frames, under the default
    /// [`DecodeLimits`].
    ///
    /// # Errors
    ///
    /// Any structural problem before the image data, or the file has no
    /// `acTL` chunk at all.
    pub fn new(r: R) -> Result<ApngDecoder<R>, DecodingError> {
        Self::new_with_limits(r, DecodeLimits::default())
    }

    /// As [`ApngDecoder::new`], but with an explicit [`DecodeLimits`] rather
    /// than the default — the same knob [`Decoder::set_decode_limits`]
    /// offers for a plain, non-animated decode.
    ///
    /// # Errors
    ///
    /// As `new`, plus: the canvas — always RGBA16 internally regardless of
    /// the source depth, and several times the size of one frame buffer —
    /// would exceed `limits.max_alloc_bytes`.
    pub fn new_with_limits(r: R, limits: DecodeLimits) -> Result<ApngDecoder<R>, DecodingError> {
        let mut decoder = Decoder::new(r);
        decoder.set_decode_limits(limits);
        decoder.set_transformations(Transformations::IDENTITY);
        let reader = decoder.read_info()?;
        let info = reader.info();
        let total_frames = info
            .animation_control
            .map(|a| a.num_frames)
            .ok_or(ParameterErrorKind::NotAnimated)?;
        let canvas_width = info.width;
        let canvas_height = info.height;
        let sixteen_bit = info.bit_depth == BitDepth::Sixteen;
        let scratch = vec![0u8; reader.checked_output_buffer_size()?];
        // `canvas` (always RGBA16, regardless of the source depth — see the
        // module doc) and `canvas_bytes` are each several times the size of
        // one frame buffer, and `canvas` is cloned wholesale into `saved`
        // the first time a frame requests `DisposeOp::Previous`, so an
        // unvalidated `IHDR` could otherwise buy far more memory than
        // `checked_output_buffer_size` above ever permits for a single
        // frame. Checked `u64` arithmetic (never `saturating_mul`, which
        // would silently hand back an undersized — and therefore
        // out-of-bounds-panicking — buffer instead of an error) plus
        // `Reader::check_alloc` against the same configured limit closes
        // that gap; `saved`'s later clone is the same size as `canvas` and
        // so needs no separate check.
        let canvas_pixels = u64::from(canvas_width)
            .checked_mul(u64::from(canvas_height))
            .ok_or(DecodingError::LimitsExceeded)?;
        let canvas_u16_len = canvas_pixels
            .checked_mul(4)
            .ok_or(DecodingError::LimitsExceeded)?;
        let canvas_bytes_len = canvas_pixels
            .checked_mul(if sixteen_bit { 8 } else { 4 })
            .ok_or(DecodingError::LimitsExceeded)?;
        let canvas_u16_len = reader.check_alloc(
            canvas_u16_len
                .checked_mul(2) // Vec<u16>: check_alloc counts bytes.
                .ok_or(DecodingError::LimitsExceeded)?,
        )? / 2;
        let canvas_bytes_len = reader.check_alloc(canvas_bytes_len)?;
        Ok(ApngDecoder {
            reader,
            scratch,
            canvas: vec![0u16; canvas_u16_len],
            saved: None,
            canvas_bytes: vec![0u8; canvas_bytes_len],
            canvas_width,
            canvas_height,
            total_frames,
            composed: 0,
            sixteen_bit,
            consumed_default_image: false,
        })
    }

    /// The number of animation frames (`acTL`'s frame count).
    #[must_use]
    pub fn num_frames(&self) -> u32 {
        self.total_frames
    }

    /// The number of times the animation should loop (`0` means forever).
    #[must_use]
    pub fn num_plays(&self) -> u32 {
        self.reader
            .info()
            .animation_control
            .map_or(0, |a| a.num_plays)
    }

    /// Skip the leading static default image, if this file has one and it
    /// has not been consumed yet. A no-op otherwise.
    fn skip_default_image_if_present(&mut self) -> Result<(), DecodingError> {
        if self.consumed_default_image {
            return Ok(());
        }
        self.consumed_default_image = true;
        if self.reader.info().frame_control.is_none() {
            // A plain default image: decode it to advance the stream, then
            // discard it — it is not part of the animation.
            self.reader.next_frame(&mut self.scratch)?;
        }
        Ok(())
    }

    /// The next sub-frame, exactly as stored, or `None` after the last one.
    ///
    /// [`ApngDecoder::next_subframe`] and [`ApngDecoder::next_composed`]
    /// advance the same underlying frame cursor, so they are not meant to
    /// be interleaved: call one or the other for the whole animation,
    /// never alternate calls to each on the same decoder.
    ///
    /// # Errors
    ///
    /// Any structural problem in the frame's chunks or its image data.
    pub fn next_subframe(&mut self) -> Result<Option<Subframe<'_>>, DecodingError> {
        self.skip_default_image_if_present()?;
        if self.composed >= self.total_frames {
            return Ok(None);
        }
        let out = self.reader.next_frame(&mut self.scratch)?;
        let control = self
            .reader
            .info()
            .frame_control
            .ok_or(ParameterErrorKind::NotAnimated)?;
        self.composed += 1;
        Ok(Some(Subframe {
            control,
            color_type: out.color_type,
            bit_depth: out.bit_depth,
            data: &self.scratch[..out.buffer_size()],
        }))
    }

    /// The next fully composed canvas frame, or `None` after the last one.
    ///
    /// Shares one frame cursor with [`ApngDecoder::next_subframe`] — see
    /// that method's doc for why the two are not meant to be interleaved.
    ///
    /// # Errors
    ///
    /// Any structural problem in the frame's chunks or its image data.
    pub fn next_composed(&mut self) -> Result<Option<ComposedFrame<'_>>, DecodingError> {
        self.skip_default_image_if_present()?;
        if self.composed >= self.total_frames {
            return Ok(None);
        }
        let out = self.reader.next_frame(&mut self.scratch)?;
        let control = self
            .reader
            .info()
            .frame_control
            .ok_or(ParameterErrorKind::NotAnimated)?;
        // Belt-and-braces: `ApngTracker::observe_fctl` (called while parsing
        // the `fcTL` chunk, before this frame's pixels were even reachable)
        // already rejected an out-of-bounds rectangle, so this can never
        // actually fire — kept so the compositor's own contract ("region
        // offsets validated") does not silently depend on that upstream
        // check never changing.
        validate_frame_bounds(&control, self.canvas_width, self.canvas_height)?;

        let is_first_composed = self.composed == 0;
        let dispose = effective_dispose_op(control.dispose_op, is_first_composed);

        let subframe_info = self.reader.info().clone();
        let image = Image {
            width: out.width,
            height: out.height,
            color_type: out.color_type,
            bit_depth: out.bit_depth,
            data: self.scratch[..out.buffer_size()].to_vec(),
            info: subframe_info,
        };
        let subframe_rgba16 = image.to_rgba16()?;

        if dispose == DisposeOp::Previous {
            self.saved = Some(self.canvas.clone());
        }
        blend_into_canvas(
            &mut self.canvas,
            self.canvas_width,
            control,
            &subframe_rgba16,
            control.blend_op,
        );

        materialize_canvas(&self.canvas, self.sixteen_bit, &mut self.canvas_bytes);
        self.composed += 1;

        match dispose {
            DisposeOp::None => {}
            DisposeOp::Background => {
                clear_rect(&mut self.canvas, self.canvas_width, control);
            }
            DisposeOp::Previous => {
                if let Some(saved) = self.saved.take() {
                    self.canvas = saved;
                }
            }
        }

        Ok(Some(ComposedFrame {
            control,
            canvas: &self.canvas_bytes,
            width: self.canvas_width,
            height: self.canvas_height,
            delay: control.delay(),
        }))
    }
}

/// A minimal RGBA8 APNG writer: `acTL`/`fcTL`/`fdAT` sequence-number
/// bookkeeping without a second implementation of it.
///
/// Every frame is written as `ColorType::Rgba`/`BitDepth::Eight`
/// (non-premultiplied, matching [`ComposedFrame::canvas`] for an 8-bit
/// source) — the counterpart of [`ApngDecoder::next_composed`]. This type
/// carries **no** lifetime parameter, unlike the general [`Encoder`]/
/// [`Writer`] pair it wraps: it never holds borrowed palette or `tRNS`
/// bytes, so there is nothing for a lifetime to track. For any other
/// colour type or bit depth, or for ancillary metadata beyond the
/// animation chunks, use [`Encoder`]/[`Writer`] directly — every method
/// this type calls (`set_animated`, `set_frame_dimension`, `write_image_data`,
/// …) is public on them too.
pub struct ApngEncoder<W: Write> {
    state: ApngEncoderState<W>,
}

enum ApngEncoderState<W: Write> {
    /// Before the first [`ApngEncoder::write_frame`] call: still accepting
    /// [`ApngEncoder::set_default_image_is_first_frame`].
    Building(Box<Encoder<'static, W>>),
    /// After the header has been committed.
    Writing(Box<Writer<W>>),
    /// Placeholder only ever observed transiently inside
    /// [`ApngEncoder::ensure_writer`]; reaching it from the outside means a
    /// previous call already failed unrecoverably.
    Poisoned,
}

impl<W: Write> ApngEncoder<W> {
    /// An animated `width` x `height` RGBA8 encoder: `num_frames` frames,
    /// looping `num_plays` times (`0` means forever).
    ///
    /// # Errors
    ///
    /// `num_frames == 0`.
    pub fn new(
        w: W,
        width: u32,
        height: u32,
        num_frames: u32,
        num_plays: u32,
    ) -> Result<ApngEncoder<W>, EncodingError> {
        let mut encoder = Encoder::new(w, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_animated(num_frames, num_plays)?;
        Ok(ApngEncoder {
            state: ApngEncoderState::Building(Box::new(encoder)),
        })
    }

    /// Whether the first [`ApngEncoder::write_frame`] call's data is frame
    /// 0 of the animation (`true`, the default) or a static default image
    /// outside it, written as a plain `IDAT` with no `fcTL` — in which case
    /// its `control` still supplies the data's dimensions (conventionally
    /// the whole canvas) but no `fcTL` chunk is emitted for it, and
    /// `num_frames` counts only the calls that follow.
    ///
    /// A no-op once the first frame has already been written — call this
    /// before the first [`ApngEncoder::write_frame`].
    pub fn set_default_image_is_first_frame(&mut self, yes: bool) {
        if let ApngEncoderState::Building(encoder) = &mut self.state {
            // Infallible here: `new` already called `set_animated`.
            let _ = encoder.set_sep_def_img(!yes);
        }
    }

    fn ensure_writer(&mut self) -> Result<&mut Writer<W>, EncodingError> {
        if matches!(self.state, ApngEncoderState::Building(_)) {
            let ApngEncoderState::Building(encoder) =
                std::mem::replace(&mut self.state, ApngEncoderState::Poisoned)
            else {
                unreachable!("just matched Building above");
            };
            self.state = ApngEncoderState::Writing(Box::new(encoder.write_header()?));
        }
        match &mut self.state {
            ApngEncoderState::Writing(writer) => Ok(writer),
            ApngEncoderState::Building(_) => unreachable!("just transitioned out of Building"),
            ApngEncoderState::Poisoned => Err(EncodingFormatErrorKind::Unrecoverable.into()),
        }
    }

    /// Write one frame: `ctl`'s rectangle, delay, blend and dispose
    /// operators, then `data` as that rectangle's RGBA8 pixels
    /// (`ctl.width * ctl.height * 4` bytes).
    ///
    /// # The first frame must cover the canvas
    ///
    /// The first call writes the **default image**, as `IDAT` — with a
    /// preceding `fcTL` by default, or without one when
    /// [`ApngEncoder::set_default_image_is_first_frame`] was turned off —
    /// and the default image is the `IHDR` image: the whole canvas, at the
    /// origin. A first `ctl` that is a sub-rectangle is therefore rejected
    /// with `OutOfBounds` rather than written as a file no decoder (this
    /// crate's own included) can read; see
    /// `encoder::image_data::check_current_frame_rect` for how the two
    /// variants fail differently. Every call **after** the first may be any
    /// sub-rectangle the canvas allows — those become `fdAT`.
    ///
    /// # Errors
    ///
    /// `data.len()` does not match `ctl`'s rectangle, the rectangle is empty
    /// or leaves the canvas, it is a sub-rectangle on the first (default
    /// image) call (above), more frames were written than
    /// [`ApngEncoder::new`] declared (only reported if `data` size
    /// mismatches would not already have caught it), or the underlying
    /// writer fails.
    pub fn write_frame(&mut self, ctl: &FrameControl, data: &[u8]) -> Result<(), EncodingError> {
        let writer = self.ensure_writer()?;
        // Reset before resize before reposition: `set_frame_dimension`
        // checks the new size against the *current* offset and
        // `set_frame_position` checks the new offset against the *current*
        // size, so setting them in the wrong order can reject a legal
        // rectangle purely because of what the previous frame left behind.
        writer.reset_frame_position()?;
        writer.set_frame_dimension(ctl.width, ctl.height)?;
        writer.set_frame_position(ctl.x_offset, ctl.y_offset)?;
        writer.set_frame_delay(ctl.delay_num, ctl.delay_den)?;
        writer.set_blend_op(ctl.blend_op)?;
        writer.set_dispose_op(ctl.dispose_op)?;
        writer.write_image_data(data)
    }

    /// Finish the file: write `IEND` and flush.
    ///
    /// # Errors
    ///
    /// The underlying writer fails, or no frame was ever written.
    pub fn finish(mut self) -> Result<(), EncodingError> {
        // `ensure_writer` commits the header even if no frame was ever
        // written (valid per `png` 0.18 semantics unless
        // `Encoder::validate_sequence` was turned on, which this wrapper
        // does not expose).
        self.ensure_writer()?;
        match std::mem::replace(&mut self.state, ApngEncoderState::Poisoned) {
            ApngEncoderState::Writing(writer) => writer.finish(),
            _ => Err(EncodingFormatErrorKind::Unrecoverable.into()),
        }
    }
}

/// Blend `src` (an `w x h` RGBA16 rectangle) into `canvas` (`canvas_width`
/// wide) at `control`'s offset, per `blend_op`.
fn blend_into_canvas(
    canvas: &mut [u16],
    canvas_width: u32,
    control: FrameControl,
    src: &[u16],
    blend_op: BlendOp,
) {
    let cw = canvas_width as usize;
    for y in 0..control.height as usize {
        let dst_row = (control.y_offset as usize + y) * cw + control.x_offset as usize;
        let src_row = y * control.width as usize;
        for x in 0..control.width as usize {
            let d = (dst_row + x) * 4;
            let s = (src_row + x) * 4;
            let Some(src_px) = src.get(s..s + 4) else {
                continue;
            };
            let Some(dst_px) = canvas.get_mut(d..d + 4) else {
                continue;
            };
            match blend_op {
                BlendOp::Source => dst_px.copy_from_slice(src_px),
                BlendOp::Over => {
                    let blended = source_over(
                        [src_px[0], src_px[1], src_px[2], src_px[3]],
                        [dst_px[0], dst_px[1], dst_px[2], dst_px[3]],
                    );
                    dst_px.copy_from_slice(&blended);
                }
            }
        }
    }
}

/// Standard non-premultiplied source-over compositing of one RGBA16 pixel
/// over another, per the Porter-Duff "over" operator.
fn source_over(src: [u16; 4], dst: [u16; 4]) -> [u16; 4] {
    const MAX: f64 = 65535.0;
    let sa = f64::from(src[3]) / MAX;
    if sa >= 1.0 {
        return src;
    }
    if sa <= 0.0 {
        return dst;
    }
    let da = f64::from(dst[3]) / MAX;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let mut out = [0u16; 4];
    for c in 0..3 {
        let s = f64::from(src[c]) / MAX;
        let d = f64::from(dst[c]) / MAX;
        let mixed = (s * sa + d * da * (1.0 - sa)) / out_a;
        out[c] = (mixed.clamp(0.0, 1.0) * MAX).round() as u16;
    }
    out[3] = (out_a.clamp(0.0, 1.0) * MAX).round() as u16;
    out
}

/// Clear `control`'s rectangle of `canvas` to fully transparent black.
fn clear_rect(canvas: &mut [u16], canvas_width: u32, control: FrameControl) {
    let cw = canvas_width as usize;
    for y in 0..control.height as usize {
        let row_start = (control.y_offset as usize + y) * cw + control.x_offset as usize;
        let Some(row) = canvas.get_mut(row_start * 4..(row_start + control.width as usize) * 4)
        else {
            continue;
        };
        row.fill(0);
    }
}

/// Materialise an RGBA16 canvas into its `png`-native byte form: big-endian
/// 16-bit samples when the source is 16-bit, else 8-bit high-byte
/// truncation (matching [`crate::Image::to_rgba8`]'s own truncation).
fn materialize_canvas(canvas: &[u16], sixteen_bit: bool, out: &mut [u8]) {
    if sixteen_bit {
        for (v, chunk) in canvas.iter().zip(out.chunks_exact_mut(2)) {
            chunk.copy_from_slice(&v.to_be_bytes());
        }
    } else {
        for (v, byte) in canvas.iter().zip(out.iter_mut()) {
            *byte = (*v >> 8) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(seq: u32, w: u32, h: u32, x: u32, y: u32) -> FrameControl {
        FrameControl {
            sequence_number: seq,
            width: w,
            height: h,
            x_offset: x,
            y_offset: y,
            ..FrameControl::default()
        }
    }

    #[test]
    fn bounds_checks() {
        assert!(validate_frame_bounds(&fc(0, 8, 8, 0, 0), 8, 8).is_ok());
        assert!(validate_frame_bounds(&fc(0, 0, 8, 0, 0), 8, 8).is_err());
        assert!(validate_frame_bounds(&fc(0, 8, 0, 0, 0), 8, 8).is_err());
        assert!(validate_frame_bounds(&fc(0, 8, 8, 1, 0), 8, 8).is_err());
        assert!(validate_frame_bounds(&fc(0, 1, 1, u32::MAX, 0), 8, 8).is_err());
    }

    #[test]
    fn sequence_numbers_form_one_counter() {
        let mut tracker = ApngTracker::default();
        let limits = DecodeLimits::default();
        tracker
            .observe_actl(
                AnimationControl {
                    num_frames: 2,
                    num_plays: 0,
                },
                &limits,
            )
            .expect("actl");
        tracker
            .observe_fctl(fc(0, 8, 8, 0, 0), (8, 8), false)
            .expect("fctl 0");
        // IDAT is frame 0; the next fcTL is sequence 1, then its fdAT is 2.
        tracker
            .observe_fctl(fc(1, 4, 4, 2, 2), (8, 8), true)
            .expect("fctl 1");
        tracker.observe_fdat(2).expect("fdat 2");
        assert!(tracker.observe_fdat(4).is_err(), "a gap must be rejected");
    }

    #[test]
    fn zero_frames_and_oversized_counts_are_rejected() {
        let mut tracker = ApngTracker::default();
        let limits = DecodeLimits {
            max_frames: 4,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            tracker
                .observe_actl(
                    AnimationControl {
                        num_frames: 0,
                        num_plays: 0
                    },
                    &limits
                )
                .unwrap_err()
                .format_kind(),
            Some(FormatErrorKind::ZeroFrames)
        ));
        assert!(matches!(
            tracker.observe_actl(
                AnimationControl {
                    num_frames: 5,
                    num_plays: 0
                },
                &limits
            ),
            Err(DecodingError::LimitsExceeded)
        ));
        assert_eq!(tracker.frame_count(), 0);
    }

    #[test]
    fn an_fctl_before_idat_must_cover_the_canvas() {
        let mut tracker = ApngTracker::default();
        assert!(
            tracker
                .observe_fctl(fc(0, 4, 4, 0, 0), (8, 8), false)
                .is_err()
        );
        let mut tracker = ApngTracker::default();
        tracker
            .observe_fctl(fc(0, 8, 8, 0, 0), (8, 8), false)
            .expect("full canvas");
        assert!(tracker.fctl_before_idat);
    }

    #[test]
    fn fdat_without_fctl_is_rejected() {
        let mut tracker = ApngTracker::default();
        assert!(matches!(
            tracker.observe_fdat(0).unwrap_err().format_kind(),
            Some(FormatErrorKind::MissingFctl)
        ));
    }

    #[test]
    fn first_frame_previous_disposal_becomes_background() {
        assert_eq!(
            effective_dispose_op(DisposeOp::Previous, true),
            DisposeOp::Background
        );
        assert_eq!(
            effective_dispose_op(DisposeOp::Previous, false),
            DisposeOp::Previous
        );
        assert_eq!(effective_dispose_op(DisposeOp::None, true), DisposeOp::None);
    }

    // ---- compositor math ----

    #[test]
    fn source_over_at_full_and_zero_opacity_is_a_plain_copy() {
        let src = [1000u16, 2000, 3000, 65535];
        let dst = [9000u16, 8000, 7000, 40000];
        assert_eq!(source_over(src, dst), src, "opaque source replaces");
        let transparent_src = [1000u16, 2000, 3000, 0];
        assert_eq!(
            source_over(transparent_src, dst),
            dst,
            "fully transparent source leaves the destination untouched"
        );
    }

    #[test]
    fn source_over_blends_half_alpha_over_opaque() {
        // 50% white over opaque black: every channel should land near the
        // midpoint, and the result is fully opaque (opaque dst forces
        // out_a = 1 regardless of src's alpha).
        let src = [65535u16, 65535, 65535, 32768]; // ~50% alpha white
        let dst = [0u16, 0, 0, 65535]; // opaque black
        let out = source_over(src, dst);
        assert_eq!(out[3], 65535, "opaque destination keeps the result opaque");
        for c in out.iter().take(3) {
            let mid = 32768i32;
            assert!(
                (*c as i32 - mid).abs() < 200,
                "channel {c} not near the midpoint"
            );
        }
    }

    #[test]
    fn source_over_ignores_a_transparent_source_even_over_a_transparent_destination() {
        // A fully transparent source must never disturb the destination's
        // channels, whatever they are — including when the destination is
        // *also* transparent and carries leftover, semantically
        // meaningless RGB values. A naive implementation that special-cased
        // "both alphas zero" into an all-zero result would be wrong: the
        // per-pixel `PNG` compositing rule is "the source contributes
        // nothing", not "the result is defined as zero".
        let src = [65535u16, 0, 0, 0];
        let dst = [0u16, 65535, 0, 0];
        assert_eq!(source_over(src, dst), dst);
    }

    #[test]
    fn blend_source_overwrites_including_alpha() {
        let mut canvas = vec![0u16; 4 * 4 * 4];
        // Pre-fill the canvas with a marker so overwrite is observable.
        canvas.iter_mut().for_each(|v| *v = 0xBEEF);
        let src = vec![100u16, 200, 300, 400, 500, 600, 700, 800]; // 2x1
        let control = fc(0, 2, 1, 1, 1);
        blend_into_canvas(&mut canvas, 4, control, &src, BlendOp::Source);
        // Pixel (x=1, y=1) of a 4-wide canvas: linear index 5, 4 u16
        // channels each.
        let row1_x1 = 5 * 4;
        assert_eq!(&canvas[row1_x1..row1_x1 + 4], &[100, 200, 300, 400]);
        assert_eq!(&canvas[row1_x1 + 4..row1_x1 + 8], &[500, 600, 700, 800]);
        // Untouched pixels keep the marker.
        assert_eq!(canvas[0], 0xBEEF);
    }

    #[test]
    fn clear_rect_zeroes_only_the_rectangle() {
        let mut canvas = vec![0xFFFFu16; 4 * 4 * 4];
        let control = fc(0, 2, 2, 1, 1);
        clear_rect(&mut canvas, 4, control);
        for y in 0..4usize {
            for x in 0..4usize {
                let inside = (1..3).contains(&x) && (1..3).contains(&y);
                let px = &canvas[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
                if inside {
                    assert_eq!(px, [0, 0, 0, 0], "({x},{y}) should be cleared");
                } else {
                    assert_eq!(px, [0xFFFF; 4], "({x},{y}) should be untouched");
                }
            }
        }
    }

    #[test]
    fn materialize_canvas_truncates_or_keeps_full_precision() {
        let canvas = [0x1234u16, 0x5678, 0x9ABC, 0xFFFF];
        let mut out8 = [0u8; 4];
        materialize_canvas(&canvas, false, &mut out8);
        assert_eq!(out8, [0x12, 0x56, 0x9A, 0xFF]);
        let mut out16 = [0u8; 8];
        materialize_canvas(&canvas, true, &mut out16);
        assert_eq!(out16, [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xFF, 0xFF]);
    }

    // ---- full ApngEncoder -> ApngDecoder round trips ----

    fn solid_rgba(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        (0..(w * h) as usize).flat_map(|_| rgba).collect()
    }

    #[test]
    fn dispose_none_leaves_the_frame_painted() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 4, 4, 2, 1).expect("new");
        let ctl0 = FrameControl {
            width: 4,
            height: 4,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl0, &solid_rgba(4, 4, [255, 0, 0, 255]))
            .expect("frame 0");
        let ctl1 = FrameControl {
            width: 1,
            height: 1,
            x_offset: 0,
            y_offset: 0,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl1, &solid_rgba(1, 1, [0, 255, 0, 255]))
            .expect("frame 1");
        enc.finish().expect("finish");

        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        let f0 = dec.next_composed().expect("compose 0").expect("some");
        assert_eq!(&f0.canvas[0..4], &[255, 0, 0, 255]);
        let f1 = dec.next_composed().expect("compose 1").expect("some");
        // Pixel (0,0) is now green (frame 1 painted it); the rest of the
        // canvas keeps frame 0's red, because dispose_op was None.
        assert_eq!(&f1.canvas[0..4], &[0, 255, 0, 255]);
        assert_eq!(&f1.canvas[4..8], &[255, 0, 0, 255]);
        assert!(dec.next_composed().expect("compose end").is_none());
    }

    #[test]
    fn dispose_background_clears_the_rect_after_the_frame() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 2, 2, 2, 1).expect("new");
        let ctl0 = FrameControl {
            width: 2,
            height: 2,
            dispose_op: DisposeOp::Background,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl0, &solid_rgba(2, 2, [10, 20, 30, 255]))
            .expect("frame 0");
        let ctl1 = FrameControl {
            width: 1,
            height: 1,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl1, &solid_rgba(1, 1, [0, 0, 0, 0]))
            .expect("frame 1");
        enc.finish().expect("finish");

        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        let f0 = dec.next_composed().expect("compose 0").expect("some");
        assert_eq!(&f0.canvas[4..8], &[10, 20, 30, 255]);
        let f1 = dec.next_composed().expect("compose 1").expect("some");
        // Frame 0's whole rect (the whole 2x2 canvas) was cleared to
        // transparent black after it was emitted; frame 1 then painted
        // (0,0) transparent black too (blend Source), so every pixel of
        // the second composed frame is transparent black.
        assert_eq!(f1.canvas, vec![0u8; 2 * 2 * 4]);
    }

    #[test]
    fn dispose_previous_restores_the_saved_canvas() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 2, 1, 3, 1).expect("new");
        let base = FrameControl {
            width: 2,
            height: 1,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&base, &solid_rgba(2, 1, [1, 2, 3, 255]))
            .expect("frame 0");
        let restoring = FrameControl {
            width: 1,
            height: 1,
            dispose_op: DisposeOp::Previous,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&restoring, &solid_rgba(1, 1, [255, 255, 255, 255]))
            .expect("frame 1");
        let after = FrameControl {
            width: 1,
            height: 1,
            x_offset: 1,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&after, &solid_rgba(1, 1, [9, 9, 9, 255]))
            .expect("frame 2");
        enc.finish().expect("finish");

        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        dec.next_composed().expect("compose 0").expect("some");
        let f1 = dec.next_composed().expect("compose 1").expect("some");
        assert_eq!(
            &f1.canvas[0..4],
            &[255, 255, 255, 255],
            "frame 1 painted white"
        );
        let f2 = dec.next_composed().expect("compose 2").expect("some");
        // Frame 1's Previous disposal restored pixel (0,0) back to frame
        // 0's red-ish colour; frame 2 then painted pixel (1,0).
        assert_eq!(
            &f2.canvas[0..4],
            &[1, 2, 3, 255],
            "restored to frame 0's pixel"
        );
        assert_eq!(&f2.canvas[4..8], &[9, 9, 9, 255]);
    }

    #[test]
    fn first_frame_previous_becomes_background_end_to_end() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 2, 2, 1, 1).expect("new");
        let ctl = FrameControl {
            width: 2,
            height: 2,
            dispose_op: DisposeOp::Previous, // has nothing to restore to
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl, &solid_rgba(2, 2, [7, 7, 7, 255]))
            .expect("frame 0");
        enc.finish().expect("finish");

        // The composed frame itself is unaffected by its own disposal (the
        // spec applies disposal *after* emitting), so it still shows the
        // painted colour; there being no second frame to observe the
        // disposal is exactly why the encoder-side proof needs a second
        // frame in `dispose_background_clears_the_rect_after_the_frame`
        // and `dispose_previous_restores_the_saved_canvas` above. This
        // test instead just confirms an animation whose very first frame
        // declares `Previous` decodes without error at all, which is what
        // the first-frame override exists to guarantee (naively "restoring
        // a canvas that was never saved" would otherwise be undefined).
        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        let f0 = dec.next_composed().expect("compose 0").expect("some");
        assert_eq!(&f0.canvas[0..4], &[7, 7, 7, 255]);
    }

    #[test]
    fn blend_over_matches_hand_computed_alpha_math() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 1, 1, 2, 1).expect("new");
        let opaque_black = FrameControl {
            width: 1,
            height: 1,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&opaque_black, &[0, 0, 0, 255])
            .expect("frame 0");
        let half_white = FrameControl {
            width: 1,
            height: 1,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Over,
            ..FrameControl::default()
        };
        enc.write_frame(&half_white, &[255, 255, 255, 128])
            .expect("frame 1");
        enc.finish().expect("finish");

        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        dec.next_composed().expect("compose 0");
        let f1 = dec.next_composed().expect("compose 1").expect("some");
        // 128/255 alpha white over opaque black, truncated through the
        // encoder's 8->16-bit replication and the compositor's 16->8-bit
        // truncation: within rounding of the exact half-blend.
        let px = f1.canvas[0];
        assert!(
            (120..=136).contains(&px),
            "blended channel {px} out of range"
        );
        assert_eq!(
            f1.canvas[3], 255,
            "opaque destination keeps the result opaque"
        );
    }

    #[test]
    fn separate_default_image_is_not_counted_as_an_animation_frame() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 2, 2, 1, 0).expect("new");
        enc.set_default_image_is_first_frame(false);
        let default_ctl = FrameControl {
            width: 2,
            height: 2,
            ..FrameControl::default()
        };
        enc.write_frame(&default_ctl, &solid_rgba(2, 2, [1, 1, 1, 255]))
            .expect("default image");
        let frame_ctl = FrameControl {
            width: 2,
            height: 2,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&frame_ctl, &solid_rgba(2, 2, [2, 2, 2, 255]))
            .expect("animation frame");
        enc.finish().expect("finish");

        let info = crate::peek_info(&buf).expect("peek");
        assert_eq!(info.animation_control.expect("actl").num_frames, 1);

        let mut dec = ApngDecoder::new(&buf[..]).expect("open");
        // Only the *second* write_frame call is a composed animation frame.
        let f0 = dec.next_composed().expect("compose").expect("some");
        assert_eq!(&f0.canvas[0..4], &[2, 2, 2, 255]);
        assert!(dec.next_composed().expect("compose end").is_none());
    }

    // ---- canvas allocation is limit-checked, not `IHDR`-trusted ----

    #[test]
    fn new_with_limits_accepts_a_small_canvas_under_a_tight_budget() {
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 2, 2, 1, 1).expect("new");
        let ctl = FrameControl {
            width: 2,
            height: 2,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl, &solid_rgba(2, 2, [9, 9, 9, 255]))
            .expect("frame");
        enc.finish().expect("finish");

        // 2x2 RGBA8 needs: scratch 16B, canvas (RGBA16) 32B, canvas_bytes
        // 16B -- all comfortably under a 1 KiB budget.
        let limits = DecodeLimits::default().with_max_alloc_bytes(1024);
        let dec = ApngDecoder::new_with_limits(&buf[..], limits);
        assert!(dec.is_ok(), "a tiny canvas must fit a 1 KiB budget");
    }

    #[test]
    fn new_with_limits_rejects_a_canvas_the_frame_buffer_check_alone_would_miss() {
        // `ApngEncoder` is RGBA8-only (see the deviations note in the
        // handoff), so for a 100x100 source: `scratch` (the native-depth
        // frame buffer `Reader::checked_output_buffer_size` already validates)
        // is `100 * 100 * 4` = 40000 bytes. `canvas` is always RGBA16
        // internally regardless of source depth (see the module doc), so its
        // footprint is double that: `100 * 100 * 4 channels * 2 bytes` =
        // 80000 bytes. A 60000-byte budget sits strictly between the two:
        // `scratch` fits, so a check that only ever validated the frame
        // buffer (as the code did before `Reader::check_alloc` was added)
        // would let this header through and then allocate the 80000-byte
        // canvas anyway, unbounded by the caller's configured limit.
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, 100, 100, 1, 1).expect("new");
        let ctl = FrameControl {
            width: 100,
            height: 100,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
            ..FrameControl::default()
        };
        enc.write_frame(&ctl, &solid_rgba(100, 100, [1, 2, 3, 255]))
            .expect("frame");
        enc.finish().expect("finish");

        let limits = DecodeLimits::default().with_max_alloc_bytes(60_000);
        assert!(
            reader_would_accept_the_scratch_buffer(&buf, limits),
            "scratch (40000B) must fit the 60000B budget on its own"
        );
        let err = ApngDecoder::new_with_limits(&buf[..], limits)
            .err()
            .expect("the larger canvas allocation must still be rejected");
        assert!(matches!(err, DecodingError::LimitsExceeded));
    }

    /// True when a plain (non-APNG-aware) decode of `buf`'s frame buffer
    /// would fit `limits` -- used to confirm the scratch-only check really
    /// would have let the rejection test's file through, so that test is
    /// actually exercising the canvas check and not merely a stricter
    /// scratch check.
    fn reader_would_accept_the_scratch_buffer(buf: &[u8], limits: DecodeLimits) -> bool {
        let mut decoder = crate::decoder::Decoder::new(buf);
        decoder.set_decode_limits(limits);
        let Ok(reader) = decoder.read_info() else {
            return false;
        };
        reader.checked_output_buffer_size().is_ok()
    }
}
