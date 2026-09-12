//! Arithmetic-coded scan decoding: `SOF9`, `SOF10` and `SOF11`.
//!
//! The three processes share the QM coder of [`crate::arith`] and differ only
//! in their conditioning models:
//!
//! * sequential and progressive DC use T.81 F.1.4.4.1's model, conditioned on
//!   the previous difference of the same component (Table F.4);
//! * AC coefficients use F.1.4.4.2's model, conditioned on the spectral index
//!   and, above `Kx`, on a second magnitude chain (Table F.5);
//! * lossless differences use H.1.2.3's two-dimensional model, conditioned on
//!   the differences already coded to the left and above (Figure H.2).
//!
//! Truncation behaves differently from a Huffman scan and deliberately so:
//! T.81 D.2.6 lets the decoder read past the last coded byte, so hitting the
//! terminating marker is normal and the byte source supplies zeros from there
//! on. A short arithmetic scan therefore decodes to noise rather than to an
//! error — libjpeg has the same property — and only a magnitude or spectral
//! overflow, or a missing restart marker, is reported.

use super::planes::Planes;
use super::progressive::Coefficients;
use super::scan::{ScanGeometry, ScanOutcome, ScanTables};
use crate::arith::decoder::ArithDecoder;
use crate::arith::qm::Bin;
use crate::arith::{
    AC_BINS, AC_X1_HIGH, AC_X1_LOW, Category, Conditioning, DC_BINS, DC_X1, DctStats,
    LOSSLESS_BINS, LosslessStats, MAGNITUDE_LIMIT, MAGNITUDE_OFFSET, lossless_context, lossless_x1,
};
use crate::error::{JpegError, Result, TableKind};
use crate::frame::{ArithmeticConditioning, FrameHeader, ScanHeader};
use crate::idct::idct_scaled_into;
use crate::tables::ZIGZAG_TO_NATURAL;

/// Decode one DC difference (T.81 figures F.19 to F.24).
///
/// `context` is the per-scan-component conditioning state and `area` the
/// per-table-slot statistics; the asymmetry is T.81's, and it is why two
/// chroma components that name one table adapt together while keeping
/// separate predictions.
fn decode_dc_diff(
    decoder: &mut ArithDecoder<'_>,
    area: &mut [Bin; DC_BINS],
    context: &mut usize,
    conditioning: &Conditioning,
    slot: usize,
) -> Option<i32> {
    let base = *context;
    if decoder.decode(&mut area[base]) == 0 {
        *context = Category::Zero.dc_context();
        return Some(0);
    }
    let sign = decoder.decode(&mut area[base + 1]);
    let mut st = base + 2 + usize::from(sign);
    let mut m = u32::from(decoder.decode(&mut area[st]));
    if m != 0 {
        st = DC_X1;
        while decoder.decode(&mut area[st]) != 0 {
            m <<= 1;
            if m == MAGNITUDE_LIMIT {
                decoder.fail();
                return None;
            }
            st += 1;
        }
    }
    *context = conditioning.classify(slot, m, sign != 0).dc_context();

    let mut value = m;
    st += MAGNITUDE_OFFSET;
    let mut bit = m >> 1;
    while bit != 0 {
        if decoder.decode(&mut area[st]) != 0 {
            value |= bit;
        }
        bit >>= 1;
    }
    let value = value as i32 + 1;
    Some(if sign != 0 { -value } else { value })
}

