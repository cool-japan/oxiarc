//! The pull decoder: [`Decoder`] and [`Reader`].
//!
//! [`Decoder`] takes any [`Read`] — not `BufRead + Seek`, which is what the
//! `png` crate 0.18 requires — reads the header, and hands back a [`Reader`]
//! that produces rows or whole frames. Internally it drives the same
//! [`StreamingDecoder`] the push API exposes, so there is one parser and one
//! set of rules.
//!
//! # Memory
//!
//! Decoding uses one two-row scratch buffer plus whatever the caller supplies.
//! The inflater is never offered more output space than the remainder of the
//! current scanline, and the number of scanlines is fixed by `IHDR`, so the
//! total decompressed size is bounded before a single compressed byte is read.

pub mod stream;
pub mod transform;
pub mod unfilter_buf;
mod zlib;

use std::io::{self, Read};

use crate::common::Transformations;
use crate::error::{DecodingError, FormatErrorKind, ParameterErrorKind};
use crate::filter::{RowFilter, unfilter};
use crate::header::{BitDepth, BytesPerPixel, ColorType};
use crate::info::Info;
use crate::interlace::{Adam7Info, Adam7Iterator, expand_pass};
use crate::limits::{DecodeLimits, Limits};

pub use stream::{ChunkAction, DecodeOptions, Decoded, StreamingDecoder};
pub use transform::output_color_type;
pub use unfilter_buf::{UnfilterBuf, UnfilterRegion};

use transform::Transform;

/// The shape of the buffer [`Reader::next_frame`] filled.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct OutputInfo {
    /// Width of the frame in pixels.
    pub width: u32,
    /// Height of the frame in pixels.
    pub height: u32,
    /// Colour type of the delivered rows.
    pub color_type: ColorType,
    /// Bit depth of the delivered rows.
    pub bit_depth: BitDepth,
    /// Bytes per delivered row.
    pub line_size: usize,
}

impl OutputInfo {
    /// The number of bytes the whole frame occupies.
    #[must_use]
    pub fn buffer_size(&self) -> usize {
        self.line_size * self.height as usize
    }
}

/// Which scanline of which pass a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterlaceInfo {
    /// A plain scanline of a non-interlaced image, with its row index.
    Null(u32),
    /// One row of an Adam7 pass.
    Adam7(Adam7Info),
}

/// A de-interlaced row.
#[derive(Debug)]
pub struct Row<'a> {
    data: &'a [u8],
}

impl Row<'_> {
    /// The row's bytes, in the output colour type and depth.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data
    }
}

/// A row as it was stored, together with its place in the interlace pattern.
#[derive(Debug)]
pub struct InterlacedRow<'a> {
    data: &'a [u8],
    interlace: InterlaceInfo,
}

impl InterlacedRow<'_> {
    /// The row's bytes, in the output colour type and depth.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// Where the row belongs in the image.
    #[must_use]
    pub fn interlace(&self) -> &InterlaceInfo {
        &self.interlace
    }
}

/// Reads from an [`std::io::Read`] and drives the push decoder.
#[derive(Debug)]
struct ReadDecoder<R: Read> {
    reader: R,
    decoder: StreamingDecoder,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
}

impl<R: Read> ReadDecoder<R> {
    fn new(reader: R, decoder: StreamingDecoder) -> ReadDecoder<R> {
        ReadDecoder {
            reader,
            decoder,
            buf: vec![0u8; 32 * 1024],
            pos: 0,
            len: 0,
            eof: false,
        }
    }

