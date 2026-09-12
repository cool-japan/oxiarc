//! [`Writer::write_image_data`]'s implementation, and the frame-planning
//! logic [`stream::StreamWriter`] shares with it.
//!
//! [`Writer::write_image_data`]: super::Writer::write_image_data
//! [`stream::StreamWriter`]: super::stream::StreamWriter

use crate::common::FrameControl;
use crate::error::{EncodingError, EncodingFormatErrorKind};
use crate::filter::{AdaptiveScratch, select_filter};
use crate::interlace::{Adam7Info, extract_pass_row, pass_dimensions};

use super::zlib::{ChunkKind, FrameEncoder, use_optimal_parsing};
use super::{Writer, buffer_size_error, check_frame_rect};

/// Where one frame's compressed payload goes, decided once per
/// [`super::Writer::write_image_data`]/[`super::stream::StreamWriter`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameDestination {
    Idat,
    Fdat,
}

/// The decision `write_image_data` and `StreamWriter` both need before the
/// first byte of a frame is written: whether to emit an `fcTL`, which chunk
/// type the pixel data becomes, and whether this frame counts toward the
/// animation total. Mirrors `png` 0.18's
/// `Writer::should_skip_frame_control_on_default_image` +
/// `write_image_data`'s `match self.info.frame_control` exactly, including
/// the "all animation frames written, later calls fall back to plain IDAT"
/// tail behaviour.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FramePlan {
    pub(crate) emit_fctl: bool,
    pub(crate) destination: FrameDestination,
    pub(crate) counts_as_animation_frame: bool,
}

pub(crate) fn plan_next_frame<W: std::io::Write>(w: &Writer<W>) -> FramePlan {
    match w.frame_control {
        None => FramePlan {
            emit_fctl: false,
            destination: FrameDestination::Idat,
            counts_as_animation_frame: false,
        },
        Some(_) if w.options.sep_def_img && w.images_written == 0 => FramePlan {
            emit_fctl: false,
            destination: FrameDestination::Idat,
            counts_as_animation_frame: false,
        },
        Some(_) => FramePlan {
            emit_fctl: true,
            destination: if w.images_written == 0 {
                FrameDestination::Idat
            } else {
                FrameDestination::Fdat
            },
            counts_as_animation_frame: true,
        },
    }
}

/// `(width, height)` of the frame this call writes: the current `fcTL`
/// rectangle when animated, else the whole canvas.
pub(crate) fn current_frame_size<W: std::io::Write>(w: &Writer<W>) -> (u32, u32) {
    match w.frame_control {
        Some(fctl) => (fctl.width, fctl.height),
        None => (w.geometry.width, w.geometry.height),
    }
}

/// Re-check the frame rectangle at the point of use, before the row stride
/// it determines is handed to `chunks_exact` (which panics on a zero chunk
/// size), and enforce the extra rule that only applies once the frame's
/// *destination* is known.
///
/// That extra rule: **whatever an animated encoder writes as `IDAT` is the
/// default image, and the default image is the `IHDR` image** — the whole
/// canvas, at the origin. Only `fdAT` sub-frames may be smaller
/// (`png-design` §8.14; the decoder side is
/// `apng::ApngTracker::observe_fctl`'s `have_idat == false` branch).
///
/// Both `IDAT` destinations need it, and they fail differently:
///
/// * **with** an `fcTL` (the ordinary first frame): the encoder wrote
///   `fcTL(4x4 at 2,2)` + `IDAT`, which `crate::decode` rejects with
///   `BadSubFrameBounds`;
/// * **without** one ([`super::Encoder::set_sep_def_img`]): no `fcTL` is
///   emitted, so nothing tells the decoder the rectangle shrank — the
///   `IDAT` stream simply carries 68 raw bytes where `IHDR` demands 264,
///   and the decoder walks off the end of the data into
///   `InvalidRowFilter`. Gating this check on "an `fcTL` is being emitted"
///   would leave exactly that second case open, which is why the condition
///   is the destination alone.
///
/// This is a deliberate **divergence from `png` 0.18**, which has the same
/// hole and even documents it as unfinished (`set_frame_position`'s
/// `// ??? TODO ??? - The next frame is the default image`). Producing an
/// unreadable file is not a compatibility feature worth preserving; a caller
/// that wants a smaller first frame should call
/// [`super::Encoder::set_sep_def_img`], which makes the default image a
/// separate still with no `fcTL` and starts the animation at the next call.
pub(crate) fn check_current_frame_rect<W: std::io::Write>(
    w: &Writer<W>,
    plan: FramePlan,
) -> Result<(), EncodingError> {
    let Some(fctl) = w.frame_control else {
        return check_frame_rect(
            0,
            0,
            w.geometry.width,
            w.geometry.height,
            w.geometry.width,
            w.geometry.height,
            true,
        );
    };
    check_frame_rect(
        fctl.x_offset,
        fctl.y_offset,
        fctl.width,
        fctl.height,
        w.geometry.width,
        w.geometry.height,
        true,
    )?;
    // Deliberately the destination alone, not `plan.emit_fctl && ...`: see
    // the second bullet in this function's doc.
    let is_default_image = plan.destination == FrameDestination::Idat;
    if is_default_image
        && (fctl.width != w.geometry.width
            || fctl.height != w.geometry.height
            || fctl.x_offset != 0
            || fctl.y_offset != 0)
    {
        return Err(EncodingFormatErrorKind::OutOfBounds.into());
    }
    Ok(())
}

