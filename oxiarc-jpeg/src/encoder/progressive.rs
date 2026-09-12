//! Progressive entropy coding (T.81 Annex G) and libjpeg's default scan
//! script.
//!
//! Four scan kinds, each with its own coder: DC first, DC refinement, AC
//! first and AC refinement. The last of those is where progressive encoders
//! traditionally go wrong, because it has to buffer correction bits and emit
//! them **after** whatever symbol they belong to, while an end-of-band run
//! may still be open across MCUs. The rules, transcribed from `jcphuff.c`:
//!
//! * correction bits accumulated in one block are emitted right after the
//!   symbol that closes the block, or carried into the pending `EOBRUN`;
//! * a pending `EOBRUN` is flushed *before* any `ZRL` or newly-nonzero
//!   symbol, and flushing it also drains the carried correction bits;
//! * `ZRL` is only emitted while `k <= EOB`, so trailing runs fold into the
//!   end-of-band code instead;
//! * the run is forced out at `EOBRUN == 0x7FFF` or when the correction
//!   buffer would overflow during the next block;
//! * a restart resets `EOBRUN` and the correction buffer for AC scans and the
//!   DC predictions for DC scans, and the pending run is emitted *before* the
//!   marker.
//!
//! Progressive frames always use generated Huffman tables: `jcmaster.c` sets
//! `optimize_coding` unconditionally for them, so every progressive scan is
//! two passes over the coefficients.

use super::bitwriter::BitWriter;
use super::coefficients::CoefficientPlane;
use super::enctable::{DerivedTable, Histogram};
use super::frame::{TableBank, scan_tables};
use super::markers;
use super::options::ScanSpec;
use super::plan::EncodePlan;
use crate::color::ColorSpace;
use crate::error::{JpegError, Result};

/// libjpeg's `MAX_CORR_BITS`: how many correction bits may be carried.
const MAX_CORRECTION_BITS: usize = 1000;

/// Where a progressive scan's symbols and bits go.
///
/// [`PhuffSink::select`] exists because an interleaved DC scan may give its
/// components different table slots — libjpeg's default YCbCr script does
/// exactly that, sending luminance through slot 0 and both chrominance
/// components through slot 1 in one scan. Both passes therefore dispatch per
/// block on the component's own slot, which is what `count_ptrs[tbl_no]` and
/// `derived_tbls[tbl_no]` do in `jcphuff.c`.
trait PhuffSink {
    /// Point subsequent symbols at one table slot.
    fn select(&mut self, slot: usize);
    /// A Huffman symbol from the selected table.
    fn symbol(&mut self, symbol: u8) -> Result<()>;
    /// Raw bits that carry no Huffman code.
    fn bits(&mut self, value: u32, size: u32);
    /// Previously buffered correction bits, one per entry.
    fn buffered(&mut self, bits: &[u8]);
    /// Flush to a byte boundary and write a restart marker.
    fn restart_marker(&mut self, index: u8);
}

/// The coding pass: real bits through the derived tables.
struct EmitPhuff<'a> {
    writer: &'a mut BitWriter,
    tables: &'a [Option<DerivedTable>; 4],
    slot: usize,
}

impl PhuffSink for EmitPhuff<'_> {
    fn select(&mut self, slot: usize) {
        self.slot = slot;
    }

    fn symbol(&mut self, symbol: u8) -> Result<()> {
        let table = self.tables[self.slot]
            .as_ref()
            .ok_or(JpegError::InvalidEncodeParameter {
                parameter: "huffman_tables",
                reason: "a progressive scan references an undefined Huffman table",
            })?;
        let (code, size) = table.lookup(symbol)?;
        self.writer.emit_bits(code, size);
        Ok(())
    }

    fn bits(&mut self, value: u32, size: u32) {
        self.writer.emit_bits(value, size);
    }

    fn buffered(&mut self, bits: &[u8]) {
        for &bit in bits {
            self.writer.emit_bits(u32::from(bit), 1);
        }
    }

    fn restart_marker(&mut self, index: u8) {
        self.writer.emit_restart(index);
    }
}

