//! Turning [`EncodeOptions`] into a validated frame layout.
//!
//! Everything libjpeg's `jpeg_set_colorspace` and `initial_setup` decide is
//! decided here, once: which components exist, their identifiers, sampling
//! factors and table slots, the MCU grid, and which `SOF` marker the frame
//! gets. Nothing downstream re-derives any of it.

use super::options::{
    ComponentIds, Density, EncodeOptions, EncodeProcess, InputColor, MarkerPolicy,
    QuantTableSource, RestartInterval, Subsampling,
};
use crate::color::ColorSpace;
use crate::error::{JpegError, Result};
use crate::frame::{ArithmeticConditioning, EntropyCoding};
use crate::quant::QuantTable;

/// The largest sampling factor T.81 allows.
const MAX_SAMPLING: u8 = 4;

/// One component of the frame being written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComponentPlan {
    /// `Ci`, the identifier written into the `SOF`.
    pub id: u8,
    /// `Hi`.
    pub h: u8,
    /// `Vi`.
    pub v: u8,
    /// `Tqi`.
    pub quant_slot: u8,
    /// `Tdj` for every scan this component appears in.
    pub dc_slot: u8,
    /// `Taj` for every scan this component appears in.
    pub ac_slot: u8,
    /// Real samples across, `ceil(width * h / hmax)`.
    pub width_samples: usize,
    /// Real samples down, `ceil(height * v / vmax)`.
    pub height_samples: usize,
    /// Real blocks across, `ceil(width_samples / 8)`.
    pub blocks_wide: usize,
    /// Real blocks down.
    pub blocks_high: usize,
    /// Blocks across including the MCU-alignment dummies.
    pub blocks_wide_padded: usize,
    /// Blocks down including the MCU-alignment dummies.
    pub blocks_high_padded: usize,
}

impl ComponentPlan {
    /// Width in samples of the padded plane the coefficient stage reads.
    pub(crate) fn padded_width(&self) -> usize {
        self.blocks_wide_padded * 8
    }

    /// Height in samples of the padded plane the coefficient stage reads.
    pub(crate) fn padded_height(&self) -> usize {
        self.blocks_high_padded * 8
    }
}

/// A validated, fully derived description of the frame to be written.
#[derive(Debug, Clone)]
pub(crate) struct EncodePlan {
    pub width: u16,
    pub height: u16,
    pub precision: u8,
    /// The colour space of the *stored* components.
    pub color: ColorSpace,
    /// How the caller's buffer is laid out.
    pub input: InputColor,
    pub components: Vec<ComponentPlan>,
    pub hmax: u8,
    pub vmax: u8,
    pub mcus_per_line: usize,
    pub mcus_per_column: usize,
    pub quant: [Option<QuantTable>; 4],
    pub process: EncodeProcess,
    pub optimize_huffman: bool,
    pub restart_interval: RestartInterval,
    pub downsampling: crate::downsample::Downsampling,
    pub write_jfif: bool,
    /// `Some(transform)` when an Adobe `APP14` segment is to be written.
    pub write_adobe: Option<u8>,
    pub density: Density,
    pub progressive_script: Option<Vec<super::options::ScanSpec>>,
    /// Huffman or arithmetic entropy coding.
    pub entropy: EntropyCoding,
    /// `DAC` conditioning bounds, used only when `entropy` is arithmetic.
    pub arithmetic: ArithmeticConditioning,
}

impl EncodePlan {
    /// The `SOF` marker code this frame needs.
    ///
    /// libjpeg's `is_baseline` predicate (`jcmarker.c`'s `write_frame_header`)
    /// is **not** "precision == 8": a frame is baseline only when it is
    /// sequential, Huffman-coded, eight-bit, uses table slots `0` or `1`
    /// only, and needs no 16-bit `DQT` entry. `cjpeg -quality 1` on an
    /// eight-bit image emits `SOF1` for exactly that last reason.
    pub(crate) fn sof_marker(&self) -> u8 {
        // T.81 Table B.1: the arithmetic codes are the Huffman ones plus 9.
        // There is no arithmetic baseline process, so `SOF9` covers both
        // eight- and twelve-bit sequential frames.
        if self.entropy == EntropyCoding::Arithmetic {
            return match self.process {
                EncodeProcess::Progressive => 0xCA,
                EncodeProcess::Lossless { .. } => 0xCB,
                EncodeProcess::Sequential => 0xC9,
            };
        }
        match self.process {
            EncodeProcess::Progressive => 0xC2,
            EncodeProcess::Lossless { .. } => 0xC3,
            EncodeProcess::Sequential => {
                if self.is_baseline() {
                    0xC0
                } else {
                    0xC1
                }
            }
        }
    }