/// Validate that another image may be written under `validate_sequence`.
pub(crate) fn validate_new_image<W: std::io::Write>(w: &Writer<W>) -> Result<(), EncodingError> {
    if !w.options.validate_sequence {
        return Ok(());
    }
    let ok = match w.animation_control {
        None => w.images_written == 0,
        Some(_) => w.frame_control.is_some(),
    };
    if ok {
        Ok(())
    } else {
        Err(EncodingFormatErrorKind::EndReached.into())
    }
}

/// After a frame has been fully written: advance the bookkeeping the way
/// `png` 0.18's `increment_images_written` does, including clearing
/// `frame_control` once every declared animation frame has been written.
pub(crate) fn advance_after_frame<W: std::io::Write>(w: &mut Writer<W>, plan: FramePlan) {
    w.images_written = w.images_written.saturating_add(1);
    if plan.counts_as_animation_frame {
        w.animation_written = w.animation_written.saturating_add(1);
    }
    if let Some(actl) = w.animation_control {
        if actl.num_frames <= w.animation_written {
            w.frame_control = None;
        }
    }
}

/// Write the `fcTL` chunk for the frame about to be written, and advance its
/// sequence number by one.
pub(crate) fn write_fctl<W: std::io::Write>(w: &mut Writer<W>) -> Result<(), EncodingError> {
    let Some(fctl) = w.frame_control.as_mut() else {
        return Ok(());
    };
    let payload = fctl_bytes(fctl);
    fctl.sequence_number = fctl.sequence_number.wrapping_add(1);
    w.write_chunk(crate::chunk::fcTL, &payload)
}

/// One Adam7 pass's row stride in bytes: `bits_per_pixel * pass_width`,
/// rounded up to a byte. The same formula [`crate::header::Ihdr::row_stride_for_width`]
/// uses, reproduced here (in `u64` for the same overflow-safety reason that
/// method uses it, then saturated back into a `usize`) because
/// [`write_rows_serial`] and [`write_rows_parallel`] take the
/// already-resolved `bits_per_pixel` rather than a full `FrameGeometry`,
/// which neither needs otherwise.
fn pass_row_stride(bits_per_pixel: u8, pass_width: u32) -> usize {
    let bits = u64::from(bits_per_pixel) * u64::from(pass_width);
    usize::try_from(bits.div_ceil(8)).unwrap_or(usize::MAX)
}

