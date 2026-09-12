//! The push decoder: feed bytes, get events.
//!
//! [`StreamingDecoder`] owns *all* chunk parsing in this crate. The pull
//! [`crate::Decoder`] is a thin adapter that reads from an
//! [`std::io::Read`] and drives this machine, so there is exactly one parser
//! and exactly one set of ordering rules.
//!
//! # Contract
//!
//! [`StreamingDecoder::update`] performs **one step** and returns how many
//! input bytes it consumed together with at most one [`Decoded`] event. A
//! caller loops until it sees the event it wants. When the machine is inside
//! the image data and the supplied [`UnfilterBuf`] is full, `update` consumes
//! nothing and returns [`Decoded::ImageData`]: the caller must drain the
//! buffer before calling again.
//!
//! ```
//! use oxiarc_png::chunk::{self, write_chunk, SIGNATURE};
//! use oxiarc_png::{Decoded, StreamingDecoder};
//!
//! // A 2x2 8-bit grayscale image, built by hand.
//! let mut png = SIGNATURE.to_vec();
//! let mut ihdr = [0u8; 13];
//! ihdr[3] = 2;      // width
//! ihdr[7] = 2;      // height
//! ihdr[8] = 8;      // bit depth
//! write_chunk(&mut png, chunk::IHDR, &ihdr).expect("write");
//! let raw = [0u8, 1, 2, 0, 3, 4];   // filter byte + 2 pixels, twice
//! let idat = oxiarc_deflate::zlib_compress(&raw, 6).expect("compress");
//! write_chunk(&mut png, chunk::IDAT, &idat).expect("write");
//! write_chunk(&mut png, chunk::IEND, &[]).expect("write");
//!
//! let mut decoder = StreamingDecoder::new();
//! let mut pos = 0;
//! let mut saw_header = false;
//! while pos < png.len() {
//!     let (consumed, event) = decoder.update(&png[pos..], None).expect("decode");
//!     pos += consumed;
//!     if let Decoded::Header { width, height, .. } = event {
//!         assert_eq!((width, height), (2, 2));
//!         saw_header = true;
//!     }
//!     if consumed == 0 {
//!         break;
//!     }
//! }
//! assert!(saw_header);
//! ```

mod chunks;

use oxiarc_core::Crc32;

use crate::chunk::{self, ChunkHeader, ChunkType, SIGNATURE};
use crate::common::{AnimationControl, FrameControl, PixelDimensions};
use crate::decoder::unfilter_buf::UnfilterBuf;
use crate::decoder::zlib::ZlibStream;
use crate::error::{DecodingError, FormatErrorKind, ParameterErrorKind};
use crate::header::{BitDepth, ColorType};
use crate::info::Info;
use crate::limits::{DecodeLimits, FrameBudget, Limits};

pub use chunks::ChunkAction;

/// Options that govern how strictly a stream is read.
///
/// The defaults reproduce `png` 0.18's lenient behaviour, which is what makes
/// real-world files decode: ancillary CRC failures are tolerated, the zlib
/// Adler-32 is not checked, and unknown ancillary chunks are kept rather than
/// dropped.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DecodeOptions {
    ignore_adler32: bool,
    ignore_crc: bool,
    skip_ancillary_crc_failures: bool,
    ignore_text_chunk: bool,
    ignore_iccp_chunk: bool,
    strict: bool,
    retain_unknown_chunks: bool,
    strict_palette_indices: bool,
    error_on_trailing_data: bool,
    limits: DecodeLimits,
}

impl Default for DecodeOptions {
    fn default() -> DecodeOptions {
        DecodeOptions {
            ignore_adler32: true,
            ignore_crc: false,
            skip_ancillary_crc_failures: true,
            ignore_text_chunk: false,
            ignore_iccp_chunk: false,
            strict: false,
            retain_unknown_chunks: true,
            strict_palette_indices: false,
            error_on_trailing_data: false,
            limits: DecodeLimits::default(),
        }
    }
}