    fn fill(&mut self) -> Result<(), DecodingError> {
        loop {
            match self.reader.read(&mut self.buf) {
                Ok(0) => {
                    self.pos = 0;
                    self.len = 0;
                    self.eof = true;
                    self.decoder.notify_eof();
                    return Ok(());
                }
                Ok(n) => {
                    self.pos = 0;
                    self.len = n;
                    return Ok(());
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(DecodingError::IoError(err)),
            }
        }
    }

    fn decode_next(
        &mut self,
        mut image_data: Option<&mut UnfilterBuf<'_>>,
    ) -> Result<Decoded, DecodingError> {
        loop {
            if self.pos == self.len && !self.eof {
                self.fill()?;
            }
            let sink = image_data.as_deref_mut();
            let (consumed, event) = self.decoder.update(&self.buf[self.pos..self.len], sink)?;
            self.pos += consumed;
            if event != Decoded::Nothing {
                return Ok(event);
            }
            if consumed == 0 && self.eof {
                return Err(DecodingError::unexpected_eof());
            }
        }
    }

    fn read_until_image_data(&mut self) -> Result<(), DecodingError> {
        loop {
            match self.decode_next(None)? {
                Decoded::ChunkBegin(_, kind)
                    if kind == crate::chunk::IDAT || kind == crate::chunk::fdAT =>
                {
                    return Ok(());
                }
                Decoded::ImageEnd => return Err(FormatErrorKind::MissingImageData.into()),
                _ => {}
            }
        }
    }

    fn read_until_end_of_input(&mut self) -> Result<(), DecodingError> {
        loop {
            if self.decoder.is_finished() {
                return Ok(());
            }
            match self.decode_next(None) {
                Ok(Decoded::ImageEnd) => return Ok(()),
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
    }

    fn info(&self) -> Option<&Info<'static>> {
        self.decoder.info()
    }
}

/// The entry point for decoding.
///
/// ```no_run
/// use std::fs::File;
/// let decoder = oxiarc_png::Decoder::new(File::open("image.png")?);
/// let mut reader = decoder.read_info()?;
/// let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
/// let info = reader.next_frame(&mut buf)?;
/// println!("{}x{} {:?}", info.width, info.height, info.color_type);
/// # Ok::<(), oxiarc_png::DecodingError>(())
/// ```
#[derive(Debug)]
pub struct Decoder<R: Read> {
    read_decoder: ReadDecoder<R>,
    transform: Transformations,
}

impl<R: Read> Decoder<R> {
    /// A decoder with the default options and limits.
    pub fn new(r: R) -> Decoder<R> {
        Decoder::new_with_options(r, DecodeOptions::default())
    }

    /// A decoder with an explicit memory budget.
    pub fn new_with_limits(r: R, limits: Limits) -> Decoder<R> {
        let mut decoder = Decoder::new(r);
        decoder.set_limits(limits);
        decoder
    }

    /// A decoder with explicit options.
    pub fn new_with_options(r: R, options: DecodeOptions) -> Decoder<R> {
        Decoder {
            read_decoder: ReadDecoder::new(r, StreamingDecoder::new_with_options(options)),
            transform: Transformations::IDENTITY,
        }
    }

    /// Replace the memory budget.
    pub fn set_limits(&mut self, limits: Limits) {
        self.read_decoder.decoder.set_limits(limits);
    }

    /// Replace the full size limits.
    pub fn set_decode_limits(&mut self, limits: DecodeLimits) {
        self.read_decoder.decoder.set_decode_limits(limits);
    }

    /// Request output transformations.
    pub fn set_transformations(&mut self, transform: Transformations) {
        self.transform = transform;
    }

    /// Do not parse the text chunks.
    pub fn set_ignore_text_chunk(&mut self, ignore: bool) {
        self.read_decoder.decoder.set_ignore_text_chunk(ignore);
    }

    /// Do not parse `iCCP`.
    pub fn set_ignore_iccp_chunk(&mut self, ignore: bool) {
        self.read_decoder.decoder.set_ignore_iccp_chunk(ignore);
    }

    /// Skip both the chunk CRCs and the zlib Adler-32.
    pub fn ignore_checksums(&mut self, ignore: bool) {
        self.read_decoder.decoder.set_ignore_crc(ignore);
        self.read_decoder.decoder.set_ignore_adler32(ignore);
    }

    /// Read every chunk up to the first image-data chunk and return the
    /// metadata gathered so far.
    ///
    /// This matches the `png` crate: metadata that only appears *after* the
    /// image data — `tIME`, text chunks, `eXIf` — is not included, because
    /// reading it would mean decoding the whole image.
    ///
    /// # Errors
    ///
    /// A structural problem before the image data, or a file with no image
    /// data at all.
    pub fn read_header_info(&mut self) -> Result<&Info<'static>, DecodingError> {
        if !self.read_decoder.decoder.in_image_data() {
            self.read_decoder.read_until_image_data()?;
        }
        self.read_decoder.info().ok_or_else(|| {
            FormatErrorKind::ChunkBeforeIhdr {
                kind: crate::chunk::IHDR,
            }
            .into()
        })
    }

    /// Read every chunk before the image data and return a row reader.
    ///
    /// # Errors
    ///
    /// A structural problem before the image data, or a limit being exceeded.
    pub fn read_info(mut self) -> Result<Reader<R>, DecodingError> {
        self.read_header_info()?;
        Reader::new(self.read_decoder, self.transform)
    }
}

/// How many channels an Apple `CgBI` row stores per pixel, when the file needs
/// its blue and red channels swapped back.
///
/// `CgBI` files store 8-bit colour as BGR(A) rather than RGB(A). Sub-byte,
/// 16-bit and palette images are left alone: Apple's tooling only produces the
/// 8-bit colour forms, and guessing at the others would corrupt conformant
/// data that merely carries a stray `CgBI` chunk.
fn cgbi_channels(info: &Info<'_>) -> Option<usize> {
    if info.cgbi.is_none() || info.bit_depth != BitDepth::Eight {
        return None;
    }
    match info.color_type {
        ColorType::Rgb => Some(3),
        ColorType::Rgba => Some(4),
        _ => None,
    }
}

/// Swap the first and third channel of every pixel of a row in place.
fn swap_bgr_in_place(row: &mut [u8], channels: usize) {
    for pixel in row.chunks_exact_mut(channels) {
        pixel.swap(0, 2);
    }
}

/// Where the reader is in the current frame.
#[derive(Debug)]
enum RowIter {
    Null { y: u32, height: u32 },
    Adam7(Box<Adam7Iterator>),
}

impl RowIter {
    fn next(&mut self) -> Option<InterlaceInfo> {
        match self {
            RowIter::Null { y, height } => {
                if *y >= *height {
                    None
                } else {
                    let info = InterlaceInfo::Null(*y);
                    *y += 1;
                    Some(info)
                }
            }
            RowIter::Adam7(iter) => iter.next().map(InterlaceInfo::Adam7),
        }
    }
}

/// Produces rows and frames.
#[derive(Debug)]
pub struct Reader<R: Read> {
    decoder: ReadDecoder<R>,
    transform: Transformations,
    transform_op: Transform,
    output: (ColorType, BitDepth),
    bpp: BytesPerPixel,
    width: u32,
    height: u32,
    interlaced: bool,
    max_raw_row: usize,
    scratch: Vec<u8>,
    filled: usize,
    second_is_current: bool,
    prev_valid: bool,
    row_buffer: Vec<u8>,
    row_buffer_len: usize,
    rows: RowIter,
    header_info: Info<'static>,
    cgbi_channels: Option<usize>,
    frame_done: bool,
    remaining_frames: usize,
    frames_emitted: usize,
    finished: bool,
}

impl<R: Read> Reader<R> {
    fn new(
        mut decoder: ReadDecoder<R>,
        transform: Transformations,
    ) -> Result<Reader<R>, DecodingError> {
        let info = decoder
            .info()
            .ok_or(FormatErrorKind::ChunkBeforeIhdr {
                kind: crate::chunk::IHDR,
            })?
            .clone();
        let transform_op = transform::create_transform(&info, transform)?;
        let output = output_color_type(
            info.color_type,
            info.bit_depth,
            info.trns.is_some(),
            transform,
        );
        let remaining_frames = if info.animation_control.is_some() {
            let extra = usize::from(!decoder.decoder.apng.fctl_before_idat);
            decoder.decoder.apng.frame_count() as usize + extra
        } else {
            1
        };
        let max_raw_row = info.raw_row_length();
        let scratch_len = max_raw_row
            .checked_mul(2)
            .ok_or(DecodingError::LimitsExceeded)?;
        decoder.decoder.limits.reserve_bytes(scratch_len)?;
        let mut reader = Reader {
            decoder,
            transform,
            transform_op,
            output,
            bpp: info.bpp_in_prediction(),
            width: info.width,
            height: info.height,
            interlaced: info.interlaced,
            max_raw_row,
            scratch: vec![0u8; scratch_len],
            filled: 0,
            second_is_current: false,
            prev_valid: false,
            row_buffer: Vec::new(),
            row_buffer_len: 0,
            rows: RowIter::Null { y: 0, height: 0 },
            header_info: info.clone(),
            cgbi_channels: cgbi_channels(&info),
            frame_done: false,
            remaining_frames,
            frames_emitted: 0,
            finished: false,
        };
        reader.start_frame(info.width, info.height, info.interlaced);
        Ok(reader)
    }