/// Decode the AC coefficients of one block over the spectral band
/// `ss..=se`, applying the point transform `al` (T.81 figures F.20 to F.24
/// and G.1.2.2).
fn decode_ac_band(
    decoder: &mut ArithDecoder<'_>,
    area: &mut [Bin; AC_BINS],
    conditioning: &Conditioning,
    slot: usize,
    band: (usize, usize),
    al: u8,
    block: &mut [i32],
) -> Option<()> {
    // `Ss`/`Se` are validated in the engine; clamping here as well keeps a
    // corrupt band from indexing the zig-zag table even if a future caller
    // forgets, at the cost of one comparison per block.
    let (ss, se) = (band.0.max(1), band.1.min(63));
    let kx = conditioning.kx(slot) as usize;
    let mut k = ss;
    while k <= se {
        let mut st = 3 * (k - 1);
        if decoder.decode(&mut area[st]) != 0 {
            break;
        }
        while decoder.decode(&mut area[st + 1]) == 0 {
            st += 3;
            k += 1;
            if k > se {
                decoder.fail();
                return None;
            }
        }
        let sign = decoder.decode_fixed();
        st += 2;
        let mut m = u32::from(decoder.decode(&mut area[st]));
        if m != 0 && decoder.decode(&mut area[st]) != 0 {
            m <<= 1;
            st = if k <= kx { AC_X1_LOW } else { AC_X1_HIGH };
            while decoder.decode(&mut area[st]) != 0 {
                m <<= 1;
                if m == MAGNITUDE_LIMIT {
                    decoder.fail();
                    return None;
                }
                st += 1;
            }
        }

        let mut value = m;
        st += MAGNITUDE_OFFSET;
        let mut bit = m >> 1;
        while bit != 0 {
            if decoder.decode(&mut area[st]) != 0 {
                value |= bit;
            }
            bit >>= 1;
        }
        let value = value as i32 + 1;
        let value = if sign != 0 { -value } else { value };
        block[ZIGZAG_TO_NATURAL[k]] = value << al;
        k += 1;
    }
    Some(())
}

/// Decode one block of an AC refinement scan (T.81 G.1.2.3).
fn decode_ac_refine(
    decoder: &mut ArithDecoder<'_>,
    area: &mut [Bin; AC_BINS],
    band: (usize, usize),
    al: u8,
    block: &mut [i32],
) -> Option<()> {
    let (ss, se) = (band.0.max(1), band.1.min(63));
    let plus = 1i32 << al;
    let minus = -1i32 << al;

    // The last index that already carries a coefficient: below it the EOB
    // decision is not coded at all, because the band cannot end there.
    let mut kex = se;
    while kex > 0 && block[ZIGZAG_TO_NATURAL[kex]] == 0 {
        kex -= 1;
    }

    let mut k = ss;
    while k <= se {
        let mut st = 3 * (k - 1);
        if k > kex && decoder.decode(&mut area[st]) != 0 {
            break;
        }
        loop {
            let index = ZIGZAG_TO_NATURAL[k];
            if block[index] != 0 {
                if decoder.decode(&mut area[st + 2]) != 0 {
                    if block[index] < 0 {
                        block[index] += minus;
                    } else {
                        block[index] += plus;
                    }
                }
                break;
            }
            if decoder.decode(&mut area[st + 1]) != 0 {
                block[index] = if decoder.decode_fixed() != 0 {
                    minus
                } else {
                    plus
                };
                break;
            }
            st += 3;
            k += 1;
            if k > se {
                decoder.fail();
                return None;
            }
        }
        k += 1;
    }
    Some(())
}

/// The table slots one scan's components name, resolved once.
struct ScanSlots {
    dc: Vec<usize>,
    ac: Vec<usize>,
}

impl ScanSlots {
    fn new(scan: &ScanHeader) -> Self {
        Self {
            dc: scan.dc_table.iter().map(|&t| usize::from(t) & 3).collect(),
            ac: scan.ac_table.iter().map(|&t| usize::from(t) & 3).collect(),
        }
    }
}

/// Reset the statistics, predictions and conditioning of one restart
/// interval (T.81 D.2.7 and E.2.4).
fn reset_dct_interval(
    stats: &mut DctStats,
    slots: &ScanSlots,
    predictions: &mut [i32; 4],
    contexts: &mut [usize; 4],
    dc_active: bool,
    ac_active: bool,
) {
    for (k, &slot) in slots.dc.iter().enumerate() {
        if dc_active {
            stats.reset_dc(slot);
            predictions[k] = 0;
            contexts[k] = 0;
        }
    }
    if ac_active {
        for &slot in &slots.ac {
            stats.reset_ac(slot);
        }
    }
}