impl DecodeOptions {
    /// Skip the zlib Adler-32 check. Default `true`.
    pub fn set_ignore_adler32(&mut self, ignore: bool) {
        self.ignore_adler32 = ignore;
    }

    /// Whether the zlib Adler-32 check is skipped.
    #[must_use]
    pub fn ignore_adler32(&self) -> bool {
        self.ignore_adler32
    }

    /// Skip every chunk CRC check. Default `false`.
    pub fn set_ignore_crc(&mut self, ignore: bool) {
        self.ignore_crc = ignore;
    }

    /// Whether chunk CRCs are skipped.
    #[must_use]
    pub fn ignore_crc(&self) -> bool {
        self.ignore_crc
    }

    /// Drop ancillary chunks whose CRC is wrong instead of failing. Default
    /// `true`.
    pub fn set_skip_ancillary_crc_failures(&mut self, skip: bool) {
        self.skip_ancillary_crc_failures = skip;
    }

    /// Do not parse `tEXt`, `zTXt` or `iTXt`. Default `false`.
    pub fn set_ignore_text_chunk(&mut self, ignore: bool) {
        self.ignore_text_chunk = ignore;
    }

    /// Do not parse `iCCP`. Default `false`.
    pub fn set_ignore_iccp_chunk(&mut self, ignore: bool) {
        self.ignore_iccp_chunk = ignore;
    }

    /// Reject everything the specification forbids, including files that the
    /// reference decoders accept. Default `false`.
    ///
    /// Strict mode turns on: the reserved type bit check, mutually exclusive
    /// `sRGB`/`iCCP`, out-of-range palette indices, trailing image data,
    /// trailing bytes after `IEND`, and Apple `CgBI` rejection.
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
        if strict {
            self.skip_ancillary_crc_failures = false;
            self.ignore_adler32 = false;
            self.strict_palette_indices = true;
            self.error_on_trailing_data = true;
        }
    }

    /// Whether strict mode is on.
    #[must_use]
    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Keep unknown ancillary chunks in [`Info::unknown_chunks`]. Default
    /// `true`; the `png` crate always discards them.
    pub fn set_retain_unknown_chunks(&mut self, retain: bool) {
        self.retain_unknown_chunks = retain;
    }

    /// Fail on a palette index with no palette entry instead of rendering it
    /// as opaque black. Default `false`.
    pub fn set_strict_palette_indices(&mut self, strict: bool) {
        self.strict_palette_indices = strict;
    }

    /// Whether out-of-range palette indices are fatal.
    #[must_use]
    pub fn strict_palette_indices(&self) -> bool {
        self.strict_palette_indices
    }

    /// Fail when bytes follow `IEND`. Default `false`.
    pub fn set_error_on_trailing_data(&mut self, error: bool) {
        self.error_on_trailing_data = error;
    }

    /// Replace the size limits.
    pub fn set_limits(&mut self, limits: DecodeLimits) {
        self.limits = limits;
    }

    /// The size limits in force.
    #[must_use]
    pub fn limits(&self) -> &DecodeLimits {
        &self.limits
    }
}

/// One step's worth of progress.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Decoded {
    /// Progress was made but nothing worth reporting happened.
    Nothing,
    /// The image header was parsed.
    Header {
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
        /// Bits per sample.
        bit_depth: BitDepth,
        /// How colours are stored.
        color_type: ColorType,
        /// Whether the image is Adam7 interlaced.
        interlaced: bool,
    },
    /// A chunk's header was read: its declared length and its type.
    ChunkBegin(u32, ChunkType),
    /// A chunk was read and parsed.
    ChunkComplete(ChunkType),
    /// An ancillary chunk failed its CRC and was dropped.
    BadAncillaryChunk(ChunkType),
    /// An ancillary chunk was skipped without being parsed.
    SkippedAncillaryChunk(ChunkType),
    /// An unrecognised ancillary chunk was kept in
    /// [`Info::unknown_chunks`].
    RetainedUnknownChunk(ChunkType),
    /// A `pHYs` chunk was parsed.
    PixelDimensions(PixelDimensions),
    /// An `acTL` chunk was parsed.
    AnimationControl(AnimationControl),
    /// An `fcTL` chunk was parsed.
    FrameControl(FrameControl),
    /// Image data was written into the supplied buffer.
    ImageData,
    /// The image data stream for the current frame ended.
    ImageDataFlushed,
    /// `IEND` was reached.
    ImageEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Signature { filled: u8 },
    Header { filled: u8 },
    ChunkData,
    ImageData,
    FdatSequence { filled: u8 },
    FlushImageData,
    Crc { filled: u8 },
    AfterIend,
}