    /// `true` when this frame carries arithmetic-coded scans.
    pub(crate) fn is_arithmetic(&self) -> bool {
        self.entropy == EntropyCoding::Arithmetic
    }

    /// Whether the frame satisfies T.81's baseline restrictions.
    pub(crate) fn is_baseline(&self) -> bool {
        if self.precision != 8 || self.entropy != EntropyCoding::Huffman {
            return false;
        }
        if self
            .components
            .iter()
            .any(|c| c.dc_slot > 1 || c.ac_slot > 1)
        {
            return false;
        }
        // The same predicate `emit_dqt` uses, so a `SOF0` can never be paired
        // with a 16-bit `DQT`: a caller-supplied table parsed from a `Pq = 1`
        // segment keeps that flag even when every value fits in a byte, and
        // T.81 B.2.4.1 forbids `Pq = 1` at eight-bit precision.
        !self
            .quant
            .iter()
            .flatten()
            .any(|table| table.precision_flag().max(table.required_precision()) == 1)
    }

    /// `1 << (P - 1)`, the level-shift centre.
    pub(crate) fn center(&self) -> i64 {
        1i64 << (self.precision - 1)
    }

    /// `(1 << P) - 1`.
    pub(crate) fn maxval(&self) -> u16 {
        ((1u32 << self.precision) - 1) as u16
    }

    /// `true` when the frame is a lossless predictive one.
    pub(crate) fn is_lossless(&self) -> bool {
        matches!(self.process, EncodeProcess::Lossless { .. })
    }

    /// Number of MCUs in one row of an interleaved scan over `components`.
    ///
    /// A single-component scan is not interleaved: its "MCU" is one block, so
    /// the row length is that component's padded block count. This is why a
    /// restart interval expressed in MCU rows has to be resolved per scan.
    pub(crate) fn mcus_per_row_for(&self, components: &[usize]) -> usize {
        if components.len() == 1 {
            let component = &self.components[components[0]];
            if self.is_lossless() {
                component.width_samples
            } else {
                component.blocks_wide
            }
        } else {
            self.mcus_per_line
        }
    }

    /// Number of MCU rows in a scan over `components`.
    pub(crate) fn mcu_rows_for(&self, components: &[usize]) -> usize {
        if components.len() == 1 {
            let component = &self.components[components[0]];
            if self.is_lossless() {
                component.height_samples
            } else {
                component.blocks_high
            }
        } else {
            self.mcus_per_column
        }
    }

    /// The `DRI` value for a scan, resolving [`RestartInterval::McuRows`]
    /// against that scan's own MCU row length.
    pub(crate) fn restart_for(&self, components: &[usize]) -> u16 {
        match self.restart_interval {
            RestartInterval::None => 0,
            RestartInterval::Mcus(n) => n,
            RestartInterval::McuRows(n) => {
                let per_row = self.mcus_per_row_for(components);
                let total = usize::from(n).saturating_mul(per_row);
                u16::try_from(total).unwrap_or(u16::MAX)
            }
        }
    }
}

/// One row of the per-colour-space component table:
/// `(id, quant_slot, dc_slot, ac_slot, takes_luma_sampling)`.
type TemplateRow = (u8, u8, u8, u8, bool);

