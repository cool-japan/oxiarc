//! Lossless predictive encoding (`SOF3`), ITU-T T.81 Annex H.
//!
//! There is no DCT, no quantisation and no AC coefficient: each sample is
//! coded as the Huffman-coded difference between it and a prediction formed
//! from already-coded neighbours
//!
//! ```text
//!   Rc Rb
//!   Ra Rx
//! ```
//!
//! The prediction rules mirror [`crate::decoder`]'s, which is verified
//! against `cjpeg -lossless`:
//!
//! * the first sample of the scan, and the first sample of every restart
//!   interval, predicts `2^(P - Pt - 1)`;
//! * the rest of that row predicts `Ra` — the row a restart interval starts
//!   in behaves as the first row of the scan;
//! * the first sample of every other row predicts `Rb`;
//! * everything else uses the selected predictor `Psv`.
//!
//! Differences are taken modulo `2^16`, and `SSSS = 16` codes a difference of
//! exactly 32 768 with no additional bits (table H.2). Annex K.3's tables
//! stop at category 11, so a lossless frame always generates its own — which
//! is what `cjpeg -lossless` does even without `-optimize`.

use super::bitwriter::BitWriter;
use super::enctable::{DerivedTable, Histogram};
use super::frame::TableBank;
use super::markers;
use super::options::EncodeProcess;
use super::plan::EncodePlan;
use super::prepare::{Samples, build_lossless_planes};
use crate::downsample::Plane;
use crate::error::{JpegError, Result};

/// Prediction `Px` for predictor selection value `psv` (T.81 table H.1).
#[inline]
fn predict(psv: u8, ra: i32, rb: i32, rc: i32) -> i32 {
    match psv {
        1 => ra,
        2 => rb,
        3 => rc,
        4 => ra + rb - rc,
        5 => ra + ((rb - rc) >> 1),
        6 => rb + ((ra - rc) >> 1),
        // `psv` is validated to be 1..=7 when the plan is built.
        _ => (ra + rb) >> 1,
    }
}

/// Split a difference into its magnitude category and the bits that follow.
///
/// The difference is first reduced modulo `2^16` into `-32768..=32767`, which
/// is the range table H.2 codes. `-32768` is the special case: category 16,
/// no additional bits.
#[inline]
fn difference_symbol(difference: i32) -> (u8, u32, u32) {
    let reduced = ((difference + 32_768) & 0xFFFF) - 32_768;
    if reduced == -32_768 {
        return (16, 0, 0);
    }
    let mut magnitude = reduced;
    let mut sent = reduced;
    if magnitude < 0 {
        magnitude = -magnitude;
        sent -= 1;
    }
    let nbits = 32 - (magnitude as u32).leading_zeros();
    (nbits as u8, sent as u32, nbits)
}

/// Where a lossless scan's differences go.
///
/// The walk hands over the raw difference rather than a Huffman symbol, so
/// that the arithmetic coder — which conditions on the differences already
/// coded to the left and above, and needs the sample's position to find them
/// — can share the prediction logic instead of duplicating it.
trait LosslessSink {
    /// Select the table slot (and scan component) of the samples that follow.
    fn select(&mut self, scan_index: usize, slot: usize);
    /// Code one difference, at `(x, y)` of the component, `row_in_band` rows
    /// into the current MCU band.
    fn sample(&mut self, difference: i32, x: usize, y: usize, row_in_band: usize) -> Result<()>;
    /// Start a new restart interval.
    fn restart_marker(&mut self, index: u8);
}

struct EmitLossless<'a> {
    writer: &'a mut BitWriter,
    tables: &'a [Option<DerivedTable>; 4],
    slot: usize,
}

impl LosslessSink for EmitLossless<'_> {
    fn select(&mut self, _scan_index: usize, slot: usize) {
        self.slot = slot;
    }

    fn sample(&mut self, difference: i32, _x: usize, _y: usize, _row: usize) -> Result<()> {
        let (symbol, value, size) = difference_symbol(difference);
        let table = self.tables[self.slot]
            .as_ref()
            .ok_or(JpegError::InvalidEncodeParameter {
                parameter: "huffman_tables",
                reason: "a lossless scan references an undefined Huffman table",
            })?;
        let (code, length) = table.lookup(symbol)?;
        self.writer.emit_code(code, length, value, size);
        Ok(())
    }

    fn restart_marker(&mut self, index: u8) {
        self.writer.emit_restart(index);
    }
}