    fn start_frame(&mut self, width: u32, height: u32, interlaced: bool) {
        self.width = width;
        self.height = height;
        self.interlaced = interlaced;
        self.rows = if interlaced {
            RowIter::Adam7(Box::new(Adam7Iterator::new(width, height)))
        } else {
            RowIter::Null { y: 0, height }
        };
        self.filled = 0;
        self.second_is_current = false;
        self.prev_valid = false;
        self.frame_done = false;
    }

    /// The metadata read so far.
    ///
    /// This is live: chunks that appear after the image data land here once
    /// [`Reader::finish`] has run.
    #[must_use]
    pub fn info(&self) -> &Info<'static> {
        // The header was parsed before this type could be constructed, so the
        // fallback is unreachable; it exists so that the accessor cannot
        // panic even if that invariant is ever broken.
        self.decoder.info().unwrap_or(&self.header_info)
    }

    /// The colour type and bit depth rows are delivered in.
    #[must_use]
    pub fn output_color_type(&self) -> (ColorType, BitDepth) {
        self.output
    }

    /// The number of bytes one delivered row of `width` pixels occupies.
    #[must_use]
    pub fn output_line_size(&self, width: u32) -> Option<usize> {
        let (color, depth) = self.output;
        let len = color.checked_raw_row_length(depth, width)?.checked_sub(1)?;
        (len <= isize::MAX as usize).then_some(len)
    }

