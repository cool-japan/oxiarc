//! # oxiarc-tar-compat
//!
//! A Pure Rust implementation of the [`tar`](https://docs.rs/tar) 0.4
//! crate's public API: [`Builder`] (`append`, `append_data` with GNU
//! long-name records, `append_link`, `append_path*`, `into_inner`, finish
//! on drop), [`Header`] (GNU / UStar / old layouts with the tar crate's
//! exact numeric encodings and checksum), [`Archive`] / [`Entries`] /
//! [`Entry`] (lazy, sequential, GNU long names and PAX records honored),
//! and [`EntryType`]. Checksums and PAX parsing reuse
//! [`oxiarc_archive::tar::TarHeader`].
//!
//! ```
//! use std::io::Read;
//! use oxiarc_tar_compat::{Archive, Builder, Header};
//!
//! let mut ar = Builder::new(Vec::new());
//! let data = b"version 1.0;";
//! let mut header = Header::new_gnu();
//! header.set_size(data.len() as u64);
//! header.set_mode(0o644);
//! header.set_cksum();
//! ar.append_data(&mut header, "graph.nnef", &data[..])?;
//! let bytes = ar.into_inner()?;
//!
//! let mut archive = Archive::new(&bytes[..]);
//! for entry in archive.entries()? {
//!     let mut entry = entry?;
//!     assert_eq!(entry.path()?.to_str(), Some("graph.nnef"));
//!     let mut text = String::new();
//!     entry.read_to_string(&mut text)?;
//!     assert_eq!(text, "version 1.0;");
//! }
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ## Differences from tar
//!
//! * Extended attributes are neither written nor restored; sparse files
//!   are read as regular data blocks.
//! * `Header` does not offer the `as_gnu`/`as_ustar` struct views.

#![warn(missing_docs)]

mod archive;
mod builder;
mod entry_type;
mod header;

pub use crate::archive::{Archive, Entries, Entry, Unpacked};
pub use crate::builder::{Builder, HeaderMode};
pub use crate::entry_type::EntryType;
pub use crate::header::Header;
