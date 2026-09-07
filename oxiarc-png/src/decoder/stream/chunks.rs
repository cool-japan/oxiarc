//! Chunk classification, ordering rules and dispatch.
//!
//! Split out of the state machine so neither file grows past the house limit,
//! and because the ordering table is worth reading on its own.

use std::borrow::Cow;

use crate::ancillary::{color, misc};
use crate::chunk::{self, ChunkType};
use crate::common::{AnimationControl, BlendOp, DisposeOp, FrameControl};
use crate::error::{DecodingError, FormatErrorKind};
use crate::header::Ihdr;
use crate::info::{CgbiInfo, Info, UnknownChunk};
use crate::text_metadata::{ITXtChunk, TEXtChunk, ZTXtChunk};

use super::{Decoded, StreamingDecoder};

/// What the decoder decided to do with a chunk before reading its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkAction {
    /// Read the payload and parse it.
    Process,
    /// Read the payload and keep it as an unknown chunk.
    Retain,
    /// Read the payload and throw it away.
    Skip,
    /// Read the payload and throw it away, because its length is wrong for
    /// its type.
    Reject,
}

/// Chunks that may appear at most once in a file.
const UNIQUE: &[ChunkType] = &[
    chunk::IHDR,
    chunk::PLTE,
    chunk::IEND,
    chunk::cHRM,
    chunk::gAMA,
    chunk::iCCP,
    chunk::sBIT,
    chunk::sRGB,
    chunk::bKGD,
    chunk::hIST,
    chunk::tRNS,
    chunk::pHYs,
    chunk::tIME,
    chunk::acTL,
    chunk::cICP,
    chunk::mDCV,
    chunk::cLLI,
    chunk::oFFs,
    chunk::sCAL,
    chunk::pCAL,
    chunk::sTER,
    chunk::CgBI,
];

/// Chunks that must precede both `PLTE` and `IDAT`.
const BEFORE_PLTE_AND_IDAT: &[ChunkType] = &[
    chunk::cHRM,
    chunk::gAMA,
    chunk::iCCP,
    chunk::sBIT,
    chunk::sRGB,
    chunk::cICP,
    chunk::mDCV,
    chunk::cLLI,
];

/// Chunks that must follow `PLTE` (when there is one) and precede `IDAT`.
const AFTER_PLTE_BEFORE_IDAT: &[ChunkType] = &[chunk::bKGD, chunk::hIST, chunk::tRNS];

/// Chunks that must simply precede `IDAT`.
const BEFORE_IDAT: &[ChunkType] = &[
    chunk::pHYs,
    chunk::sPLT,
    chunk::oFFs,
    chunk::sCAL,
    chunk::pCAL,
    chunk::sTER,
    chunk::acTL,
];

/// The length a known chunk must have, as an inclusive range.
fn length_range(kind: ChunkType) -> Option<(u32, u32)> {
    let unbounded = chunk::MAX_CHUNK_LEN;
    Some(match kind {
        chunk::IHDR => (13, 13),
        chunk::PLTE => (3, 768),
        chunk::IEND => (0, 0),
        chunk::sBIT => (1, 4),
        chunk::tRNS => (1, 256),
        chunk::pHYs => (9, 9),
        chunk::gAMA => (4, 4),
        chunk::acTL => (8, 8),
        chunk::fcTL => (26, 26),
        chunk::cHRM => (32, 32),
        chunk::sRGB => (1, 1),
        chunk::cICP => (4, 4),
        chunk::mDCV => (24, 24),
        chunk::cLLI => (8, 8),
        chunk::bKGD => (1, 6),
        chunk::hIST => (2, 512),
        chunk::tIME => (7, 7),
        chunk::oFFs => (9, 9),
        chunk::sTER => (1, 1),
        chunk::CgBI => (4, 4),
        chunk::eXIf | chunk::iCCP | chunk::tEXt | chunk::zTXt | chunk::iTXt => (0, unbounded),
        chunk::sPLT | chunk::sCAL | chunk::pCAL => (0, unbounded),
        _ => return None,
    })
}

/// Chunks whose wrong length is fatal rather than merely disqualifying.
fn length_is_fatal(kind: ChunkType) -> bool {
    kind == chunk::IHDR || kind == chunk::PLTE || kind == chunk::IEND || kind == chunk::fcTL
}

