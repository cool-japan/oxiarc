//! Row assembly: upsample every component, convert colour, crop to the frame
//! rectangle and hand one interleaved row to the caller.
//!
//! Work is done a row at a time into a single reusable buffer, so a decode
//! allocates the component planes and nothing else per row.

use super::planes::Planes;
use crate::color::ColorSpace;
use crate::color::cmyk::{cmyk_passthrough, ycck_to_cmyk};
use crate::color::ycbcr::YcbcrTables;
use crate::error::{JpegError, Result, UnsupportedFeature};
use crate::frame::FrameHeader;
use crate::upsample::Upsampler;

/// How one output row is assembled from the component rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copy component samples through, interleaved, with no transform.
    Passthrough,
    /// YCbCr to RGB.
    YcbcrToRgb,
    /// Take the luma component and drop the chroma.
    YcbcrToLuma,
    /// YCCK to CMYK.
    YcckToCmyk,
    /// CMYK, with the Adobe inversion applied when the marker was present.
    CmykPassthrough,
}

/// Everything the row renderer needs, resolved once per decode.
pub(crate) struct OutputPlan {
    /// Output width in pixels.
    pub(crate) width: usize,
    /// Output height in rows.
    pub(crate) height: usize,
    /// Samples per output pixel.
    pub(crate) components: usize,
    mode: Mode,
    upsamplers: Vec<Upsampler>,
    tables: YcbcrTables,
    invert_cmyk: bool,
}

impl OutputPlan {
    /// Resolve the transform from `input` to `output` for `frame`.
    pub(crate) fn new(
        frame: &FrameHeader,
        planes: &Planes,
        input: ColorSpace,
        output: ColorSpace,
        raw_components: bool,
        fancy_upsampling: bool,
        has_adobe: bool,
    ) -> Result<Self> {
        let count = frame.components.len();
        let mode = if raw_components {
            Mode::Passthrough
        } else {
            match (input, output) {
                (ColorSpace::Ycbcr, ColorSpace::Rgb) => Mode::YcbcrToRgb,
                (ColorSpace::Ycbcr, ColorSpace::Luma) => Mode::YcbcrToLuma,
                (ColorSpace::Ycck, ColorSpace::Cmyk) => Mode::YcckToCmyk,
                (ColorSpace::Cmyk, ColorSpace::Cmyk) => Mode::CmykPassthrough,
                (a, b) if a == b => Mode::Passthrough,
                _ => {
                    return Err(JpegError::Unsupported(UnsupportedFeature::ColorTransform(
                        output.num_components() as u8,
                    )));
                }
            }
        };

        let components = match mode {
            Mode::Passthrough | Mode::CmykPassthrough => count,
            Mode::YcbcrToRgb => 3,
            Mode::YcbcrToLuma => 1,
            Mode::YcckToCmyk => 4,
        };

        let mut upsamplers = Vec::with_capacity(count);
        for (index, component) in frame.components.iter().enumerate() {
            upsamplers.push(Upsampler::new(
                planes.width(index),
                planes.height(index),
                planes.stride(index),
                component.h,
                component.v,
                frame.hmax,
                frame.vmax,
                fancy_upsampling,
            ));
        }

        // The chroma tables are 1 MiB at 16-bit precision, so only build
        // them for the modes that actually convert.
        let tables = match mode {
            Mode::YcbcrToRgb | Mode::YcckToCmyk => YcbcrTables::new(frame.precision),
            _ => YcbcrTables::maxval_only(frame.precision),
        };

        Ok(Self {
            width: usize::from(frame.width),
            height: usize::from(frame.height),
            components,
            mode,
            upsamplers,
            tables,
            invert_cmyk: has_adobe,
        })
    }

    /// Samples in one output row.
    pub(crate) fn row_len(&self) -> usize {
        self.width * self.components
    }

    /// Total samples in the output image.
    #[cfg(test)]
    pub(crate) fn total_samples(&self) -> u64 {
        self.width as u64 * self.height as u64 * self.components as u64
    }