pub(crate) fn write_image_data<W: std::io::Write>(
    w: &mut Writer<W>,
    data: &[u8],
) -> Result<(), EncodingError> {
    if w.geometry.color_type == crate::ColorType::Indexed && !w.geometry.has_palette {
        return Err(EncodingFormatErrorKind::NoPalette.into());
    }
    validate_new_image(w)?;
    let plan = plan_next_frame(w);
    check_current_frame_rect(w, plan)?;

    let (width, height) = current_frame_size(w);
    let row_stride = w.geometry.row_stride_for_width(width);
    let expected = row_stride.saturating_mul(height as usize);
    if data.len() != expected {
        return Err(buffer_size_error(expected, data.len()));
    }

    if plan.emit_fctl {
        write_fctl(w)?;
    }

    let level = w.options.compression.level();
    let optimal = use_optimal_parsing(level);
    let mut encoder = FrameEncoder::new(level, optimal, w.options.idat_chunk_size);
    let bpp = w.geometry.bpp();
    let bits_per_pixel = w.geometry.bits_per_pixel();
    let mut scratch = AdaptiveScratch::new();
    // Always initialised (even when unused, i.e. `Idat`) so the borrow
    // `ChunkKind::Fdat` takes below is never conditionally-uninitialised.
    let mut seq_holder: u32 = w.frame_control.map_or(0, |f| f.sequence_number);
    let mut kind = match plan.destination {
        FrameDestination::Idat => ChunkKind::Idat,
        FrameDestination::Fdat => ChunkKind::Fdat(&mut seq_holder),
    };

    // `should_parallelize` counts transmission-order rows -- for an
    // interlaced image that's the sum over the seven Adam7 passes' row
    // counts, not the image height -- so it is computed unconditionally
    // (cheap: seven `pass_dimensions` calls) but only *read* when the
    // `parallel` feature is compiled in.
    #[cfg_attr(not(feature = "parallel"), allow(unused_variables))]
    let total_transmission_rows = if w.geometry.interlaced {
        (0..7u8)
            .map(|pass| pass_dimensions(width, height, usize::from(pass)).1 as usize)
            .sum()
    } else {
        height as usize
    };

    #[cfg(feature = "parallel")]
    {
        if super::parallel::should_parallelize(total_transmission_rows) {
            write_rows_parallel(
                w.options.filter,
                data,
                row_stride,
                width,
                height,
                w.geometry.interlaced,
                bits_per_pixel,
                bpp,
                level,
                &mut encoder,
                &mut w.w,
                &mut kind,
            )?;
        } else {
            write_rows_serial(
                w.options.filter,
                data,
                row_stride,
                width,
                height,
                w.geometry.interlaced,
                bits_per_pixel,
                bpp,
                level,
                &mut scratch,
                &mut encoder,
                &mut w.w,
                &mut kind,
            )?;
        }
    }
    #[cfg(not(feature = "parallel"))]
    {
        write_rows_serial(
            w.options.filter,
            data,
            row_stride,
            width,
            height,
            w.geometry.interlaced,
            bits_per_pixel,
            bpp,
            level,
            &mut scratch,
            &mut encoder,
            &mut w.w,
            &mut kind,
        )?;
    }
    encoder.finish(level, &mut w.w, kind)?;
    if plan.destination == FrameDestination::Fdat {
        if let Some(fctl) = w.frame_control.as_mut() {
            fctl.sequence_number = seq_holder;
        }
    }

    advance_after_frame(w, plan);
    Ok(())
}

/// Filter and push every row of one frame, one row at a time, on the
/// current thread — the only row-processing path when the `parallel`
/// feature is off, and the small-frame fallback when it is on (see
/// `super::parallel`'s module doc for why `rayon` dispatch overhead can
/// exceed its benefit below some row count).
#[allow(clippy::too_many_arguments)]
fn write_rows_serial<W: std::io::Write>(
    filter_choice: crate::Filter,
    data: &[u8],
    row_stride: usize,
    width: u32,
    height: u32,
    interlaced: bool,
    bits_per_pixel: u8,
    bpp: crate::header::BytesPerPixel,
    level: u8,
    scratch: &mut AdaptiveScratch,
    encoder: &mut FrameEncoder,
    out: &mut W,
    kind: &mut ChunkKind<'_>,
) -> Result<(), EncodingError> {
    let mut push_row = |previous: &[u8],
                        raw: &[u8],
                        scratch: &mut AdaptiveScratch,
                        out: &mut W,
                        kind: &mut ChunkKind<'_>|
     -> Result<(), EncodingError> {
        let filter = select_filter(filter_choice, bpp, previous, raw, scratch);
        encoder.push_row(level, filter.into_u8(), scratch.filtered(), out, kind)
    };

    if interlaced {
        for pass in 0..7u8 {
            let (pw, ph) = pass_dimensions(width, height, usize::from(pass));
            if pw == 0 || ph == 0 {
                continue;
            }
            let pass_stride = pass_row_stride(bits_per_pixel, pw);
            // The filter's "previous row" resets to none at the start of
            // every pass; the zlib stream itself does not reset.
            let mut prev: Vec<u8> = Vec::new();
            let mut row = vec![0u8; pass_stride];
            for line in 0..ph {
                row.fill(0);
                // `Adam7Info::new` only returns `None` for `pass == 0` or
                // `width == 0`, neither reachable here (`pass + 1 <= 7`,
                // and `write_header` already rejected a zero-width image);
                // the fallback recomputes exactly the same `samples` field
                // `Adam7Info::new` would (`pass_width(width, pass) == pw`
                // by construction), so it is not a dummy value.
                let info = Adam7Info::new(pass + 1, line, width).unwrap_or(Adam7Info {
                    pass: pass + 1,
                    line,
                    width,
                    samples: pw,
                });
                extract_pass_row(data, row_stride, &info, bits_per_pixel, &mut row);
                push_row(&prev, &row, scratch, out, kind)?;
                prev.clear();
                prev.extend_from_slice(&row);
            }
        }
    } else {
        let mut prev: Vec<u8> = Vec::new();
        for row in data.chunks_exact(row_stride) {
            push_row(&prev, row, scratch, out, kind)?;
            prev.clear();
            prev.extend_from_slice(row);
        }
    }
    Ok(())
}

