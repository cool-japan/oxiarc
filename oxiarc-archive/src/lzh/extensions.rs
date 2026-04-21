//! LZH/LHA extension-header encoders and decoders.
//!
//! This module implements the seven Unix-oriented LZH extension headers
//! that are emitted by modern LHA producers and consumed by `unlha`,
//! `lhasa`, and the GNU / macOS `lha` variants. The set covers all
//! metadata that the reader in [`super`] is expected to round-trip.
//!
//! ## Encoded types
//!
//! | Type  | Meaning                   | Payload                                     |
//! |-------|---------------------------|---------------------------------------------|
//! | 0x40  | DOS attribute             | 1 byte                                      |
//! | 0x41  | Unix gid + uid            | 2 bytes LE gid, then 2 bytes LE uid         |
//! | 0x42  | Unix group name           | variable-length UTF-8 (lossy-decoded back)  |
//! | 0x43  | Unix user name            | variable-length UTF-8                       |
//! | 0x44  | Unix mtime (u32 LE)       | 4 bytes                                     |
//! | 0x46  | Comment                   | variable-length UTF-8                       |
//! | 0x50  | Unix file permission      | 2 bytes LE                                  |
//!
//! Spec reference: "LHA header / extended header 形式", UNIX 系 LHA
//! tools, and the lhasa documentation. The byte order of 0x41 is
//! historically **gid first, uid second**; both the reader and the
//! writer here use that convention.
//!
//! The wire format for an individual extension header inside a
//! Level-2/Level-3 header stream is:
//!
//! ```text
//! [next_size: u16_le or u32_le][type: u8][data: next_size - 1 bytes]
//! ```
//!
//! where the leading `next_size` is a 16-bit field for Level-2 headers
//! and a 32-bit field for Level-3 headers. The emission helpers below
//! return just the `[type + data]` payload; the caller in
//! `super::LzhWriter::write_level3_header` wraps each payload with the
//! appropriate 32-bit size prefix.

/// Optional metadata that callers can attach to an LZH entry before
/// writing it. All fields default to `None` and only populated fields
/// are emitted as extension headers.
///
/// ## Example
/// ```no_run
/// use oxiarc_archive::lzh::{LzhWriter, LzhExtensionMetadata};
///
/// let mut out = Vec::new();
/// let mut writer = LzhWriter::new(&mut out).with_header_level(3);
///
/// let meta = LzhExtensionMetadata {
///     unix_uid: Some(1000),
///     unix_gid: Some(1000),
///     unix_user_name: Some("alice".into()),
///     unix_group_name: Some("users".into()),
///     unix_mtime: Some(1_704_067_200),
///     unix_permission: Some(0o644),
///     comment: Some("hello".into()),
///     dos_attr: Some(0x20),
/// };
/// writer
///     .add_file_with_metadata("readme.txt", b"hi", &meta)
///     .expect("add_file_with_metadata");
/// writer.finish().expect("finish");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LzhExtensionMetadata {
    /// DOS attribute byte (LZH extension 0x40). Typical values are
    /// `0x20` (archive) and `0x10` (directory).
    pub dos_attr: Option<u8>,

    /// Unix UID (LZH extension 0x41, second u16 LE in the payload).
    pub unix_uid: Option<u16>,

    /// Unix GID (LZH extension 0x41, first u16 LE in the payload).
    pub unix_gid: Option<u16>,

    /// Unix group name (LZH extension 0x42).
    pub unix_group_name: Option<String>,

    /// Unix user name (LZH extension 0x43).
    pub unix_user_name: Option<String>,

    /// Unix mtime, seconds since epoch, u32 LE (LZH extension 0x44).
    ///
    /// Note that this co-exists with the mtime field in the fixed part
    /// of the header; extension 0x44 is the higher-fidelity value and
    /// is the one the reader surfaces when both are present.
    pub unix_mtime: Option<u32>,

    /// Free-form comment (LZH extension 0x46).
    pub comment: Option<String>,

    /// Unix file permission bits, u16 LE (LZH extension 0x50). Callers
    /// typically set this to the three-digit octal mode such as
    /// `0o644` or `0o755`.
    pub unix_permission: Option<u16>,
}

impl LzhExtensionMetadata {
    /// Return `true` when at least one extension-header-bearing field
    /// is populated. Used by the writer to decide whether to emit
    /// extension headers at all.
    pub fn any_set(&self) -> bool {
        self.dos_attr.is_some()
            || self.unix_uid.is_some()
            || self.unix_gid.is_some()
            || self.unix_group_name.is_some()
            || self.unix_user_name.is_some()
            || self.unix_mtime.is_some()
            || self.comment.is_some()
            || self.unix_permission.is_some()
    }
}

/// Build the `[type + data]` payload for a single extension header.
///
/// The returned vector does NOT include the leading size field; the
/// caller wraps the payload with either a `u16_le` (Level 2) or
/// `u32_le` (Level 3) size prefix.
fn payload(ext_type: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + data.len());
    v.push(ext_type);
    v.extend_from_slice(data);
    v
}

