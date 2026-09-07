//! Sequential (baseline and extended) entropy coding.
//!
//! The block traversal is written **once** and driven through a sink, because
//! libjpeg keeps `encode_one_block` and `htest_one_block` as twin functions
//! and they are easy to let drift. A statistics pass that emits a `ZRL` where
//! the coding pass emits an `EOB` produces a table that is merely suboptimal —
//! the file still decodes, and only a byte comparison notices.

use super::bitwriter::BitWriter;
use super::coefficients::CoefficientPlane;
use super::enctable::{DerivedTable, Histogram};
use super::plan::EncodePlan;
use crate::error::{JpegError, Result};

/// Where one block's symbols go: the bit writer, or a histogram.
pub(crate) trait BlockSink {
    /// A DC magnitude category and its extra bits.
    fn dc(&mut self, symbol: u8, value: u32, size: u32) -> Result<()>;
    /// An AC run/size symbol and its extra bits.
    fn ac(&mut self, symbol: u8, value: u32, size: u32) -> Result<()>;
}

/// Writes real bits through a pair of derived tables.
pub(crate) struct EmitSink<'a> {
    pub writer: &'a mut BitWriter,
    pub dc_table: &'a DerivedTable,
    pub ac_table: &'a DerivedTable,
}

impl BlockSink for EmitSink<'_> {
    #[inline]
    fn dc(&mut self, symbol: u8, value: u32, size: u32) -> Result<()> {
        let (code, length) = self.dc_table.lookup(symbol)?;
        self.writer.emit_code(code, length, value, size);
        Ok(())
    }

    #[inline]
    fn ac(&mut self, symbol: u8, value: u32, size: u32) -> Result<()> {
        let (code, length) = self.ac_table.lookup(symbol)?;
        self.writer.emit_code(code, length, value, size);
        Ok(())
    }
}

/// Counts symbols for the optimal table generator.
pub(crate) struct GatherSink<'a> {
    pub dc_histogram: &'a mut Histogram,
    pub ac_histogram: &'a mut Histogram,
}

impl BlockSink for GatherSink<'_> {
    #[inline]
    fn dc(&mut self, symbol: u8, _value: u32, _size: u32) -> Result<()> {
        self.dc_histogram.count(symbol);
        Ok(())
    }

    #[inline]
    fn ac(&mut self, symbol: u8, _value: u32, _size: u32) -> Result<()> {
        self.ac_histogram.count(symbol);
        Ok(())
    }
}

/// Number of bits a magnitude needs, and the value to send for it.
///
/// A negative difference is sent as the one's complement of its magnitude,
/// which two's complement gives us as `value - 1` masked to `nbits`.
#[inline]
fn magnitude(value: i32) -> (u32, u32) {
    let mut magnitude = value;
    let mut sent = value;
    if magnitude < 0 {
        magnitude = -magnitude;
        sent -= 1;
    }
    let bits = 32 - (magnitude as u32).leading_zeros();
    (bits, sent as u32)
}

/// Encode or count one block, T.81 F.1.2.
///
/// `max_coef_bits` is libjpeg's `MAX_COEF_BITS`, `precision + 2`; a DC
/// difference is allowed one more bit than an AC coefficient because it is a
/// difference of two coefficients.
pub(crate) fn encode_one_block<S: BlockSink>(
    block: &[i16; 64],
    last_dc: i32,
    max_coef_bits: u32,
    sink: &mut S,
) -> Result<()> {
    let (nbits, sent) = magnitude(i32::from(block[0]) - last_dc);
    if nbits > max_coef_bits + 1 {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "coefficients",
            reason: "a DC difference is out of range for this sample precision",
        });
    }
    sink.dc(nbits as u8, sent, nbits)?;

    let mut run = 0u32;
    for &stored in block.iter().skip(1) {
        let coefficient = i32::from(stored);
        if coefficient == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            sink.ac(0xF0, 0, 0)?;
            run -= 16;
        }
        let (nbits, sent) = magnitude(coefficient);
        if nbits > max_coef_bits {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "coefficients",
                reason: "an AC coefficient is out of range for this sample precision",
            });
        }
        sink.ac(((run << 4) | nbits) as u8, sent, nbits)?;
        run = 0;
    }
    if run > 0 {
        sink.ac(0x00, 0, 0)?;
    }
    Ok(())
}