#[derive(Debug)]
struct CurrentChunk {
    kind: ChunkType,
    remaining: u32,
    action: ChunkAction,
    crc: Crc32,
    data: Vec<u8>,
    reserved: usize,
}

impl Default for CurrentChunk {
    fn default() -> CurrentChunk {
        CurrentChunk {
            kind: chunk::IEND,
            remaining: 0,
            action: ChunkAction::Skip,
            crc: Crc32::new(),
            data: Vec::new(),
            reserved: 0,
        }
    }
}

/// The push decoder.
#[derive(Debug)]
pub struct StreamingDecoder {
    state: State,
    scratch: [u8; 8],
    current: CurrentChunk,
    pending_header: Option<ChunkHeader>,
    pub(crate) info: Option<Info<'static>>,
    pub(crate) options: DecodeOptions,
    pub(crate) limits: Limits,
    pub(crate) inflater: ZlibStream,
    pub(crate) apng: crate::apng::ApngTracker,
    pub(crate) frame_budget: FrameBudget,
    pub(crate) have_plte: bool,
    pub(crate) have_idat: bool,
    pub(crate) idat_run_closed: bool,
    pub(crate) seen: Vec<ChunkType>,
    pub(crate) pending_cgbi: Option<crate::info::CgbiInfo>,
    pub(crate) retained_unknown_bytes: usize,
    pub(crate) extra_image_data: bool,
    image_data_active: bool,
    input_ended: bool,
    fatal: bool,
}

impl Default for StreamingDecoder {
    fn default() -> StreamingDecoder {
        StreamingDecoder::new()
    }
}

impl StreamingDecoder {
    /// A decoder with the default (lenient) options.
    #[must_use]
    pub fn new() -> StreamingDecoder {
        StreamingDecoder::new_with_options(DecodeOptions::default())
    }

    /// A decoder with explicit options.
    #[must_use]
    pub fn new_with_options(options: DecodeOptions) -> StreamingDecoder {
        let inflater = ZlibStream::new(!options.ignore_adler32, false);
        StreamingDecoder {
            state: State::Signature { filled: 0 },
            scratch: [0; 8],
            current: CurrentChunk::default(),
            pending_header: None,
            info: None,
            options,
            limits: Limits::default(),
            inflater,
            apng: crate::apng::ApngTracker::default(),
            frame_budget: FrameBudget::default(),
            have_plte: false,
            have_idat: false,
            idat_run_closed: false,
            seen: Vec::new(),
            pending_cgbi: None,
            retained_unknown_bytes: 0,
            extra_image_data: false,
            image_data_active: false,
            input_ended: false,
            fatal: false,
        }
    }

    /// Return the decoder to its initial state, keeping the options.
    pub fn reset(&mut self) {
        let options = self.options.clone();
        let limits = self.limits;
        *self = StreamingDecoder::new_with_options(options);
        self.limits = limits;
    }