/// The statistics pass: symbols counted, bits discarded.
struct GatherPhuff<'a> {
    histograms: &'a mut [Histogram; 4],
    slot: usize,
}

impl PhuffSink for GatherPhuff<'_> {
    fn select(&mut self, slot: usize) {
        self.slot = slot;
    }
    fn symbol(&mut self, symbol: u8) -> Result<()> {
        self.histograms[self.slot].count(symbol);
        Ok(())
    }
    fn bits(&mut self, _value: u32, _size: u32) {}
    fn buffered(&mut self, _bits: &[u8]) {}
    fn restart_marker(&mut self, _index: u8) {}
}

/// Coder state that survives across the blocks of one scan.
struct Coder<S: PhuffSink> {
    sink: S,
    eob_run: u32,
    /// libjpeg's `bit_buffer`, holding `BE` correction bits.
    correction: Vec<u8>,
    max_coef_bits: u32,
}

impl<S: PhuffSink> Coder<S> {
    fn new(sink: S, precision: u8) -> Self {
        Self {
            sink,
            eob_run: 0,
            correction: Vec::with_capacity(MAX_CORRECTION_BITS + 64),
            max_coef_bits: u32::from(precision) + 2,
        }
    }

    /// `emit_eobrun`: close the pending end-of-band run and drain the
    /// correction bits it carried.
    fn flush_eob_run(&mut self) -> Result<()> {
        if self.eob_run == 0 {
            return Ok(());
        }
        let nbits = 31 - self.eob_run.leading_zeros();
        self.sink.symbol((nbits << 4) as u8)?;
        if nbits > 0 {
            self.sink.bits(self.eob_run, nbits);
        }
        self.eob_run = 0;
        let carried = std::mem::take(&mut self.correction);
        self.sink.buffered(&carried);
        self.correction = carried;
        self.correction.clear();
        Ok(())
    }

