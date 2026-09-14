//! `tar` 0.4.46 API shim for `[patch.crates-io]`, backed by
//! [`oxiarc_tar_compat`]. See that crate's documentation for the supported
//! surface and the documented differences from upstream.

#![forbid(unsafe_code)]

pub use oxiarc_tar_compat::*;