    /// The metadata read so far, once `IHDR` has been seen.
    #[must_use]
    pub fn info(&self) -> Option<&Info<'static>> {
        self.info.as_ref()
    }

    /// The options in force.
    #[must_use]
    pub fn options(&self) -> &DecodeOptions {
        &self.options
    }

    /// Replace the `png`-shaped memory budget.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Replace the full size limits.
    pub fn set_decode_limits(&mut self, limits: DecodeLimits) {
        self.options.limits = limits;
    }

    /// Skip the zlib Adler-32 check.
    ///
    /// Returns `true` if the setting took effect, which it does not once image
    /// data has started.
    pub fn set_ignore_adler32(&mut self, ignore: bool) -> bool {
        if self.have_idat {
            return false;
        }
        self.options.ignore_adler32 = ignore;
        self.inflater.set_verify_adler32(!ignore);
        true
    }

    /// Skip every chunk CRC check.
    pub fn set_ignore_crc(&mut self, ignore: bool) {
        self.options.ignore_crc = ignore;
    }

    /// Drop ancillary chunks whose CRC is wrong instead of failing.
    pub fn set_skip_ancillary_crc_failures(&mut self, skip: bool) {
        self.options.skip_ancillary_crc_failures = skip;
    }

    /// Do not parse the text chunks.
    pub fn set_ignore_text_chunk(&mut self, ignore: bool) {
        self.options.ignore_text_chunk = ignore;
    }

    /// Do not parse `iCCP`.
    pub fn set_ignore_iccp_chunk(&mut self, ignore: bool) {
        self.options.ignore_iccp_chunk = ignore;
    }

    /// Tell the decoder that no more input will arrive.
    ///
    /// This is what turns a truncated file into a diagnosable error instead of
    /// a decoder that waits forever: the remaining image data is flushed with
    /// `FlushMode::Finish`, and a stream that ends mid-symbol fails.
    pub fn notify_eof(&mut self) {
        self.input_ended = true;
    }

    /// Whether `IEND` has been reached.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state == State::AfterIend
    }

    /// Whether the decoder is inside a run of image-data chunks.
    #[must_use]
    pub fn in_image_data(&self) -> bool {
        self.image_data_active
    }

    /// Whether the compressed image data produced more bytes than the header
    /// allows.
    #[must_use]
    pub fn saw_extra_image_data(&self) -> bool {
        self.extra_image_data
    }

    /// Take one step.
    ///
    /// Returns the number of bytes of `buf` consumed and the event, if any.
    /// See the module documentation for the full contract.
    ///
    /// # Errors
    ///
    /// Any structural problem with the stream. Errors are latched: a second
    /// call after a failure reports [`ParameterErrorKind::PolledAfterFatalError`].
    pub fn update(
        &mut self,
        buf: &[u8],
        image_data: Option<&mut UnfilterBuf<'_>>,
    ) -> Result<(usize, Decoded), DecodingError> {
        if self.fatal {
            return Err(ParameterErrorKind::PolledAfterFatalError.into());
        }
        match self.next_state(buf, image_data) {
            Ok(progress) => Ok(progress),
            Err(err) => {
                self.fatal = true;
                Err(err)
            }
        }
    }

    fn next_state(
        &mut self,
        buf: &[u8],
        image_data: Option<&mut UnfilterBuf<'_>>,
    ) -> Result<(usize, Decoded), DecodingError> {
        match self.state {
            State::Signature { filled } => self.step_signature(buf, filled),
            State::Header { filled } => self.step_header(buf, filled),
            State::ChunkData => self.step_chunk_data(buf),
            State::ImageData => self.step_image_data(buf, image_data),
            State::FdatSequence { filled } => self.step_fdat_sequence(buf, filled),
            State::FlushImageData => self.step_flush(image_data),
            State::Crc { filled } => self.step_crc(buf, filled),
            State::AfterIend => self.step_after_iend(buf),
        }
    }

    fn step_signature(
        &mut self,
        buf: &[u8],
        filled: u8,
    ) -> Result<(usize, Decoded), DecodingError> {
        let want = 8 - usize::from(filled);
        let take = want.min(buf.len());
        if take == 0 {
            if self.input_ended {
                return Err(DecodingError::unexpected_eof());
            }
            return Ok((0, Decoded::Nothing));
        }
        let start = usize::from(filled);
        self.scratch[start..start + take].copy_from_slice(&buf[..take]);
        let filled = filled + take as u8;
        if usize::from(filled) < 8 {
            self.state = State::Signature { filled };
            return Ok((take, Decoded::Nothing));
        }
        if self.scratch != SIGNATURE {
            return Err(FormatErrorKind::InvalidSignature.into());
        }
        self.state = State::Header { filled: 0 };
        Ok((take, Decoded::Nothing))
    }

    fn step_header(&mut self, buf: &[u8], filled: u8) -> Result<(usize, Decoded), DecodingError> {
        let want = 8 - usize::from(filled);
        let take = want.min(buf.len());
        if take == 0 {
            if self.input_ended {
                return Err(if filled == 0 && self.have_idat {
                    FormatErrorKind::MissingIend.into()
                } else {
                    DecodingError::unexpected_eof()
                });
            }
            return Ok((0, Decoded::Nothing));
        }
        let start = usize::from(filled);
        self.scratch[start..start + take].copy_from_slice(&buf[..take]);
        let filled = filled + take as u8;
        if usize::from(filled) < 8 {
            self.state = State::Header { filled };
            return Ok((take, Decoded::Nothing));
        }
        let header = ChunkHeader::parse(&self.scratch)?;
        if header.length > self.options.limits.max_chunk_len {
            return Err(DecodingError::LimitsExceeded);
        }
        let is_image = header.kind == chunk::IDAT || header.kind == chunk::fdAT;
        if self.image_data_active && !is_image {
            // The run of image-data chunks has ended: drain the inflater
            // before touching the chunk that follows it.
            self.pending_header = Some(header);
            self.state = State::FlushImageData;
            return Ok((take, Decoded::Nothing));
        }
        let event = self.begin_chunk(header)?;
        Ok((take, event))
    }

    fn begin_chunk(&mut self, header: ChunkHeader) -> Result<Decoded, DecodingError> {
        self.check_order(header.kind)?;
        let action = self.classify_chunk(header.kind, header.length)?;
        self.release_chunk_reservation();
        let mut crc = Crc32::new();
        crc.update(&header.kind.as_bytes());
        let reserved = if matches!(action, ChunkAction::Process | ChunkAction::Retain) {
            let len = header.length as usize;
            self.limits.reserve_bytes(len)?;
            len
        } else {
            0
        };
        self.current = CurrentChunk {
            kind: header.kind,
            remaining: header.length,
            action,
            crc,
            data: Vec::new(),
            reserved,
        };
        if header.kind == chunk::IDAT || header.kind == chunk::fdAT {
            self.start_image_data(header)?;
        } else if header.length == 0 {
            self.state = State::Crc { filled: 0 };
        } else {
            self.state = State::ChunkData;
        }
        Ok(Decoded::ChunkBegin(header.length, header.kind))
    }

    fn release_chunk_reservation(&mut self) {
        if self.current.reserved > 0 {
            let reserved = self.current.reserved;
            self.limits.free_bytes(reserved);
            self.current.reserved = 0;
        }
    }

    fn start_image_data(&mut self, header: ChunkHeader) -> Result<(), DecodingError> {
        if header.kind == chunk::fdAT {
            if header.length < 4 {
                return Err(FormatErrorKind::FdatShorterThanFourBytes.into());
            }
            self.state = State::FdatSequence { filled: 0 };
        } else {
            self.state = if header.length == 0 {
                State::Crc { filled: 0 }
            } else {
                State::ImageData
            };
        }
        if !self.image_data_active {
            self.image_data_active = true;
        }
        if header.kind == chunk::IDAT {
            self.have_idat = true;
        }
        Ok(())
    }

    fn step_chunk_data(&mut self, buf: &[u8]) -> Result<(usize, Decoded), DecodingError> {
        let take = (self.current.remaining as usize).min(buf.len());
        if take == 0 {
            if self.current.remaining == 0 {
                self.state = State::Crc { filled: 0 };
                return Ok((0, Decoded::Nothing));
            }
            if self.input_ended {
                return Err(DecodingError::unexpected_eof());
            }
            return Ok((0, Decoded::Nothing));
        }
        let data = &buf[..take];
        if !self.options.ignore_crc {
            self.current.crc.update(data);
        }
        if matches!(
            self.current.action,
            ChunkAction::Process | ChunkAction::Retain
        ) {
            self.current.data.extend_from_slice(data);
        }
        self.current.remaining -= take as u32;
        if self.current.remaining == 0 {
            self.state = State::Crc { filled: 0 };
        }
        Ok((take, Decoded::Nothing))
    }

    fn step_fdat_sequence(
        &mut self,
        buf: &[u8],
        filled: u8,
    ) -> Result<(usize, Decoded), DecodingError> {
        let want = 4 - usize::from(filled);
        let take = want.min(buf.len()).min(self.current.remaining as usize);
        if take == 0 {
            if self.input_ended {
                return Err(DecodingError::unexpected_eof());
            }
            return Ok((0, Decoded::Nothing));
        }
        let start = usize::from(filled);
        self.scratch[start..start + take].copy_from_slice(&buf[..take]);
        if !self.options.ignore_crc {
            self.current.crc.update(&buf[..take]);
        }
        self.current.remaining -= take as u32;
        let filled = filled + take as u8;
        if usize::from(filled) < 4 {
            self.state = State::FdatSequence { filled };
            return Ok((take, Decoded::Nothing));
        }
        let sequence = u32::from_be_bytes([
            self.scratch[0],
            self.scratch[1],
            self.scratch[2],
            self.scratch[3],
        ]);
        self.apng.observe_fdat(sequence)?;
        self.state = if self.current.remaining == 0 {
            State::Crc { filled: 0 }
        } else {
            State::ImageData
        };
        Ok((take, Decoded::Nothing))
    }

    fn step_image_data(
        &mut self,
        buf: &[u8],
        image_data: Option<&mut UnfilterBuf<'_>>,
    ) -> Result<(usize, Decoded), DecodingError> {
        if self.current.remaining == 0 {
            self.state = State::Crc { filled: 0 };
            return Ok((0, Decoded::Nothing));
        }
        let avail = (self.current.remaining as usize).min(buf.len());
        let Some(sink) = image_data else {
            // The caller does not want image data: consume the payload.
            if avail == 0 {
                if self.input_ended {
                    self.pending_header = None;
                    self.state = State::FlushImageData;
                }
                return Ok((0, Decoded::Nothing));
            }
            if !self.options.ignore_crc {
                self.current.crc.update(&buf[..avail]);
            }
            self.current.remaining -= avail as u32;
            if self.current.remaining == 0 {
                self.state = State::Crc { filled: 0 };
            }
            return Ok((avail, Decoded::Nothing));
        };
        if sink.free_space() == 0 {
            return Ok((0, Decoded::ImageData));
        }
        if avail == 0 {
            if self.input_ended {
                self.pending_header = None;
                self.state = State::FlushImageData;
            }
            return Ok((0, Decoded::Nothing));
        }
        if self.inflater.is_finished() {
            // Bytes after the end of the zlib stream. Consume them; whether
            // they are fatal is decided by the strictness setting.
            self.extra_image_data = true;
            if self.options.error_on_trailing_data {
                return Err(FormatErrorKind::ExtraImageData.into());
            }
            if !self.options.ignore_crc {
                self.current.crc.update(&buf[..avail]);
            }
            self.current.remaining -= avail as u32;
            if self.current.remaining == 0 {
                self.state = State::Crc { filled: 0 };
            }
            return Ok((avail, Decoded::Nothing));
        }
        let before = self.inflater.total_out();
        let progress = self
            .inflater
            .decompress(&buf[..avail], sink.unfilled_mut(), false)?;
        if !self.options.ignore_crc {
            self.current.crc.update(&buf[..progress.consumed]);
        }
        self.current.remaining -= progress.consumed as u32;
        sink.advance(progress.produced);
        let grew = self.inflater.total_out() - before;
        self.frame_budget.charge(grew, &self.options.limits)?;
        if self.current.remaining == 0 {
            self.state = State::Crc { filled: 0 };
        }
        if progress.consumed == 0 && progress.produced == 0 {
            return Ok((0, Decoded::ImageData));
        }
        Ok((progress.consumed, Decoded::ImageData))
    }

    fn step_flush(
        &mut self,
        image_data: Option<&mut UnfilterBuf<'_>>,
    ) -> Result<(usize, Decoded), DecodingError> {
        if let Some(sink) = image_data {
            if sink.free_space() == 0 {
                return Ok((0, Decoded::ImageData));
            }
            let before = self.inflater.total_out();
            let progress = self.inflater.decompress(&[], sink.unfilled_mut(), true)?;
            sink.advance(progress.produced);
            let grew = self.inflater.total_out() - before;
            self.frame_budget.charge(grew, &self.options.limits)?;
            if progress.produced > 0 && !progress.finished {
                return Ok((0, Decoded::ImageData));
            }
            self.finish_image_data_run()?;
            return Ok((0, Decoded::ImageDataFlushed));
        }
        // No sink: find out whether anything is left over, then drop the
        // stream rather than expanding it into a buffer nobody wants.
        if self.inflater.has_started() && !self.inflater.is_finished() {
            let mut probe = [0u8; 1];
            let progress = self.inflater.decompress(&[], &mut probe, true)?;
            if progress.produced > 0 {
                self.extra_image_data = true;
                if self.options.error_on_trailing_data {
                    return Err(FormatErrorKind::ExtraImageData.into());
                }
            }
        }
        self.inflater.reset();
        self.finish_image_data_run()?;
        Ok((0, Decoded::ImageDataFlushed))
    }

    fn finish_image_data_run(&mut self) -> Result<(), DecodingError> {
        self.image_data_active = false;
        self.idat_run_closed = true;
        if let Some(header) = self.pending_header.take() {
            self.begin_chunk(header)?;
        } else {
            self.state = State::Header { filled: 0 };
        }
        Ok(())
    }

    fn step_crc(&mut self, buf: &[u8], filled: u8) -> Result<(usize, Decoded), DecodingError> {
        let want = 4 - usize::from(filled);
        let take = want.min(buf.len());
        if take == 0 {
            if self.input_ended {
                return Err(DecodingError::unexpected_eof());
            }
            return Ok((0, Decoded::Nothing));
        }
        let start = usize::from(filled);
        self.scratch[start..start + take].copy_from_slice(&buf[..take]);
        let filled = filled + take as u8;
        if usize::from(filled) < 4 {
            self.state = State::Crc { filled };
            return Ok((take, Decoded::Nothing));
        }
        let stored = u32::from_be_bytes([
            self.scratch[0],
            self.scratch[1],
            self.scratch[2],
            self.scratch[3],
        ]);
        let kind = self.current.kind;
        let computed = std::mem::replace(&mut self.current.crc, Crc32::new()).finalize();
        let mut bad_crc = false;
        if !self.options.ignore_crc && stored != computed {
            if chunk::is_critical(kind) || !self.options.skip_ancillary_crc_failures {
                return Err(FormatErrorKind::CrcMismatch {
                    crc_val: stored,
                    crc_sum: computed,
                    chunk: kind,
                }
                .into());
            }
            bad_crc = true;
        }
        let event = if bad_crc {
            self.current.data.clear();
            Decoded::BadAncillaryChunk(kind)
        } else {
            self.complete_chunk(kind)?
        };
        self.release_chunk_reservation();
        self.state = if kind == chunk::IEND {
            State::AfterIend
        } else {
            State::Header { filled: 0 }
        };
        Ok((take, event))
    }

    fn step_after_iend(&mut self, buf: &[u8]) -> Result<(usize, Decoded), DecodingError> {
        if buf.is_empty() {
            return Ok((0, Decoded::ImageEnd));
        }
        if self.options.error_on_trailing_data {
            return Err(FormatErrorKind::TrailingData.into());
        }
        Ok((buf.len(), Decoded::ImageEnd))
    }
}

#[cfg(test)]
mod tests;
