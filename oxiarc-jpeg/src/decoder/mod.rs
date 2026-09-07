//! The public decoding API.

#[cfg(feature = "arithmetic")]
mod arith;
mod engine;
mod lossless;
mod output;
#[cfg(feature = "rayon")]
mod parallel;
mod planes;
mod progressive;
mod scan;

use std::io::Read;

use engine::Engine;

use crate::color::ColorSpace;
use crate::error::{JpegError, LimitKind, Result};
use crate::frame::{CodingProcess, EntropyCoding, FrameHeader};
use crate::limits::DecodeLimits;
use crate::metadata::{AdobeHeader, AppSegment, JfifHeader};
use crate::tableset::TableSet;

/// Which chroma upsampling kernel to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Upsampling {
    /// libjpeg's triangle filter for the `h2v1`, `h1v2` and `h2v2` ratios,
    /// replication for every other ratio. This is `djpeg`'s default.
    #[default]
    Fancy,
    /// Sample replication for every ratio, matching `djpeg -nosmooth`.
    Box,
}

/// A reduced- or enlarged-scale IDCT output size: libjpeg's `-scale M/N`
/// with `N` fixed at 8, the DCT block size.
///
/// Each `8x8` block of decoded coefficients reconstructs to `M x M` output
/// samples instead of the native `8x8`, through the same scaled-IDCT
/// machinery libjpeg uses — there is no separate resize pass, and the
/// entropy decode is completely unaffected: every coefficient is still
/// decoded, only the final reconstruction changes. `M` in `1..=4` shrinks the
/// image (`1/8`, `1/4`, `3/8`, `1/2`, ...); `M == 8` is native resolution;
/// `M` in `9..=16` enlarges it.
///
/// The frame's output dimensions are `ceil(width * M / 8)` and
/// `ceil(height * M / 8)`, exactly as libjpeg computes them — see
/// [`ImageInfo::scaled_width`] and [`ImageInfo::scaled_height`].
///
/// Byte parity with `djpeg -dct int -scale M/8` is verified for
/// `M` in `{1, 2, 4, 8}`, which libjpeg reconstructs with dedicated `1x1`,
/// `2x2`, `4x4` and (the native, unscaled) `8x8` kernels. The other twelve
/// values each get their own hand-derived fixed-point kernel in libjpeg
/// (`jidctint.c`'s `jpeg_idct_3x3` .. `jpeg_idct_16x16`); this crate uses one
/// general kernel for all of them instead and verifies it against `djpeg`
/// to a numeric tolerance rather than byte parity — see the crate's
/// `README.md` for the measured error and the reasoning.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::Scale;
///
/// assert_eq!(Scale::default(), Scale::FULL);
/// assert_eq!(Scale::FULL.numerator(), 8);
/// assert_eq!(Scale::ONE_QUARTER.numerator(), 2);
/// assert_eq!(Scale::new(3)?.numerator(), 3);
/// assert!(Scale::new(0).is_err());
/// assert!(Scale::new(17).is_err());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scale {
    numerator: u8,
}

impl Scale {
    /// `8/8`: native resolution. The default, and what every release before
    /// this option existed always produced.
    pub const FULL: Scale = Scale { numerator: 8 };
    /// `4/8` = `1/2`.
    pub const ONE_HALF: Scale = Scale { numerator: 4 };
    /// `2/8` = `1/4`.
    pub const ONE_QUARTER: Scale = Scale { numerator: 2 };
    /// `1/8`.
    pub const ONE_EIGHTH: Scale = Scale { numerator: 1 };

    /// `numerator / 8`, matching libjpeg's `-scale numerator/8`.
    ///
    /// # Errors
    ///
    /// [`JpegError::InvalidDecodeParameter`] when `numerator` is `0` or
    /// greater than `16` — the range every reduced- and enlarged-scale IDCT
    /// kernel libjpeg defines covers, and the only range this crate
    /// implements a kernel for.
    pub fn new(numerator: u8) -> Result<Self> {
        if (1..=16).contains(&numerator) {
            Ok(Self { numerator })
        } else {
            Err(JpegError::InvalidDecodeParameter {
                parameter: "scale",
                reason: "numerator must be 1..=16 (libjpeg's -scale M/8)",
            })
        }
    }

    /// The `M` in `M/8`, always `1..=16`.
    #[must_use]
    pub const fn numerator(self) -> u8 {
        self.numerator
    }