/// Component identifiers, sampling factors and table slots per colour space.
///
/// Transcribed from libjpeg's `jpeg_set_colorspace` `SET_COMP` calls. The
/// `Unknown(2)` row is this crate's own addition — real libjpeg's own
/// `JCS_UNKNOWN` branch is a loop (`for (ci = 0; ci < num_components; ci++)
/// SET_COMP(ci, ci, 1,1, 0,0,0);`), so it exists for any component count;
/// this crate accepts it only at exactly two, because that is the one count
/// [`conversion_is_supported`] and [`super::prepare::conversion_for`] wire an
/// [`InputColor`] to ([`InputColor::LumaAlpha`], preserving the alpha channel
/// a decode would otherwise drop). A row for one, three or four components
/// would sit unreachable behind those two gates — this crate's other colour
/// spaces already cover those counts — so it is deliberately not added.
fn component_template(color: ColorSpace) -> Result<Vec<TemplateRow>> {
    Ok(match color {
        ColorSpace::Luma => vec![(1, 0, 0, 0, true)],
        ColorSpace::Rgb => vec![
            (b'R', 0, 0, 0, false),
            (b'G', 0, 0, 0, false),
            (b'B', 0, 0, 0, false),
        ],
        ColorSpace::Ycbcr => vec![(1, 0, 0, 0, true), (2, 1, 1, 1, false), (3, 1, 1, 1, false)],
        ColorSpace::Cmyk => vec![
            (b'C', 0, 0, 0, false),
            (b'M', 0, 0, 0, false),
            (b'Y', 0, 0, 0, false),
            (b'K', 0, 0, 0, false),
        ],
        ColorSpace::Ycck => vec![
            (1, 0, 0, 0, true),
            (2, 1, 1, 1, false),
            (3, 1, 1, 1, false),
            (4, 0, 0, 0, true),
        ],
        // libjpeg's `JCS_UNKNOWN`, restricted to the two-component case: ids
        // `1`, `2`, both on quantisation and Huffman slot 0, neither ever
        // subsampled (no chroma to decimate, and `luma_sampled = false`
        // exempts both from `Subsampling`'s named ratios — only
        // `Subsampling::Custom` can move them off `1x1`). No JFIF or Adobe
        // marker is written for it either: `color` is neither `Luma` nor
        // `Ycbcr` (the `write_jfif` `Auto` rule) nor `Rgb`/`Cmyk`/`Ycck` (the
        // `write_adobe` one), so both fall through to their `_` arm in
        // `build_plan` with no code change needed there.
        ColorSpace::Unknown(2) => vec![(1, 0, 0, 0, false), (2, 0, 0, 0, false)],
        ColorSpace::Unknown(_) => {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "jpeg_color_space",
                reason: "only a two-component Unknown colour space can be encoded; JPEG's \
                         named colour spaces already cover one, three and four components",
            });
        }
    })
}

/// Whether the conversion from `input` to `color` is one this crate performs.
fn conversion_is_supported(input: InputColor, color: ColorSpace) -> bool {
    use ColorSpace::{Cmyk, Luma, Rgb, Ycbcr, Ycck};
    match input {
        InputColor::Luma => color == Luma,
        // Dropping the alpha channel into a one-component `Luma` frame is
        // still the default (`InputColor::default_jpeg_color_space`); asking
        // for `Unknown(2)` instead keeps it, as a second, untransformed
        // component (see `conversion_for` in `super::prepare`).
        InputColor::LumaAlpha => matches!(color, Luma | ColorSpace::Unknown(2)),
        InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra => {
            matches!(color, Luma | Ycbcr | Rgb)
        }
        InputColor::Ycbcr => matches!(color, Ycbcr | Luma),
        InputColor::Cmyk => matches!(color, Cmyk | Ycck),
        InputColor::Ycck => color == Ycck,
    }
}

