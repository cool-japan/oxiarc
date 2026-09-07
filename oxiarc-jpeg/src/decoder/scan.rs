//! Shared scan machinery and the sequential (`SOF0`/`SOF1`) entropy decoder.

use super::planes::Planes;
use crate::error::{JpegError, Result, TableKind};
use crate::frame::{Component, FrameHeader, ScanHeader};
use crate::huffman::{BitReader, HuffmanTable};
use crate::idct::idct_scaled_into;
use crate::quant::QuantTable;
use crate::tables::ZIGZAG_TO_NATURAL;

/// The tables a scan resolves its selectors against.
pub(crate) struct ScanTables<'a> {
    /// DC Huffman tables by `Th`.
    pub(crate) dc: &'a [Option<HuffmanTable>; 4],
    /// AC Huffman tables by `Th`.
    pub(crate) ac: &'a [Option<HuffmanTable>; 4],
    /// Quantisation tables by `Tq`.
    pub(crate) quant: &'a [Option<QuantTable>; 4],
}

/// Outcome of one scan: how many entropy bytes it consumed, and whether the
/// data ran out before the last unit.
pub(crate) struct ScanOutcome {
    /// Bytes of `entropy` the scan used, up to its terminating marker.
    pub(crate) consumed: usize,
    /// `true` when the decoder had to fabricate padding bits.
    pub(crate) truncated: bool,
}

/// How a scan walks the frame.
pub(crate) struct ScanGeometry {
    /// `true` when the scan interleaves more than one component.
    pub(crate) interleaved: bool,
    /// Total number of MCUs (interleaved) or blocks (non-interleaved).
    pub(crate) units: u64,
    /// MCUs per row, for the interleaved case.
    mcus_per_line: u32,
    /// Blocks per row of the single component, for the non-interleaved case.
    blocks_per_line: u32,
}

impl ScanGeometry {
    /// Derive the geometry of `scan` inside `frame`.
    pub(crate) fn new(frame: &FrameHeader, scan: &ScanHeader) -> Result<Self> {
        if scan.is_interleaved() {
            let units = u64::from(frame.mcus_per_line) * u64::from(frame.mcus_per_column);
            Ok(Self {
                interleaved: true,
                units,
                mcus_per_line: frame.mcus_per_line,
                blocks_per_line: 0,
            })
        } else {
            let index = *scan.component_indices.first().ok_or(JpegError::malformed(
                "SOS",
                0,
                "scan has no components",
            ))?;
            let component = &frame.components[index];
            let units =
                u64::from(component.blocks_per_line) * u64::from(component.blocks_per_column);
            Ok(Self {
                interleaved: false,
                units,
                mcus_per_line: 0,
                blocks_per_line: component.blocks_per_line,
            })
        }
    }

    /// Visit every block of one unit as `(scan component index, block column,
    /// block row)`.
    pub(crate) fn blocks_of_unit<F>(
        &self,
        frame: &FrameHeader,
        scan: &ScanHeader,
        unit: u64,
        mut visit: F,
    ) -> Result<()>
    where
        F: FnMut(usize, u32, u32) -> Result<()>,
    {
        if self.interleaved {
            let mcu_x = (unit % u64::from(self.mcus_per_line)) as u32;
            let mcu_y = (unit / u64::from(self.mcus_per_line)) as u32;
            for (k, &index) in scan.component_indices.iter().enumerate() {
                let component: &Component = &frame.components[index];
                for v in 0..u32::from(component.v) {
                    for h in 0..u32::from(component.h) {
                        visit(
                            k,
                            mcu_x * u32::from(component.h) + h,
                            mcu_y * u32::from(component.v) + v,
                        )?;
                    }
                }
            }
        } else {
            let bcol = (unit % u64::from(self.blocks_per_line)) as u32;
            let brow = (unit / u64::from(self.blocks_per_line)) as u32;
            visit(0, bcol, brow)?;
        }
        Ok(())
    }
}