    /// Always `8`: this crate exposes libjpeg's `-scale M/N` only for the
    /// `N == 8` family (every `M` libjpeg itself defines a kernel for).
    #[must_use]
    pub const fn denominator(self) -> u8 {
        8
    }

    /// `true` for [`Scale::FULL`], the only value that changes nothing.
    #[must_use]
    pub const fn is_full(self) -> bool {
        self.numerator == 8
    }
}

impl Default for Scale {
    fn default() -> Self {
        Scale::FULL
    }
}

/// `ceil(dim * numerator / 8)`, libjpeg's `jdiv_round_up` applied to its
/// `output_width`/`output_height` formula. `numerator` is a [`Scale`]'s, so
/// `1..=16`; `dim` is a frame's `width` or `height`, so the product fits
/// comfortably in `u32` (`0xFFFF * 16 = 0xFFFF0`).
pub(crate) const fn scaled_dim(dim: u16, numerator: u8) -> u32 {
    (dim as u32 * numerator as u32).div_ceil(8)
}

/// Options that control one decode.
///
/// The struct is deliberately *not* `#[non_exhaustive]` so that callers can
/// write `DecodeOptions { raw_components: true, ..Default::default() }`, which
/// is how `oxiarc-tiff` drives it.
#[derive(Debug, Clone)]
pub struct DecodeOptions {
    /// Resource caps applied to this decode.
    pub limits: DecodeLimits,
    /// Forced output colour space. `None` applies libjpeg's default: YCbCr
    /// becomes RGB, YCCK becomes CMYK, everything else passes through.
    pub output_color_space: Option<ColorSpace>,
    /// Deliver interleaved component samples with no colour transform.
    ///
    /// Chroma is still upsampled to full resolution, so the output has one
    /// sample per component per pixel. This is what a TIFF reader wants,
    /// because TIFF owns the colour pipeline through
    /// `PhotometricInterpretation`, `ReferenceBlackWhite` and
    /// `YCbCrCoefficients`.
    pub raw_components: bool,
    /// Which upsampling kernel to use.
    pub upsampling: Upsampling,
    /// Reduced- or enlarged-scale decode via the scaled IDCT, libjpeg's
    /// `-scale M/8`. [`Scale::FULL`] (the default) decodes at native
    /// resolution, unchanged from every release before this option existed.
    ///
    /// Ignored for a lossless frame (`SOF3`/`SOF11`): T.81's lossless mode has
    /// no DCT to scale, and libjpeg hardwires "no scaling" for it too, so a
    /// lossless decode always produces `width x height` regardless of this
    /// field.
    pub scale: Scale,
    /// Return what was decoded when the entropy data ends early, instead of
    /// reporting [`JpegError::UnexpectedEof`].
    pub tolerate_truncated: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            limits: DecodeLimits::default(),
            output_color_space: None,
            raw_components: false,
            upsampling: Upsampling::Fancy,
            scale: Scale::FULL,
            tolerate_truncated: false,
        }
    }
}

impl DecodeOptions {
    /// Options tuned for untrusted input: [`DecodeLimits::strict`].
    #[must_use]
    pub fn strict() -> Self {
        Self {
            limits: DecodeLimits::strict(),
            ..Self::default()
        }
    }

    /// Options that deliver raw component samples, as `oxiarc-tiff` uses.
    #[must_use]
    pub fn raw() -> Self {
        Self {
            raw_components: true,
            ..Self::default()
        }
    }
}

/// One component's identity and sampling, as the `SOF` declared it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentInfo {
    /// Component identifier `Ci`.
    pub id: u8,
    /// Horizontal sampling factor `Hi`.
    pub h: u8,
    /// Vertical sampling factor `Vi`.
    pub v: u8,
    /// Quantisation table selector `Tqi`.
    pub quant_table: u8,
}