/// Build and validate the frame layout.
pub(crate) fn build_plan(
    options: &EncodeOptions,
    width: u16,
    height: u16,
    input: InputColor,
) -> Result<EncodePlan> {
    if width == 0 || height == 0 {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "dimensions",
            reason: "width and height must both be at least 1",
        });
    }

    let lossless = matches!(options.process, EncodeProcess::Lossless { .. });
    let precision = options.precision;
    if lossless {
        if !(2..=16).contains(&precision) {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "precision",
                reason: "lossless frames need a precision of 2..=16",
            });
        }
    } else if precision != 8 && precision != 12 {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "precision",
            reason: "DCT frames need a precision of 8 or 12",
        });
    }

    if let EncodeProcess::Lossless {
        predictor,
        point_transform,
    } = options.process
    {
        if !(1..=7).contains(&predictor) {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "predictor",
                reason: "the lossless predictor Psv must be 1..=7",
            });
        }
        if point_transform >= precision {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "point_transform",
                reason: "the point transform Pt must be less than the precision",
            });
        }
    }

    // A lossless frame that decimated its chroma would not be lossless, so the
    // sampling factors are forced to 1x1 exactly as `cjpeg -lossless` does.
    // Its natural colour space is RGB for the same reason: the YCbCr matrix
    // is not reversible in integers.
    let color = match options.jpeg_color_space {
        Some(color) => color,
        None if lossless => match input {
            InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra => {
                ColorSpace::Rgb
            }
            other => other.default_jpeg_color_space(),
        },
        None => input.default_jpeg_color_space(),
    };

    if !conversion_is_supported(input, color) {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "jpeg_color_space",
            reason: "this crate does not convert between those two colour spaces",
        });
    }

    let template = component_template(color)?;
    let count = template.len();

    let (luma_h, luma_v) = if lossless {
        (1, 1)
    } else {
        options.subsampling.luma_factors()
    };

    let mut components = Vec::with_capacity(count);
    for (index, &(default_id, quant_slot, dc_slot, ac_slot, luma_sampled)) in
        template.iter().enumerate()
    {
        let (h, v) = match options.subsampling {
            Subsampling::Custom(factors) if !lossless => factors[index],
            _ => {
                if luma_sampled && matches!(color, ColorSpace::Ycbcr | ColorSpace::Ycck) {
                    (luma_h, luma_v)
                } else {
                    (1, 1)
                }
            }
        };
        if !(1..=MAX_SAMPLING).contains(&h) || !(1..=MAX_SAMPLING).contains(&v) {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "subsampling",
                reason: "sampling factors must be 1..=4",
            });
        }
        let id = match options.component_ids {
            ComponentIds::Auto => default_id,
            ComponentIds::Sequential => (index + 1) as u8,
            ComponentIds::Rgb => *b"RGB\0".get(index).unwrap_or(&0),
            ComponentIds::Cmyk => *b"CMYK".get(index).unwrap_or(&0),
            ComponentIds::Custom(ids) => ids[index],
        };
        components.push(ComponentPlan {
            id,
            h,
            v,
            quant_slot,
            dc_slot,
            ac_slot,
            width_samples: 0,
            height_samples: 0,
            blocks_wide: 0,
            blocks_high: 0,
            blocks_wide_padded: 0,
            blocks_high_padded: 0,
        });
    }

    if (matches!(options.component_ids, ComponentIds::Rgb) && count != 3)
        || (matches!(options.component_ids, ComponentIds::Cmyk) && count != 4)
    {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "component_ids",
            reason: "the identifier set does not match the component count",
        });
    }

    let hmax = components.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = components.iter().map(|c| c.v).max().unwrap_or(1);
    for component in &components {
        if hmax % component.h != 0 || vmax % component.v != 0 {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "subsampling",
                reason: "every sampling factor must divide the frame maximum exactly",
            });
        }
    }

    let width_usize = usize::from(width);
    let height_usize = usize::from(height);
    let mcu_width = if lossless { 1 } else { 8 } * usize::from(hmax);
    let mcu_height = if lossless { 1 } else { 8 } * usize::from(vmax);
    let mcus_per_line = width_usize.div_ceil(mcu_width);
    let mcus_per_column = height_usize.div_ceil(mcu_height);

    for component in &mut components {
        let h = usize::from(component.h);
        let v = usize::from(component.v);
        component.width_samples = (width_usize * h).div_ceil(usize::from(hmax));
        component.height_samples = (height_usize * v).div_ceil(usize::from(vmax));
        component.blocks_wide = component.width_samples.div_ceil(8);
        component.blocks_high = component.height_samples.div_ceil(8);
        if lossless {
            component.blocks_wide_padded = component.blocks_wide;
            component.blocks_high_padded = component.blocks_high;
        } else {
            component.blocks_wide_padded = mcus_per_line * h;
            component.blocks_high_padded = mcus_per_column * v;
        }
    }

    let quant = build_quant_tables(options, &components, precision)?;

    // Only an arithmetic frame writes `DAC`; a Huffman plan ignores the field
    // entirely, so an out-of-range value there is not an error.
    if options.entropy == EntropyCoding::Arithmetic {
        options.arithmetic.validate()?;
    }

    // Progressive frames and wide precisions both need generated tables:
    // libjpeg forces `optimize_coding` for progressive (`jcmaster.c`'s
    // "TEMPORARY HACK"), and Annex K.3's tables stop at DC category 11 and AC
    // category 10, while twelve-bit samples reach 15 and 14.
    // Lossless frames force it too: table H.2's categories reach 16 while
    // Annex K.3 stops at 11, and `cjpeg -lossless` generates a table even
    // without `-optimize`.
    // An arithmetic frame has no Huffman tables to optimise; libjpeg ignores
    // `-optimize` there for the same reason.
    let optimize_huffman = options.entropy == EntropyCoding::Huffman
        && (options.optimize_huffman
            || matches!(options.process, EncodeProcess::Progressive)
            || lossless
            || precision > 8);

    let write_jfif = match options.write_jfif {
        MarkerPolicy::Always => true,
        MarkerPolicy::Never => false,
        MarkerPolicy::Auto => matches!(color, ColorSpace::Luma | ColorSpace::Ycbcr),
    };
    let adobe_transform = match color {
        ColorSpace::Ycbcr => 1,
        ColorSpace::Ycck => 2,
        _ => 0,
    };
    let write_adobe = match options.write_adobe {
        MarkerPolicy::Always => Some(adobe_transform),
        MarkerPolicy::Never => None,
        MarkerPolicy::Auto => match color {
            ColorSpace::Rgb | ColorSpace::Cmyk | ColorSpace::Ycck => Some(adobe_transform),
            _ => None,
        },
    };

    Ok(EncodePlan {
        width,
        height,
        precision,
        color,
        input,
        components,
        hmax,
        vmax,
        mcus_per_line,
        mcus_per_column,
        quant,
        process: options.process,
        optimize_huffman,
        restart_interval: options.restart_interval,
        downsampling: options.downsampling,
        write_jfif,
        write_adobe,
        density: options.density,
        progressive_script: options.progressive_script.clone(),
        entropy: options.entropy,
        arithmetic: options.arithmetic,
    })
}