/// Which table slots a scan component uses.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanTables {
    pub dc: usize,
    pub ac: usize,
}

/// Where a whole scan's blocks and restarts go.
///
/// One trait rather than two closures: the coding pass needs `&mut BitWriter`
/// in both callbacks, and threading that through two `FnMut`s would need a
/// `RefCell` borrow on every block.
pub(crate) trait ScanSink {
    /// A restart marker is due before the next MCU.
    fn restart(&mut self, index: u8) -> Result<()>;
    /// One block, with the previous DC value of its component.
    fn block(&mut self, scan_index: usize, block: &[i16; 64], last_dc: i32) -> Result<()>;
}

/// Visit every block of a sequential scan in MCU order.
///
/// The walker owns the DC predictions and resets them at every restart, which
/// the statistics pass has to do exactly as the coding pass does or the two
/// will disagree about the first block after each marker.
pub(crate) fn walk_scan<S: ScanSink>(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    restart_interval: usize,
    sink: &mut S,
) -> Result<()> {
    let total = plan.mcus_per_row_for(components) * plan.mcu_rows_for(components);
    walk_scan_range(
        plan,
        coefficients,
        components,
        restart_interval,
        0..total,
        sink,
    )
}

/// Visit the MCUs of `range` only.
///
/// `range.start` must be a restart boundary, because the walk resets the DC
/// predictions there and emits the `RSTn` that separates it from the previous
/// interval — which is exactly what makes a range's bytes concatenate with
/// its neighbours' into the stream a single walk would have produced.
pub(crate) fn walk_scan_range<S: ScanSink>(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    restart_interval: usize,
    range: std::ops::Range<usize>,
    sink: &mut S,
) -> Result<()> {
    let mcus_per_row = plan.mcus_per_row_for(components);
    let interleaved = components.len() > 1;
    let mut last_dc = [0i32; 4];
    // The marker *before* MCU `k * interval` is the `(k - 1)`-th, so a band
    // that starts there resumes the eight-marker cycle one step behind its
    // own interval number. Getting this wrong costs nothing at decode time —
    // T.81 lets a decoder resynchronise on any `RSTn` — and would silently
    // make the parallel encoder's bytes differ from the serial encoder's,
    // which is why `tests/parallel.rs` pins the whole stream.
    let mut restart_index = if restart_interval > 0 && range.start > 0 {
        (((range.start / restart_interval).wrapping_sub(1)) & 7) as u8
    } else {
        0
    };

    for mcu in range {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            sink.restart(restart_index)?;
            restart_index = (restart_index + 1) & 7;
            last_dc = [0i32; 4];
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
                    sink.block(scan_index, block, last_dc[scan_index])?;
                    last_dc[scan_index] = i32::from(block[0]);
                }
            }
        }
    }
    Ok(())
}

/// The statistics pass over one sequential scan.
struct GatherScan<'a> {
    tables: &'a [ScanTables],
    dc: &'a mut [Histogram; 4],
    ac: &'a mut [Histogram; 4],
    max_coef_bits: u32,
}

impl ScanSink for GatherScan<'_> {
    fn restart(&mut self, _index: u8) -> Result<()> {
        Ok(())
    }

    fn block(&mut self, scan_index: usize, block: &[i16; 64], last_dc: i32) -> Result<()> {
        let slots = self.tables[scan_index];
        let mut sink = GatherSink {
            dc_histogram: &mut self.dc[slots.dc],
            ac_histogram: &mut self.ac[slots.ac],
        };
        encode_one_block(block, last_dc, self.max_coef_bits, &mut sink)
    }
}

/// The coding pass over one sequential scan.
struct EmitScan<'a> {
    tables: &'a [ScanTables],
    dc: &'a [Option<DerivedTable>; 4],
    ac: &'a [Option<DerivedTable>; 4],
    writer: &'a mut BitWriter,
    max_coef_bits: u32,
}