    /// Render every row, handing each to `sink` as `(row index, samples)`.
    pub(crate) fn render<F>(&self, planes: &Planes, mut sink: F) -> Result<()>
    where
        F: FnMut(usize, &[u16]),
    {
        if self.width == 0 || self.height == 0 {
            return Ok(());
        }
        let used = match self.mode {
            Mode::YcbcrToLuma => 1,
            _ => self.upsamplers.len(),
        };
        let mut component_rows = vec![vec![0u16; self.width]; used.max(1)];
        let mut out_row = vec![0u16; self.row_len()];

        for y in 0..self.height {
            for (index, row) in component_rows.iter_mut().enumerate() {
                self.upsamplers[index].row(planes.plane(index), y, row);
            }
            self.assemble(&component_rows, &mut out_row);
            sink(y, &out_row);
        }
        Ok(())
    }

    /// Combine the per-component rows into one interleaved output row.
    fn assemble(&self, rows: &[Vec<u16>], out: &mut [u16]) {
        match self.mode {
            Mode::YcbcrToLuma => {
                out.copy_from_slice(&rows[0][..self.width]);
            }
            Mode::YcbcrToRgb => {
                for (x, pixel) in out.chunks_exact_mut(3).enumerate() {
                    let (r, g, b) = self.tables.to_rgb(rows[0][x], rows[1][x], rows[2][x]);
                    pixel[0] = r;
                    pixel[1] = g;
                    pixel[2] = b;
                }
            }
            Mode::YcckToCmyk => {
                for (x, pixel) in out.chunks_exact_mut(4).enumerate() {
                    let converted = ycck_to_cmyk(
                        &self.tables,
                        rows[0][x],
                        rows[1][x],
                        rows[2][x],
                        rows[3][x],
                        self.invert_cmyk,
                    );
                    pixel.copy_from_slice(&converted);
                }
            }
            Mode::CmykPassthrough => {
                let maxval = self.tables.maxval();
                for (x, pixel) in out.chunks_exact_mut(4).enumerate() {
                    let converted = cmyk_passthrough(
                        maxval,
                        [rows[0][x], rows[1][x], rows[2][x], rows[3][x]],
                        self.invert_cmyk,
                    );
                    pixel.copy_from_slice(&converted);
                }
            }
            Mode::Passthrough => {
                let n = self.components;
                for (x, pixel) in out.chunks_exact_mut(n).enumerate() {
                    for (c, slot) in pixel.iter_mut().enumerate() {
                        *slot = rows[c][x];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::parse_sof;
    use crate::limits::DecodeLimits;

    fn frame(y: u16, x: u16, comps: &[(u8, u8, u8, u8)]) -> FrameHeader {
        let mut payload = vec![8u8];
        payload.extend_from_slice(&y.to_be_bytes());
        payload.extend_from_slice(&x.to_be_bytes());
        payload.push(comps.len() as u8);
        for &(id, h, v, tq) in comps {
            payload.push(id);
            payload.push((h << 4) | v);
            payload.push(tq);
        }
        parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("SOF")
    }

    fn planes_for(frame: &FrameHeader) -> Planes {
        Planes::allocate(frame, &DecodeLimits::default()).expect("planes")
    }

    #[test]
    fn grayscale_passthrough_crops_mcu_padding() {
        let frame = frame(3, 5, &[(1, 1, 1, 0)]);
        let mut planes = planes_for(&frame);
        let stride = planes.stride(0);
        let base = planes.offset(0);
        for y in 0..8 {
            for x in 0..8 {
                planes.data_mut()[base + y * stride + x] = (y * 8 + x) as u16;
            }
        }
        let plan = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Luma,
            ColorSpace::Luma,
            false,
            true,
            false,
        )
        .expect("plan");
        assert_eq!(plan.components, 1);
        assert_eq!(plan.row_len(), 5);
        assert_eq!(plan.total_samples(), 15);

        let mut rows = Vec::new();
        plan.render(&planes, |y, row| rows.push((y, row.to_vec())))
            .expect("render");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, vec![0, 1, 2, 3, 4]);
        assert_eq!(rows[2].1, vec![16, 17, 18, 19, 20]);
    }

    #[test]
    fn ycbcr_to_rgb_interleaves_three_channels() {
        let frame = frame(1, 2, &[(1, 1, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let mut planes = planes_for(&frame);
        for index in 0..3 {
            let base = planes.offset(index);
            let value = if index == 0 { 200 } else { 128 };
            planes.data_mut()[base] = value;
            planes.data_mut()[base + 1] = value;
        }
        let plan = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Ycbcr,
            ColorSpace::Rgb,
            false,
            true,
            false,
        )
        .expect("plan");
        assert_eq!(plan.components, 3);
        let mut rows = Vec::new();
        plan.render(&planes, |_, row| rows.push(row.to_vec()))
            .expect("render");
        assert_eq!(rows[0], vec![200, 200, 200, 200, 200, 200]);
    }

    #[test]
    fn ycbcr_to_luma_drops_the_chroma_components() {
        let frame = frame(1, 2, &[(1, 1, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let mut planes = planes_for(&frame);
        let base = planes.offset(0);
        planes.data_mut()[base] = 42;
        planes.data_mut()[base + 1] = 43;
        let plan = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Ycbcr,
            ColorSpace::Luma,
            false,
            true,
            false,
        )
        .expect("plan");
        assert_eq!(plan.components, 1);
        let mut rows = Vec::new();
        plan.render(&planes, |_, row| rows.push(row.to_vec()))
            .expect("render");
        assert_eq!(rows[0], vec![42, 43]);
    }

    #[test]
    fn raw_components_skips_the_colour_transform() {
        let frame = frame(1, 2, &[(1, 1, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let mut planes = planes_for(&frame);
        for index in 0..3 {
            let base = planes.offset(index);
            planes.data_mut()[base] = (index * 10) as u16;
            planes.data_mut()[base + 1] = (index * 10 + 1) as u16;
        }
        let plan = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Ycbcr,
            ColorSpace::Rgb,
            true,
            true,
            false,
        )
        .expect("plan");
        assert_eq!(plan.components, 3);
        let mut rows = Vec::new();
        plan.render(&planes, |_, row| rows.push(row.to_vec()))
            .expect("render");
        assert_eq!(rows[0], vec![0, 10, 20, 1, 11, 21]);
    }

    #[test]
    fn cmyk_inversion_follows_the_adobe_marker() {
        let frame = frame(
            1,
            1,
            &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0), (4, 1, 1, 0)],
        );
        let mut planes = planes_for(&frame);
        for index in 0..4 {
            let base = planes.offset(index);
            planes.data_mut()[base] = (index * 20) as u16;
        }
        let plain = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Cmyk,
            ColorSpace::Cmyk,
            false,
            true,
            false,
        )
        .expect("plan");
        let mut rows = Vec::new();
        plain
            .render(&planes, |_, row| rows.push(row.to_vec()))
            .expect("render");
        assert_eq!(rows[0], vec![0, 20, 40, 60]);

        let adobe = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Cmyk,
            ColorSpace::Cmyk,
            false,
            true,
            true,
        )
        .expect("plan");
        let mut rows = Vec::new();
        adobe
            .render(&planes, |_, row| rows.push(row.to_vec()))
            .expect("render");
        assert_eq!(rows[0], vec![255, 235, 215, 195]);
    }

    #[test]
    fn an_unsupported_transform_is_a_named_error() {
        let frame = frame(1, 1, &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0)]);
        let planes = planes_for(&frame);
        assert!(matches!(
            OutputPlan::new(
                &frame,
                &planes,
                ColorSpace::Rgb,
                ColorSpace::Cmyk,
                false,
                true,
                false,
            ),
            Err(JpegError::Unsupported(UnsupportedFeature::ColorTransform(
                _
            )))
        ));
    }

    #[test]
    fn a_zero_height_frame_renders_no_rows() {
        let frame = frame(0, 8, &[(1, 1, 1, 0)]);
        let planes = planes_for(&frame);
        let plan = OutputPlan::new(
            &frame,
            &planes,
            ColorSpace::Luma,
            ColorSpace::Luma,
            false,
            true,
            false,
        )
        .expect("plan");
        let mut count = 0;
        plan.render(&planes, |_, _| count += 1).expect("render");
        assert_eq!(count, 0);
    }
}
