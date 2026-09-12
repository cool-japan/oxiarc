//! Progressive (`SOF2`) scan decoding, ITU-T T.81 Annex G.
//!
//! Four scan kinds share one coefficient buffer per component: DC first, DC
//! refine, AC first and AC refine. The AC refinement procedure (G.1.2.3) is
//! transcribed from libjpeg's `decode_mcu_AC_refine` because its three-way
//! interaction between the correction bits, the zero run and the end-of-band
//! run is the classic source of silent corruption in progressive decoders —
//! correction bits are appended to already-nonzero coefficients even inside an
//! EOB run, and a newly nonzero coefficient is written *after* that walk.

use super::planes::Planes;
use super::scan::{ScanGeometry, ScanOutcome, ScanTables, huffman_table, take_restart};
use crate::error::{JpegError, LimitKind, Result, TableKind};
use crate::frame::{FrameHeader, ScanHeader};
use crate::huffman::BitReader;
use crate::idct::idct_scaled_into;
use crate::limits::{DecodeLimits, checked_product3};
use crate::tables::ZIGZAG_TO_NATURAL;

/// One coefficient buffer per component.
#[derive(Debug, Default)]
pub(crate) struct Coefficients {
    planes: Vec<Vec<i32>>,
    strides: Vec<usize>,
}

impl Coefficients {
    /// Allocate one buffer per component, checked against
    /// [`DecodeLimits::max_coefficient_bytes`].
    ///
    /// Rows are the MCU-padded block count so that an interleaved DC scan and
    /// a non-interleaved AC scan address the same block identically.
    pub(crate) fn allocate(frame: &FrameHeader, limits: &DecodeLimits) -> Result<Self> {
        let mut planes = Vec::with_capacity(frame.components.len());
        let mut strides = Vec::with_capacity(frame.components.len());
        let mut total: u64 = 0;
        for component in &frame.components {
            let entries = checked_product3(
                u64::from(component.blocks_per_line_padded),
                u64::from(component.blocks_per_column_padded),
                64,
                LimitKind::CoefficientMemory,
            )?;
            total = total
                .checked_add(entries.saturating_mul(4))
                .ok_or(JpegError::LimitExceeded(LimitKind::CoefficientMemory))?;
            limits.check_coefficient_bytes(total)?;
            let entries = usize::try_from(entries)
                .map_err(|_| JpegError::LimitExceeded(LimitKind::CoefficientMemory))?;
            planes.push(vec![0i32; entries]);
            strides.push(component.blocks_per_line_padded as usize);
        }
        Ok(Self { planes, strides })
    }

    /// One block of component `index`, or `None` when the coordinates fall
    /// outside the padded grid (which happens for the trailing blocks of a
    /// non-interleaved scan and is not an error).
    #[cfg(feature = "arithmetic")]
    pub(crate) fn block_mut(&mut self, index: usize, bcol: u32, brow: u32) -> Option<&mut [i32]> {
        let offset = self.block_offset(index, bcol, brow)?;
        Some(&mut self.planes[index][offset..offset + 64])
    }

    /// Offset of one block inside component `index`'s buffer.
    fn block_offset(&self, index: usize, bcol: u32, brow: u32) -> Option<usize> {
        let stride = self.strides[index];
        if bcol as usize >= stride {
            return None;
        }
        let start = (brow as usize)
            .checked_mul(stride)?
            .checked_add(bcol as usize)?
            .checked_mul(64)?;
        if start + 64 <= self.planes[index].len() {
            Some(start)
        } else {
            None
        }
    }
}

