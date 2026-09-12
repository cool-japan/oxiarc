//! `image`-shaped codec wrappers: `codecs::{png,jpeg,tiff}::{Encoder,
//! Decoder}`, each implementing [`crate::ImageDecoder`] /
//! [`crate::ImageEncoder`] over the real `oxiarc-png` / `oxiarc-jpeg` /
//! `oxiarc-tiff` codec crates.
//!
//! [`crate::DynamicImage`] and [`crate::ImageReader`] are built on top of
//! these and are almost always the more convenient entry point; use these
//! directly for the same reason `image::codecs::*` exists: fine control
//! over one format's own options (PNG filter/compression, JPEG quality,
//! ...), or writing through [`crate::DynamicImage::write_with_encoder`].

pub mod jpeg;
pub mod png;
pub mod tiff;
