//! `flate2` 1.1.10 API shim for `[patch.crates-io]`, backed by
//! [`oxiarc_flate2_compat`]. See that crate's documentation for the supported
//! surface and the documented differences from upstream.

#![forbid(unsafe_code)]

pub use oxiarc_flate2_compat::*;