/// Decode a sequential arithmetic scan (`SOF9`) straight into the planes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_sequential_arith(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    dac: &ArithmeticConditioning,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let geometry = ScanGeometry::new(frame, scan)?;
    let slots = ScanSlots::new(scan);
    let conditioning = Conditioning::new(dac);

    let mut quant_tables = Vec::with_capacity(scan.component_indices.len());
    for &index in &scan.component_indices {
        let tq = frame.components[index].quant_table;
        quant_tables.push(
            tables
                .quant
                .get(usize::from(tq))
                .and_then(Option::as_ref)
                .ok_or(JpegError::UndefinedTable {
                    kind: TableKind::Quantisation,
                    index: tq,
                })?,
        );
    }

    let center = 1i32 << (frame.precision - 1);
    let maxval = (1i32 << frame.precision) - 1;
    let mut decoder = ArithDecoder::new(entropy);
    let mut stats = DctStats::new();
    let mut predictions = [0i32; 4];
    let mut contexts = [0usize; 4];
    let mut block = [0i32; 64];
    let mut units_to_restart = if restart_interval == 0 {
        u64::MAX
    } else {
        u64::from(restart_interval)
    };

    let mut unit = 0u64;
    'scan: while unit < geometry.units {
        if units_to_restart == 0 {
            if !decoder.take_restart() {
                if tolerate_truncated {
                    break;
                }
                return Err(JpegError::eof("restart marker"));
            }
            reset_dct_interval(
                &mut stats,
                &slots,
                &mut predictions,
                &mut contexts,
                true,
                true,
            );
            units_to_restart = u64::from(restart_interval);
        }
        units_to_restart -= 1;

        let mut failed = false;
        geometry.blocks_of_unit(frame, scan, unit, |k, bcol, brow| {
            block.fill(0);
            let dc_slot = slots.dc[k];
            let diff = decode_dc_diff(
                &mut decoder,
                stats.dc(dc_slot),
                &mut contexts[k],
                &conditioning,
                dc_slot,
            );
            let Some(diff) = diff else {
                failed = true;
                return Ok(());
            };
            // T.81 F.1.4.4.1.1 works modulo 2^16, which libjpeg mirrors by
            // masking the running prediction to sixteen bits.
            let wrapped = (predictions[k].wrapping_add(diff)) & 0xFFFF;
            predictions[k] = i32::from(wrapped as u16 as i16);
            block[0] = predictions[k];

            let ac_slot = slots.ac[k];
            if decode_ac_band(
                &mut decoder,
                stats.ac(ac_slot),
                &conditioning,
                ac_slot,
                (1, 63),
                0,
                &mut block,
            )
            .is_none()
            {
                failed = true;
                return Ok(());
            }

            let index = scan.component_indices[k];
            let output_size = planes.output_size(index);
            let size = usize::from(output_size);
            let stride = planes.stride(index);
            let offset =
                planes.offset(index) + brow as usize * size * stride + bcol as usize * size;
            idct_scaled_into(
                &block,
                quant_tables[k].natural(),
                planes.data_mut(),
                offset,
                stride,
                center,
                maxval,
                output_size,
            );
            Ok(())
        })?;
        if failed {
            break 'scan;
        }
        unit += 1;
    }

    finish_scan(&mut decoder, unit, geometry.units, tolerate_truncated)
}