/// What a decoder learned from the frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Frame width `X`, as the `SOF` (and `DNL`, if any) declared it —
    /// **not** affected by [`DecodeOptions::scale`]. Use
    /// [`ImageInfo::scaled_width`] to size a decode buffer.
    pub width: u16,
    /// Frame height `Y`, already resolved through `DNL` if the `SOF` said 0.
    /// Like [`ImageInfo::width`], unaffected by [`DecodeOptions::scale`].
    pub height: u16,
    /// Output width once [`DecodeOptions::scale`] is applied:
    /// `ceil(width * scale.numerator() / 8)`, libjpeg's `output_width`. Equal
    /// to `width` at [`Scale::FULL`], and always equal to `width` for a
    /// lossless frame (`scale` is ignored there — see
    /// [`DecodeOptions::scale`]).
    ///
    /// This is the width a decode actually produces: what
    /// [`Decoder::output_buffer_size`], [`Decoder::decode`] and
    /// [`Decoder::decode_into_strided`]'s row length are all sized from.
    pub scaled_width: u32,
    /// The height counterpart of [`ImageInfo::scaled_width`].
    pub scaled_height: u32,
    /// Sample precision `P`.
    pub precision: u8,
    /// Number of components `Nf`.
    pub num_components: u8,
    /// Per-component identity and sampling; only the first `num_components`
    /// entries are meaningful. Use [`ImageInfo::components`].
    components: [ComponentInfo; 4],
    /// The colour space the components are believed to be in.
    pub input_color_space: ColorSpace,
    /// The colour space a decode will produce.
    pub output_color_space: ColorSpace,
    /// Which coding process the `SOF` selected.
    pub process: CodingProcess,
    /// Which entropy coder the `SOF` selected.
    pub entropy: EntropyCoding,
    /// The Adobe `APP14` transform code, if that marker was present.
    pub adobe_transform: Option<u8>,
    /// `true` when a `JFIF` `APP0` marker was present.
    pub has_jfif: bool,
    /// `true` when an `Adobe` `APP14` marker was present.
    pub has_adobe: bool,
    /// The most recent `DRI` interval seen so far, or 0 if none.
    ///
    /// libjpeg and libtiff emit `DRI` *after* `SOF`, in the scan header, so
    /// this is usually 0 in the [`ImageInfo`] that [`Decoder::read_info`]
    /// returns and non-zero in the one [`Decoder::info`] returns after
    /// [`Decoder::decode`]. Read it after decoding, or read
    /// [`crate::TableSet::restart_interval`] when the tables arrive
    /// out of band as TIFF's tag 347 does.
    pub restart_interval: u16,
    /// `(Hmax, Vmax)` over all components — the frame's subsampling grid.
    pub subsampling: (u8, u8),
}

impl ImageInfo {
    /// The components this frame declares.
    #[must_use]
    pub fn components(&self) -> &[ComponentInfo] {
        &self.components[..usize::from(self.num_components).min(4)]
    }

    /// Samples per output pixel once the colour transform has been applied.
    #[must_use]
    pub fn output_components(&self) -> usize {
        let n = self.output_color_space.num_components();
        if n == 0 {
            usize::from(self.num_components)
        } else {
            n
        }
    }

    /// The pixel format a decode will produce.
    #[must_use]
    pub fn pixel_format(&self) -> PixelFormat {
        let wide = self.precision > 8;
        match self.output_color_space {
            ColorSpace::Luma if !wide => PixelFormat::L8,
            ColorSpace::Luma => PixelFormat::L16,
            ColorSpace::Rgb if !wide => PixelFormat::Rgb8,
            ColorSpace::Rgb => PixelFormat::Rgb16,
            ColorSpace::Cmyk if !wide => PixelFormat::Cmyk8,
            ColorSpace::Cmyk => PixelFormat::Cmyk16,
            _ if wide => PixelFormat::Raw16(self.output_components() as u8),
            _ => PixelFormat::Raw8(self.output_components() as u8),
        }
    }
}

/// The layout a decode writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PixelFormat {
    /// One 8-bit luminance sample per pixel.
    L8,
    /// One 16-bit luminance sample per pixel.
    L16,
    /// Three 8-bit samples per pixel, red first.
    Rgb8,
    /// Three 16-bit samples per pixel, red first.
    Rgb16,
    /// Four 8-bit samples per pixel: cyan, magenta, yellow, black.
    Cmyk8,
    /// Four 16-bit samples per pixel: cyan, magenta, yellow, black.
    Cmyk16,
    /// Untransformed 8-bit component samples, interleaved.
    Raw8(u8),
    /// Untransformed 16-bit component samples, interleaved.
    Raw16(u8),
}

impl PixelFormat {
    /// Samples per pixel.
    #[must_use]
    pub const fn components(self) -> usize {
        match self {
            PixelFormat::L8 | PixelFormat::L16 => 1,
            PixelFormat::Rgb8 | PixelFormat::Rgb16 => 3,
            PixelFormat::Cmyk8 | PixelFormat::Cmyk16 => 4,
            PixelFormat::Raw8(n) | PixelFormat::Raw16(n) => n as usize,
        }
    }