impl StreamingDecoder {
    /// Decide what to do with a chunk from its type and declared length.
    pub(super) fn classify_chunk(
        &mut self,
        kind: ChunkType,
        length: u32,
    ) -> Result<ChunkAction, DecodingError> {
        if chunk::reserved_set(kind) && self.options.strict {
            return Err(FormatErrorKind::ReservedBitSet { kind }.into());
        }
        if kind == chunk::IDAT || kind == chunk::fdAT {
            return Ok(ChunkAction::Process);
        }
        let Some((low, high)) = length_range(kind) else {
            if chunk::is_critical(kind) {
                return Err(FormatErrorKind::UnrecognizedCriticalChunk { type_str: kind }.into());
            }
            let retain = self.options.retain_unknown_chunks
                && self.retained_unknown_bytes.saturating_add(length as usize)
                    <= self.options.limits.max_unknown_chunk_bytes;
            return Ok(if retain {
                ChunkAction::Retain
            } else {
                ChunkAction::Skip
            });
        };
        if length < low || length > high {
            if length_is_fatal(kind) {
                return Err(FormatErrorKind::ChunkLengthWrong { kind }.into());
            }
            return Ok(ChunkAction::Reject);
        }
        let ignored = match kind {
            chunk::tEXt | chunk::zTXt | chunk::iTXt => self.options.ignore_text_chunk,
            chunk::iCCP => self.options.ignore_iccp_chunk,
            _ => false,
        };
        if ignored {
            return Ok(ChunkAction::Skip);
        }
        // Bound the unbounded ones against the configured limits.
        let cap = match kind {
            chunk::iCCP => Some(self.options.limits.max_iccp_bytes),
            chunk::tEXt | chunk::zTXt | chunk::iTXt => Some(self.options.limits.max_text_bytes),
            chunk::eXIf | chunk::sPLT | chunk::sCAL | chunk::pCAL => {
                Some(self.options.limits.max_unknown_chunk_bytes)
            }
            _ => None,
        };
        if let Some(cap) = cap {
            if length as usize > cap {
                return Err(DecodingError::LimitsExceeded);
            }
        }
        Ok(ChunkAction::Process)
    }

    /// Enforce the chunk-ordering table.
    pub(super) fn check_order(&mut self, kind: ChunkType) -> Result<(), DecodingError> {
        if self.info.is_none() && kind != chunk::IHDR && kind != chunk::CgBI {
            return Err(FormatErrorKind::ChunkBeforeIhdr { kind }.into());
        }
        if UNIQUE.contains(&kind) && self.seen.contains(&kind) {
            return Err(FormatErrorKind::DuplicateChunk { kind }.into());
        }
        if kind == chunk::IDAT && self.idat_run_closed {
            return Err(FormatErrorKind::UnexpectedRestartOfDataChunkSequence { kind }.into());
        }
        if BEFORE_PLTE_AND_IDAT.contains(&kind) {
            if self.have_plte {
                return Err(FormatErrorKind::AfterPlte { kind }.into());
            }
            if self.have_idat {
                return Err(FormatErrorKind::AfterIdat { kind }.into());
            }
        }
        if AFTER_PLTE_BEFORE_IDAT.contains(&kind) {
            if self.have_idat {
                return Err(FormatErrorKind::OutsidePlteIdat { kind }.into());
            }
            let indexed = self
                .info
                .as_ref()
                .is_some_and(|i| i.color_type == crate::ColorType::Indexed);
            if indexed && !self.have_plte {
                return Err(FormatErrorKind::BeforePlte { kind }.into());
            }
        }
        if BEFORE_IDAT.contains(&kind) && self.have_idat {
            return Err(FormatErrorKind::AfterIdat { kind }.into());
        }
        if kind == chunk::PLTE && self.have_idat {
            return Err(FormatErrorKind::AfterIdat { kind }.into());
        }
        if kind == chunk::fdAT && !self.have_idat {
            return Err(FormatErrorKind::ChunkBeforeIhdr { kind }.into());
        }
        if kind == chunk::IEND && !self.have_idat {
            return Err(FormatErrorKind::MissingImageData.into());
        }
        if self.options.strict && kind == chunk::iCCP && self.seen.contains(&chunk::sRGB) {
            return Err(FormatErrorKind::SrgbAndIccp.into());
        }
        if self.options.strict && kind == chunk::sRGB && self.seen.contains(&chunk::iCCP) {
            return Err(FormatErrorKind::SrgbAndIccp.into());
        }
        if UNIQUE.contains(&kind) {
            self.seen.push(kind);
        }
        Ok(())
    }