/// Decode a progressive arithmetic scan (`SOF10`) into the coefficient
/// buffers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_progressive_arith(
    frame: &FrameHeader,
    scan: &ScanHeader,
    dac: &ArithmeticConditioning,
    restart_interval: u16,
    entropy: &[u8],
    coefficients: &mut Coefficients,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let geometry = ScanGeometry::new(frame, scan)?;
    let slots = ScanSlots::new(scan);
    let conditioning = Conditioning::new(dac);
    let is_dc = scan.spectral_start == 0;
    let refinement = scan.approx_high != 0;
    let band = (
        usize::from(scan.spectral_start),
        usize::from(scan.spectral_end),
    );

    let mut decoder = ArithDecoder::new(entropy);
    let mut stats = DctStats::new();
    let mut predictions = [0i32; 4];
    let mut contexts = [0usize; 4];
    let mut units_to_restart = if restart_interval == 0 {
        u64::MAX
    } else {
        u64::from(restart_interval)
    };

    let mut unit = 0u64;
    'scan: while unit < geometry.units {
        if units_to_restart == 0 {
            if !decoder.take_restart() {
                if tolerate_truncated {
                    break;
                }
                return Err(JpegError::eof("restart marker"));
            }
            reset_dct_interval(
                &mut stats,
                &slots,
                &mut predictions,
                &mut contexts,
                is_dc && !refinement,
                !is_dc,
            );
            units_to_restart = u64::from(restart_interval);
        }
        units_to_restart -= 1;

        let mut failed = false;
        geometry.blocks_of_unit(frame, scan, unit, |k, bcol, brow| {
            let index = scan.component_indices[k];
            let Some(block) = coefficients.block_mut(index, bcol, brow) else {
                // A block outside the padded grid is not coded at all.
                return Ok(());
            };
            if is_dc {
                if refinement {
                    if decoder.decode_fixed() != 0 {
                        block[0] |= 1 << scan.approx_low;
                    }
                } else {
                    let dc_slot = slots.dc[k];
                    let diff = decode_dc_diff(
                        &mut decoder,
                        stats.dc(dc_slot),
                        &mut contexts[k],
                        &conditioning,
                        dc_slot,
                    );
                    let Some(diff) = diff else {
                        failed = true;
                        return Ok(());
                    };
                    let wrapped = (predictions[k].wrapping_add(diff)) & 0xFFFF;
                    predictions[k] = i32::from(wrapped as u16 as i16);
                    block[0] = predictions[k] << scan.approx_low;
                }
                return Ok(());
            }

            let ac_slot = slots.ac[k];
            let outcome = if refinement {
                decode_ac_refine(
                    &mut decoder,
                    stats.ac(ac_slot),
                    band,
                    scan.approx_low,
                    block,
                )
            } else {
                decode_ac_band(
                    &mut decoder,
                    stats.ac(ac_slot),
                    &conditioning,
                    ac_slot,
                    band,
                    scan.approx_low,
                    block,
                )
            };
            if outcome.is_none() {
                failed = true;
            }
            Ok(())
        })?;
        if failed {
            break 'scan;
        }
        unit += 1;
    }

    finish_scan(&mut decoder, unit, geometry.units, tolerate_truncated)
}

/// Close a scan: position on the terminating marker and report what happened.
fn finish_scan(
    decoder: &mut ArithDecoder<'_>,
    decoded: u64,
    total: u64,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let truncated = decoder.faulted() || decoded < total;
    if truncated && !tolerate_truncated {
        return Err(JpegError::InvalidArithmeticCode { mcu: decoded });
    }
    decoder.seek_marker();
    Ok(ScanOutcome {
        consumed: decoder.byte_offset(),
        truncated,
    })
}

/// The neighbouring conditioning categories of a lossless scan.
///
/// `above` holds one category per sample column of the component, so that the
/// sample being coded can read the category of the difference coded directly
/// above it; `left` holds one per row of the current MCU band, because an
/// interleaved MCU codes several rows before returning to the next column.
struct LosslessContext {
    above: Vec<Vec<Category>>,
    left: Vec<Vec<Category>>,
}

impl LosslessContext {
    /// Room for every component of the scan, all categories zero, which is
    /// what T.81 H.1.2.3.1 requires at the start of a scan.
    fn new(widths: &[usize], rows: &[usize]) -> Self {
        Self {
            above: widths
                .iter()
                .map(|&w| vec![Category::Zero; w + 1])
                .collect(),
            left: rows.iter().map(|&v| vec![Category::Zero; v]).collect(),
        }
    }

    /// Reset every category, as a restart interval requires.
    fn reset(&mut self) {
        for row in &mut self.above {
            row.fill(Category::Zero);
        }
        for row in &mut self.left {
            row.fill(Category::Zero);
        }
    }

    /// Clear the "difference to the left" at the start of a sample row.
    fn start_of_row(&mut self, component: usize, row_in_band: usize) {
        self.left[component][row_in_band] = Category::Zero;
    }
}

