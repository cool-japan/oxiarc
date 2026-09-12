//! JPEG encoding: baseline, extended, progressive and lossless.
//!
//! [`Encoder`] writes a complete interchange datastream; [`Encoder::
//! write_tables_only`] and [`Encoder::encode_scan_only`] write the two halves
//! TIFF's `Compression = 7` needs instead.

#[cfg(feature = "arithmetic")]
mod arith;
mod bitwriter;
mod coefficients;
mod enctable;
mod frame;
mod lossless;
mod markers;
mod options;
#[cfg(all(test, feature = "jpeg-oracle"))]
mod oracle;
#[cfg(feature = "rayon")]
mod parallel;
mod plan;
mod prepare;
mod progressive;
mod sequential;

use std::io::Write;

pub use options::{
    ComponentIds, Density, EncodeOptions, EncodeProcess, InputColor, MarkerPolicy,
    QuantTableSource, RestartInterval, ScanSpec, Subsampling,
};

use crate::error::{JpegError, Result};
use crate::frame::EntropyCoding;
use crate::tableset::{TableSet, TablesMode};
use plan::build_plan;
use prepare::Samples;

/// Build the abbreviated table stream a TIFF `JPEGTables` tag carries.
///
/// The bytes are what libtiff writes for the same quality and colour space:
/// `SOI`, the `DQT` segments each component references in `Tq` order, the
/// `DHT` segments in `DC0 AC0 DC1 AC1` order, then `EOI`.
///
/// Only standard Annex K.3 Huffman tables can be shared this way, so
/// [`EncodeOptions::optimize_huffman`] must be off.
///
/// # Errors
///
/// Returns [`JpegError::InvalidEncodeParameter`] when the options ask for
/// generated tables, or for a process that has no shareable tables.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::{EncodeOptions, InputColor, TablesMode, table_set};
///
/// let tables = table_set(&EncodeOptions::tiff_strip(75), InputColor::Rgb)?;
/// let blob = tables.emit(TablesMode::BOTH);
/// assert_eq!(&blob[..2], &[0xFF, 0xD8]);
/// assert_eq!(tables.quant[0].expect("luma").natural()[0], 8);
/// # Ok(())
/// # }
/// ```
pub fn table_set(options: &EncodeOptions, input: InputColor) -> Result<TableSet> {
    let arithmetic = options.entropy == EntropyCoding::Arithmetic;
    if options.optimize_huffman && !arithmetic {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "optimize_huffman",
            reason: "generated tables cannot be shared through a TIFF JPEGTables tag",
        });
    }
    let plan = build_plan(options, 8, 8, input)?;
    if plan.optimize_huffman {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "process",
            reason: "this process generates its Huffman tables per image",
        });
    }
    let mut set = TableSet {
        quant: plan.quant,
        ..TableSet::default()
    };
    if arithmetic {
        // An arithmetic frame has no Huffman tables at all; what a container
        // has to share out of band is the quantisation and the conditioning.
        set.arithmetic = plan.arithmetic;
        return Ok(set);
    }
    let mut bank = frame::TableBank::new();
    bank.install_standard(&plan)?;
    set.dc_huffman = bank.dc;
    set.ac_huffman = bank.ac;
    Ok(set)
}

/// Encode one image into a fresh `Vec` at the given quality.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::{Decoder, InputColor, encode_to_vec};
///
/// let pixels = vec![128u8; 16 * 16 * 3];
/// let jpeg = encode_to_vec(&pixels, 16, 16, InputColor::Rgb, 90)?;
/// assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
///
/// let info = Decoder::new(&jpeg[..]).read_info()?;
/// assert_eq!((info.width, info.height), (16, 16));
/// # Ok(())
/// # }
/// ```
pub fn encode_to_vec(
    pixels: &[u8],
    width: u16,
    height: u16,
    color: InputColor,
    quality: u8,
) -> Result<Vec<u8>> {
    let options = EncodeOptions {
        quality,
        ..Default::default()
    };
    encode_to_vec_with_options(pixels, width, height, color, &options)
}