/// Decode one progressive scan into the coefficient buffers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_progressive(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    restart_interval: u16,
    entropy: &[u8],
    coefficients: &mut Coefficients,
    predictions: &mut [i32; 4],
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let geometry = ScanGeometry::new(frame, scan)?;
    let mut reader = BitReader::new(entropy);
    let mut eob_run = 0u32;
    predictions.fill(0);

    let mut units_to_restart = if restart_interval == 0 {
        u64::MAX
    } else {
        u64::from(restart_interval)
    };
    let mut unit = 0u64;
    while unit < geometry.units {
        if units_to_restart == 0 {
            if !take_restart(&mut reader) {
                if tolerate_truncated {
                    break;
                }
                return Err(JpegError::eof("restart marker"));
            }
            predictions.fill(0);
            eob_run = 0;
            units_to_restart = u64::from(restart_interval);
        }
        units_to_restart -= 1;

        let unit_result = geometry.blocks_of_unit(frame, scan, unit, |k, bcol, brow| {
            let index = scan.component_indices[k];
            let Some(offset) = coefficients.block_offset(index, bcol, brow) else {
                return Ok(());
            };
            let block: &mut [i32] = &mut coefficients.planes[index][offset..offset + 64];
            if scan.spectral_start == 0 {
                if scan.approx_high == 0 {
                    let table = huffman_table(tables.dc, scan.dc_table[k], TableKind::DcHuffman)?;
                    let t = reader.decode(table, unit)?;
                    if t > 15 {
                        return Err(JpegError::InvalidHuffmanCode { mcu: unit });
                    }
                    let diff = reader.receive_extend(u32::from(t));
                    predictions[k] = predictions[k].wrapping_add(diff);
                    block[0] = predictions[k] << scan.approx_low;
                } else if reader.get_bit() != 0 {
                    block[0] |= 1 << scan.approx_low;
                }
                return Ok(());
            }

            let table = huffman_table(tables.ac, scan.ac_table[k], TableKind::AcHuffman)?;
            if scan.approx_high == 0 {
                decode_ac_first(&mut reader, table, block, scan, &mut eob_run, unit)
            } else {
                decode_ac_refine(&mut reader, table, block, scan, &mut eob_run, unit)
            }
        });
        if let Err(err) = unit_result {
            if tolerate_truncated {
                break;
            }
            return Err(err);
        }
        unit += 1;
    }

    let truncated = reader.fabricated_bits() > 0 || unit < geometry.units;
    if truncated && !tolerate_truncated {
        return Err(JpegError::eof("entropy-coded segment"));
    }
    reader.seek_marker();
    Ok(ScanOutcome {
        consumed: reader.byte_offset(),
        truncated,
    })
}

/// Annex G.1.2.2: first AC scan of a band.
fn decode_ac_first(
    reader: &mut BitReader<'_>,
    table: &crate::huffman::HuffmanTable,
    block: &mut [i32],
    scan: &ScanHeader,
    eob_run: &mut u32,
    unit: u64,
) -> Result<()> {
    if *eob_run > 0 {
        *eob_run -= 1;
        return Ok(());
    }
    let mut k = usize::from(scan.spectral_start);
    let end = usize::from(scan.spectral_end);
    while k <= end {
        let rs = reader.decode(table, unit)?;
        let s = rs & 0x0F;
        let r = usize::from(rs >> 4);
        if s == 0 {
            if r != 15 {
                *eob_run = 1u32 << r;
                if r > 0 {
                    *eob_run += reader.get_bits(r as u32);
                }
                *eob_run -= 1;
                break;
            }
            k += 16;
        } else {
            k += r;
            if k > end || k > 63 {
                return Err(JpegError::InvalidHuffmanCode { mcu: unit });
            }
            block[ZIGZAG_TO_NATURAL[k]] = reader.receive_extend(u32::from(s)) << scan.approx_low;
            k += 1;
        }
    }
    Ok(())
}