/// Decode one lossless difference (T.81 H.1.2.3, Table H.3).
fn decode_lossless_diff(
    decoder: &mut ArithDecoder<'_>,
    area: &mut [Bin; LOSSLESS_BINS],
    conditioning: &Conditioning,
    slot: usize,
    left: Category,
    above: Category,
) -> Option<(i32, Category)> {
    let base = lossless_context(left, above);
    if decoder.decode(&mut area[base]) == 0 {
        return Some((0, Category::Zero));
    }
    let sign = decoder.decode(&mut area[base + 1]);
    let mut st = base + 2 + usize::from(sign);
    let mut m = u32::from(decoder.decode(&mut area[st]));
    if m != 0 {
        st = lossless_x1(above);
        while decoder.decode(&mut area[st]) != 0 {
            m <<= 1;
            if m == MAGNITUDE_LIMIT {
                decoder.fail();
                return None;
            }
            st += 1;
        }
    }
    let category = conditioning.classify(slot, m, sign != 0);

    let mut value = m;
    st += MAGNITUDE_OFFSET;
    let mut bit = m >> 1;
    while bit != 0 {
        if decoder.decode(&mut area[st]) != 0 {
            value |= bit;
        }
        bit >>= 1;
    }
    let value = value as i32 + 1;
    Some((if sign != 0 { -value } else { value }, category))
}

/// Prediction `Px` for predictor selection value `psv` (T.81 Table H.1).
#[inline]
fn predict(psv: u8, ra: i32, rb: i32, rc: i32) -> i32 {
    match psv {
        1 => ra,
        2 => rb,
        3 => rc,
        4 => ra + rb - rc,
        5 => ra + ((rb - rc) >> 1),
        6 => rb + ((ra - rc) >> 1),
        // `psv` is validated to be 1..=7 before this is reached.
        _ => (ra + rb) >> 1,
    }
}