/// Emit the `0x40` DOS-attribute extension payload.
pub fn encode_dos_attr(attr: u8) -> Vec<u8> {
    payload(0x40, &[attr])
}

/// Emit the `0x41` gid+uid extension payload.
///
/// Byte layout: `gid_le(2) | uid_le(2)`.
pub fn encode_unix_ids(gid: u16, uid: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(4);
    data.extend_from_slice(&gid.to_le_bytes());
    data.extend_from_slice(&uid.to_le_bytes());
    payload(0x41, &data)
}

/// Emit the `0x42` group-name extension payload.
pub fn encode_group_name(name: &str) -> Vec<u8> {
    payload(0x42, name.as_bytes())
}

/// Emit the `0x43` user-name extension payload.
pub fn encode_user_name(name: &str) -> Vec<u8> {
    payload(0x43, name.as_bytes())
}

/// Emit the `0x44` Unix mtime extension payload.
pub fn encode_unix_mtime(mtime: u32) -> Vec<u8> {
    payload(0x44, &mtime.to_le_bytes())
}

/// Emit the `0x46` comment extension payload.
pub fn encode_comment(comment: &str) -> Vec<u8> {
    payload(0x46, comment.as_bytes())
}

/// Emit the `0x50` Unix file-permission extension payload.
pub fn encode_unix_permission(mode: u16) -> Vec<u8> {
    payload(0x50, &mode.to_le_bytes())
}

/// Ordered list of `[type + data]` extension payloads for the supplied
/// metadata, in the canonical emission order.
///
/// The canonical order is: filename (0x01) and directory (0x02) are
/// emitted by the caller first, then this helper returns:
///
/// `0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50`
///
/// Only fields that are `Some(..)` produce payloads.
pub fn encode_metadata_payloads(meta: &LzhExtensionMetadata) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();

    if let Some(attr) = meta.dos_attr {
        out.push(encode_dos_attr(attr));
    }
    // 0x41 requires BOTH uid and gid to be meaningful; if the caller
    // only supplied one, fill the missing half with 0 so the extension
    // round-trips without silently dropping data.
    if meta.unix_uid.is_some() || meta.unix_gid.is_some() {
        let gid = meta.unix_gid.unwrap_or(0);
        let uid = meta.unix_uid.unwrap_or(0);
        out.push(encode_unix_ids(gid, uid));
    }
    if let Some(ref group) = meta.unix_group_name {
        out.push(encode_group_name(group));
    }
    if let Some(ref user) = meta.unix_user_name {
        out.push(encode_user_name(user));
    }
    if let Some(mtime) = meta.unix_mtime {
        out.push(encode_unix_mtime(mtime));
    }
    if let Some(ref comment) = meta.comment {
        out.push(encode_comment(comment));
    }
    if let Some(perm) = meta.unix_permission {
        out.push(encode_unix_permission(perm));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_dos_attr() {
        let p = encode_dos_attr(0x20);
        assert_eq!(p, vec![0x40, 0x20]);
    }

    #[test]
    fn test_encode_unix_ids() {
        // gid=100, uid=1000
        let p = encode_unix_ids(100, 1000);
        // type + gid_le(2) + uid_le(2)
        assert_eq!(p, vec![0x41, 100, 0, 0xE8, 0x03]);
    }

    #[test]
    fn test_encode_unix_mtime() {
        let p = encode_unix_mtime(0x1234_5678);
        assert_eq!(p, vec![0x44, 0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn test_encode_unix_permission() {
        let p = encode_unix_permission(0o755);
        // 0o755 = 0x01ED
        assert_eq!(p, vec![0x50, 0xED, 0x01]);
    }

    #[test]
    fn test_encode_metadata_empty() {
        let m = LzhExtensionMetadata::default();
        assert!(!m.any_set());
        assert!(encode_metadata_payloads(&m).is_empty());
    }

    #[test]
    fn test_encode_metadata_order() {
        let m = LzhExtensionMetadata {
            dos_attr: Some(0x20),
            unix_uid: Some(1000),
            unix_gid: Some(100),
            unix_group_name: Some("users".into()),
            unix_user_name: Some("alice".into()),
            unix_mtime: Some(0x1234_5678),
            comment: Some("note".into()),
            unix_permission: Some(0o644),
        };
        let payloads = encode_metadata_payloads(&m);
        assert_eq!(payloads.len(), 7);
        // Check canonical order by inspecting the type byte in each payload
        let types: Vec<u8> = payloads.iter().map(|p| p[0]).collect();
        assert_eq!(types, vec![0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50]);
    }

    #[test]
    fn test_encode_metadata_partial_unix_ids() {
        // Only uid supplied — gid zero-filled
        let m = LzhExtensionMetadata {
            unix_uid: Some(1000),
            ..Default::default()
        };
        let payloads = encode_metadata_payloads(&m);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0], vec![0x41, 0, 0, 0xE8, 0x03]);
    }
}