/// Resolve one Huffman table selector.
pub(crate) fn huffman_table(
    slots: &[Option<HuffmanTable>; 4],
    index: u8,
    kind: TableKind,
) -> Result<&HuffmanTable> {
    slots
        .get(usize::from(index))
        .and_then(Option::as_ref)
        .ok_or(JpegError::UndefinedTable { kind, index })
}

/// Handle a restart marker between two intervals.
///
/// Returns `false` when the entropy data ended instead, which a tolerant
/// decode treats as a short scan and a strict decode rejects.
pub(crate) fn take_restart(reader: &mut BitReader<'_>) -> bool {
    match reader.seek_marker() {
        Some(code) if (0xD0..=0xD7).contains(&code) => {
            reader.consume_marker();
            true
        }
        _ => false,
    }
}

/// Decode one 8x8 block of a sequential scan into `block`.
fn decode_block(
    reader: &mut BitReader<'_>,
    dc_table: &HuffmanTable,
    ac_table: &HuffmanTable,
    prediction: &mut i32,
    block: &mut [i32; 64],
    unit: u64,
) -> Result<()> {
    block.fill(0);

    let t = reader.decode(dc_table, unit)?;
    let diff = if t == 0 {
        0
    } else {
        if t > 15 {
            return Err(JpegError::InvalidHuffmanCode { mcu: unit });
        }
        reader.receive_extend(u32::from(t))
    };
    *prediction = prediction.wrapping_add(diff);
    block[0] = *prediction;

    let mut k = 1usize;
    while k < 64 {
        let rs = reader.decode(ac_table, unit)?;
        let s = rs & 0x0F;
        let r = usize::from(rs >> 4);
        if s == 0 {
            if r != 15 {
                break;
            }
            k += 16;
        } else {
            k += r;
            if k > 63 {
                return Err(JpegError::InvalidHuffmanCode { mcu: unit });
            }
            block[ZIGZAG_TO_NATURAL[k]] = reader.receive_extend(u32::from(s));
            k += 1;
        }
    }
    Ok(())
}