/// Encode one image into a fresh `Vec` with full control over the options.
pub fn encode_to_vec_with_options(
    pixels: &[u8],
    width: u16,
    height: u16,
    color: InputColor,
    options: &EncodeOptions,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, options.clone());
    encoder.encode(pixels, width, height, color)?;
    encoder.finish()?;
    Ok(out)
}

/// Encode one image of wider-than-eight-bit samples into a fresh `Vec`.
pub fn encode_u16_to_vec_with_options(
    pixels: &[u16],
    width: u16,
    height: u16,
    color: InputColor,
    options: &EncodeOptions,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, options.clone());
    encoder.encode_u16(pixels, width, height, color)?;
    encoder.finish()?;
    Ok(out)
}

/// A JPEG encoder writing into any [`Write`].
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::{Encoder, InputColor, Subsampling};
///
/// let mut out = Vec::new();
/// let mut encoder = Encoder::new(&mut out);
/// encoder.set_quality(90).set_subsampling(Subsampling::S444);
/// encoder.add_comment(b"made by oxiarc-jpeg")?;
/// encoder.encode(&[200u8; 8 * 8], 8, 8, InputColor::Luma)?;
/// encoder.finish()?;
///
/// assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Encoder<W: Write> {
    writer: W,
    options: EncodeOptions,
    extra_markers: Vec<u8>,
}

impl<W: Write> Encoder<W> {
    /// A new encoder with `cjpeg`'s defaults: quality 75, 4:2:0, sequential,
    /// standard Huffman tables.
    pub fn new(writer: W) -> Self {
        Self::with_options(writer, EncodeOptions::default())
    }

    /// A new encoder with explicit options.
    pub fn with_options(writer: W, options: EncodeOptions) -> Self {
        Self {
            writer,
            options,
            extra_markers: Vec::new(),
        }
    }

    /// The options this encoder will use.
    pub fn options(&self) -> &EncodeOptions {
        &self.options
    }

    /// Replace the options wholesale.
    pub fn set_options(&mut self, options: EncodeOptions) -> &mut Self {
        self.options = options;
        self
    }

    /// Set the quality, `1..=100`.
    pub fn set_quality(&mut self, quality: u8) -> &mut Self {
        self.options.quality = quality;
        self
    }

    /// Set the chroma sampling ratio.
    pub fn set_subsampling(&mut self, subsampling: Subsampling) -> &mut Self {
        self.options.subsampling = subsampling;
        self
    }

    /// Choose the coding process.
    pub fn set_process(&mut self, process: EncodeProcess) -> &mut Self {
        self.options.process = process;
        self
    }

    /// Write a progressive frame using libjpeg's default scan script.
    pub fn set_progressive(&mut self, progressive: bool) -> &mut Self {
        self.options.process = if progressive {
            EncodeProcess::Progressive
        } else {
            EncodeProcess::Sequential
        };
        self
    }

    /// Supply a custom progressive scan script.
    pub fn set_progressive_script(&mut self, script: Option<Vec<ScanSpec>>) -> &mut Self {
        self.options.progressive_script = script;
        self
    }

    /// Generate Huffman tables from the image's own statistics.
    pub fn set_optimize_huffman(&mut self, optimize: bool) -> &mut Self {
        self.options.optimize_huffman = optimize;
        self
    }

    /// Set the restart marker spacing.
    pub fn set_restart_interval(&mut self, restart: RestartInterval) -> &mut Self {
        self.options.restart_interval = restart;
        self
    }

    /// Set the sample precision.
    pub fn set_precision(&mut self, precision: u8) -> &mut Self {
        self.options.precision = precision;
        self
    }

