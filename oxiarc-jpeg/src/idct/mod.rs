//! Inverse DCT.
//!
//! Only the "islow" (accurate integer) transform is provided at native
//! (`8x8`) resolution, because it is the one every reference decoder
//! produces bit-identical output with: `djpeg -dct int` is the byte-parity
//! oracle this crate is tested against. The `-dct fast` and `-dct float`
//! variants are not bit-reproducible even between builds of libjpeg and are
//! deliberately absent.
//!
//! [`scaled`] adds the reduced- and enlarged-size kernels
//! [`crate::DecodeOptions::scale`] selects.

mod islow;
mod scaled;

pub(crate) use islow::idct_islow_into;
pub(crate) use scaled::idct_scaled_into;