    /// `encode_mcu_DC_first`, one block.
    fn dc_first(&mut self, block: &[i16; 64], last_dc: i32, al: u8) -> Result<i32> {
        let value = i32::from(block[0]) >> al;
        let difference = value - last_dc;
        let mut magnitude = difference;
        let mut sent = difference;
        if magnitude < 0 {
            magnitude = -magnitude;
            sent -= 1;
        }
        let nbits = 32 - (magnitude as u32).leading_zeros();
        if nbits > self.max_coef_bits + 1 {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "coefficients",
                reason: "a DC difference is out of range for this sample precision",
            });
        }
        self.sink.symbol(nbits as u8)?;
        if nbits > 0 {
            self.sink.bits(sent as u32, nbits);
        }
        Ok(value)
    }

    /// `encode_mcu_DC_refine`, one block: one bit, no table.
    fn dc_refine(&mut self, block: &[i16; 64], al: u8) {
        self.sink.bits((i32::from(block[0]) >> al) as u32, 1);
    }

    /// `encode_mcu_AC_first`, one block.
    fn ac_first(&mut self, block: &[i16; 64], ss: u8, se: u8, al: u8) -> Result<()> {
        let mut run = 0u32;
        for &stored in &block[usize::from(ss)..=usize::from(se)] {
            let coefficient = i32::from(stored);
            if coefficient == 0 {
                run += 1;
                continue;
            }
            // The point transform on an AC coefficient is a division that
            // rounds towards zero, so the shift happens on the magnitude.
            let (magnitude, sent) = if coefficient < 0 {
                let magnitude = (-coefficient) >> al;
                (magnitude, !magnitude)
            } else {
                let magnitude = coefficient >> al;
                (magnitude, magnitude)
            };
            if magnitude == 0 {
                run += 1;
                continue;
            }
            self.flush_eob_run()?;
            while run > 15 {
                self.sink.symbol(0xF0)?;
                run -= 16;
            }
            let nbits = 32 - (magnitude as u32).leading_zeros();
            if nbits > self.max_coef_bits {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "coefficients",
                    reason: "an AC coefficient is out of range for this sample precision",
                });
            }
            self.sink.symbol(((run << 4) | nbits) as u8)?;
            self.sink.bits(sent as u32, nbits);
            run = 0;
        }
        if run > 0 {
            self.eob_run += 1;
            if self.eob_run == 0x7FFF {
                self.flush_eob_run()?;
            }
        }
        Ok(())
    }

    /// `encode_mcu_AC_refine`, one block.
    fn ac_refine(&mut self, block: &[i16; 64], ss: u8, se: u8, al: u8) -> Result<()> {
        // Pre-pass: transformed magnitudes and the index of the last
        // newly-nonzero coefficient.
        let mut absolute = [0i32; 64];
        let mut eob = 0usize;
        for k in usize::from(ss)..=usize::from(se) {
            let coefficient = i32::from(block[k]);
            let magnitude = if coefficient < 0 {
                -coefficient
            } else {
                coefficient
            } >> al;
            absolute[k] = magnitude;
            if magnitude == 1 {
                eob = k;
            }
        }

        let mut run = 0u32;
        let mut pending: Vec<u8> = Vec::new();
        for k in usize::from(ss)..=usize::from(se) {
            let magnitude = absolute[k];
            if magnitude == 0 {
                run += 1;
                continue;
            }
            // A run longer than fifteen only needs a ZRL while there is still
            // a newly-nonzero coefficient ahead; past that it folds into the
            // end-of-band run.
            while run > 15 && k <= eob {
                self.flush_eob_run()?;
                self.sink.symbol(0xF0)?;
                run -= 16;
                self.sink.buffered(&pending);
                pending.clear();
            }
            if magnitude > 1 {
                // Already nonzero in an earlier scan: one correction bit.
                pending.push((magnitude & 1) as u8);
                continue;
            }
            self.flush_eob_run()?;
            self.sink.symbol(((run << 4) | 1) as u8)?;
            let sign = u32::from(block[k] >= 0);
            self.sink.bits(sign, 1);
            self.sink.buffered(&pending);
            pending.clear();
            run = 0;
        }
        if run > 0 || !pending.is_empty() {
            self.eob_run += 1;
            self.correction.extend_from_slice(&pending);
            if self.eob_run == 0x7FFF || self.correction.len() > MAX_CORRECTION_BITS - 64 + 1 {
                self.flush_eob_run()?;
            }
        }
        Ok(())
    }
}

/// Run one progressive scan over every block it covers.
///
/// `restart_interval` is in MCUs of this scan, which for a single-component
/// scan means blocks.
fn run_scan<S: PhuffSink>(
    coder: &mut Coder<S>,
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    scan: &ScanSpec,
    tables: &[super::sequential::ScanTables],
    restart_interval: usize,
) -> Result<()> {
    let components = &scan.components;
    let mcus_per_row = plan.mcus_per_row_for(components);
    let mcu_rows = plan.mcu_rows_for(components);
    let interleaved = components.len() > 1;
    let is_dc = scan.is_dc();
    let mut last_dc = vec![0i32; components.len()];
    let mut restart_index = 0u8;

    for mcu in 0..mcus_per_row * mcu_rows {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            coder.flush_eob_run()?;
            coder.sink.restart_marker(restart_index);
            restart_index = (restart_index + 1) & 7;
            if is_dc {
                last_dc.iter_mut().for_each(|value| *value = 0);
            } else {
                coder.eob_run = 0;
                coder.correction.clear();
            }
        }
        let mcu_x = mcu % mcus_per_row;
        let mcu_y = mcu / mcus_per_row;
        for (scan_index, &component_index) in components.iter().enumerate() {
            let component = &plan.components[component_index];
            let plane = &coefficients[component_index];
            let (h, v) = if interleaved {
                (usize::from(component.h), usize::from(component.v))
            } else {
                (1, 1)
            };
            for by in 0..v {
                for bx in 0..h {
                    let block_x = if interleaved { mcu_x * h + bx } else { mcu_x };
                    let block_y = if interleaved { mcu_y * v + by } else { mcu_y };
                    let block = plane.block(block_x, block_y);
                    coder.sink.select(if is_dc {
                        tables[scan_index].dc
                    } else {
                        tables[scan_index].ac
                    });
                    if is_dc {
                        if scan.approx_high == 0 {
                            last_dc[scan_index] =
                                coder.dc_first(block, last_dc[scan_index], scan.approx_low)?;
                        } else {
                            coder.dc_refine(block, scan.approx_low);
                        }
                    } else if scan.approx_high == 0 {
                        coder.ac_first(
                            block,
                            scan.spectral_start,
                            scan.spectral_end,
                            scan.approx_low,
                        )?;
                    } else {
                        coder.ac_refine(
                            block,
                            scan.spectral_start,
                            scan.spectral_end,
                            scan.approx_low,
                        )?;
                    }
                }
            }
        }
    }
    coder.flush_eob_run()
}

