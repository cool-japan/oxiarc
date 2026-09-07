//! The marker loop that ties parsing, entropy decoding and output together.

use super::lossless::decode_lossless;
use super::output::OutputPlan;
use super::planes::Planes;
use super::progressive::{Coefficients, decode_progressive, render_coefficients};
use super::scan::{ScanTables, decode_sequential};
use super::{ComponentInfo, DecodeOptions, ImageInfo, Upsampling};
use crate::color::{default_output_color_space, guess_input_color_space};
use crate::error::{JpegError, LimitKind, Result, UnsupportedFeature};
use crate::frame::{
    CodingProcess, EntropyCoding, FrameHeader, parse_sof, parse_sos, validate_dct_scan,
};
use crate::huffman::parse_dht;
use crate::metadata::Metadata;
use crate::parser::Scanner;
use crate::quant::parse_dqt;
use crate::tableset::TableSet;

/// Skip an entropy-coded segment, returning the offset of the `0xFF` that
/// starts the next non-restart marker (or the end of `data`).
fn skip_entropy(data: &[u8], mut pos: usize) -> usize {
    while pos < data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let mut probe = pos + 1;
        while data.get(probe) == Some(&0xFF) {
            probe += 1;
        }
        match data.get(probe) {
            None => return data.len(),
            Some(0) => pos = probe + 1,
            Some(&code) if (0xD0..=0xD7).contains(&code) => pos = probe + 1,
            Some(_) => return pos,
        }
    }
    data.len()
}

/// Look ahead for the `DNL` segment that resolves a `SOF` with `Y == 0`.
fn find_dnl(data: &[u8], from: usize) -> Option<u16> {
    let mut scanner = Scanner::new(data);
    scanner.seek(from);
    loop {
        let segment = scanner.next_segment().ok()??;
        match segment.code {
            0xDC => {
                if segment.payload.len() < 2 {
                    return None;
                }
                return Some(u16::from_be_bytes([segment.payload[0], segment.payload[1]]));
            }
            0xD9 => return None,
            0xDA => {
                let next = skip_entropy(data, scanner.position());
                scanner.seek(next);
            }
            _ => {}
        }
    }
}

/// One decode in progress.
pub(crate) struct Engine {
    pub(crate) options: DecodeOptions,
    pub(crate) tables: TableSet,
    pub(crate) frame: Option<FrameHeader>,
    pub(crate) metadata: Metadata,
    pub(crate) info: Option<ImageInfo>,
    planes: Planes,
    coefficients: Coefficients,
    scans: u32,
    header_end: usize,
    decoded: bool,
    /// `true` when a scan ended before all its units were decoded.
    pub(crate) truncated: bool,
}

impl Engine {
    /// A fresh engine with the given options.
    pub(crate) fn new(options: DecodeOptions) -> Self {
        Self {
            options,
            tables: TableSet::default(),
            frame: None,
            metadata: Metadata::default(),
            info: None,
            planes: Planes::default(),
            coefficients: Coefficients::default(),
            scans: 0,
            header_end: 0,
            decoded: false,
            truncated: false,
        }
    }

    /// Prime the engine with out-of-band tables.
    ///
    /// Tables loaded this way are overridden by any the datastream itself
    /// carries, which is what T.81's abbreviated-format rules require.
    pub(crate) fn load_tables(&mut self, tables: &TableSet) {
        self.tables.merge_from(tables);
    }

    /// Parse markers up to and including the first `SOF`.
    pub(crate) fn read_headers(&mut self, data: &[u8]) -> Result<ImageInfo> {
        if let Some(info) = self.info {
            return Ok(info);
        }
        let mut scanner = Scanner::new(data);
        while let Some(segment) = scanner.next_segment()? {
            match segment.code {
                0xD8 => {}
                0xD9 => break,
                0xDA => break,
                0xC4 | 0xCC | 0xDB | 0xDD => {
                    self.apply_table_segment(segment.code, segment.payload, segment.offset)?
                }
                0xFE | 0xE0..=0xEF => self.metadata.push(segment.code, segment.payload),
                0xDE | 0xDF => {
                    return Err(JpegError::Unsupported(UnsupportedFeature::Hierarchical));
                }
                code if (0xC0..=0xCF).contains(&code) => {
                    let mut frame =
                        parse_sof(code, segment.payload, segment.offset, &self.options.limits)?;
                    if frame.height == 0 {
                        let height =
                            find_dnl(data, scanner.position()).ok_or(JpegError::MissingDnl)?;
                        if height == 0 {
                            return Err(JpegError::MissingDnl);
                        }
                        self.options
                            .limits
                            .check_dimensions(u32::from(frame.width), u32::from(height))?;
                        frame.set_height(height);
                    }
                    self.header_end = scanner.position();
                    let info = self.build_info(&frame);
                    self.frame = Some(frame);
                    self.info = Some(info);
                    return Ok(info);
                }
                _ => {}
            }
        }
        Err(JpegError::AbbreviatedWithoutFrame)
    }

