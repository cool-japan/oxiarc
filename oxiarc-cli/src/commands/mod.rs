//! Command implementations for OxiArc CLI.

pub mod convert;
pub mod create;
pub mod detect;
pub mod extract;
pub mod info;
pub mod list;
pub mod test;

pub use convert::cmd_convert;
pub use create::{CompressionLevel, OutputFormat, cmd_create};
pub use detect::cmd_detect;
pub use extract::cmd_extract;
pub use info::cmd_info;
pub use list::cmd_list;
pub use test::cmd_test;