/// Decode a lossless arithmetic scan (`SOF11`) into the sample planes.
///
/// The walk is the one the Huffman lossless decoder uses — T.81 H.1.2.1's
/// prediction, including the "first row of the restart interval" rule — with
/// the two-dimensional conditioning of H.1.2.3 carried alongside it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_lossless_arith(
    frame: &FrameHeader,
    scan: &ScanHeader,
    dac: &ArithmeticConditioning,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let psv = scan.spectral_start;
    if !(1..=7).contains(&psv) {
        return Err(JpegError::Unsupported(
            crate::error::UnsupportedFeature::LosslessPredictor(psv),
        ));
    }
    let point_transform = scan.approx_low;
    if point_transform >= frame.precision {
        return Err(JpegError::malformed(
            "SOS",
            0,
            "point transform is not smaller than the sample precision",
        ));
    }

    let count = scan.component_indices.len();
    let slots = ScanSlots::new(scan);
    let conditioning = Conditioning::new(dac);
    let interleaved = scan.is_interleaved();

    let mut extents = Vec::with_capacity(count);
    let mut context_widths = Vec::with_capacity(count);
    let mut band_rows = Vec::with_capacity(count);
    for &index in &scan.component_indices {
        let component = &frame.components[index];
        let h = usize::from(component.h);
        let v = usize::from(component.v);
        extents.push((
            component.width_samples as usize,
            component.height_samples as usize,
            h,
            v,
        ));
        // Padding samples are coded like any other, so the conditioning has
        // to have room for them.
        context_widths.push(if interleaved {
            (frame.mcus_per_line as usize).saturating_mul(h).max(1)
        } else {
            (component.width_samples as usize).max(1)
        });
        band_rows.push(if interleaved { v } else { 1 });
    }
    let mut context = LosslessContext::new(&context_widths, &band_rows);

    let units: u64 = if interleaved {
        u64::from(frame.mcus_per_line) * u64::from(frame.mcus_per_column)
    } else {
        let (w, h, _, _) = extents[0];
        (w as u64) * (h as u64)
    };
    let row_units = if interleaved {
        u64::from(frame.mcus_per_line).max(1)
    } else {
        extents[0].0.max(1) as u64
    };

    let default_prediction = 1i32 << (frame.precision - point_transform - 1);
    let mut decoder = ArithDecoder::new(entropy);
    let mut stats = LosslessStats::new();
    let mut units_to_restart = if restart_interval == 0 {
        u64::MAX
    } else {
        u64::from(restart_interval)
    };
    let mut interval_first_row = [0usize; 4];
    let mut fresh_interval = true;
    let mut unit = 0u64;
    let mut positions = [(0usize, 0usize, 0usize); 16];

    'outer: while unit < units {
        if units_to_restart == 0 {
            if !decoder.take_restart() {
                if tolerate_truncated {
                    break;
                }
                return Err(JpegError::eof("restart marker"));
            }
            for &slot in &slots.dc {
                stats.reset(slot);
            }
            context.reset();
            units_to_restart = u64::from(restart_interval);
            fresh_interval = true;
            let row = (unit / row_units) as usize;
            for (k, slot) in interval_first_row.iter_mut().enumerate().take(count) {
                *slot = if interleaved { row * extents[k].3 } else { row };
            }
        }
        units_to_restart -= 1;

        for k in 0..count {
            let index = scan.component_indices[k];
            let (comp_width, comp_height, h, v) = extents[k];
            let sample_count = if interleaved {
                let mcu_x = (unit % row_units) as usize;
                let mcu_y = (unit / row_units) as usize;
                let mut n = 0;
                for dy in 0..v {
                    for dx in 0..h {
                        if n < positions.len() {
                            positions[n] = (mcu_x * h + dx, mcu_y * v + dy, dy);
                            n += 1;
                        }
                    }
                }
                n
            } else {
                positions[0] = ((unit % row_units) as usize, (unit / row_units) as usize, 0);
                1
            };

            for position in positions.iter().take(sample_count) {
                let (x, y, dy) = *position;
                if x == 0 {
                    context.start_of_row(k, dy);
                }
                let left = context.left[k][dy];
                let above = context.above[k][x.min(context_widths[k] - 1)];
                let slot = slots.dc[k];
                let decoded = decode_lossless_diff(
                    &mut decoder,
                    stats.area(slot),
                    &conditioning,
                    slot,
                    left,
                    above,
                );
                let Some((diff, category)) = decoded else {
                    break 'outer;
                };
                context.left[k][dy] = category;
                context.above[k][x.min(context_widths[k] - 1)] = category;

                // Samples outside the component's real extent are MCU
                // padding: they are coded, and condition their neighbours,
                // but they are not stored.
                if x >= comp_width || y >= comp_height {
                    continue;
                }
                let stride = planes.stride(index);
                let base = planes.offset(index);
                let data = planes.data_mut();
                let at = base + y * stride + x;
                let ra = if x > 0 { i32::from(data[at - 1]) } else { 0 };
                let rb = if y > 0 {
                    i32::from(data[at - stride])
                } else {
                    0
                };
                let rc = if x > 0 && y > 0 {
                    i32::from(data[at - stride - 1])
                } else {
                    0
                };
                let px = if fresh_interval {
                    default_prediction
                } else if y == interval_first_row[k] {
                    if x == 0 { default_prediction } else { ra }
                } else if x == 0 {
                    rb
                } else {
                    predict(psv, ra, rb, rc)
                };
                fresh_interval = false;
                data[at] = (px.wrapping_add(diff) as u32 & 0xFFFF) as u16;
            }
        }

        unit += 1;
    }

    let outcome = finish_scan(&mut decoder, unit, units, tolerate_truncated)?;

    if point_transform > 0 {
        for &index in &scan.component_indices {
            let stride = planes.stride(index);
            let base = planes.offset(index);
            let rows = planes.padded_height(index);
            let data = planes.data_mut();
            for row in 0..rows {
                for x in 0..stride {
                    let at = base + row * stride + x;
                    data[at] = data[at].wrapping_shl(u32::from(point_transform));
                }
            }
        }
    }

    Ok(outcome)
}