/// As [`write_rows_serial`], but filters rows in parallel (see
/// `super::parallel`'s module doc), then feeds the results to `Deflater`
/// one row at a time, in transmission order, exactly as the serial path
/// does — only *filtering* is parallel; compression stays a single
/// sequential stream, so the emitted bytes are identical either way
/// (`parallel_and_serial_row_paths_emit_identical_bytes` proves it).
///
/// Rows are gathered, filtered and drained in **windows** of
/// [`super::parallel::PARALLEL_ROW_WINDOW`] rather than all at once. This
/// is load-bearing, not tidiness: gathering the whole frame first would
/// hold each raw row *twice* (once as the row, once as the next row's
/// `previous`) plus every filtered row, i.e. roughly three times the
/// image in memory before a single byte is compressed — a real regression
/// against the serial path, which holds two rows. Windowing bounds the
/// extra memory at `PARALLEL_ROW_WINDOW` rows regardless of image size,
/// and also removes a `Vec::with_capacity(height)` whose size came
/// straight from a caller-supplied dimension.
#[cfg(feature = "parallel")]
#[allow(clippy::too_many_arguments)]
fn write_rows_parallel<W: std::io::Write>(
    filter_choice: crate::Filter,
    data: &[u8],
    row_stride: usize,
    width: u32,
    height: u32,
    interlaced: bool,
    bits_per_pixel: u8,
    bpp: crate::header::BytesPerPixel,
    level: u8,
    encoder: &mut FrameEncoder,
    out: &mut W,
    kind: &mut ChunkKind<'_>,
) -> Result<(), EncodingError> {
    let window = super::parallel::PARALLEL_ROW_WINDOW;
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(window);

    // Filter everything gathered so far in parallel, then push it to the
    // (serial) `Deflater` in order, and drop it.
    fn drain<W: std::io::Write>(
        rows: &mut Vec<(Vec<u8>, Vec<u8>)>,
        filter_choice: crate::Filter,
        bpp: crate::header::BytesPerPixel,
        level: u8,
        encoder: &mut FrameEncoder,
        out: &mut W,
        kind: &mut ChunkKind<'_>,
    ) -> Result<(), EncodingError> {
        if rows.is_empty() {
            return Ok(());
        }
        let filtered = super::parallel::filter_rows_parallel(filter_choice, bpp, rows);
        rows.clear();
        for with_type in &filtered {
            // `filter_rows_parallel` already prepends the filter byte, so
            // split it back off rather than have `push_row` re-copy the row.
            let (filter_byte, samples) = with_type.split_first().unwrap_or((&0, &[]));
            encoder.push_row(level, *filter_byte, samples, out, kind)?;
        }
        Ok(())
    }

    if interlaced {
        for pass in 0..7u8 {
            let (pw, ph) = pass_dimensions(width, height, usize::from(pass));
            if pw == 0 || ph == 0 {
                continue;
            }
            let pass_stride = pass_row_stride(bits_per_pixel, pw);
            let mut prev: Vec<u8> = Vec::new();
            for line in 0..ph {
                let info = Adam7Info::new(pass + 1, line, width).unwrap_or(Adam7Info {
                    pass: pass + 1,
                    line,
                    width,
                    samples: pw,
                });
                let mut row = vec![0u8; pass_stride];
                extract_pass_row(data, row_stride, &info, bits_per_pixel, &mut row);
                rows.push((row.clone(), prev));
                prev = row;
                if rows.len() >= window {
                    drain(&mut rows, filter_choice, bpp, level, encoder, out, kind)?;
                }
            }
            // Draining at every pass boundary is not required for
            // correctness — each gathered entry carries its own `previous`
            // row, so a window straddling two passes still filters each row
            // against the right neighbour — but it keeps every batch's rows
            // the same stride, which is the shape the serial path has too.
            drain(&mut rows, filter_choice, bpp, level, encoder, out, kind)?;
        }
    } else {
        let mut prev: Vec<u8> = Vec::new();
        for row in data.chunks_exact(row_stride) {
            rows.push((row.to_vec(), prev));
            prev = row.to_vec();
            if rows.len() >= window {
                drain(&mut rows, filter_choice, bpp, level, encoder, out, kind)?;
            }
        }
    }
    drain(&mut rows, filter_choice, bpp, level, encoder, out, kind)
}