/// Decode a sequential (`SOF0`/`SOF1`) scan straight into the sample planes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_sequential(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    let geometry = ScanGeometry::new(frame, scan)?;
    let count = scan.component_indices.len();

    let mut dc_tables = Vec::with_capacity(count);
    let mut ac_tables = Vec::with_capacity(count);
    let mut quant_tables = Vec::with_capacity(count);
    for (k, &index) in scan.component_indices.iter().enumerate() {
        dc_tables.push(huffman_table(
            tables.dc,
            scan.dc_table[k],
            TableKind::DcHuffman,
        )?);
        ac_tables.push(huffman_table(
            tables.ac,
            scan.ac_table[k],
            TableKind::AcHuffman,
        )?);
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
    let mut reader = BitReader::new(entropy);
    let mut prediction = [0i32; 4];
    let mut block = [0i32; 64];
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
            prediction = [0i32; 4];
            units_to_restart = u64::from(restart_interval);
        }
        units_to_restart -= 1;

        let unit_result = geometry.blocks_of_unit(frame, scan, unit, |k, bcol, brow| {
            let index = scan.component_indices[k];
            let output_size = planes.output_size(index);
            let size = usize::from(output_size);
            let stride = planes.stride(index);
            let offset =
                planes.offset(index) + brow as usize * size * stride + bcol as usize * size;
            decode_block(
                &mut reader,
                dc_tables[k],
                ac_tables[k],
                &mut prediction[k],
                &mut block,
                unit,
            )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{parse_sof, parse_sos};
    use crate::limits::DecodeLimits;

    fn frame(marker: u8, y: u16, x: u16, comps: &[(u8, u8, u8, u8)]) -> FrameHeader {
        let mut payload = vec![8u8];
        payload.extend_from_slice(&y.to_be_bytes());
        payload.extend_from_slice(&x.to_be_bytes());
        payload.push(comps.len() as u8);
        for &(id, h, v, tq) in comps {
            payload.push(id);
            payload.push((h << 4) | v);
            payload.push(tq);
        }
        parse_sof(marker, &payload, 0, &DecodeLimits::default()).expect("SOF")
    }

    #[test]
    fn interleaved_geometry_visits_every_block_of_an_mcu() {
        let frame = frame(0xC0, 32, 32, &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let scan =
            parse_sos(&[3u8, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0x00], 0, &frame).expect("SOS");
        let geometry = ScanGeometry::new(&frame, &scan).expect("geometry");
        assert!(geometry.interleaved);
        assert_eq!(geometry.units, 4);

        let mut visited = Vec::new();
        geometry
            .blocks_of_unit(&frame, &scan, 3, |k, bcol, brow| {
                visited.push((k, bcol, brow));
                Ok(())
            })
            .expect("visit");
        assert_eq!(
            visited,
            vec![
                (0, 2, 2),
                (0, 3, 2),
                (0, 2, 3),
                (0, 3, 3),
                (1, 1, 1),
                (2, 1, 1),
            ]
        );
    }

    #[test]
    fn non_interleaved_geometry_walks_the_unpadded_grid() {
        // 20x20 luma at 2x2 with 1x1 chroma: chroma is 10x10 samples => 2x2
        // blocks, while the padded grid would be 2x2 as well; use a size where
        // they differ.
        let frame = frame(0xC0, 20, 20, &[(1, 2, 2, 0), (2, 1, 1, 1)]);
        let scan = parse_sos(&[1u8, 2, 0x11, 0, 63, 0x00], 0, &frame).expect("SOS");
        let geometry = ScanGeometry::new(&frame, &scan).expect("geometry");
        assert!(!geometry.interleaved);
        // Chroma is ceil(20/2) = 10 samples => 2 blocks per line and column.
        assert_eq!(geometry.units, 4);

        let mut visited = Vec::new();
        geometry
            .blocks_of_unit(&frame, &scan, 3, |k, bcol, brow| {
                visited.push((k, bcol, brow));
                Ok(())
            })
            .expect("visit");
        assert_eq!(visited, vec![(0, 1, 1)]);
    }

    #[test]
    fn missing_tables_are_named_errors() {
        let slots: [Option<HuffmanTable>; 4] = [None, None, None, None];
        assert!(matches!(
            huffman_table(&slots, 2, TableKind::DcHuffman),
            Err(JpegError::UndefinedTable {
                kind: TableKind::DcHuffman,
                index: 2
            })
        ));
        assert!(matches!(
            huffman_table(&slots, 9, TableKind::AcHuffman),
            Err(JpegError::UndefinedTable { index: 9, .. })
        ));
    }

    #[test]
    fn take_restart_accepts_any_rst_index() {
        let data = [0xFFu8, 0xD5, 0x11];
        let mut reader = BitReader::new(&data);
        assert!(take_restart(&mut reader));
        assert_eq!(reader.byte_offset(), 2);

        let data = [0xFFu8, 0xD9];
        let mut reader = BitReader::new(&data);
        assert!(!take_restart(&mut reader), "EOI is not a restart");

        let mut reader = BitReader::new(&[]);
        assert!(!take_restart(&mut reader), "end of data is not a restart");
    }

    #[test]
    fn decode_block_rejects_a_run_past_the_block_end() {
        // A DC table that decodes `0` from bit `0`, and an AC table that
        // decodes 0xF1 (run 15, size 1) from bit `0`.
        let mut dc_bits = [0u8; 16];
        dc_bits[0] = 1;
        let dc = HuffmanTable::new(dc_bits, vec![0]).expect("dc");
        let mut ac_bits = [0u8; 16];
        ac_bits[0] = 1;
        let ac = HuffmanTable::new(ac_bits, vec![0xF1]).expect("ac");

        let data = [0u8; 32];
        let mut reader = BitReader::new(&data);
        let mut prediction = 0;
        let mut block = [0i32; 64];
        assert!(matches!(
            decode_block(&mut reader, &dc, &ac, &mut prediction, &mut block, 7),
            Err(JpegError::InvalidHuffmanCode { mcu: 7 })
        ));
    }
}