    /// The number of bytes a whole frame occupies.
    #[must_use]
    pub fn output_buffer_size(&self) -> Option<usize> {
        let line = self.output_line_size(self.width)?;
        let total = line.checked_mul(usize::try_from(self.height).ok()?)?;
        (total <= isize::MAX as usize).then_some(total)
    }

    /// The number of bytes a whole frame occupies, checked against
    /// [`DecodeLimits::max_alloc_bytes`].
    ///
    /// [`Reader::output_buffer_size`] is plain arithmetic: for a hostile
    /// header it will report a size no machine can allocate. This is the
    /// accessor to use immediately before `vec![0; n]`, and it is what
    /// [`crate::decode`] uses. Row-by-row decoding through
    /// [`Reader::next_row`] never needs a frame-sized buffer and is
    /// deliberately not subject to this bound, so a very large image can still
    /// be streamed under the default limits.
    ///
    /// [`DecodeLimits::max_alloc_bytes`]: crate::DecodeLimits::max_alloc_bytes
    ///
    /// # Errors
    ///
    /// The frame size does not fit in `usize`, or exceeds the configured
    /// allocation limit.
    pub fn checked_output_buffer_size(&self) -> Result<usize, DecodingError> {
        let size = self
            .output_buffer_size()
            .ok_or(DecodingError::LimitsExceeded)?;
        self.check_alloc(size as u64)
    }

    /// Check an arbitrary allocation size against this reader's configured
    /// [`DecodeLimits::max_alloc_bytes`], the same bound
    /// [`Reader::checked_output_buffer_size`] applies to the frame buffer.
    ///
    /// Exists so other in-crate allocators driven by this reader's header
    /// fields — the APNG compositor's canvas, in particular, which is
    /// several times the size of one frame buffer — are gated by the same
    /// limit rather than trusting `IHDR`'s width/height unchecked.
    ///
    /// [`DecodeLimits::max_alloc_bytes`]: crate::DecodeLimits::max_alloc_bytes
    pub(crate) fn check_alloc(&self, bytes: u64) -> Result<usize, DecodingError> {
        self.decoder.decoder.options().limits().check_alloc(bytes)
    }

    /// The requested transformations.
    #[must_use]
    pub fn transformations(&self) -> Transformations {
        self.transform
    }

    fn raw_row_len(&self, interlace: &InterlaceInfo) -> usize {
        let info = self.info();
        match interlace {
            InterlaceInfo::Null(_) => info.raw_row_length_from_width(self.width),
            InterlaceInfo::Adam7(a) => info.raw_row_length_from_width(a.samples),
        }
    }

    fn out_row_len(&self, interlace: &InterlaceInfo) -> Option<usize> {
        match interlace {
            InterlaceInfo::Null(_) => self.output_line_size(self.width),
            InterlaceInfo::Adam7(a) => self.output_line_size(a.samples),
        }
    }