/// Fill the quantisation table slots the components actually reference.
///
/// A lossless frame has no quantisation at all: T.81 H.1 says `Tq` shall be
/// zero and no `DQT` is written, which is what `cjpeg -lossless` does.
fn build_quant_tables(
    options: &EncodeOptions,
    components: &[ComponentPlan],
    precision: u8,
) -> Result<[Option<QuantTable>; 4]> {
    let mut quant: [Option<QuantTable>; 4] = [None; 4];
    if matches!(options.process, EncodeProcess::Lossless { .. }) {
        return Ok(quant);
    }
    let baseline = options.force_baseline;
    for component in components {
        let slot = usize::from(component.quant_slot);
        if quant[slot].is_some() {
            continue;
        }
        quant[slot] = Some(match &options.quant_tables {
            QuantTableSource::AnnexK => {
                let base = if slot == 0 {
                    QuantTable::annex_k_luma()
                } else {
                    QuantTable::annex_k_chroma()
                };
                base.scaled_for_quality(options.quality, baseline)
            }
            QuantTableSource::Flat(value) => {
                let clamped = (*value).clamp(1, if baseline { 255 } else { 32_767 });
                QuantTable::from_natural([clamped; 64])
            }
            QuantTableSource::Custom(tables) => {
                let table = tables[slot].ok_or(JpegError::InvalidEncodeParameter {
                    parameter: "quant_tables",
                    reason: "a component references a table slot that was left empty",
                })?;
                // T.81 B.2.4.1 gives `Qk` the range 1..=255 (`Pq = 0`) or
                // 1..=65535 (`Pq = 1`); zero is not a quantiser. Without this
                // the forward quantiser divides by it, and a caller-supplied
                // table would panic the encoder rather than be rejected.
                if table.natural().contains(&0) {
                    return Err(JpegError::InvalidEncodeParameter {
                        parameter: "quant_tables",
                        reason: "a quantiser value is zero; T.81 requires 1..=65535",
                    });
                }
                table
            }
        });
    }
    if precision == 8 {
        for table in quant.iter().flatten() {
            if table.precision_flag().max(table.required_precision()) == 1 && options.force_baseline
            {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "quant_tables",
                    reason: "force_baseline is set but a quantiser exceeds 255",
                });
            }
        }
    }
    Ok(quant)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(options: &EncodeOptions, w: u16, h: u16, input: InputColor) -> EncodePlan {
        build_plan(options, w, h, input).expect("valid plan")
    }

    #[test]
    fn ycbcr_defaults_match_jpeg_set_colorspace() {
        let plan = make(&EncodeOptions::default(), 17, 19, InputColor::Rgb);
        assert_eq!(plan.color, ColorSpace::Ycbcr);
        let ids: Vec<u8> = plan.components.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        let factors: Vec<(u8, u8)> = plan.components.iter().map(|c| (c.h, c.v)).collect();
        assert_eq!(factors, vec![(2, 2), (1, 1), (1, 1)]);
        let quant: Vec<u8> = plan.components.iter().map(|c| c.quant_slot).collect();
        assert_eq!(quant, vec![0, 1, 1]);
        assert!(plan.write_jfif);
        assert_eq!(plan.write_adobe, None);
    }

    #[test]
    fn rgb_uses_letter_ids_one_table_and_an_adobe_marker() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Rgb),
            ..Default::default()
        };
        let plan = make(&options, 17, 19, InputColor::Rgb);
        let ids: Vec<u8> = plan.components.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![b'R', b'G', b'B']);
        assert!(plan.components.iter().all(|c| (c.h, c.v) == (1, 1)));
        assert!(plan.components.iter().all(|c| c.quant_slot == 0));
        assert!(!plan.write_jfif);
        assert_eq!(plan.write_adobe, Some(0));
        assert!(plan.quant[1].is_none(), "only one DQT is referenced");
    }

    /// libjpeg's `JCS_UNKNOWN` shape, restricted to two components: `1`, `2`,
    /// no subsampling, one shared table slot, no metadata markers.
    #[test]
    fn unknown_two_component_layout_is_sequential_unsubsampled_and_unmarked() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            ..Default::default()
        };
        let plan = make(&options, 17, 19, InputColor::LumaAlpha);
        let ids: Vec<u8> = plan.components.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(
            plan.components.iter().all(|c| (c.h, c.v) == (1, 1)),
            "neither component is subsampled by default"
        );
        assert!(plan.components.iter().all(|c| c.quant_slot == 0));
        assert!(plan.components.iter().all(|c| c.dc_slot == 0));
        assert!(plan.components.iter().all(|c| c.ac_slot == 0));
        assert!(plan.quant[1].is_none(), "only one DQT is referenced");
        assert!(
            !plan.write_jfif,
            "libjpeg writes no JFIF marker for JCS_UNKNOWN"
        );
        assert_eq!(
            plan.write_adobe, None,
            "libjpeg writes no Adobe marker for JCS_UNKNOWN"
        );
    }

    /// `Subsampling::Custom` can still move a two-component frame off `1x1`,
    /// even though nothing chooses that automatically.
    #[test]
    fn unknown_two_component_layout_honours_an_explicit_custom_subsampling() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            subsampling: Subsampling::Custom([(2, 1), (1, 1), (1, 1), (1, 1)]),
            ..Default::default()
        };
        let plan = make(&options, 17, 19, InputColor::LumaAlpha);
        let factors: Vec<(u8, u8)> = plan.components.iter().map(|c| (c.h, c.v)).collect();
        assert_eq!(factors, vec![(2, 1), (1, 1)]);
    }

    /// Only the exact count `2` is accepted; every other `Unknown` count is a
    /// named error, not a panic or a guess.
    #[test]
    fn component_template_accepts_only_unknown_two() {
        assert!(component_template(ColorSpace::Unknown(2)).is_ok());
        for other in [0u8, 1, 3, 4, 5, 6, 255] {
            assert!(
                component_template(ColorSpace::Unknown(other)).is_err(),
                "Unknown({other}) should be refused"
            );
        }
    }

    /// [`InputColor::Luma`] alone has only one channel, so it cannot fill a
    /// two-component frame; only [`InputColor::LumaAlpha`] can.
    #[test]
    fn only_luma_alpha_reaches_the_two_component_colour_space() {
        assert!(conversion_is_supported(
            InputColor::LumaAlpha,
            ColorSpace::Unknown(2)
        ));
        assert!(!conversion_is_supported(
            InputColor::Luma,
            ColorSpace::Unknown(2)
        ));
        for other in [
            InputColor::Rgb,
            InputColor::Rgba,
            InputColor::Bgr,
            InputColor::Bgra,
            InputColor::Ycbcr,
            InputColor::Cmyk,
            InputColor::Ycck,
        ] {
            assert!(!conversion_is_supported(other, ColorSpace::Unknown(2)));
        }
        // `LumaAlpha` still defaults to dropping the alpha channel: asking
        // for the two-component space is opt-in, not automatic.
        assert!(!conversion_is_supported(
            InputColor::LumaAlpha,
            ColorSpace::Unknown(3)
        ));
        assert_eq!(
            InputColor::LumaAlpha.default_jpeg_color_space(),
            ColorSpace::Luma
        );
    }

    /// A one-channel buffer cannot feed a two-component frame, at the
    /// `build_plan` level rather than the private helpers directly.
    #[test]
    fn luma_alone_cannot_fill_an_unknown_two_frame() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            ..Default::default()
        };
        assert!(build_plan(&options, 8, 8, InputColor::Luma).is_err());
    }

    #[test]
    fn cmyk_and_ycck_layouts_follow_libjpeg() {
        let plan = make(&EncodeOptions::default(), 8, 8, InputColor::Cmyk);
        assert_eq!(plan.color, ColorSpace::Cmyk);
        let ids: Vec<u8> = plan.components.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![b'C', b'M', b'Y', b'K']);
        assert_eq!(plan.write_adobe, Some(0));

        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Ycck),
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Cmyk);
        let factors: Vec<(u8, u8)> = plan.components.iter().map(|c| (c.h, c.v)).collect();
        assert_eq!(factors, vec![(2, 2), (1, 1), (1, 1), (2, 2)], "K follows Y");
        assert_eq!(plan.write_adobe, Some(2));
    }

    /// The geometry a 17x19 4:2:0 frame gets: three block columns of real Y
    /// data but four encoded, because the MCU grid is 16 wide.
    #[test]
    fn dummy_blocks_appear_where_the_mcu_grid_overshoots() {
        let plan = make(&EncodeOptions::default(), 17, 19, InputColor::Rgb);
        assert_eq!((plan.mcus_per_line, plan.mcus_per_column), (2, 2));
        let luma = &plan.components[0];
        assert_eq!((luma.width_samples, luma.height_samples), (17, 19));
        assert_eq!((luma.blocks_wide, luma.blocks_high), (3, 3));
        assert_eq!((luma.blocks_wide_padded, luma.blocks_high_padded), (4, 4));
        let chroma = &plan.components[1];
        assert_eq!((chroma.width_samples, chroma.height_samples), (9, 10));
        assert_eq!((chroma.blocks_wide, chroma.blocks_high), (2, 2));
        assert_eq!(
            (chroma.blocks_wide_padded, chroma.blocks_high_padded),
            (2, 2)
        );
    }

    #[test]
    fn quality_one_is_not_baseline_but_forced_baseline_is() {
        let options = EncodeOptions {
            quality: 1,
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Rgb);
        assert!(!plan.is_baseline(), "16-bit quantisers are not baseline");
        assert_eq!(plan.sof_marker(), 0xC1);

        let options = EncodeOptions {
            quality: 1,
            force_baseline: true,
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Rgb);
        assert!(plan.is_baseline());
        assert_eq!(plan.sof_marker(), 0xC0);
    }

    #[test]
    fn twelve_bit_is_sof1_and_forces_generated_tables() {
        let options = EncodeOptions {
            precision: 12,
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Luma);
        assert_eq!(plan.sof_marker(), 0xC1);
        assert!(plan.optimize_huffman);
    }

    #[test]
    fn progressive_forces_generated_tables() {
        let options = EncodeOptions {
            process: EncodeProcess::Progressive,
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Rgb);
        assert_eq!(plan.sof_marker(), 0xC2);
        assert!(plan.optimize_huffman);
    }

    #[test]
    fn lossless_is_rgb_unsubsampled_and_has_no_quant_tables() {
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor: 1,
                point_transform: 0,
            },
            subsampling: Subsampling::S420,
            ..Default::default()
        };
        let plan = make(&options, 17, 19, InputColor::Rgb);
        assert_eq!(plan.color, ColorSpace::Rgb);
        assert!(plan.components.iter().all(|c| (c.h, c.v) == (1, 1)));
        assert!(plan.quant.iter().all(Option::is_none));
        assert_eq!(plan.sof_marker(), 0xC3);
        assert_eq!((plan.mcus_per_line, plan.mcus_per_column), (17, 19));
    }

    #[test]
    fn restart_in_rows_resolves_per_scan() {
        let options = EncodeOptions {
            restart_interval: RestartInterval::McuRows(1),
            ..Default::default()
        };
        let plan = make(&options, 17, 19, InputColor::Rgb);
        assert_eq!(plan.restart_for(&[0, 1, 2]), 2, "frame MCU row is 2 wide");
        assert_eq!(plan.restart_for(&[0]), 3, "luma has 3 real block columns");
        assert_eq!(plan.restart_for(&[1]), 2);
    }

    #[test]
    fn bad_settings_are_rejected() {
        let bad = [
            EncodeOptions {
                precision: 10,
                ..Default::default()
            },
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 8,
                    point_transform: 0,
                },
                ..Default::default()
            },
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 1,
                    point_transform: 8,
                },
                precision: 8,
                ..Default::default()
            },
            EncodeOptions {
                subsampling: Subsampling::Custom([(3, 1), (2, 1), (1, 1), (1, 1)]),
                ..Default::default()
            },
            EncodeOptions {
                component_ids: ComponentIds::Cmyk,
                ..Default::default()
            },
            EncodeOptions {
                force_baseline: true,
                quant_tables: QuantTableSource::Custom(Box::new([
                    Some(QuantTable::from_natural([400; 64])),
                    Some(QuantTable::annex_k_chroma()),
                    None,
                    None,
                ])),
                ..Default::default()
            },
            EncodeOptions {
                quant_tables: QuantTableSource::Custom(Box::new([None; 4])),
                ..Default::default()
            },
        ];
        for (index, options) in bad.iter().enumerate() {
            assert!(
                build_plan(options, 8, 8, InputColor::Rgb).is_err(),
                "case {index} should be rejected"
            );
        }
        assert!(build_plan(&EncodeOptions::default(), 0, 8, InputColor::Rgb).is_err());
        let unsupported = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Cmyk),
            ..Default::default()
        };
        assert!(build_plan(&unsupported, 8, 8, InputColor::Rgb).is_err());
    }

    #[test]
    fn flat_tables_are_clamped_and_shared() {
        let options = EncodeOptions {
            quant_tables: QuantTableSource::Flat(0),
            ..Default::default()
        };
        let plan = make(&options, 8, 8, InputColor::Rgb);
        assert_eq!(plan.quant[0].expect("luma").value(0), 1);
    }
}