struct GatherLossless<'a> {
    histograms: &'a mut [Histogram; 4],
    slot: usize,
}

impl LosslessSink for GatherLossless<'_> {
    fn select(&mut self, _scan_index: usize, slot: usize) {
        self.slot = slot;
    }
    fn sample(&mut self, difference: i32, _x: usize, _y: usize, _row: usize) -> Result<()> {
        let (symbol, _, _) = difference_symbol(difference);
        self.histograms[self.slot].count(symbol);
        Ok(())
    }
    fn restart_marker(&mut self, _index: u8) {}
}

/// The point-transformed sample planes a lossless scan predicts from.
struct Transformed {
    planes: Vec<Vec<i32>>,
    widths: Vec<usize>,
    heights: Vec<usize>,
}

impl Transformed {
    fn new(plan: &EncodePlan, planes: &[Plane], point_transform: u8) -> Self {
        let mut data = Vec::with_capacity(planes.len());
        let mut widths = Vec::with_capacity(planes.len());
        let mut heights = Vec::with_capacity(planes.len());
        for (component, plane) in plan.components.iter().zip(planes.iter()) {
            let width = component.width_samples.min(plane.width());
            let height = component.height_samples.min(plane.height());
            let mut samples = vec![0i32; width * height];
            for y in 0..height {
                let row = plane.row(y);
                for x in 0..width {
                    samples[y * width + x] = i32::from(row[x] >> point_transform);
                }
            }
            data.push(samples);
            widths.push(width);
            heights.push(height);
        }
        Self {
            planes: data,
            widths,
            heights,
        }
    }

    #[inline]
    fn at(&self, component: usize, x: usize, y: usize) -> i32 {
        self.planes[component][y * self.widths[component] + x]
    }
}

/// Walk one lossless scan, coding every sample.
#[allow(clippy::too_many_arguments)]
fn run_scan<S: LosslessSink>(
    sink: &mut S,
    plan: &EncodePlan,
    samples: &Transformed,
    components: &[usize],
    psv: u8,
    point_transform: u8,
    restart_interval: usize,
) -> Result<()> {
    let default_prediction = 1i32 << (plan.precision - point_transform - 1);
    let interleaved = components.len() > 1;
    let units_per_row = plan.mcus_per_row_for(components);
    let unit_rows = plan.mcu_rows_for(components);

    let mut interval_first_row = [0usize; 4];
    let mut fresh_interval = true;
    let mut restart_index = 0u8;

    for unit in 0..units_per_row * unit_rows {
        if restart_interval > 0 && unit > 0 && unit % restart_interval == 0 {
            sink.restart_marker(restart_index);
            restart_index = (restart_index + 1) & 7;
            fresh_interval = true;
            let row = unit / units_per_row;
            for (k, slot) in interval_first_row
                .iter_mut()
                .enumerate()
                .take(components.len())
            {
                let component = &plan.components[components[k]];
                *slot = if interleaved {
                    row * usize::from(component.v)
                } else {
                    row
                };
            }
        }
        let unit_x = unit % units_per_row;
        let unit_y = unit / units_per_row;

        for (scan_index, &component_index) in components.iter().enumerate() {
            let component = &plan.components[component_index];
            sink.select(scan_index, usize::from(component.dc_slot));
            let (h, v) = if interleaved {
                (usize::from(component.h), usize::from(component.v))
            } else {
                (1, 1)
            };
            let width = samples.widths[component_index];
            let height = samples.heights[component_index];
            for dy in 0..v {
                for dx in 0..h {
                    let x = if interleaved { unit_x * h + dx } else { unit_x };
                    let y = if interleaved { unit_y * v + dy } else { unit_y };
                    if x >= width || y >= height {
                        // MCU padding: the decoder codes and discards these,
                        // so a zero difference is the cheapest legal filler.
                        // Unreachable from this crate's own plans, which force
                        // 1x1 sampling for lossless frames.
                        sink.sample(0, x, y, dy)?;
                        continue;
                    }
                    let value = samples.at(component_index, x, y);
                    let ra = if x > 0 {
                        samples.at(component_index, x - 1, y)
                    } else {
                        0
                    };
                    let rb = if y > 0 {
                        samples.at(component_index, x, y - 1)
                    } else {
                        0
                    };
                    let rc = if x > 0 && y > 0 {
                        samples.at(component_index, x - 1, y - 1)
                    } else {
                        0
                    };
                    let prediction = if fresh_interval {
                        default_prediction
                    } else if y == interval_first_row[scan_index] {
                        if x == 0 { default_prediction } else { ra }
                    } else if x == 0 {
                        rb
                    } else {
                        predict(psv, ra, rb, rc)
                    };
                    fresh_interval = false;
                    sink.sample(value - prediction, x, y, dy)?;
                }
            }
        }
    }
    Ok(())
}