/// libjpeg's `jpeg_simple_progression` scan script.
///
/// The ten-scan YCbCr variant is chosen only for a three-component YCbCr
/// frame; every other colour space gets the six-scan general script (two DC
/// scans and four AC scans per component).
#[must_use]
pub(crate) fn default_script(plan: &EncodePlan) -> Vec<ScanSpec> {
    let count = plan.components.len();
    let all: Vec<usize> = (0..count).collect();
    let mut script = Vec::new();

    if count == 3 && plan.color == ColorSpace::Ycbcr {
        script.push(ScanSpec::dc(all.clone(), 0, 1));
        script.push(ScanSpec::ac(0, 1, 5, 0, 2));
        script.push(ScanSpec::ac(2, 1, 63, 0, 1));
        script.push(ScanSpec::ac(1, 1, 63, 0, 1));
        script.push(ScanSpec::ac(0, 6, 63, 0, 2));
        script.push(ScanSpec::ac(0, 1, 63, 2, 1));
        script.push(ScanSpec::dc(all, 1, 0));
        script.push(ScanSpec::ac(2, 1, 63, 1, 0));
        script.push(ScanSpec::ac(1, 1, 63, 1, 0));
        script.push(ScanSpec::ac(0, 1, 63, 1, 0));
    } else {
        // The DC refinement comes *before* the final AC pass here, which is
        // not where libjpeg 6b put it. Measured from libjpeg-turbo 3.1.4.1:
        // `cjpeg -progressive -rgb` emits DC(0,1), AC(1..5,0,2) x N,
        // AC(6..63,0,2) x N, AC(1..63,2,1) x N, DC(1,0), AC(1..63,1,0) x N.
        script.push(ScanSpec::dc(all.clone(), 0, 1));
        for index in 0..count {
            script.push(ScanSpec::ac(index, 1, 5, 0, 2));
        }
        for index in 0..count {
            script.push(ScanSpec::ac(index, 6, 63, 0, 2));
        }
        for index in 0..count {
            script.push(ScanSpec::ac(index, 1, 63, 2, 1));
        }
        script.push(ScanSpec::dc(all, 1, 0));
        for index in 0..count {
            script.push(ScanSpec::ac(index, 1, 63, 1, 0));
        }
    }
    script
}