    /// Apply one `DQT`, `DHT`, `DAC` or `DRI` segment.
    fn apply_table_segment(&mut self, code: u8, payload: &[u8], offset: usize) -> Result<()> {
        match code {
            0xDB => parse_dqt(payload, offset, &mut self.tables.quant),
            0xC4 => parse_dht(
                payload,
                offset,
                &mut self.tables.dc_huffman,
                &mut self.tables.ac_huffman,
            ),
            0xCC => self.tables.arithmetic.parse(payload, offset),
            _ => {
                if payload.len() < 2 {
                    return Err(JpegError::malformed("DRI", offset, "segment too short"));
                }
                self.tables.restart_interval = Some(u16::from_be_bytes([payload[0], payload[1]]));
                Ok(())
            }
        }
    }

    /// Build the public [`ImageInfo`] for a parsed frame.
    fn build_info(&self, frame: &FrameHeader) -> ImageInfo {
        let ids: Vec<u8> = frame.components.iter().map(|c| c.id).collect();
        let adobe_transform = self.metadata.adobe.map(|a| a.transform);
        let input = guess_input_color_space(&ids, self.metadata.jfif.is_some(), adobe_transform);
        let output = if self.options.raw_components {
            input
        } else {
            self.options
                .output_color_space
                .unwrap_or_else(|| default_output_color_space(input))
        };

        let mut components = [ComponentInfo {
            id: 0,
            h: 1,
            v: 1,
            quant_table: 0,
        }; 4];
        for (slot, component) in components.iter_mut().zip(frame.components.iter()) {
            *slot = ComponentInfo {
                id: component.id,
                h: component.h,
                v: component.v,
                quant_table: component.quant_table,
            };
        }

        ImageInfo {
            width: frame.width,
            height: frame.height,
            precision: frame.precision,
            num_components: frame.components.len() as u8,
            components,
            input_color_space: input,
            output_color_space: output,
            process: frame.process,
            entropy: frame.entropy,
            adobe_transform,
            has_jfif: self.metadata.jfif.is_some(),
            has_adobe: self.metadata.adobe.is_some(),
            restart_interval: self.tables.restart_interval.unwrap_or(0),
            subsampling: (frame.hmax, frame.vmax),
        }
    }

    /// Decode every scan.
    pub(crate) fn decode_scans(&mut self, data: &[u8]) -> Result<()> {
        if self.decoded {
            return Ok(());
        }
        self.read_headers(data)?;
        let frame = self
            .frame
            .as_ref()
            .ok_or(JpegError::AbbreviatedWithoutFrame)?;
        if frame.entropy == EntropyCoding::Arithmetic {
            return Err(JpegError::Unsupported(UnsupportedFeature::ArithmeticCoding));
        }

        self.planes = Planes::allocate(frame, &self.options.limits)?;
        if frame.is_progressive() {
            self.coefficients = Coefficients::allocate(frame, &self.options.limits)?;
        }

        let mut scanner = Scanner::new(data);
        scanner.seek(self.header_end);
        let mut predictions = [0i32; 4];

        while let Some(segment) = scanner.next_segment()? {
            match segment.code {
                0xD9 => break,
                0xC4 | 0xCC | 0xDB | 0xDD => {
                    self.apply_table_segment(segment.code, segment.payload, segment.offset)?;
                }
                0xFE | 0xE0..=0xEF => self.metadata.push(segment.code, segment.payload),
                // A DNL after the first scan only has to be well formed:
                // `read_headers` already resolved the height from it.
                0xDC if segment.payload.len() < 2 => {
                    return Err(JpegError::malformed(
                        "DNL",
                        segment.offset,
                        "segment too short",
                    ));
                }
                0xDC => {}
                0xDE | 0xDF => {
                    return Err(JpegError::Unsupported(UnsupportedFeature::Hierarchical));
                }
                code if (0xC0..=0xCF).contains(&code) => {
                    return Err(JpegError::malformed(
                        "SOF",
                        segment.offset,
                        "a second frame header is not supported",
                    ));
                }
                0xDA => {
                    self.scans += 1;
                    if self.scans > self.options.limits.max_scans {
                        return Err(JpegError::LimitExceeded(LimitKind::Scans));
                    }
                    let entropy_start = scanner.position();
                    let consumed = self.decode_one_scan(
                        segment.payload,
                        segment.offset,
                        &data[entropy_start..],
                        &mut predictions,
                    )?;
                    scanner.seek(entropy_start + consumed);
                }
                _ => {}
            }
        }

        if self.scans == 0 {
            return Err(JpegError::NoScan);
        }

        let frame = self
            .frame
            .as_ref()
            .ok_or(JpegError::AbbreviatedWithoutFrame)?;
        if frame.is_progressive() {
            let tables = ScanTables {
                dc: &self.tables.dc_huffman,
                ac: &self.tables.ac_huffman,
                quant: &self.tables.quant,
            };
            render_coefficients(frame, &tables, &self.coefficients, &mut self.planes)?;
        }
        self.decoded = true;
        Ok(())
    }