    fn fill_row(&mut self, rowlen: usize) -> Result<(), DecodingError> {
        let base = if self.second_is_current {
            self.max_raw_row
        } else {
            0
        };
        let mut stalls = 0u32;
        while self.filled < rowlen {
            let produced;
            let event;
            {
                let Reader {
                    decoder,
                    scratch,
                    filled,
                    ..
                } = self;
                let window = &mut scratch[base + *filled..base + rowlen];
                let mut local = 0usize;
                let mut sink = UnfilterBuf::new(window, &mut local);
                event = decoder.decode_next(Some(&mut sink))?;
                produced = sink.filled();
            }
            self.filled += produced;
            if produced == 0 {
                stalls += 1;
                if stalls > 64 {
                    return Err(FormatErrorKind::NoMoreImageData.into());
                }
            } else {
                stalls = 0;
            }
            if matches!(event, Decoded::ImageDataFlushed | Decoded::ImageEnd)
                && self.filled < rowlen
            {
                return Err(FormatErrorKind::NoMoreImageData.into());
            }
        }
        Ok(())
    }

    /// Read the next row into `out`, returning where it belongs.
    ///
    /// `out` must be at least [`Reader::output_line_size`] bytes for the row's
    /// pixel count; for an interlaced image that is the pass width, not the
    /// image width.
    pub fn read_row(&mut self, out: &mut [u8]) -> Result<Option<InterlaceInfo>, DecodingError> {
        if self.finished || self.frame_done {
            return Ok(None);
        }
        let Some(interlace) = self.rows.next() else {
            self.frame_done = true;
            return Ok(None);
        };
        if let InterlaceInfo::Adam7(a) = &interlace {
            if a.line == 0 {
                self.prev_valid = false;
            }
        }
        let rowlen = self.raw_row_len(&interlace);
        let out_len = self
            .out_row_len(&interlace)
            .ok_or(DecodingError::LimitsExceeded)?;
        if out.len() < out_len {
            return Err(ParameterErrorKind::ImageBufferSize {
                expected: out_len,
                actual: out.len(),
            }
            .into());
        }
        self.fill_row(rowlen)?;

        let (first, second) = self.scratch.split_at_mut(self.max_raw_row);
        let (cur, other) = if self.second_is_current {
            (second, first)
        } else {
            (first, second)
        };
        let filter = RowFilter::from_u8(cur[0])?;
        let previous: &[u8] = if self.prev_valid {
            &other[1..rowlen]
        } else {
            &[]
        };
        unfilter(filter, self.bpp, previous, &mut cur[1..rowlen]);
        if let Some(channels) = self.cgbi_channels {
            swap_bgr_in_place(&mut cur[1..rowlen], channels);
        }
        let strict = self.decoder.decoder.options.strict_palette_indices();
        self.transform_op
            .apply(&cur[1..rowlen], &mut out[..out_len], strict)?;

        self.second_is_current = !self.second_is_current;
        self.prev_valid = true;
        self.filled = 0;
        Ok(Some(interlace))
    }