/// Reject a script that cannot produce a decodable frame (T.81 G.1.1.1).
pub(crate) fn validate_script(plan: &EncodePlan, script: &[ScanSpec]) -> Result<()> {
    if script.is_empty() {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "progressive_script",
            reason: "a progressive frame needs at least one scan",
        });
    }
    let count = plan.components.len();
    // Every component must receive a DC first pass before anything else, and
    // the successive approximation of each band must descend by one bit.
    let mut dc_sent = vec![false; count];
    let mut dc_low = vec![0u8; count];
    let mut ac_low = vec![[0u8; 64]; count];
    let mut ac_started = vec![[false; 64]; count];
    for scan in script {
        if scan.components.is_empty() || scan.components.len() > 4 {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "progressive_script",
                reason: "a scan must name between one and four components",
            });
        }
        for &index in &scan.components {
            if index >= count {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "progressive_script",
                    reason: "a scan names a component the frame does not have",
                });
            }
        }
        if scan.approx_low > 13 || scan.approx_high > 13 {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "progressive_script",
                reason: "successive approximation bit positions must be 0..=13",
            });
        }
        if scan.is_dc() {
            if scan.spectral_end != 0 {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "progressive_script",
                    reason: "a DC scan must have Se = 0",
                });
            }
            for &index in &scan.components {
                if scan.approx_high == 0 {
                    if dc_sent[index] {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "a component received two DC first passes",
                        });
                    }
                    dc_sent[index] = true;
                } else {
                    if !dc_sent[index] || scan.approx_high != dc_low[index] {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "a DC refinement does not continue the previous pass",
                        });
                    }
                    if scan.approx_low + 1 != scan.approx_high {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "successive approximation must descend one bit at a time",
                        });
                    }
                }
                dc_low[index] = scan.approx_low;
            }
        } else {
            if scan.components.len() != 1 {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "progressive_script",
                    reason: "an AC scan must name exactly one component",
                });
            }
            if scan.spectral_start > scan.spectral_end || scan.spectral_end > 63 {
                return Err(JpegError::InvalidEncodeParameter {
                    parameter: "progressive_script",
                    reason: "the spectral band must satisfy 1 <= Ss <= Se <= 63",
                });
            }
            let index = scan.components[0];
            for k in usize::from(scan.spectral_start)..=usize::from(scan.spectral_end) {
                if scan.approx_high == 0 {
                    if ac_started[index][k] {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "a coefficient received two AC first passes",
                        });
                    }
                    ac_started[index][k] = true;
                } else {
                    if !ac_started[index][k] || scan.approx_high != ac_low[index][k] {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "an AC refinement does not continue the previous pass",
                        });
                    }
                    if scan.approx_low + 1 != scan.approx_high {
                        return Err(JpegError::InvalidEncodeParameter {
                            parameter: "progressive_script",
                            reason: "successive approximation must descend one bit at a time",
                        });
                    }
                }
                ac_low[index][k] = scan.approx_low;
            }
        }
    }
    if !dc_sent.iter().all(|&sent| sent) {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "progressive_script",
            reason: "every component needs a DC first pass",
        });
    }
    Ok(())
}

/// Write every scan of a progressive frame.
pub(crate) fn write_scans(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    bank: &mut TableBank,
    out: &mut Vec<u8>,
) -> Result<()> {
    let script = match &plan.progressive_script {
        Some(script) => script.clone(),
        None => default_script(plan),
    };
    validate_script(plan, &script)?;

    let mut last_restart = 0u16;
    for scan in &script {
        let restart = plan.restart_for(&scan.components);
        let tables = scan_tables(plan, &scan.components);
        let is_dc = scan.is_dc();
        let needs_table = !is_dc || scan.approx_high == 0;

        if needs_table {
            let mut histograms: [Histogram; 4] = std::array::from_fn(|_| Histogram::default());
            gather(plan, coefficients, scan, &tables, restart, &mut histograms)?;
            let mut slots: Vec<usize> = tables
                .iter()
                .map(|t| if is_dc { t.dc } else { t.ac })
                .collect();
            slots.sort_unstable();
            slots.dedup();
            for slot in slots {
                let table = histograms[slot].optimal_table()?;
                if is_dc {
                    bank.set_dc(slot, table);
                } else {
                    bank.set_ac(slot, table);
                }
            }
        }

        for (scan_index, _) in scan.components.iter().enumerate() {
            if is_dc {
                if scan.approx_high == 0 {
                    bank.emit_dc(tables[scan_index].dc, out)?;
                }
            } else {
                bank.emit_ac(tables[scan_index].ac, out)?;
            }
        }
        if restart != last_restart {
            markers::dri(out, restart);
            last_restart = restart;
        }
        markers::sos(
            out,
            plan,
            &scan.components,
            scan.spectral_start,
            scan.spectral_end,
            scan.approx_high,
            scan.approx_low,
        )?;

        let mut writer = BitWriter::with_capacity(1 << 15);
        emit(
            plan,
            coefficients,
            scan,
            &tables,
            restart,
            bank,
            &mut writer,
        )?;
        out.extend_from_slice(&writer.finish());
    }
    Ok(())
}