/// The lossless scan sink for arithmetic coding.
#[cfg(feature = "arithmetic")]
struct ArithLossless<'a> {
    coder: &'a mut super::arith::LosslessCoder,
    scan_index: usize,
}

#[cfg(feature = "arithmetic")]
impl LosslessSink for ArithLossless<'_> {
    fn select(&mut self, scan_index: usize, _slot: usize) {
        self.scan_index = scan_index;
    }

    fn sample(&mut self, difference: i32, x: usize, _y: usize, row: usize) -> Result<()> {
        self.coder.sample(self.scan_index, difference, x, row);
        Ok(())
    }

    fn restart_marker(&mut self, index: u8) {
        self.coder.restart_marker(index);
    }
}

/// Write a lossless arithmetic frame (`SOF11`, T.81 Annex H with the
/// two-dimensional statistical model of H.1.2.3).
///
/// A `DAC` segment is written for the `Td` tables the scan uses. libjpeg's
/// `emit_dac` would write nothing here — its rule keys on `Ss == 0`, which no
/// lossless scan satisfies — but libjpeg cannot code this process at all, and
/// writing the conditioning explicitly is both legal and what lets a custom
/// `L`/`U` survive a round trip.
#[cfg(feature = "arithmetic")]
#[allow(clippy::too_many_arguments)]
fn encode_lossless_arith(
    plan: &EncodePlan,
    samples: &Transformed,
    components: &[usize],
    predictor: u8,
    point_transform: u8,
    restart: usize,
    extra_markers: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let interleaved = components.len() > 1;
    let widths: Vec<usize> = components
        .iter()
        .map(|&index| {
            if interleaved {
                plan.mcus_per_line
                    .saturating_mul(usize::from(plan.components[index].h))
            } else {
                samples.widths[index]
            }
            .max(1)
        })
        .collect();

    markers::soi(out);
    if plan.write_jfif {
        markers::jfif(out, plan.density);
    }
    if let Some(transform) = plan.write_adobe {
        markers::adobe(out, transform);
    }
    out.extend_from_slice(extra_markers);
    markers::sof(out, plan)?;
    markers::dac(out, plan, components, true, false);
    if restart > 0 {
        markers::dri(out, restart as u16);
    }
    markers::sos(out, plan, components, predictor, 0, 0, point_transform)?;

    let mut coder = super::arith::LosslessCoder::new(plan, components, &widths, interleaved);
    {
        let mut sink = ArithLossless {
            coder: &mut coder,
            scan_index: 0,
        };
        run_scan(
            &mut sink,
            plan,
            samples,
            components,
            predictor,
            point_transform,
            restart,
        )?;
    }
    let mut bytes = coder.encoder.finish();
    out.append(&mut bytes);
    markers::eoi(out);
    Ok(())
}