/// Serialise an `fcTL` payload.
#[must_use]
pub(crate) fn fctl_bytes(f: &FrameControl) -> [u8; 26] {
    let mut d = [0u8; 26];
    d[0..4].copy_from_slice(&f.sequence_number.to_be_bytes());
    d[4..8].copy_from_slice(&f.width.to_be_bytes());
    d[8..12].copy_from_slice(&f.height.to_be_bytes());
    d[12..16].copy_from_slice(&f.x_offset.to_be_bytes());
    d[16..20].copy_from_slice(&f.y_offset.to_be_bytes());
    d[20..22].copy_from_slice(&f.delay_num.to_be_bytes());
    d[22..24].copy_from_slice(&f.delay_den.to_be_bytes());
    d[24] = f.dispose_op as u8;
    d[25] = f.blend_op as u8;
    d
}

// Everything in this module compares the two row paths, so it exists only
// when the parallel one does.
#[cfg(all(test, feature = "parallel"))]
mod tests {
    use super::*;
    use crate::header::BytesPerPixel;

    /// A deterministic byte pattern, so the two row paths get identical
    /// input without pulling in a random-number dependency.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64).wrapping_mul(2_654_435_761) >> 17) as u8)
            .collect()
    }

    /// The `parallel` feature must be a pure speed knob: the two row paths
    /// have to emit **byte-identical** `IDAT` payloads, not merely
    /// equivalent filter choices.
    ///
    /// `encoder::parallel`'s own
    /// `parallel_filtering_matches_serial_filtering_row_by_row` compares
    /// filter *output*; this compares the compressed chunk bytes the writer
    /// actually emits, which is the guarantee callers depend on and the one
    /// the row-windowing change could have broken.
    #[test]
    fn parallel_and_serial_row_paths_emit_identical_bytes() {
        // Deliberately more rows than `PARALLEL_ROW_WINDOW` (256), so the
        // windowed drain fires several times mid-frame rather than once at
        // the end -- the case a single-window frame would never exercise.
        for (width, height, interlaced) in [
            (61u32, 700u32, false),
            (61, 700, true),
            (5, 300, false),
            (128, 96, true),
        ] {
            let bpp = BytesPerPixel::Four;
            let bits_per_pixel = 32u8;
            let row_stride = width as usize * 4;
            let data = pattern(row_stride * height as usize);

            let mut serial_out = Vec::new();
            let mut scratch = AdaptiveScratch::new();
            let mut enc = FrameEncoder::new(6, false, 8192);
            write_rows_serial(
                crate::Filter::Adaptive,
                &data,
                row_stride,
                width,
                height,
                interlaced,
                bits_per_pixel,
                bpp,
                6,
                &mut scratch,
                &mut enc,
                &mut serial_out,
                &mut ChunkKind::Idat,
            )
            .expect("serial rows");
            enc.finish(6, &mut serial_out, ChunkKind::Idat)
                .expect("serial finish");

            let mut parallel_out = Vec::new();
            let mut enc = FrameEncoder::new(6, false, 8192);
            write_rows_parallel(
                crate::Filter::Adaptive,
                &data,
                row_stride,
                width,
                height,
                interlaced,
                bits_per_pixel,
                bpp,
                6,
                &mut enc,
                &mut parallel_out,
                &mut ChunkKind::Idat,
            )
            .expect("parallel rows");
            enc.finish(6, &mut parallel_out, ChunkKind::Idat)
                .expect("parallel finish");

            assert_eq!(
                serial_out, parallel_out,
                "{width}x{height} interlaced={interlaced}: the parallel row path \
                 must emit the same bytes as the serial one"
            );
        }
    }
}