    /// `true` when samples do not fit in a byte.
    #[must_use]
    pub const fn is_wide(self) -> bool {
        matches!(
            self,
            PixelFormat::L16 | PixelFormat::Rgb16 | PixelFormat::Cmyk16 | PixelFormat::Raw16(_)
        )
    }
}

/// A pull decoder over any [`Read`] source.
///
/// # Buffering
///
/// The source is read into memory in one pass before parsing, bounded by
/// [`DecodeLimits::max_input_bytes`] (1 GiB by default). JPEG entropy decoding
/// needs random access to the scan data — progressive frames revisit every
/// block once per scan — so a streaming parse would have to buffer the same
/// bytes anyway. Slice callers that already hold the data can use
/// [`decode_abbreviated_into`] and avoid the copy entirely.
pub struct Decoder<R: Read> {
    reader: R,
    data: Vec<u8>,
    filled: bool,
    engine: Engine,
}

impl<R: Read> Decoder<R> {
    /// A decoder with default options.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
    /// use oxiarc_jpeg::Decoder;
    ///
    /// // A one-pixel grey JPEG produced by the crate's own test helpers.
    /// let bytes: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1;
    /// let mut decoder = Decoder::new(bytes);
    /// let info = decoder.read_info()?;
    /// assert_eq!((info.width, info.height), (1, 1));
    /// assert_eq!(decoder.decode()?.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(reader: R) -> Self {
        Self::with_options(reader, DecodeOptions::default())
    }

    /// A decoder with explicit options.
    pub fn with_options(reader: R, options: DecodeOptions) -> Self {
        Self {
            reader,
            data: Vec::new(),
            filled: false,
            engine: Engine::new(options),
        }
    }

    /// Prime the decoder with out-of-band tables, as TIFF's `JPEGTables` tag
    /// supplies them.
    ///
    /// Tables the datastream itself carries override these.
    pub fn load_tables(&mut self, tables: &TableSet) {
        self.engine.load_tables(tables);
    }

    /// Read markers up to and including the first `SOF`.
    ///
    /// Idempotent: a second call returns the cached result.
    pub fn read_info(&mut self) -> Result<ImageInfo> {
        self.fill()?;
        let data = std::mem::take(&mut self.data);
        let result = self.engine.read_headers(&data);
        self.data = data;
        result
    }

    /// The frame information, if [`Decoder::read_info`] has run.
    #[must_use]
    pub fn info(&self) -> Option<ImageInfo> {
        self.engine.info
    }

    /// The parsed frame header, if [`Decoder::read_info`] has run.
    ///
    /// `oxiarc-tiff` uses this to compare the `SOF` sampling factors against
    /// tag 530 `YCbCrSubSampling`, where the `SOF` is authoritative.
    #[must_use]
    pub fn frame_header(&self) -> Option<&FrameHeader> {
        self.engine.frame.as_ref()
    }

    /// Number of samples the decoded image occupies, or `None` before
    /// [`Decoder::read_info`].
    ///
    /// Reflects [`DecodeOptions::scale`]: sized from
    /// [`ImageInfo::scaled_width`]/[`ImageInfo::scaled_height`], not
    /// [`ImageInfo::width`]/[`ImageInfo::height`].
    #[must_use]
    pub fn output_buffer_size(&self) -> Option<usize> {
        let info = self.engine.info?;
        Some(info.scaled_width as usize * info.scaled_height as usize * info.output_components())
    }

    /// The pixel format a decode will produce.
    #[must_use]
    pub fn pixel_format(&self) -> Option<PixelFormat> {
        self.engine.info.map(|info| info.pixel_format())
    }

    /// Decode the whole image into a freshly allocated buffer.
    pub fn decode(&mut self) -> Result<Vec<u8>> {
        self.run()?;
        let plan = self.engine.plan()?;
        self.require_narrow()?;
        let mut out = vec![0u8; plan.row_len() * plan.height];
        let width = plan.row_len();
        plan.render(self.engine.planes(), |y, row| {
            let base = y * width;
            for (slot, &value) in out[base..base + width].iter_mut().zip(row) {
                *slot = value as u8;
            }
        })?;
        Ok(out)
    }

    /// Decode into a caller-owned buffer of exactly
    /// [`Decoder::output_buffer_size`] bytes.
    pub fn decode_into(&mut self, out: &mut [u8]) -> Result<()> {
        self.run()?;
        let plan = self.engine.plan()?;
        self.require_narrow()?;
        self.write_u8(&plan, out, plan.row_len())
    }

    /// Decode into a caller-owned buffer whose rows are `stride` bytes apart.
    ///
    /// This is the entry point a tiled TIFF reader wants: the destination can
    /// be a sub-rectangle of a larger image, so no per-tile copy is needed.
    ///
    /// `stride` is in **samples**, so it is `width * components` for a packed
    /// destination and larger for a sub-rectangle. Only the first
    /// `width * components` samples of each row are written; the padding
    /// between rows is left exactly as the caller left it.
    ///
    /// # Errors
    ///
    /// [`JpegError::BufferTooSmall`] when `stride` is narrower than one row,
    /// when `out` holds fewer than `stride * (height - 1) + width * components`
    /// samples, or when that product does not fit a `usize` — an absurd
    /// `stride` is reported, never multiplied.
    /// [`JpegError::PrecisionMismatch`] for a frame wider than eight bits; use
    /// [`Decoder::decode_into_u16_strided`] for those.
    pub fn decode_into_strided(&mut self, out: &mut [u8], stride: usize) -> Result<()> {
        self.run()?;
        let plan = self.engine.plan()?;
        self.require_narrow()?;
        self.write_u8(&plan, out, stride)
    }

    /// Decode a frame of any precision into 16-bit samples.
    pub fn decode_u16(&mut self) -> Result<Vec<u16>> {
        self.run()?;
        let plan = self.engine.plan()?;
        let mut out = vec![0u16; plan.row_len() * plan.height];
        let width = plan.row_len();
        plan.render(self.engine.planes(), |y, row| {
            out[y * width..y * width + width].copy_from_slice(row);
        })?;
        Ok(out)
    }

    /// Decode into a caller-owned 16-bit buffer.
    pub fn decode_into_u16(&mut self, out: &mut [u16]) -> Result<()> {
        self.run()?;
        let plan = self.engine.plan()?;
        write_u16(&plan, self.engine.planes(), out, plan.row_len())
    }

    /// Decode into a caller-owned 16-bit buffer whose rows are `stride`
    /// elements apart.
    ///
    /// The bounds rules of [`Decoder::decode_into_strided`] apply unchanged,
    /// counted in `u16` elements rather than bytes.
    pub fn decode_into_u16_strided(&mut self, out: &mut [u16], stride: usize) -> Result<()> {
        self.run()?;
        let plan = self.engine.plan()?;
        write_u16(&plan, self.engine.planes(), out, stride)
    }

    /// `true` when a scan ended before every unit was decoded and
    /// [`DecodeOptions::tolerate_truncated`] allowed the decode to continue.
    #[must_use]
    pub fn was_truncated(&self) -> bool {
        self.engine.truncated
    }

    /// The `JFIF` `APP0` header, if the stream carried one.
    #[must_use]
    pub fn jfif(&self) -> Option<&JfifHeader> {
        self.engine.metadata.jfif.as_ref()
    }

    /// The `Adobe` `APP14` header, if the stream carried one.
    #[must_use]
    pub fn adobe(&self) -> Option<&AdobeHeader> {
        self.engine.metadata.adobe.as_ref()
    }

    /// The EXIF payload, with its `Exif\0\0` prefix removed.
    #[must_use]
    pub fn exif(&self) -> Option<&[u8]> {
        self.engine.metadata.exif.as_deref()
    }

    /// The XMP packet, with its namespace URI prefix removed.
    #[must_use]
    pub fn xmp(&self) -> Option<&[u8]> {
        self.engine.metadata.xmp.as_deref()
    }

    /// The ICC profile, reassembled from its `APP2` chunks.
    pub fn icc_profile(&self) -> Result<Option<Vec<u8>>> {
        self.engine.metadata.icc_profile()
    }

    /// `COM` segment contents, in order.
    #[must_use]
    pub fn comments(&self) -> &[Vec<u8>] {
        &self.engine.metadata.comments
    }

    /// Every `APPn` and `COM` segment, verbatim and in order.
    #[must_use]
    pub fn app_segments(&self) -> &[AppSegment] {
        &self.engine.metadata.segments
    }

    /// Give the source reader back.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Read the source into memory, once.
    fn fill(&mut self) -> Result<()> {
        if self.filled {
            return Ok(());
        }
        let limit = self.engine.options.limits.max_input_bytes;
        let cap = limit.saturating_add(1);
        let mut source = (&mut self.reader).take(cap);
        source.read_to_end(&mut self.data)?;
        if self.data.len() as u64 > limit {
            return Err(JpegError::LimitExceeded(LimitKind::InputBytes));
        }
        self.filled = true;
        Ok(())
    }

    /// Ensure headers are read and every scan decoded.
    fn run(&mut self) -> Result<()> {
        self.fill()?;
        let data = std::mem::take(&mut self.data);
        let result = self.engine.decode_scans(&data);
        self.data = data;
        result
    }

    /// Reject 8-bit output for a frame with wider samples.
    fn require_narrow(&self) -> Result<u8> {
        let precision = self.engine.info.map(|info| info.precision).unwrap_or(8);
        if precision > 8 {
            return Err(JpegError::PrecisionMismatch { precision });
        }
        Ok(precision)
    }

    /// Render into a `u8` destination.
    fn write_u8(&self, plan: &output::OutputPlan, out: &mut [u8], stride: usize) -> Result<()> {
        let width = plan.row_len();
        if stride < width {
            return Err(JpegError::BufferTooSmall {
                need: width,
                got: stride,
            });
        }
        let need = required_len(stride, width, plan.height).ok_or(JpegError::BufferTooSmall {
            need: usize::MAX,
            got: out.len(),
        })?;
        if out.len() < need {
            return Err(JpegError::BufferTooSmall {
                need,
                got: out.len(),
            });
        }
        plan.render(self.engine.planes(), |y, row| {
            let base = y * stride;
            for (slot, &value) in out[base..base + width].iter_mut().zip(row) {
                *slot = value as u8;
            }
        })
    }
}