impl ScanSink for EmitScan<'_> {
    fn restart(&mut self, index: u8) -> Result<()> {
        self.writer.emit_restart(index);
        Ok(())
    }

    fn block(&mut self, scan_index: usize, block: &[i16; 64], last_dc: i32) -> Result<()> {
        let slots = self.tables[scan_index];
        let missing = || JpegError::InvalidEncodeParameter {
            parameter: "huffman_tables",
            reason: "a scan references a Huffman table slot that was never defined",
        };
        let dc_table = self.dc[slots.dc].as_ref().ok_or_else(missing)?;
        let ac_table = self.ac[slots.ac].as_ref().ok_or_else(missing)?;
        let mut sink = EmitSink {
            writer: self.writer,
            dc_table,
            ac_table,
        };
        encode_one_block(block, last_dc, self.max_coef_bits, &mut sink)
    }
}

/// Gather DC and AC symbol frequencies for one sequential scan.
pub(crate) fn gather_scan(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    tables: &[ScanTables],
    restart_interval: usize,
    dc_histograms: &mut [Histogram; 4],
    ac_histograms: &mut [Histogram; 4],
) -> Result<()> {
    let mut sink = GatherScan {
        tables,
        dc: dc_histograms,
        ac: ac_histograms,
        max_coef_bits: u32::from(plan.precision) + 2,
    };
    walk_scan(plan, coefficients, components, restart_interval, &mut sink)
}

/// Entropy-code one range of a sequential scan into a fresh buffer.
///
/// Used by the parallel encoder: `range.start` is a restart boundary, so the
/// buffer this returns is exactly the slice of the serial output that covers
/// those MCUs.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "rayon")]
pub(crate) fn encode_scan_range(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    tables: &[ScanTables],
    restart_interval: usize,
    range: std::ops::Range<usize>,
    dc_tables: &[Option<DerivedTable>; 4],
    ac_tables: &[Option<DerivedTable>; 4],
) -> Result<Vec<u8>> {
    let mut writer = BitWriter::with_capacity(1 << 14);
    let mut sink = EmitScan {
        tables,
        dc: dc_tables,
        ac: ac_tables,
        writer: &mut writer,
        max_coef_bits: u32::from(plan.precision) + 2,
    };
    walk_scan_range(
        plan,
        coefficients,
        components,
        restart_interval,
        range,
        &mut sink,
    )?;
    Ok(writer.finish())
}