    /// Clamp quantiser values to `1..=255` so the frame stays baseline.
    pub fn set_force_baseline(&mut self, force: bool) -> &mut Self {
        self.options.force_baseline = force;
        self
    }

    /// Override the JPEG colour space.
    pub fn set_jpeg_color_space(&mut self, color: Option<crate::ColorSpace>) -> &mut Self {
        self.options.jpeg_color_space = color;
        self
    }

    /// Choose the `SOF` component identifiers.
    pub fn set_component_ids(&mut self, ids: ComponentIds) -> &mut Self {
        self.options.component_ids = ids;
        self
    }

    /// Control the `JFIF` `APP0` segment.
    pub fn set_write_jfif(&mut self, policy: MarkerPolicy) -> &mut Self {
        self.options.write_jfif = policy;
        self
    }

    /// Control the Adobe `APP14` segment.
    pub fn set_write_adobe(&mut self, policy: MarkerPolicy) -> &mut Self {
        self.options.write_adobe = policy;
        self
    }

    /// Set the density recorded in `APP0`.
    pub fn set_density(&mut self, density: Density) -> &mut Self {
        self.options.density = density;
        self
    }

    /// Choose the chroma decimation filter.
    pub fn set_downsampling(&mut self, downsampling: crate::Downsampling) -> &mut Self {
        self.options.downsampling = downsampling;
        self
    }

    /// Choose the entropy coder.
    ///
    /// [`EntropyCoding::Arithmetic`] writes `SOF9`, `SOF10` or `SOF11` and
    /// needs the `arithmetic` feature; without it, encoding returns
    /// [`UnsupportedFeature::ArithmeticCoding`](crate::UnsupportedFeature::ArithmeticCoding).
    pub fn set_entropy(&mut self, entropy: EntropyCoding) -> &mut Self {
        self.options.entropy = entropy;
        self
    }

    /// Set the arithmetic conditioning bounds written in `DAC`.
    ///
    /// Only meaningful when the entropy coder is arithmetic. The default is
    /// T.81's `L = 0`, `U = 1`, `Kx = 5`, which is what every libjpeg-derived
    /// encoder writes.
    pub fn set_arithmetic_conditioning(
        &mut self,
        conditioning: crate::ArithmeticConditioning,
    ) -> &mut Self {
        self.options.arithmetic = conditioning;
        self
    }

    /// Queue an `APPn` segment, `n` in `0..=15`.
    ///
    /// Queued segments are written after `APP0`/`APP14` and before the first
    /// `DQT`, which is where libjpeg puts markers an application supplies.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] for `n > 15` or a payload
    /// longer than 65 533 bytes.
    pub fn add_app_segment(&mut self, n: u8, data: &[u8]) -> Result<&mut Self> {
        markers::app(&mut self.extra_markers, n, data)?;
        Ok(self)
    }

    /// Queue an ICC profile, split across as many `APP2` segments as it needs.
    ///
    /// The profile is the raw ICC bytes; the `ICC_PROFILE\0` prefix and the
    /// chunk numbering are added here, so what
    /// [`crate::Decoder::icc_profile`] returns can be handed straight back.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] for a profile needing
    /// more than 255 chunks.
    pub fn add_icc_profile(&mut self, profile: &[u8]) -> Result<&mut Self> {
        markers::icc_profile(&mut self.extra_markers, profile)?;
        Ok(self)
    }