/// Render into a `u16` destination.
fn write_u16(
    plan: &output::OutputPlan,
    planes: &planes::Planes,
    out: &mut [u16],
    stride: usize,
) -> Result<()> {
    let width = plan.row_len();
    if stride < width {
        return Err(JpegError::BufferTooSmall {
            need: width,
            got: stride,
        });
    }
    let need = required_len(stride, width, plan.height).ok_or(JpegError::BufferTooSmall {
        need: usize::MAX,
        got: out.len(),
    })?;
    if out.len() < need {
        return Err(JpegError::BufferTooSmall {
            need,
            got: out.len(),
        });
    }
    plan.render(planes, |y, row| {
        let base = y * stride;
        out[base..base + width].copy_from_slice(row);
    })
}

/// Elements a strided destination must hold: full strides for every row but
/// the last, which only needs its own samples.
///
/// `None` when the product does not fit a `usize`, which no buffer could
/// satisfy; the callers turn that into [`JpegError::BufferTooSmall`] rather
/// than letting the multiplication overflow.
fn required_len(stride: usize, width: usize, height: usize) -> Option<usize> {
    if height == 0 {
        return Some(0);
    }
    stride.checked_mul(height - 1)?.checked_add(width)
}

/// Decode a scan-only abbreviated datastream with out-of-band tables.
///
/// This is the entry point `oxiarc-tiff` uses for `Compression = 7`: the
/// `JPEGTables` tag is parsed once into a [`TableSet`], every strip or tile is
/// a complete `SOI ... EOI` datastream carrying only the frame and scan
/// headers, and the two are decoded together without ever being concatenated.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::{DecodeOptions, decode_abbreviated_into};
///
/// let scan: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1;
/// let mut out = [0u8; 1];
/// let info = decode_abbreviated_into(None, scan, &DecodeOptions::raw(), &mut out)?;
/// assert_eq!(info.num_components, 1);
/// # Ok(())
/// # }
/// ```
pub fn decode_abbreviated_into(
    tables: Option<&TableSet>,
    scan: &[u8],
    options: &DecodeOptions,
    out: &mut [u8],
) -> Result<ImageInfo> {
    let mut engine = Engine::new(options.clone());
    if let Some(tables) = tables {
        engine.load_tables(tables);
    }
    engine.decode_scans(scan)?;
    let info = engine.info.ok_or(JpegError::AbbreviatedWithoutFrame)?;
    if info.precision > 8 {
        return Err(JpegError::PrecisionMismatch {
            precision: info.precision,
        });
    }
    let plan = engine.plan()?;
    let width = plan.row_len();
    let need = required_len(width, width, plan.height).ok_or(JpegError::BufferTooSmall {
        need: usize::MAX,
        got: out.len(),
    })?;
    if out.len() < need {
        return Err(JpegError::BufferTooSmall {
            need,
            got: out.len(),
        });
    }
    plan.render(engine.planes(), |y, row| {
        let base = y * width;
        for (slot, &value) in out[base..base + width].iter_mut().zip(row) {
            *slot = value as u8;
        }
    })?;
    Ok(info)
}