/// Encode a complete lossless frame into `out`.
pub(crate) fn encode_lossless_frame(
    plan: &EncodePlan,
    pixels: &Samples<'_>,
    extra_markers: &[u8],
    abbreviated: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let EncodeProcess::Lossless {
        predictor,
        point_transform,
    } = plan.process
    else {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "process",
            reason: "the lossless encoder was called for a DCT frame",
        });
    };

    let planes = build_lossless_planes(plan, pixels)?;
    let samples = Transformed::new(plan, &planes, point_transform);
    let components: Vec<usize> = (0..plan.components.len()).collect();
    let restart = usize::from(plan.restart_for(&components));

    #[cfg(feature = "arithmetic")]
    if plan.is_arithmetic() {
        return encode_lossless_arith(
            plan,
            &samples,
            &components,
            predictor,
            point_transform,
            restart,
            extra_markers,
            out,
        );
    }

    let mut histograms: [Histogram; 4] = std::array::from_fn(|_| Histogram::default());
    {
        let mut sink = GatherLossless {
            histograms: &mut histograms,
            slot: 0,
        };
        run_scan(
            &mut sink,
            plan,
            &samples,
            &components,
            predictor,
            point_transform,
            restart,
        )?;
    }

    let mut bank = TableBank::new();
    for (slot, histogram) in histograms.iter().enumerate() {
        if !histogram.is_empty() {
            bank.set_dc(slot, histogram.optimal_table()?);
        }
    }

    markers::soi(out);
    if plan.write_jfif {
        markers::jfif(out, plan.density);
    }
    if let Some(transform) = plan.write_adobe {
        markers::adobe(out, transform);
    }
    out.extend_from_slice(extra_markers);
    markers::sof(out, plan)?;
    if abbreviated {
        bank.dc_sent = [true; 4];
    }
    for &index in &components {
        bank.emit_dc(usize::from(plan.components[index].dc_slot), out)?;
    }
    if restart > 0 {
        markers::dri(out, restart as u16);
    }
    markers::sos(out, plan, &components, predictor, 0, 0, point_transform)?;

    let mut writer = BitWriter::with_capacity(1 << 16);
    {
        let mut sink = EmitLossless {
            writer: &mut writer,
            tables: &bank.dc_derived,
            slot: 0,
        };
        run_scan(
            &mut sink,
            plan,
            &samples,
            &components,
            predictor,
            point_transform,
            restart,
        )?;
    }
    out.extend_from_slice(&writer.finish());
    markers::eoi(out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictors_match_table_h1() {
        assert_eq!(predict(1, 10, 20, 30), 10);
        assert_eq!(predict(2, 10, 20, 30), 20);
        assert_eq!(predict(3, 10, 20, 30), 30);
        assert_eq!(predict(4, 10, 20, 30), 0);
        assert_eq!(predict(5, 10, 20, 30), 10 + ((20 - 30) >> 1));
        assert_eq!(predict(6, 10, 20, 30), 20 + ((10 - 30) >> 1));
        assert_eq!(predict(7, 10, 20, 30), 15);
    }

    /// The shifts are arithmetic, so a negative intermediate floors rather
    /// than truncating. `(20 - 30) >> 1` is -5 either way, but `-11 >> 1` is
    /// -6 and `-11 / 2` is -5.
    #[test]
    fn predictor_shifts_are_arithmetic() {
        assert_eq!(predict(5, 0, 0, 11), (0 - 11) >> 1);
        assert_eq!(predict(5, 0, 0, 11), -6);
    }

    #[test]
    fn difference_categories_follow_table_h2() {
        assert_eq!(difference_symbol(0), (0, 0, 0));
        assert_eq!(difference_symbol(1).0, 1);
        assert_eq!(difference_symbol(-1).0, 1);
        assert_eq!(difference_symbol(255).0, 8);
        assert_eq!(difference_symbol(-32_768), (16, 0, 0));
        assert_eq!(difference_symbol(32_768), (16, 0, 0), "wraps to the same");
        assert_eq!(difference_symbol(32_767).0, 15);
    }

    #[test]
    fn negative_differences_send_the_ones_complement() {
        let (bits, value, size) = difference_symbol(-1);
        assert_eq!((bits, size), (1, 1));
        assert_eq!(value & 1, 0);
        let (bits, value, size) = difference_symbol(-2);
        assert_eq!((bits, size), (2, 2));
        assert_eq!(value & 0b11, 0b01);
    }

    /// Differences outside the sixteen-bit window fold back into it, which is
    /// what makes the decoder's `(px + diff) & 0xFFFF` reconstruct exactly.
    #[test]
    fn differences_reduce_modulo_two_to_the_sixteen() {
        for (difference, expected) in [(65_536, 0), (65_537, 1), (-65_535, 1), (-40_000, 25_536)] {
            let (symbol, value, size) = difference_symbol(difference);
            let reduced: i32 = if symbol == 16 {
                -32_768
            } else if symbol == 0 {
                0
            } else {
                let raw = (value & ((1u32 << size) - 1)) as i32;
                if raw < (1 << (size - 1)) {
                    raw - (1 << size) + 1
                } else {
                    raw
                }
            };
            assert_eq!(
                (reduced & 0xFFFF),
                (expected & 0xFFFF),
                "difference {difference}"
            );
        }
    }
}
