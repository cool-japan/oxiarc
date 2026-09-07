//! Inverse DCT.
//!
//! Only the "islow" (accurate integer) transform is provided, because it is
//! the one every reference decoder produces bit-identical output with:
//! `djpeg -dct int` is the byte-parity oracle this crate is tested against.
//! The `-dct fast` and `-dct float` variants are not bit-reproducible even
//! between builds of libjpeg and are deliberately absent.

mod islow;

pub(crate) use islow::idct_islow_into;
