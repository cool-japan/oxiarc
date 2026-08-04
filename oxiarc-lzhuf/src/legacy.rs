//! Legacy LZH-family compression methods.
//!
//! Beyond the LHarc/LHA `-lh0-`..`-lh7-` line, `.lzh` containers in the wild
//! also carry methods from LArc (`-lzs-`, `-lz4-`, `-lz5-`), from LHarc 2.x
//! (`-lh2-`, `-lh3-`) and from PMarc (`-pm0-`, `-pm2-`). They share the LZH
//! container but not the codec: LArc has no entropy coding at all and addresses
//! history *absolutely*, LHarc 2.x uses adaptive or block-static Huffman with a
//! table format unrelated to lh4-lh7's, and PMarc is a different design again.
//!
//! | Method | Codec | Window | Direction |
//! |--------|-------|--------|-----------|
//! | `-lzs-` | LZSS, 11-bit index + 4-bit length | 2 KiB | decode + encode |
//! | `-lz4-` | stored | — | decode + encode |
//! | `-lz5-` | LZSS, 12-bit index + 4-bit length | 4 KiB | decode + encode |
//! | `-lh2-` | adaptive Huffman + LZSS | 8 KiB | decode + encode |
//! | `-lh3-` | block-static Huffman + LZSS | 8 KiB | decode + encode |
//! | `-pm0-` | stored | — | decode + encode |
//!
//! `-pm1-`/`-pm2-` (PMarc) are recognised as method identifiers but are not
//! implemented; see the crate's `TODO.md` for the specific gap.

pub(crate) mod dynhuff;
pub(crate) mod huffcode;
pub mod larc;
pub mod lh2;
pub mod lh3;
pub(crate) mod ring;

pub use larc::{decode_lz5, decode_lzs, encode_lz5, encode_lzs};
pub use lh2::{decode_lh2, encode_lh2};
pub use lh3::{PositionTablePolicy, decode_lh3, encode_lh3, encode_lh3_with};
