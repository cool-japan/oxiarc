//! Lossless predictive decoding (`SOF3`), ITU-T T.81 Annex H.
//!
//! A lossless frame has no quantisation tables, no 8x8 blocks and no AC
//! coefficients: each sample is the Huffman-coded difference between the
//! sample and a prediction formed from its already-reconstructed neighbours
//!
//! ```text
//!   Rc Rb
//!   Ra Rx
//! ```
//!
//! | `Psv` (`Ss`) | Prediction `Px` |
//! |---|---|
//! | 1 | `Ra` |
//! | 2 | `Rb` |
//! | 3 | `Rc` |
//! | 4 | `Ra + Rb - Rc` |
//! | 5 | `Ra + ((Rb - Rc) >> 1)` |
//! | 6 | `Rb + ((Ra - Rc) >> 1)` |
//! | 7 | `(Ra + Rb) >> 1` |
//!
//! `Psv = 0` selects the differential (hierarchical) process, which this crate
//! does not support and reports as
//! [`crate::UnsupportedFeature::LosslessPredictor`].
//!
//! Unlike the DCT processes, a lossless MCU is `Hi` x `Vi` **samples**
//! (T.81 A.2.3), so an interleaved lossless scan walks an eight-times finer
//! grid than a sequential DCT scan of the same frame does.

use super::planes::Planes;
use super::scan::{ScanOutcome, ScanTables, huffman_table, take_restart};
use crate::error::{JpegError, Result, TableKind, UnsupportedFeature};
use crate::frame::{FrameHeader, ScanHeader};
use crate::huffman::{BitReader, HuffmanTable};

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
        // `psv` is validated to be 1..=7 before this is reached.
        _ => (ra + rb) >> 1,
    }
}

/// Decode one Huffman-coded difference (T.81 H.1.2.2, table H.2).
///
/// `SSSS == 16` codes `DIFF = 32768` with no additional bits.
#[inline]
fn decode_difference(reader: &mut BitReader<'_>, table: &HuffmanTable, unit: u64) -> Result<i32> {
    let ssss = reader.decode(table, unit)?;
    match ssss {
        0 => Ok(0),
        16 => Ok(32_768),
        1..=15 => Ok(reader.receive_extend(u32::from(ssss))),
        _ => Err(JpegError::InvalidHuffmanCode { mcu: unit }),
    }
}

/// Decode a lossless scan into the sample planes.
///
/// Returns the number of entropy bytes consumed; the point transform is
/// applied to the components this scan covered before returning, because
/// prediction operates on untransformed values (T.81 H.1.2.1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_lossless(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let psv = scan.spectral_start;
    if !(1..=7).contains(&psv) {
        return Err(JpegError::Unsupported(
            UnsupportedFeature::LosslessPredictor(psv),
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
    let mut dc_tables = Vec::with_capacity(count);
    for k in 0..count {
        dc_tables.push(huffman_table(
            tables.dc,
            scan.dc_table[k],
            TableKind::DcHuffman,
        )?);
    }

    let default_prediction = 1i32 << (frame.precision - point_transform - 1);
    let mut reader = BitReader::new(entropy);
    let interleaved = scan.is_interleaved();

    // Sample extent this scan walks, per scan component.
    let mut extents = Vec::with_capacity(count);
    for &index in &scan.component_indices {
        let component = &frame.components[index];
        extents.push((
            component.width_samples as usize,
            component.height_samples as usize,
            u32::from(component.h) as usize,
            u32::from(component.v) as usize,
        ));
    }

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

    let mut units_to_restart = if restart_interval == 0 {
        u64::MAX
    } else {
        u64::from(restart_interval)
    };
    // The sample row in which the current restart interval began, per scan
    // component: T.81 H.1.2.1 makes that row behave as the first row of the
    // scan (default prediction for its first sample, `Ra` for the rest).
    let mut interval_first_row = [0usize; 4];
    let mut fresh_interval = true;
    let mut unit = 0u64;
    let mut failed: Option<JpegError> = None;
    let mut positions = [(0usize, 0usize); 16];

    'outer: while unit < units {
        if units_to_restart == 0 {
            if !take_restart(&mut reader) {
                if tolerate_truncated {
                    break;
                }
                return Err(JpegError::eof("restart marker"));
            }
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
                            positions[n] = (mcu_x * h + dx, mcu_y * v + dy);
                            n += 1;
                        }
                    }
                }
                n
            } else {
                positions[0] = ((unit % row_units) as usize, (unit / row_units) as usize);
                1
            };

            for &(x, y) in positions.iter().take(sample_count) {
                let diff = match decode_difference(&mut reader, dc_tables[k], unit) {
                    Ok(diff) => diff,
                    Err(err) => {
                        if !tolerate_truncated {
                            failed = Some(err);
                        }
                        break 'outer;
                    }
                };
                // Samples outside the component's real extent are MCU padding:
                // they are coded but discarded.
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

    if let Some(err) = failed {
        return Err(err);
    }

    let truncated = reader.fabricated_bits() > 0 || unit < units;
    if truncated && !tolerate_truncated {
        return Err(JpegError::eof("entropy-coded segment"));
    }

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

    reader.seek_marker();
    Ok(ScanOutcome {
        consumed: reader.byte_offset(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictor_table_matches_t81_table_h1() {
        let (ra, rb, rc) = (100, 40, 10);
        assert_eq!(predict(1, ra, rb, rc), 100);
        assert_eq!(predict(2, ra, rb, rc), 40);
        assert_eq!(predict(3, ra, rb, rc), 10);
        assert_eq!(predict(4, ra, rb, rc), 130);
        assert_eq!(predict(5, ra, rb, rc), 100 + ((40 - 10) >> 1));
        assert_eq!(predict(6, ra, rb, rc), 40 + ((100 - 10) >> 1));
        assert_eq!(predict(7, ra, rb, rc), 70);
    }

    /// The shifts in predictors 5-7 are arithmetic (floor), not truncating
    /// division: `(0 - 3) >> 1` is -2, not -1.
    #[test]
    fn predictor_shift_is_arithmetic() {
        assert_eq!(predict(5, 10, 0, 3), 8);
        assert_eq!(predict(6, 0, 10, 3), 8);
        assert_eq!(predict(7, 0, -3, 0), -2);
    }

    #[test]
    fn ssss_16_is_the_special_difference() {
        let mut bits = [0u8; 16];
        bits[0] = 1;
        let table = HuffmanTable::new(bits, vec![16]).expect("table");
        let data = [0u8; 2];
        let mut reader = BitReader::new(&data);
        assert_eq!(
            decode_difference(&mut reader, &table, 0).expect("diff"),
            32_768
        );
    }

    #[test]
    fn ssss_zero_is_a_zero_difference() {
        let mut bits = [0u8; 16];
        bits[0] = 1;
        let table = HuffmanTable::new(bits, vec![0]).expect("table");
        let data = [0u8; 2];
        let mut reader = BitReader::new(&data);
        assert_eq!(decode_difference(&mut reader, &table, 0).expect("diff"), 0);
    }

    #[test]
    fn ssss_above_16_is_an_error() {
        let mut bits = [0u8; 16];
        bits[0] = 1;
        let table = HuffmanTable::new(bits, vec![17]).expect("table");
        let data = [0u8; 2];
        let mut reader = BitReader::new(&data);
        assert!(matches!(
            decode_difference(&mut reader, &table, 5),
            Err(JpegError::InvalidHuffmanCode { mcu: 5 })
        ));
    }
}
