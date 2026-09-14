//! `zip` 8.6.0 API shim for `[patch.crates-io]`, backed by
//! [`oxiarc_zip_compat`]. See that crate's documentation for the supported
//! surface and the documented differences from upstream.

#![forbid(unsafe_code)]

pub use oxiarc_zip_compat::*;