/// Annex G.1.2.3: refinement AC scan of a band.
fn decode_ac_refine(
    reader: &mut BitReader<'_>,
    table: &crate::huffman::HuffmanTable,
    block: &mut [i32],
    scan: &ScanHeader,
    eob_run: &mut u32,
    unit: u64,
) -> Result<()> {
    let p1 = 1i32 << scan.approx_low;
    let m1 = -1i32 << scan.approx_low;
    let start = usize::from(scan.spectral_start);
    let end = usize::from(scan.spectral_end);
    let mut k = start;

    if *eob_run == 0 {
        while k <= end {
            let rs = reader.decode(table, unit)?;
            let s = rs & 0x0F;
            let mut r = i32::from(rs >> 4);
            let mut newly_nonzero = 0i32;
            if s != 0 {
                // The size of a newly nonzero coefficient is always 1.
                if s != 1 {
                    return Err(JpegError::InvalidHuffmanCode { mcu: unit });
                }
                newly_nonzero = if reader.get_bit() != 0 { p1 } else { m1 };
            } else if r != 15 {
                *eob_run = 1u32 << r;
                if r > 0 {
                    *eob_run += reader.get_bits(r as u32);
                }
                break;
            }

            // Walk forward over already-nonzero coefficients, appending a
            // correction bit to each, and over `r` still-zero coefficients.
            loop {
                let position = ZIGZAG_TO_NATURAL[k];
                if block[position] != 0 {
                    if reader.get_bit() != 0 && (block[position] & p1) == 0 {
                        if block[position] >= 0 {
                            block[position] += p1;
                        } else {
                            block[position] += m1;
                        }
                    }
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
                if k > end {
                    break;
                }
            }

            if newly_nonzero != 0 {
                if k > end || k > 63 {
                    return Err(JpegError::InvalidHuffmanCode { mcu: unit });
                }
                block[ZIGZAG_TO_NATURAL[k]] = newly_nonzero;
            }
            k += 1;
        }
    }

    if *eob_run > 0 {
        while k <= end {
            let position = ZIGZAG_TO_NATURAL[k];
            if block[position] != 0 && reader.get_bit() != 0 && (block[position] & p1) == 0 {
                if block[position] >= 0 {
                    block[position] += p1;
                } else {
                    block[position] += m1;
                }
            }
            k += 1;
        }
        *eob_run -= 1;
    }
    Ok(())
}

/// Dequantise every block and run the inverse DCT into the sample planes.
pub(crate) fn render_coefficients(
    frame: &FrameHeader,
    tables: &ScanTables<'_>,
    coefficients: &Coefficients,
    planes: &mut Planes,
) -> Result<()> {
    let center = 1i32 << (frame.precision - 1);
    let maxval = (1i32 << frame.precision) - 1;
    let mut block = [0i32; 64];

    for (index, component) in frame.components.iter().enumerate() {
        let tq = component.quant_table;
        let quant = tables
            .quant
            .get(usize::from(tq))
            .and_then(Option::as_ref)
            .ok_or(JpegError::UndefinedTable {
                kind: TableKind::Quantisation,
                index: tq,
            })?;
        let stride = planes.stride(index);
        let base = planes.offset(index);
        let output_size = planes.output_size(index);
        let size = usize::from(output_size);
        let block_stride = coefficients.strides[index];
        let rows = component.blocks_per_column_padded as usize;
        for brow in 0..rows {
            for bcol in 0..block_stride {
                let offset = (brow * block_stride + bcol) * 64;
                if offset + 64 > coefficients.planes[index].len() {
                    continue;
                }
                block.copy_from_slice(&coefficients.planes[index][offset..offset + 64]);
                idct_scaled_into(
                    &block,
                    quant.natural(),
                    planes.data_mut(),
                    base + brow * size * stride + bcol * size,
                    stride,
                    center,
                    maxval,
                    output_size,
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{parse_sof, parse_sos};
    use crate::huffman::HuffmanTable;

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
        parse_sof(0xC2, &payload, 0, &DecodeLimits::default()).expect("SOF2")
    }

    fn scan_header(spec: &[u8], frame: &FrameHeader) -> ScanHeader {
        parse_sos(spec, 0, frame).expect("SOS")
    }

    #[test]
    fn coefficient_buffers_are_mcu_padded() {
        let frame = frame(20, 20, &[(1, 2, 2, 0), (2, 1, 1, 1)]);
        let coefs = Coefficients::allocate(&frame, &DecodeLimits::default()).expect("allocate");
        // Luma: 2 MCUs across x 2 blocks = 4 blocks per line.
        assert_eq!(coefs.strides[0], 4);
        assert_eq!(coefs.planes[0].len(), 4 * 4 * 64);
        assert_eq!(coefs.strides[1], 2);
    }

    #[test]
    fn coefficient_budget_is_enforced() {
        let frame = frame(4000, 4000, &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0)]);
        let limits = DecodeLimits {
            max_coefficient_bytes: 1 << 16,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            Coefficients::allocate(&frame, &limits),
            Err(JpegError::LimitExceeded(LimitKind::CoefficientMemory))
        ));
    }

    #[test]
    fn block_offset_rejects_out_of_range_coordinates() {
        let frame = frame(16, 16, &[(1, 1, 1, 0)]);
        let coefs = Coefficients::allocate(&frame, &DecodeLimits::default()).expect("allocate");
        assert_eq!(coefs.block_offset(0, 0, 0), Some(0));
        assert_eq!(coefs.block_offset(0, 1, 1), Some(3 * 64));
        assert_eq!(coefs.block_offset(0, 2, 0), None);
        assert_eq!(coefs.block_offset(0, 0, 99), None);
    }

    /// One AC-first band with an explicit EOB run of length four: the first
    /// block consumes the run and the next three are skipped.
    #[test]
    fn ac_first_eob_run_spans_blocks() {
        let frame = frame(8, 8, &[(1, 1, 1, 0)]);
        let scan = scan_header(&[1u8, 1, 0x00, 1, 63, 0x00], &frame);
        // A one-bit AC table where code `0` is symbol 0x20 (r = 2, s = 0),
        // i.e. EOBr with r = 2 => run = 4 + two appended bits.
        let mut bits = [0u8; 16];
        bits[0] = 1;
        let table = HuffmanTable::new(bits, vec![0x20]).expect("table");

        // Bits: `0` (the EOB symbol) then `00` (appended) => run = 4.
        let data = [0b0000_0000u8];
        let mut reader = BitReader::new(&data);
        let mut block = [0i32; 64];
        let mut eob_run = 0u32;
        decode_ac_first(&mut reader, &table, &mut block, &scan, &mut eob_run, 0)
            .expect("first block");
        assert_eq!(eob_run, 3);
        for _ in 0..3 {
            decode_ac_first(&mut reader, &table, &mut block, &scan, &mut eob_run, 0)
                .expect("skipped block");
        }
        assert_eq!(eob_run, 0);
        assert!(block.iter().all(|&c| c == 0));
    }

    /// The refinement pass appends a correction bit to an already-nonzero
    /// coefficient and leaves a coefficient that is already at the target bit
    /// alone.
    #[test]
    fn ac_refine_appends_correction_bits() {
        let frame = frame(8, 8, &[(1, 1, 1, 0)]);
        // Ss = 1, Se = 2, Ah = 1, Al = 0.
        let scan = scan_header(&[1u8, 1, 0x00, 1, 2, 0x10], &frame);
        let mut bits = [0u8; 16];
        bits[0] = 1;
        // Symbol 0x00 = EOBr with r = 0 => EOB run of one block.
        let table = HuffmanTable::new(bits, vec![0x00]).expect("table");

        let mut block = [0i32; 64];
        // Bit 0 of each coefficient must be clear, otherwise libjpeg's
        // "already set it" guard suppresses the increment.
        block[ZIGZAG_TO_NATURAL[1]] = 4;
        block[ZIGZAG_TO_NATURAL[2]] = -4;

        // Bit stream: `0` selects the EOB symbol, then one correction bit per
        // nonzero coefficient in the band: `1` then `1`.
        let data = [0b0110_0000u8];
        let mut reader = BitReader::new(&data);
        let mut eob_run = 0u32;
        decode_ac_refine(&mut reader, &table, &mut block, &scan, &mut eob_run, 0).expect("refine");
        assert_eq!(block[ZIGZAG_TO_NATURAL[1]], 5, "positive grows by +1");
        assert_eq!(block[ZIGZAG_TO_NATURAL[2]], -5, "negative grows by -1");
        assert_eq!(eob_run, 0);

        // A coefficient that already carries the bit is left alone, which is
        // what stops a refinement pass from double-counting.
        let mut block = [0i32; 64];
        block[ZIGZAG_TO_NATURAL[1]] = 5;
        block[ZIGZAG_TO_NATURAL[2]] = -5;
        let mut reader = BitReader::new(&data);
        let mut eob_run = 0u32;
        decode_ac_refine(&mut reader, &table, &mut block, &scan, &mut eob_run, 0).expect("refine");
        assert_eq!(block[ZIGZAG_TO_NATURAL[1]], 5);
        assert_eq!(block[ZIGZAG_TO_NATURAL[2]], -5);
    }

    /// A newly nonzero coefficient is written after the walk, at the position
    /// the zero run landed on.
    #[test]
    fn ac_refine_places_a_new_coefficient_after_the_zero_run() {
        let frame = frame(8, 8, &[(1, 1, 1, 0)]);
        let scan = scan_header(&[1u8, 1, 0x00, 1, 4, 0x10], &frame);
        let mut bits = [0u8; 16];
        bits[0] = 1;
        // Symbol 0x11: r = 1 (skip one zero), s = 1 (new coefficient).
        let table = HuffmanTable::new(bits, vec![0x11]).expect("table");

        let mut block = [0i32; 64];
        // Bits: `0` symbol, `1` sign (positive), then the walk needs no
        // correction bits because every coefficient in the run is zero.
        let data = [0b0100_0000u8];
        let mut reader = BitReader::new(&data);
        let mut eob_run = 0u32;
        // A single symbol then the band ends; tolerate the fabricated tail.
        let _ = decode_ac_refine(&mut reader, &table, &mut block, &scan, &mut eob_run, 0);
        assert_eq!(block[ZIGZAG_TO_NATURAL[2]], 1, "new coefficient at k = 2");
        assert_eq!(block[ZIGZAG_TO_NATURAL[1]], 0, "run position stays zero");
    }

    #[test]
    fn ac_refine_rejects_a_size_other_than_one() {
        let frame = frame(8, 8, &[(1, 1, 1, 0)]);
        let scan = scan_header(&[1u8, 1, 0x00, 1, 63, 0x10], &frame);
        let mut bits = [0u8; 16];
        bits[0] = 1;
        let table = HuffmanTable::new(bits, vec![0x02]).expect("table");
        let mut block = [0i32; 64];
        let data = [0u8; 4];
        let mut reader = BitReader::new(&data);
        let mut eob_run = 0u32;
        assert!(decode_ac_refine(&mut reader, &table, &mut block, &scan, &mut eob_run, 3).is_err());
    }

    #[test]
    fn ac_first_rejects_a_run_past_the_band() {
        let frame = frame(8, 8, &[(1, 1, 1, 0)]);
        let scan = scan_header(&[1u8, 1, 0x00, 1, 5, 0x00], &frame);
        let mut bits = [0u8; 16];
        bits[0] = 1;
        // r = 15, s = 1 skips past Se = 5.
        let table = HuffmanTable::new(bits, vec![0xF1]).expect("table");
        let mut block = [0i32; 64];
        let data = [0u8; 4];
        let mut reader = BitReader::new(&data);
        let mut eob_run = 0u32;
        assert!(decode_ac_first(&mut reader, &table, &mut block, &scan, &mut eob_run, 1).is_err());
    }
}