/// The statistics pass for one scan: one traversal, counting into whichever
/// slot each block's component names.
fn gather(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    scan: &ScanSpec,
    tables: &[super::sequential::ScanTables],
    restart: u16,
    histograms: &mut [Histogram; 4],
) -> Result<()> {
    let sink = GatherPhuff {
        histograms,
        slot: 0,
    };
    let mut coder = Coder::new(sink, plan.precision);
    run_scan(
        &mut coder,
        plan,
        coefficients,
        scan,
        tables,
        usize::from(restart),
    )
}

/// The coding pass for one scan.
fn emit(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    scan: &ScanSpec,
    tables: &[super::sequential::ScanTables],
    restart: u16,
    bank: &TableBank,
    writer: &mut BitWriter,
) -> Result<()> {
    let derived = if scan.is_dc() {
        &bank.dc_derived
    } else {
        &bank.ac_derived
    };
    let sink = EmitPhuff {
        writer,
        tables: derived,
        slot: 0,
    };
    let mut coder = Coder::new(sink, plan.precision);
    run_scan(
        &mut coder,
        plan,
        coefficients,
        scan,
        tables,
        usize::from(restart),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::options::{EncodeOptions, EncodeProcess, InputColor};
    use crate::encoder::plan::build_plan;

    fn plan(input: InputColor) -> EncodePlan {
        let options = EncodeOptions {
            process: EncodeProcess::Progressive,
            ..Default::default()
        };
        build_plan(&options, 17, 19, input).expect("plan")
    }

    /// The exact ten scans `cjpeg -progressive` writes for a YCbCr image,
    /// in order. `tests/encode_oracle.rs` checks the bytes; this checks the
    /// script without needing the tool.
    #[test]
    fn the_ycbcr_script_matches_jpeg_simple_progression() {
        let script = default_script(&plan(InputColor::Rgb));
        let shapes: Vec<(Vec<usize>, u8, u8, u8, u8)> = script
            .iter()
            .map(|s| {
                (
                    s.components.clone(),
                    s.spectral_start,
                    s.spectral_end,
                    s.approx_high,
                    s.approx_low,
                )
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                (vec![0, 1, 2], 0, 0, 0, 1),
                (vec![0], 1, 5, 0, 2),
                (vec![2], 1, 63, 0, 1),
                (vec![1], 1, 63, 0, 1),
                (vec![0], 6, 63, 0, 2),
                (vec![0], 1, 63, 2, 1),
                (vec![0, 1, 2], 0, 0, 1, 0),
                (vec![2], 1, 63, 1, 0),
                (vec![1], 1, 63, 1, 0),
                (vec![0], 1, 63, 1, 0),
            ]
        );
    }

    /// The general script, measured from `cjpeg -progressive -rgb` on
    /// libjpeg-turbo 3.1.4.1. Note the DC refinement is the fifth scan, not
    /// the last — libjpeg 6b put it last and libjpeg-turbo moved it.
    #[test]
    fn the_general_script_puts_the_dc_refinement_before_the_final_ac_pass() {
        let script = default_script(&plan(InputColor::Luma));
        let shapes: Vec<(u8, u8, u8, u8)> = script
            .iter()
            .map(|s| {
                (
                    s.spectral_start,
                    s.spectral_end,
                    s.approx_high,
                    s.approx_low,
                )
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                (0, 0, 0, 1),
                (1, 5, 0, 2),
                (6, 63, 0, 2),
                (1, 63, 2, 1),
                (0, 0, 1, 0),
                (1, 63, 1, 0),
            ]
        );

        let plan = plan(InputColor::Cmyk);
        let script = default_script(&plan);
        assert_eq!(script.len(), 2 + 4 * 4);
        assert!(script[13].is_dc(), "the DC refinement is scan 14 of 18");
    }

    #[test]
    fn the_default_scripts_validate() {
        for input in [InputColor::Rgb, InputColor::Luma, InputColor::Cmyk] {
            let plan = plan(input);
            let script = default_script(&plan);
            validate_script(&plan, &script).expect("default script is legal");
        }
    }

    #[test]
    fn broken_scripts_are_rejected() {
        let plan = plan(InputColor::Rgb);
        let bad: Vec<Vec<ScanSpec>> = vec![
            vec![],
            vec![ScanSpec::ac(0, 1, 63, 0, 0)],
            vec![
                ScanSpec::dc(vec![0, 1, 2], 0, 0),
                ScanSpec::ac(0, 0, 63, 0, 0),
            ],
            vec![
                ScanSpec::dc(vec![0, 1, 2], 0, 0),
                ScanSpec::ac(0, 1, 63, 3, 0),
            ],
            vec![
                ScanSpec::dc(vec![0, 1, 2], 0, 1),
                ScanSpec::dc(vec![0, 1, 2], 0, 0),
            ],
            vec![ScanSpec::dc(vec![0, 9], 0, 0)],
            vec![ScanSpec {
                components: vec![0, 1],
                spectral_start: 1,
                spectral_end: 63,
                approx_high: 0,
                approx_low: 0,
            }],
        ];
        for (index, script) in bad.iter().enumerate() {
            assert!(validate_script(&plan, script).is_err(), "case {index}");
        }
    }

    #[test]
    fn eob_run_lengths_use_the_right_symbol() {
        struct Recorder {
            symbols: Vec<u8>,
            bits: Vec<(u32, u32)>,
        }
        impl PhuffSink for Recorder {
            fn select(&mut self, _slot: usize) {}
            fn symbol(&mut self, symbol: u8) -> Result<()> {
                self.symbols.push(symbol);
                Ok(())
            }
            fn bits(&mut self, value: u32, size: u32) {
                self.bits.push((value, size));
            }
            fn buffered(&mut self, _bits: &[u8]) {}
            fn restart_marker(&mut self, _index: u8) {}
        }

        let mut coder = Coder::new(
            Recorder {
                symbols: Vec::new(),
                bits: Vec::new(),
            },
            8,
        );
        coder.eob_run = 1;
        coder.flush_eob_run().expect("flush");
        assert_eq!(coder.sink.symbols, vec![0x00]);
        assert!(coder.sink.bits.is_empty(), "EOBRUN 1 sends no extra bits");

        coder.sink.symbols.clear();
        coder.eob_run = 5;
        coder.flush_eob_run().expect("flush");
        assert_eq!(coder.sink.symbols, vec![0x20]);
        assert_eq!(coder.sink.bits, vec![(5, 2)]);
    }

    #[test]
    fn dc_first_uses_an_arithmetic_shift() {
        struct Recorder {
            symbols: Vec<u8>,
        }
        impl PhuffSink for Recorder {
            fn select(&mut self, _slot: usize) {}
            fn symbol(&mut self, symbol: u8) -> Result<()> {
                self.symbols.push(symbol);
                Ok(())
            }
            fn bits(&mut self, _value: u32, _size: u32) {}
            fn buffered(&mut self, _bits: &[u8]) {}
            fn restart_marker(&mut self, _index: u8) {}
        }
        let mut block = [0i16; 64];
        block[0] = -3;
        let mut coder = Coder::new(
            Recorder {
                symbols: Vec::new(),
            },
            8,
        );
        // -3 >> 1 is -2 (floor), while -3 / 2 truncates to -1.
        let value = coder.dc_first(&block, 0, 1).expect("dc");
        assert_eq!(value, -2);
    }
}