/// Like [`decode_abbreviated_into`], but for frames of any precision.
pub fn decode_abbreviated_into_u16(
    tables: Option<&TableSet>,
    scan: &[u8],
    options: &DecodeOptions,
    out: &mut [u16],
) -> Result<ImageInfo> {
    let mut engine = Engine::new(options.clone());
    if let Some(tables) = tables {
        engine.load_tables(tables);
    }
    engine.decode_scans(scan)?;
    let info = engine.info.ok_or(JpegError::AbbreviatedWithoutFrame)?;
    let plan = engine.plan()?;
    write_u16(&plan, engine.planes(), out, plan.row_len())?;
    Ok(info)
}

/// Decode a scan-only abbreviated datastream, allocating the output.
pub fn decode_abbreviated(
    tables: Option<&TableSet>,
    scan: &[u8],
    options: &DecodeOptions,
) -> Result<(ImageInfo, Vec<u8>)> {
    let mut engine = Engine::new(options.clone());
    if let Some(tables) = tables {
        engine.load_tables(tables);
    }
    engine.decode_scans(scan)?;
    let info = engine.info.ok_or(JpegError::AbbreviatedWithoutFrame)?;
    if info.precision > 8 {
        return Err(JpegError::PrecisionMismatch {
            precision: info.precision,
        });
    }
    let plan = engine.plan()?;
    let width = plan.row_len();
    let mut out = vec![0u8; width * plan.height];
    plan.render(engine.planes(), |y, row| {
        let base = y * width;
        for (slot, &value) in out[base..base + width].iter_mut().zip(row) {
            *slot = value as u8;
        }
    })?;
    Ok((info, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_len_leaves_the_last_row_short() {
        assert_eq!(required_len(10, 6, 3), Some(26));
        assert_eq!(required_len(6, 6, 3), Some(18));
        assert_eq!(required_len(10, 6, 0), Some(0));
        // A stride no buffer could satisfy is reported, never multiplied.
        assert_eq!(required_len(usize::MAX, 6, 2), None);
        assert_eq!(required_len(usize::MAX / 2, 6, 3), None);
    }

    #[test]
    fn pixel_format_component_counts() {
        assert_eq!(PixelFormat::L8.components(), 1);
        assert_eq!(PixelFormat::Rgb16.components(), 3);
        assert_eq!(PixelFormat::Cmyk8.components(), 4);
        assert_eq!(PixelFormat::Raw8(2).components(), 2);
        assert!(!PixelFormat::L8.is_wide());
        assert!(PixelFormat::Raw16(2).is_wide());
    }

    #[test]
    fn options_presets() {
        assert!(DecodeOptions::raw().raw_components);
        assert_eq!(
            DecodeOptions::strict().limits.max_pixels,
            DecodeLimits::strict().max_pixels
        );
        assert_eq!(DecodeOptions::default().upsampling, Upsampling::Fancy);
    }

    #[test]
    fn a_stream_without_a_frame_header_is_a_named_error() {
        let tables = TableSet::default().emit(crate::TablesMode::BOTH);
        let mut decoder = Decoder::new(tables.as_slice());
        assert!(matches!(
            decoder.read_info(),
            Err(JpegError::AbbreviatedWithoutFrame)
        ));
    }

    #[test]
    fn the_input_budget_is_enforced() {
        let data = vec![0u8; 4096];
        let options = DecodeOptions {
            limits: DecodeLimits {
                max_input_bytes: 16,
                ..DecodeLimits::default()
            },
            ..DecodeOptions::default()
        };
        let mut decoder = Decoder::with_options(data.as_slice(), options);
        assert!(matches!(
            decoder.read_info(),
            Err(JpegError::LimitExceeded(LimitKind::InputBytes))
        ));
    }

    #[test]
    fn into_inner_returns_the_source() {
        let data: &[u8] = &[0xFF, 0xD8];
        let decoder = Decoder::new(data);
        assert_eq!(decoder.into_inner().len(), 2);
    }
}