    /// Read the next row, de-interlaced, into an internal buffer.
    pub fn next_row(&mut self) -> Result<Option<Row<'_>>, DecodingError> {
        match self.read_row_into_buffer()? {
            None => Ok(None),
            Some(_) => Ok(Some(Row {
                data: &self.row_buffer[..self.row_buffer_len],
            })),
        }
    }

    /// Read the next row together with its interlace position.
    pub fn next_interlaced_row(&mut self) -> Result<Option<InterlacedRow<'_>>, DecodingError> {
        match self.read_row_into_buffer()? {
            None => Ok(None),
            Some(interlace) => Ok(Some(InterlacedRow {
                data: &self.row_buffer[..self.row_buffer_len],
                interlace,
            })),
        }
    }

    fn read_row_into_buffer(&mut self) -> Result<Option<InterlaceInfo>, DecodingError> {
        let capacity = self
            .output_line_size(self.width)
            .ok_or(DecodingError::LimitsExceeded)?;
        let mut buffer = std::mem::take(&mut self.row_buffer);
        if buffer.len() < capacity {
            buffer.resize(capacity, 0);
        }
        let result = self.read_row(&mut buffer);
        self.row_buffer = buffer;
        match result? {
            None => {
                self.row_buffer_len = 0;
                Ok(None)
            }
            Some(interlace) => {
                self.row_buffer_len = self
                    .out_row_len(&interlace)
                    .ok_or(DecodingError::LimitsExceeded)?;
                Ok(Some(interlace))
            }
        }
    }

    /// Decode a whole frame into `buf`.
    ///
    /// `buf` must be at least [`Reader::output_buffer_size`] bytes.
    pub fn next_frame(&mut self, buf: &mut [u8]) -> Result<OutputInfo, DecodingError> {
        if self.finished {
            return Err(ParameterErrorKind::PolledAfterEndOfImage.into());
        }
        if self.frames_emitted > 0 {
            self.advance_to_next_frame()?;
        }
        let line_size = self
            .output_line_size(self.width)
            .ok_or(DecodingError::LimitsExceeded)?;
        let height = usize::try_from(self.height).map_err(|_| DecodingError::LimitsExceeded)?;
        let required = line_size
            .checked_mul(height)
            .ok_or(DecodingError::LimitsExceeded)?;
        if buf.len() < required {
            return Err(ParameterErrorKind::ImageBufferSize {
                expected: required,
                actual: buf.len(),
            }
            .into());
        }
        if self.interlaced {
            buf[..required].fill(0);
            let (color, depth) = self.output;
            let bits_per_pixel = color.samples_u8() * (depth as u8);
            let capacity = line_size;
            let mut row = std::mem::take(&mut self.row_buffer);
            if row.len() < capacity {
                row.resize(capacity, 0);
            }
            loop {
                let step = self.read_row(&mut row);
                let Some(interlace) = (match step {
                    Ok(value) => value,
                    Err(err) => {
                        self.row_buffer = row;
                        return Err(err);
                    }
                }) else {
                    break;
                };
                if let InterlaceInfo::Adam7(a) = interlace {
                    let len = self.output_line_size(a.samples).unwrap_or(0);
                    expand_pass(
                        &mut buf[..required],
                        line_size,
                        &row[..len],
                        &a,
                        bits_per_pixel,
                    );
                }
            }
            self.row_buffer = row;
        } else {
            let mut y = 0usize;
            while y < height {
                let start = y * line_size;
                let Some(_) = self.read_row(&mut buf[start..start + line_size])? else {
                    break;
                };
                y += 1;
            }
            if y < height {
                return Err(FormatErrorKind::NoMoreImageData.into());
            }
        }
        self.finish_frame()?;
        self.frames_emitted += 1;
        if self.frames_emitted >= self.remaining_frames {
            self.finished = true;
        }
        let (color_type, bit_depth) = self.output;
        Ok(OutputInfo {
            width: self.width,
            height: self.height,
            color_type,
            bit_depth,
            line_size,
        })
    }

    /// Advance to the next animation frame and return its control chunk.
    pub fn next_frame_info(&mut self) -> Result<&crate::FrameControl, DecodingError> {
        if self.info().animation_control.is_none() {
            return Err(ParameterErrorKind::NotAnimated.into());
        }
        loop {
            match self.decoder.decode_next(None)? {
                Decoded::FrameControl(_) => break,
                Decoded::ImageEnd => return Err(ParameterErrorKind::PolledAfterEndOfImage.into()),
                _ => {}
            }
        }
        let frame = self
            .info()
            .frame_control
            .ok_or(ParameterErrorKind::NotAnimated)?;
        self.start_frame(frame.width, frame.height, self.info().interlaced);
        self.info()
            .frame_control
            .as_ref()
            .ok_or_else(|| ParameterErrorKind::NotAnimated.into())
    }

    fn advance_to_next_frame(&mut self) -> Result<(), DecodingError> {
        self.next_frame_info()?;
        Ok(())
    }

    /// After the last row, check whether the compressed stream had more to say.
    fn finish_frame(&mut self) -> Result<(), DecodingError> {
        let mut probe = [0u8; 1];
        let mut filled = 0usize;
        let probed = {
            let mut sink = UnfilterBuf::new(&mut probe, &mut filled);
            // One step is enough: either the stream is done, or it produced a
            // byte the image header did not account for.
            self.decoder.decode_next(Some(&mut sink))
        };
        let strict = self.decoder.decoder.options.strict();
        // The frame the caller asked for is already complete and correct, so a
        // fault found while looking past its end is reported only under
        // `strict`; lenient mode keeps the good frame rather than discarding it
        // over a damaged trailer.
        if strict {
            probed?;
        }
        let leftover = filled > 0 || self.decoder.decoder.saw_extra_image_data();
        if leftover && strict {
            return Err(FormatErrorKind::ExtraImageData.into());
        }
        Ok(())
    }

    /// Read the rest of the file, so trailing chunks land in [`Reader::info`].
    pub fn finish(&mut self) -> Result<(), DecodingError> {
        if self.finished && self.decoder.decoder.is_finished() {
            return Ok(());
        }
        self.finished = true;
        self.decoder.read_until_end_of_input()
    }

    /// The push decoder underneath, for callers that need chunk-level access.
    #[must_use]
    pub fn streaming_decoder(&self) -> &StreamingDecoder {
        &self.decoder.decoder
    }
}