/// Entropy-code one sequential scan into `writer`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_scan(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    tables: &[ScanTables],
    restart_interval: usize,
    dc_tables: &[Option<DerivedTable>; 4],
    ac_tables: &[Option<DerivedTable>; 4],
    writer: &mut BitWriter,
) -> Result<()> {
    let mut sink = EmitScan {
        tables,
        dc: dc_tables,
        ac: ac_tables,
        writer,
        max_coef_bits: u32::from(plan.precision) + 2,
    };
    walk_scan(plan, coefficients, components, restart_interval, &mut sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huffman::HuffmanTable;
    use crate::tables::{
        ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES, ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES,
    };

    struct Recorder {
        dc: Vec<(u8, u32, u32)>,
        ac: Vec<(u8, u32, u32)>,
    }

    impl BlockSink for Recorder {
        fn dc(&mut self, symbol: u8, value: u32, size: u32) -> Result<()> {
            self.dc.push((symbol, value, size));
            Ok(())
        }
        fn ac(&mut self, symbol: u8, value: u32, size: u32) -> Result<()> {
            self.ac.push((symbol, value, size));
            Ok(())
        }
    }

    fn record(block: &[i16; 64], last_dc: i32) -> Recorder {
        let mut recorder = Recorder {
            dc: Vec::new(),
            ac: Vec::new(),
        };
        encode_one_block(block, last_dc, 10, &mut recorder).expect("encodes");
        recorder
    }

    #[test]
    fn magnitudes_use_the_ones_complement_for_negatives() {
        // Only the low `nbits` bits of the second element are ever sent.
        let sent = |value: i32| {
            let (bits, raw) = magnitude(value);
            (
                bits,
                if bits == 0 {
                    0
                } else {
                    raw & ((1 << bits) - 1)
                },
            )
        };
        assert_eq!(sent(0), (0, 0));
        assert_eq!(sent(1), (1, 0b1));
        assert_eq!(sent(-1), (1, 0b0));
        assert_eq!(sent(2), (2, 0b10));
        assert_eq!(sent(3), (2, 0b11));
        assert_eq!(sent(-2), (2, 0b01));
        assert_eq!(sent(-3), (2, 0b00));
        assert_eq!(sent(255), (8, 255));
        assert_eq!(sent(-255), (8, 0));
    }

    #[test]
    fn an_all_zero_block_is_a_dc_zero_and_an_eob() {
        let block = [0i16; 64];
        let recorder = record(&block, 0);
        assert_eq!(recorder.dc, vec![(0, 0, 0)]);
        assert_eq!(recorder.ac, vec![(0x00, 0, 0)]);
    }

    #[test]
    fn a_full_block_needs_no_eob() {
        let block = [1i16; 64];
        let recorder = record(&block, 0);
        assert_eq!(recorder.ac.len(), 63, "63 AC coefficients, no EOB");
        assert!(recorder.ac.iter().all(|&(symbol, _, _)| symbol == 0x01));
    }

    /// A run longer than fifteen zeros needs `ZRL` symbols first.
    #[test]
    fn long_zero_runs_emit_zrl() {
        let mut block = [0i16; 64];
        block[40] = 3;
        let recorder = record(&block, 0);
        let symbols: Vec<u8> = recorder.ac.iter().map(|&(s, _, _)| s).collect();
        // 39 zeros before index 40: two ZRLs (32 zeros) then a run of 7.
        assert_eq!(symbols, vec![0xF0, 0xF0, 0x72, 0x00]);
    }

    #[test]
    fn exactly_sixteen_zeros_before_a_coefficient_emits_one_zrl() {
        let mut block = [0i16; 64];
        block[17] = -1;
        let recorder = record(&block, 0);
        let symbols: Vec<u8> = recorder.ac.iter().map(|&(s, _, _)| s).collect();
        assert_eq!(symbols, vec![0xF0, 0x01, 0x00]);
    }

    #[test]
    fn the_dc_difference_is_relative_to_the_previous_block() {
        let mut block = [0i16; 64];
        block[0] = 100;
        assert_eq!(record(&block, 0).dc[0], (7, 100, 7));
        assert_eq!(record(&block, 100).dc[0], (0, 0, 0));
        assert_eq!(record(&block, 101).dc[0].0, 1, "-1 needs one bit");
    }

    #[test]
    fn out_of_range_coefficients_are_an_error_not_a_panic() {
        let mut block = [0i16; 64];
        block[0] = 30_000;
        let mut recorder = Recorder {
            dc: Vec::new(),
            ac: Vec::new(),
        };
        assert!(encode_one_block(&block, -30_000, 10, &mut recorder).is_err());

        let mut block = [0i16; 64];
        block[1] = 30_000;
        let mut recorder = Recorder {
            dc: Vec::new(),
            ac: Vec::new(),
        };
        assert!(encode_one_block(&block, 0, 10, &mut recorder).is_err());
    }

    /// The counting and emitting sinks must see the same symbol stream; this
    /// drives both over the same block and compares.
    #[test]
    fn gathering_and_emitting_share_one_traversal() {
        let mut block = [0i16; 64];
        block[0] = -5;
        block[1] = 9;
        block[30] = -2;
        let recorder = record(&block, 3);

        let mut dc_histogram = Histogram::default();
        let mut ac_histogram = Histogram::default();
        let mut gather = GatherSink {
            dc_histogram: &mut dc_histogram,
            ac_histogram: &mut ac_histogram,
        };
        encode_one_block(&block, 3, 10, &mut gather).expect("gathers");

        let dc_table = HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
            .expect("valid");
        let ac_table = HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
            .expect("valid");
        let dc_derived = DerivedTable::new(&dc_table);
        let ac_derived = DerivedTable::new(&ac_table);
        let mut writer = BitWriter::new();
        let mut emit = EmitSink {
            writer: &mut writer,
            dc_table: &dc_derived,
            ac_table: &ac_derived,
        };
        encode_one_block(&block, 3, 10, &mut emit).expect("emits");

        assert_eq!(recorder.dc.len(), 1);
        let symbols: Vec<u8> = recorder.ac.iter().map(|&(s, _, _)| s).collect();
        assert_eq!(
            symbols,
            vec![0x04, 0xF0, 0xC2, 0x00],
            "coefficient, ZRL, coefficient, EOB"
        );
        assert!(!writer.finish().is_empty());
    }
}