    /// Queue an EXIF `APP1` segment. `data` is the payload **without** the
    /// `Exif\0\0` prefix, matching [`crate::Decoder::exif`].
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] for an oversized payload.
    pub fn add_exif(&mut self, data: &[u8]) -> Result<&mut Self> {
        markers::exif(&mut self.extra_markers, data)?;
        Ok(self)
    }

    /// Queue an XMP `APP1` segment. `data` is the packet **without** its
    /// namespace URI prefix, matching [`crate::Decoder::xmp`].
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] for an oversized payload.
    pub fn add_xmp(&mut self, data: &[u8]) -> Result<&mut Self> {
        markers::xmp(&mut self.extra_markers, data)?;
        Ok(self)
    }

    /// Queue a `COM` segment.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::InvalidEncodeParameter`] for an oversized payload.
    pub fn add_comment(&mut self, text: &[u8]) -> Result<&mut Self> {
        markers::comment(&mut self.extra_markers, text)?;
        Ok(self)
    }

    /// Encode a complete datastream from eight-bit interleaved samples.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::BufferTooSmall`] when `pixels` is shorter than
    /// `width * height * color.channels()`, and
    /// [`JpegError::InvalidEncodeParameter`] when the options contradict each
    /// other or the input colour space cannot reach the JPEG one.
    pub fn encode(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        color: InputColor,
    ) -> Result<()> {
        let bytes = self.frame_bytes(&Samples::Eight(pixels), width, height, color, false)?;
        self.writer.write_all(&bytes)?;
        Ok(())
    }

    /// Encode a complete datastream from samples of nine to sixteen bits.
    ///
    /// # Errors
    ///
    /// As [`Encoder::encode`].
    pub fn encode_u16(
        &mut self,
        pixels: &[u16],
        width: u16,
        height: u16,
        color: InputColor,
    ) -> Result<()> {
        let bytes = self.frame_bytes(&Samples::Wide(pixels), width, height, color, false)?;
        self.writer.write_all(&bytes)?;
        Ok(())
    }

    /// Encode a complete datastream from separate component planes.
    ///
    /// Each plane is `width * height` samples. The planes are interleaved into
    /// a temporary buffer first, so this costs one extra copy; it exists
    /// because TIFF's `PlanarConfiguration = 2` hands over data this way.
    ///
    /// # Errors
    ///
    /// As [`Encoder::encode`], plus
    /// [`JpegError::InvalidEncodeParameter`] when the plane count does not
    /// match the input colour space.
    pub fn encode_planar(
        &mut self,
        planes: &[&[u8]],
        width: u16,
        height: u16,
        color: InputColor,
    ) -> Result<()> {
        let channels = color.channels();
        if planes.len() != channels {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "planes",
                reason: "the plane count does not match the input colour space",
            });
        }
        let count = usize::from(width) * usize::from(height);
        for plane in planes {
            if plane.len() < count {
                return Err(JpegError::BufferTooSmall {
                    need: count,
                    got: plane.len(),
                });
            }
        }
        let mut interleaved = vec![0u8; count * channels];
        for (channel, plane) in planes.iter().enumerate() {
            for (index, slot) in interleaved
                .iter_mut()
                .skip(channel)
                .step_by(channels)
                .enumerate()
            {
                *slot = plane[index];
            }
        }
        self.encode(&interleaved, width, height, color)
    }

    /// Write only the abbreviated table stream: `SOI`, the tables, `EOI`.
    ///
    /// This is TIFF tag 347. Nothing else is written, and a later call to
    /// [`Encoder::encode_scan_only`] produces the matching strips.
    ///
    /// # Errors
    ///
    /// As [`table_set`].
    pub fn write_tables_only(&mut self, mode: TablesMode, color: InputColor) -> Result<()> {
        let set = table_set(&self.options, color)?;
        self.writer.write_all(&set.emit(mode))?;
        Ok(())
    }

    /// Write one TIFF strip or tile: `SOI`, `SOF`, `SOS`, entropy data, `EOI`,
    /// with every table segment suppressed.
    ///
    /// The `JFIF` and Adobe markers are suppressed too unless the caller set
    /// [`MarkerPolicy::Always`], because libtiff writes neither and the
    /// component identifiers carry the colour space instead.
    ///
    /// # Errors
    ///
    /// As [`Encoder::encode`], plus
    /// [`JpegError::InvalidEncodeParameter`] when the frame would generate
    /// its own Huffman tables — which every progressive, twelve-bit and
    /// lossless frame does — because those cannot be shared out of band.
    pub fn encode_scan_only(
        &mut self,
        pixels: &[u8],
        width: u16,
        height: u16,
        color: InputColor,
    ) -> Result<()> {
        let bytes = self.frame_bytes(&Samples::Eight(pixels), width, height, color, true)?;
        self.writer.write_all(&bytes)?;
        Ok(())
    }

    /// Flush and return the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns [`JpegError::Io`] if the flush fails.
    pub fn finish(mut self) -> Result<W> {
        self.writer.flush()?;
        Ok(self.writer)
    }

    /// Return the underlying writer without flushing.
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Build the whole datastream in memory.
    fn frame_bytes(
        &self,
        pixels: &Samples<'_>,
        width: u16,
        height: u16,
        color: InputColor,
        abbreviated: bool,
    ) -> Result<Vec<u8>> {
        let mut options = self.options.clone();
        if abbreviated {
            if options.write_jfif != MarkerPolicy::Always {
                options.write_jfif = MarkerPolicy::Never;
            }
            if options.write_adobe != MarkerPolicy::Always {
                options.write_adobe = MarkerPolicy::Never;
            }
        }
        let plan = build_plan(&options, width, height, color)?;
        // The check is against the *plan*, not the options: progressive
        // frames, twelve-bit frames and lossless frames all turn table
        // generation on after the options are read, and a stream that claims
        // its tables are elsewhere must not then carry `DHT` segments.
        if abbreviated && plan.optimize_huffman {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "optimize_huffman",
                reason: "an abbreviated scan cannot carry generated Huffman tables",
            });
        }
        #[cfg(not(feature = "arithmetic"))]
        if plan.is_arithmetic() {
            return Err(JpegError::Unsupported(
                crate::error::UnsupportedFeature::ArithmeticCoding,
            ));
        }
        let mut out = Vec::with_capacity(1 << 16);
        if plan.is_lossless() {
            lossless::encode_lossless_frame(
                &plan,
                pixels,
                &self.extra_markers,
                abbreviated,
                &mut out,
            )?;
        } else {
            #[cfg(feature = "arithmetic")]
            if plan.is_arithmetic() {
                arith::encode_dct_frame(&plan, pixels, &self.extra_markers, abbreviated, &mut out)?;
                return Ok(out);
            }
            frame::encode_dct_frame(&plan, pixels, &self.extra_markers, abbreviated, &mut out)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorSpace, DecodeOptions, Decoder, TablesMode, decode_abbreviated_into};

    fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * channels);
        for y in 0..height {
            for x in 0..width {
                let values = [
                    ((x * 7 + y * 13) % 256) as u8,
                    if (x / 5 + y / 3) % 2 == 0 { 240 } else { 24 },
                    ((x * 3 + y * 5) % 256) as u8,
                ];
                for c in 0..channels {
                    out.push(values[c % 3]);
                }
            }
        }
        out
    }

    #[test]
    fn a_baseline_frame_round_trips_through_our_own_decoder() {
        let pixels = source(31, 17, 3);
        let jpeg = encode_to_vec(&pixels, 31, 17, InputColor::Rgb, 90).expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!((info.width, info.height), (31, 17));
        assert_eq!(info.output_color_space, ColorSpace::Rgb);
        let decoded = decoder.decode().expect("decode");
        assert_eq!(decoded.len(), 31 * 17 * 3);

        // The source has hard chroma edges, so 4:2:0 loses a lot of them; a
        // 4:4:4 encode of the same image must stay close.
        let options = EncodeOptions {
            quality: 95,
            subsampling: crate::Subsampling::S444,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 31, 17, InputColor::Rgb, &options).expect("encode");
        let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
        let error: f64 = decoded
            .iter()
            .zip(pixels.iter())
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / decoded.len() as f64;
        assert!(error < 20.0, "mean squared error {error}");
    }

    #[test]
    fn odd_sizes_encode_and_decode_without_panicking() {
        for &(width, height) in &[(1u16, 1u16), (1, 33), (33, 1), (7, 9), (16, 16), (17, 19)] {
            for color in [InputColor::Luma, InputColor::Rgb, InputColor::Cmyk] {
                let pixels = source(usize::from(width), usize::from(height), color.channels());
                let jpeg = encode_to_vec(&pixels, width, height, color, 75).expect("encode");
                let mut decoder = Decoder::new(&jpeg[..]);
                let info = decoder.read_info().expect("info");
                assert_eq!((info.width, info.height), (width, height));
                decoder.decode().expect("decode");
            }
        }
    }

    #[test]
    fn progressive_frames_round_trip() {
        let pixels = source(23, 29, 3);
        let options = EncodeOptions {
            process: EncodeProcess::Progressive,
            quality: 85,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 23, 29, InputColor::Rgb, &options).expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!(info.process, crate::CodingProcess::Progressive);
        let decoded = decoder.decode().expect("decode");
        assert_eq!(decoded.len(), 23 * 29 * 3);
    }

    #[test]
    fn lossless_frames_round_trip_exactly() {
        let pixels = source(19, 13, 3);
        for predictor in 1..=7u8 {
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor,
                    point_transform: 0,
                },
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(&pixels, 19, 13, InputColor::Rgb, &options)
                .expect("encode");
            let mut decoder = Decoder::new(&jpeg[..]);
            let info = decoder.read_info().expect("info");
            assert_eq!(info.process, crate::CodingProcess::Lossless);
            let decoded = decoder.decode().expect("decode");
            assert_eq!(decoded, pixels, "predictor {predictor}");
        }
    }

    #[test]
    fn restart_intervals_survive_a_round_trip() {
        let pixels = source(40, 40, 3);
        for restart in [
            RestartInterval::Mcus(1),
            RestartInterval::Mcus(3),
            RestartInterval::McuRows(1),
        ] {
            let options = EncodeOptions {
                restart_interval: restart,
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(&pixels, 40, 40, InputColor::Rgb, &options)
                .expect("encode");
            // The DRI segment sits between the DHT segments and the SOS, so
            // it is only visible once the scan header has been parsed.
            assert!(
                jpeg.windows(2).any(|pair| pair == [0xFF, 0xDD]),
                "no DRI segment for {restart:?}"
            );
            assert!(
                jpeg.windows(2)
                    .any(|pair| pair[0] == 0xFF && (0xD0..=0xD7).contains(&pair[1])),
                "no RST markers for {restart:?}"
            );
            let mut decoder = Decoder::new(&jpeg[..]);
            decoder.read_info().expect("info");
            decoder.decode().expect("decode");
        }
    }

    #[test]
    fn metadata_survives_a_round_trip() {
        let mut out = Vec::new();
        let mut encoder = Encoder::new(&mut out);
        encoder.add_comment(b"hello").expect("comment");
        encoder.add_exif(b"II*\0extra").expect("exif");
        encoder.add_xmp(b"<x:xmpmeta/>").expect("xmp");
        encoder.add_icc_profile(&vec![7u8; 300]).expect("icc");
        encoder
            .encode(&[128u8; 8 * 8], 8, 8, InputColor::Luma)
            .expect("encode");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(&out[..]);
        decoder.read_info().expect("info");
        assert_eq!(decoder.comments(), &[b"hello".to_vec()]);
        assert_eq!(decoder.exif(), Some(&b"II*\0extra"[..]));
        assert_eq!(decoder.xmp(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(decoder.icc_profile().expect("icc"), Some(vec![7u8; 300]));
    }

    #[test]
    fn the_abbreviated_pair_decodes_through_the_tiff_entry_points() {
        let pixels = source(24, 16, 3);
        let options = EncodeOptions::tiff_strip(75);

        let mut tables_blob = Vec::new();
        let mut encoder = Encoder::with_options(&mut tables_blob, options.clone());
        encoder
            .write_tables_only(TablesMode::BOTH, InputColor::Rgb)
            .expect("tables");
        encoder.finish().expect("finish");

        let mut strip = Vec::new();
        let mut encoder = Encoder::with_options(&mut strip, options);
        encoder
            .encode_scan_only(&pixels, 24, 16, InputColor::Rgb)
            .expect("strip");
        encoder.finish().expect("finish");

        assert_eq!(&strip[..4], &[0xFF, 0xD8, 0xFF, 0xC0], "SOI then SOF0");

        let tables = TableSet::parse(&tables_blob).expect("parse");
        let mut out = vec![0u8; 24 * 16 * 3];
        let info = decode_abbreviated_into(Some(&tables), &strip, &DecodeOptions::raw(), &mut out)
            .expect("decode");
        assert_eq!((info.width, info.height), (24, 16));
        assert_eq!(info.num_components, 3);
    }

    #[test]
    fn raw_rgb_and_cmyk_strips_use_letter_component_ids() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Rgb),
            ..EncodeOptions::tiff_strip(75)
        };
        let jpeg = encode_to_vec_with_options(&[1u8, 2, 3], 1, 1, InputColor::Rgb, &options)
            .expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        let ids: Vec<u8> = info.components().iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![b'R', b'G', b'B']);
        assert!(!info.has_jfif);
        assert!(!info.has_adobe);

        let options = EncodeOptions::tiff_strip(75);
        let jpeg = encode_to_vec_with_options(&[1u8, 2, 3, 4], 1, 1, InputColor::Cmyk, &options)
            .expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        let ids: Vec<u8> = info.components().iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![b'C', b'M', b'Y', b'K']);
    }

    #[test]
    fn planar_input_matches_interleaved_input() {
        let width = 12u16;
        let height = 9u16;
        let count = usize::from(width) * usize::from(height);
        let r: Vec<u8> = (0..count).map(|i| (i % 251) as u8).collect();
        let g: Vec<u8> = (0..count).map(|i| (i % 97) as u8).collect();
        let b: Vec<u8> = (0..count).map(|i| (i % 61) as u8).collect();
        let mut interleaved = Vec::with_capacity(count * 3);
        for index in 0..count {
            interleaved.extend_from_slice(&[r[index], g[index], b[index]]);
        }

        let expected =
            encode_to_vec(&interleaved, width, height, InputColor::Rgb, 80).expect("encode");
        let mut out = Vec::new();
        let mut encoder = Encoder::with_options(
            &mut out,
            EncodeOptions {
                quality: 80,
                ..Default::default()
            },
        );
        encoder
            .encode_planar(&[&r, &g, &b], width, height, InputColor::Rgb)
            .expect("planar");
        encoder.finish().expect("finish");
        assert_eq!(out, expected);
    }

    #[test]
    fn bad_inputs_are_errors_not_panics() {
        assert!(encode_to_vec(&[0u8; 3], 0, 1, InputColor::Rgb, 75).is_err());
        assert!(encode_to_vec(&[0u8; 2], 1, 1, InputColor::Rgb, 75).is_err());
        let options = EncodeOptions {
            optimize_huffman: true,
            ..EncodeOptions::tiff_strip(75)
        };
        assert!(table_set(&options, InputColor::Rgb).is_err());
    }

    #[test]
    fn twelve_bit_frames_round_trip() {
        let count = 16 * 16;
        let pixels: Vec<u16> = (0..count).map(|i| ((i * 17) % 4096) as u16).collect();
        let options = EncodeOptions {
            precision: 12,
            quality: 90,
            ..Default::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 16, 16, InputColor::Luma, &options)
            .expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!(info.precision, 12);
        let decoded = decoder.decode_u16().expect("decode");
        assert_eq!(decoded.len(), count);
    }
}