    /// Parse a chunk whose payload has been read and whose CRC checked out.
    pub(super) fn complete_chunk(&mut self, kind: ChunkType) -> Result<Decoded, DecodingError> {
        let data = std::mem::take(&mut self.current.data);
        match self.current.action {
            ChunkAction::Skip => return Ok(Decoded::SkippedAncillaryChunk(kind)),
            ChunkAction::Reject => return Ok(Decoded::ChunkComplete(kind)),
            ChunkAction::Retain => {
                self.retained_unknown_bytes =
                    self.retained_unknown_bytes.saturating_add(data.len());
                if let Some(info) = self.info.as_mut() {
                    info.unknown_chunks.push(UnknownChunk {
                        kind,
                        data,
                        safe_to_copy: chunk::safe_to_copy(kind),
                    });
                }
                return Ok(Decoded::RetainedUnknownChunk(kind));
            }
            ChunkAction::Process => {}
        }
        self.parse_chunk(kind, &data)
    }

    fn parse_chunk(&mut self, kind: ChunkType, data: &[u8]) -> Result<Decoded, DecodingError> {
        match kind {
            chunk::CgBI => {
                let flags = [data[0], data[1], data[2], data[3]];
                if self.options.strict {
                    return Err(FormatErrorKind::CgbiUnsupported.into());
                }
                self.pending_cgbi = Some(CgbiInfo {
                    flags,
                    premultiplied_alpha: true,
                });
                self.inflater.set_raw_deflate(true);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::IHDR => self.parse_ihdr(data),
            chunk::PLTE => {
                let color_type = self.info_ref()?.color_type;
                let palette = misc::parse_plte(data, color_type)?;
                self.have_plte = true;
                self.info_mut()?.palette = Some(Cow::Owned(palette));
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::tRNS => {
                let (color_type, bit_depth, entries) = {
                    let info = self.info_ref()?;
                    (info.color_type, info.bit_depth, info.palette_entries())
                };
                if let Some(trns) = misc::parse_trns(data, color_type, bit_depth, entries)? {
                    let info = self.info_mut()?;
                    info.trns = Some(Cow::Owned(trns.normalized));
                    info.trns_original = Some(trns.raw);
                }
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::IDAT | chunk::fdAT => Ok(Decoded::ChunkComplete(kind)),
            chunk::IEND => Ok(Decoded::ImageEnd),
            chunk::gAMA => {
                let gamma = color::parse_gama(data)?;
                let info = self.info_mut()?;
                info.gama_chunk = Some(gamma);
                if info.srgb.is_none() {
                    info.source_gamma = Some(gamma);
                }
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::cHRM => {
                let chrm = color::parse_chrm(data)?;
                let info = self.info_mut()?;
                info.chrm_chunk = Some(chrm);
                if info.srgb.is_none() {
                    info.source_chromaticities = Some(chrm);
                }
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::sRGB => {
                let intent = color::parse_srgb(data)?;
                let info = self.info_mut()?;
                info.srgb = Some(intent);
                // An sRGB chunk overrides gAMA and cHRM with the sRGB values.
                info.source_gamma = Some(crate::ScaledFloat::from_scaled(45455));
                info.source_chromaticities = Some(crate::SourceChromaticities::from_srgb());
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::iCCP => {
                let limit = self.options.limits.max_iccp_bytes;
                let (_name, profile) = color::parse_iccp(data, limit)?;
                self.info_mut()?.icc_profile = Some(Cow::Owned(profile));
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::cICP => {
                let cicp = color::parse_cicp(data)?;
                self.info_mut()?.coding_independent_code_points = Some(cicp);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::mDCV => {
                let mdcv = color::parse_mdcv(data)?;
                self.info_mut()?.mastering_display_color_volume = Some(mdcv);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::cLLI => {
                let clli = color::parse_clli(data)?;
                self.info_mut()?.content_light_level = Some(clli);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::sBIT => {
                let (color_type, bit_depth) = {
                    let info = self.info_ref()?;
                    (info.color_type, info.bit_depth)
                };
                let sbit = color::parse_sbit(data, color_type, bit_depth)?;
                self.info_mut()?.sbit = Some(Cow::Owned(sbit));
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::bKGD => {
                let color_type = self.info_ref()?.color_type;
                let bkgd = color::parse_bkgd(data, color_type)?;
                self.info_mut()?.bkgd = Some(Cow::Owned(bkgd));
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::hIST => {
                let entries = self.info_ref()?.palette_entries();
                let hist = color::parse_hist(data, entries)?;
                self.info_mut()?.hist = Some(hist);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::sPLT => {
                let splt = color::parse_splt(data)?;
                self.info_mut()?.splt.push(splt);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::pHYs => {
                let dims = misc::parse_phys(data)?;
                self.info_mut()?.pixel_dims = Some(dims);
                Ok(Decoded::PixelDimensions(dims))
            }
            chunk::tIME => {
                let time = misc::parse_time(data)?;
                self.info_mut()?.time = Some(time);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::oFFs => {
                let offs = misc::parse_offs(data)?;
                self.info_mut()?.offs = Some(offs);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::sCAL => {
                let scal = misc::parse_scal(data)?;
                self.info_mut()?.scal = Some(scal);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::pCAL => {
                let pcal = misc::parse_pcal(data)?;
                self.info_mut()?.pcal = Some(pcal);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::sTER => {
                let ster = misc::parse_ster(data)?;
                self.info_mut()?.ster = Some(ster);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::eXIf => {
                self.info_mut()?.exif_metadata = Some(Cow::Owned(data.to_vec()));
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::tEXt => {
                let text = TEXtChunk::parse(data)?;
                self.info_mut()?.uncompressed_latin1_text.push(text);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::zTXt => {
                let limit = self.options.limits.max_text_bytes;
                let mut text = ZTXtChunk::parse(data)?;
                text.decompress_text_with_limit(limit)?;
                self.info_mut()?.compressed_latin1_text.push(text);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::iTXt => {
                let limit = self.options.limits.max_text_bytes;
                let mut text = ITXtChunk::parse(data)?;
                text.decompress_text_with_limit(limit)?;
                self.info_mut()?.utf8_text.push(text);
                Ok(Decoded::ChunkComplete(kind))
            }
            chunk::acTL => {
                let control = AnimationControl {
                    num_frames: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
                    num_plays: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                };
                let limits = self.options.limits;
                self.apng.observe_actl(control, &limits)?;
                self.info_mut()?.animation_control = Some(control);
                Ok(Decoded::AnimationControl(control))
            }
            chunk::fcTL => self.parse_fctl(data),
            _ => Ok(Decoded::ChunkComplete(kind)),
        }
    }

    fn parse_ihdr(&mut self, data: &[u8]) -> Result<Decoded, DecodingError> {
        let ihdr = Ihdr::parse(data)?;
        self.options
            .limits()
            .check_dimensions(ihdr.width, ihdr.height)?;
        let mut info = Info::from_ihdr(&ihdr);
        info.cgbi = self.pending_cgbi;
        if let Some(cgbi) = self.pending_cgbi {
            info.cgbi = Some(CgbiInfo {
                premultiplied_alpha: cgbi.premultiplied_alpha
                    && matches!(
                        ihdr.color_type,
                        crate::ColorType::Rgba | crate::ColorType::GrayscaleAlpha
                    ),
                ..cgbi
            });
        }
        self.info = Some(info);
        Ok(Decoded::Header {
            width: ihdr.width,
            height: ihdr.height,
            bit_depth: ihdr.bit_depth,
            color_type: ihdr.color_type,
            interlaced: ihdr.interlace.is_interlaced(),
        })
    }

    fn parse_fctl(&mut self, data: &[u8]) -> Result<Decoded, DecodingError> {
        let word = |i: usize| u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        let half = |i: usize| u16::from_be_bytes([data[i], data[i + 1]]);
        let frame = FrameControl {
            sequence_number: word(0),
            width: word(4),
            height: word(8),
            x_offset: word(12),
            y_offset: word(16),
            delay_num: half(20),
            delay_den: half(22),
            dispose_op: DisposeOp::from_u8(data[24])
                .ok_or(FormatErrorKind::MalformedChunk { kind: chunk::fcTL })?,
            blend_op: BlendOp::from_u8(data[25])
                .ok_or(FormatErrorKind::MalformedChunk { kind: chunk::fcTL })?,
        };
        let canvas = {
            let info = self.info_ref()?;
            (info.width, info.height)
        };
        let have_idat = self.have_idat;
        self.apng.observe_fctl(frame, canvas, have_idat)?;
        if have_idat {
            // Every animation frame is its own zlib stream.
            self.inflater.reset();
        }
        self.info_mut()?.frame_control = Some(frame);
        Ok(Decoded::FrameControl(frame))
    }

    fn info_ref(&self) -> Result<&Info<'static>, DecodingError> {
        self.info
            .as_ref()
            .ok_or_else(|| FormatErrorKind::ChunkBeforeIhdr { kind: chunk::IHDR }.into())
    }

    fn info_mut(&mut self) -> Result<&mut Info<'static>, DecodingError> {
        self.info
            .as_mut()
            .ok_or_else(|| FormatErrorKind::ChunkBeforeIhdr { kind: chunk::IHDR }.into())
    }
}