    /// Parse one `SOS` and run the matching entropy decoder.
    fn decode_one_scan(
        &mut self,
        payload: &[u8],
        offset: usize,
        entropy: &[u8],
        predictions: &mut [i32; 4],
    ) -> Result<usize> {
        let frame = self
            .frame
            .as_ref()
            .ok_or(JpegError::AbbreviatedWithoutFrame)?;
        let scan = parse_sos(payload, offset, frame)?;
        let restart_interval = self.tables.restart_interval.unwrap_or(0);
        let tables = ScanTables {
            dc: &self.tables.dc_huffman,
            ac: &self.tables.ac_huffman,
            quant: &self.tables.quant,
        };
        let tolerate = self.options.tolerate_truncated;

        let outcome = match frame.process {
            CodingProcess::Lossless => decode_lossless(
                frame,
                &scan,
                &tables,
                restart_interval,
                entropy,
                &mut self.planes,
                tolerate,
            )?,
            CodingProcess::Progressive => {
                validate_dct_scan(&scan, frame, offset)?;
                decode_progressive(
                    frame,
                    &scan,
                    &tables,
                    restart_interval,
                    entropy,
                    &mut self.coefficients,
                    predictions,
                    tolerate,
                )?
            }
            _ => {
                validate_dct_scan(&scan, frame, offset)?;
                decode_sequential(
                    frame,
                    &scan,
                    &tables,
                    restart_interval,
                    entropy,
                    &mut self.planes,
                    tolerate,
                )?
            }
        };
        self.truncated |= outcome.truncated;
        Ok(outcome.consumed)
    }

    /// Build the output plan for the decoded image.
    pub(crate) fn plan(&self) -> Result<OutputPlan> {
        let frame = self
            .frame
            .as_ref()
            .ok_or(JpegError::AbbreviatedWithoutFrame)?;
        let info = self.info.ok_or(JpegError::AbbreviatedWithoutFrame)?;
        OutputPlan::new(
            frame,
            &self.planes,
            info.input_color_space,
            info.output_color_space,
            self.options.raw_components,
            self.options.upsampling == Upsampling::Fancy,
            info.has_adobe,
        )
    }

    /// The decoded component planes.
    pub(crate) fn planes(&self) -> &Planes {
        &self.planes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_entropy_walks_past_stuffing_and_restarts() {
        let data = [0x01u8, 0xFF, 0x00, 0x02, 0xFF, 0xD0, 0x03, 0xFF, 0xD9];
        assert_eq!(skip_entropy(&data, 0), 7);
        let data = [0x01u8, 0x02];
        assert_eq!(skip_entropy(&data, 0), 2);
        let data = [0xFFu8, 0xFF];
        assert_eq!(skip_entropy(&data, 0), 2);
    }

    #[test]
    fn find_dnl_locates_the_height_after_a_scan() {
        // SOS header, two entropy bytes, DNL(17), EOI.
        let mut data = vec![0xFFu8, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00];
        data.extend_from_slice(&[0xAB, 0xCD]);
        data.extend_from_slice(&[0xFF, 0xDC, 0x00, 0x04, 0x00, 0x11]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(find_dnl(&data, 0), Some(17));
    }

    #[test]
    fn find_dnl_gives_up_at_eoi() {
        let data = [0xFFu8, 0xD9, 0xFF, 0xDC, 0x00, 0x04, 0x00, 0x11];
        assert_eq!(find_dnl(&data, 0), None);
        assert_eq!(find_dnl(&[], 0), None);
    }
}
