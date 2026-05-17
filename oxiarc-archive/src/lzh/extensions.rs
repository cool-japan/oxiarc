//! LZH/LHA extension-header encoders and decoders.
//!
//! This module implements the standard LHA extension headers emitted by
//! modern LHA producers (lhasa, GNU lha, macOS lha) and required for
//! full metadata round-trips.
//!
//! ## Encoded types
//!
//! | Type  | Meaning                             | Payload                                           |
//! |-------|-------------------------------------|---------------------------------------------------|
//! | 0x40  | OS/2 / MS-DOS attributes            | 2 bytes LE (attribute word)                       |
//! | 0x41  | Windows timestamps (FILETIME×3)     | 24 bytes: creation(8) + access(8) + modify(8)     |
//! | 0x42  | Uncompressed size 64-bit            | 8 bytes LE u64                                    |
//! | 0x43  | Compressed size 64-bit              | 8 bytes LE u64                                    |
//! | 0x44  | Comment                             | variable-length UTF-8 string                      |
//! | 0x46  | Unix file permissions               | 2 bytes LE (mode_t low 16 bits)                   |
//! | 0x50  | Unix owner names                    | user\0group (two null-terminated strings)         |
//! | 0x51  | Unix owner IDs                      | 8 bytes: uid(4 LE) + gid(4 LE)                    |
//! | 0x54  | Unix mtime (legacy bonus)           | 4 bytes LE u32 seconds-since-epoch                |
//!
//! Spec reference: standard LHA / lhasa header format documentation.
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
///     unix_owner_user: Some("alice".into()),
///     unix_owner_group: Some("users".into()),
///     unix_mtime: Some(1_704_067_200),
///     unix_permission: Some(0o644),
///     comment: Some("hello".into()),
///     dos_attr: Some(0x0020),
///     ..Default::default()
/// };
/// writer
///     .add_file_with_metadata("readme.txt", b"hi", &meta)
///     .expect("add_file_with_metadata");
/// writer.finish().expect("finish");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LzhExtensionMetadata {
    /// OS/2 / MS-DOS attribute word (LZH extension 0x40), 2-byte LE.
    /// Typical values: `0x0020` (archive), `0x0010` (directory).
    pub dos_attr: Option<u16>,

    /// Windows FILETIME creation timestamp (LZH extension 0x41, first 8 bytes).
    /// 100-nanosecond intervals since 1601-01-01 UTC.
    pub windows_creation: Option<u64>,

    /// Windows FILETIME last-access timestamp (LZH extension 0x41, second 8 bytes).
    pub windows_access: Option<u64>,

    /// Windows FILETIME last-modification timestamp (LZH extension 0x41, third 8 bytes).
    pub windows_modify: Option<u64>,

    /// Uncompressed file size as 64-bit value (LZH extension 0x42).
    /// When `None`, the 32-bit size in the base header is used. Set
    /// automatically by the writer when the size exceeds `u32::MAX`.
    pub uncompressed_size64: Option<u64>,

    /// Compressed (stored) file size as 64-bit value (LZH extension 0x43).
    /// Set automatically by the writer when the size exceeds `u32::MAX`.
    pub compressed_size64: Option<u64>,

    /// Free-form UTF-8 comment (LZH extension 0x44).
    pub comment: Option<String>,

    /// Unix file permission bits, u16 LE (LZH extension 0x46). Callers
    /// typically set this to the three-digit octal mode such as
    /// `0o644` or `0o755`.
    pub unix_permission: Option<u16>,

    /// Unix owner user name (part of LZH extension 0x50, `user\0group`).
    pub unix_owner_user: Option<String>,

    /// Unix owner group name (part of LZH extension 0x50, `user\0group`).
    pub unix_owner_group: Option<String>,

    /// Unix UID (LZH extension 0x51, first u32 LE in the payload).
    pub unix_uid: Option<u32>,

    /// Unix GID (LZH extension 0x51, second u32 LE in the payload).
    pub unix_gid: Option<u32>,

    /// Unix mtime, seconds since epoch, u32 LE (LZH extension 0x54).
    ///
    /// Note that this co-exists with the mtime field in the fixed part
    /// of the header; extension 0x54 is the higher-fidelity value and
    /// is the one the reader surfaces when both are present.
    pub unix_mtime: Option<u32>,
}

impl LzhExtensionMetadata {
    /// Return `true` when at least one extension-header-bearing field
    /// is populated. Used by the writer to decide whether to emit
    /// extension headers at all.
    pub fn any_set(&self) -> bool {
        self.dos_attr.is_some()
            || self.windows_creation.is_some()
            || self.windows_access.is_some()
            || self.windows_modify.is_some()
            || self.uncompressed_size64.is_some()
            || self.compressed_size64.is_some()
            || self.comment.is_some()
            || self.unix_permission.is_some()
            || self.unix_owner_user.is_some()
            || self.unix_owner_group.is_some()
            || self.unix_uid.is_some()
            || self.unix_gid.is_some()
            || self.unix_mtime.is_some()
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

/// Emit the `0x40` OS/2 / MS-DOS attribute extension payload (2 bytes LE).
pub fn encode_dos_attr(attr: u16) -> Vec<u8> {
    payload(0x40, &attr.to_le_bytes())
}

/// Emit the `0x41` Windows timestamps extension payload (24 bytes).
///
/// Byte layout: `creation_le(8) | access_le(8) | modify_le(8)`.
/// Each value is a Windows FILETIME (100-nanosecond ticks since 1601-01-01 UTC).
pub fn encode_windows_timestamps(creation: u64, access: u64, modify: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&creation.to_le_bytes());
    data.extend_from_slice(&access.to_le_bytes());
    data.extend_from_slice(&modify.to_le_bytes());
    payload(0x41, &data)
}

/// Emit the `0x42` uncompressed-size-64 extension payload (8 bytes LE).
pub fn encode_uncompressed_size64(size: u64) -> Vec<u8> {
    payload(0x42, &size.to_le_bytes())
}

/// Emit the `0x43` compressed-size-64 extension payload (8 bytes LE).
pub fn encode_compressed_size64(size: u64) -> Vec<u8> {
    payload(0x43, &size.to_le_bytes())
}

/// Emit the `0x44` comment extension payload (variable-length UTF-8).
pub fn encode_comment(comment: &str) -> Vec<u8> {
    payload(0x44, comment.as_bytes())
}

/// Emit the `0x46` Unix file-permission extension payload (2 bytes LE).
pub fn encode_unix_permission(mode: u16) -> Vec<u8> {
    payload(0x46, &mode.to_le_bytes())
}

/// Emit the `0x50` Unix owner names extension payload.
///
/// Byte layout: `user_name_bytes | 0x00 | group_name_bytes`.
/// Both names are UTF-8 encoded; the NUL byte separates them.
pub fn encode_unix_owner_names(user: &str, group: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(user.len() + 1 + group.len());
    data.extend_from_slice(user.as_bytes());
    data.push(0x00);
    data.extend_from_slice(group.as_bytes());
    payload(0x50, &data)
}

/// Emit the `0x51` Unix owner IDs extension payload (8 bytes).
///
/// Byte layout: `uid_le(4) | gid_le(4)`.
pub fn encode_unix_owner_ids(uid: u32, gid: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&uid.to_le_bytes());
    data.extend_from_slice(&gid.to_le_bytes());
    payload(0x51, &data)
}

/// Emit the `0x54` Unix mtime extension payload (4 bytes LE u32 seconds-since-epoch).
pub fn encode_unix_mtime(mtime: u32) -> Vec<u8> {
    payload(0x54, &mtime.to_le_bytes())
}

/// Ordered list of `[type + data]` extension payloads for the supplied
/// metadata, in the canonical emission order.
///
/// The canonical order is: filename (0x01) and directory (0x02) are
/// emitted by the caller first, then this helper returns:
///
/// `0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50, 0x51, 0x54`
///
/// Only fields that are `Some(..)` produce payloads.
pub fn encode_metadata_payloads(meta: &LzhExtensionMetadata) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();

    // 0x40 — OS/2 / MS-DOS attributes (2 bytes LE)
    if let Some(attr) = meta.dos_attr {
        out.push(encode_dos_attr(attr));
    }

    // 0x41 — Windows FILETIME × 3 (24 bytes).
    // Emitted when any of the three timestamp fields is set;
    // missing fields are filled with zero (epoch / unspecified).
    if meta.windows_creation.is_some()
        || meta.windows_access.is_some()
        || meta.windows_modify.is_some()
    {
        let creation = meta.windows_creation.unwrap_or(0);
        let access = meta.windows_access.unwrap_or(0);
        let modify = meta.windows_modify.unwrap_or(0);
        out.push(encode_windows_timestamps(creation, access, modify));
    }

    // 0x42 — uncompressed size as u64
    if let Some(sz) = meta.uncompressed_size64 {
        out.push(encode_uncompressed_size64(sz));
    }

    // 0x43 — compressed size as u64
    if let Some(sz) = meta.compressed_size64 {
        out.push(encode_compressed_size64(sz));
    }

    // 0x44 — comment (UTF-8)
    if let Some(ref comment) = meta.comment {
        out.push(encode_comment(comment));
    }

    // 0x46 — Unix file permissions
    if let Some(perm) = meta.unix_permission {
        out.push(encode_unix_permission(perm));
    }

    // 0x50 — Unix owner names (user + group, NUL-separated)
    // Emitted when either name is set; missing half uses empty string.
    if meta.unix_owner_user.is_some() || meta.unix_owner_group.is_some() {
        let user = meta.unix_owner_user.as_deref().unwrap_or("");
        let group = meta.unix_owner_group.as_deref().unwrap_or("");
        out.push(encode_unix_owner_names(user, group));
    }

    // 0x51 — Unix owner IDs (uid u32 LE, gid u32 LE)
    if meta.unix_uid.is_some() || meta.unix_gid.is_some() {
        let uid = meta.unix_uid.unwrap_or(0);
        let gid = meta.unix_gid.unwrap_or(0);
        out.push(encode_unix_owner_ids(uid, gid));
    }

    // 0x54 — Unix mtime (bonus, preserves round-trip for Unix archives)
    if let Some(mtime) = meta.unix_mtime {
        out.push(encode_unix_mtime(mtime));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_dos_attr() {
        // 0x40 + 2 bytes LE
        let p = encode_dos_attr(0x0020);
        assert_eq!(p, vec![0x40, 0x20, 0x00]);
    }

    #[test]
    fn test_encode_windows_timestamps() {
        let creation: u64 = 0x0102_0304_0506_0708;
        let access: u64 = 0;
        let modify: u64 = 0xFFFF_FFFF_FFFF_FFFF;
        let p = encode_windows_timestamps(creation, access, modify);
        assert_eq!(p[0], 0x41);
        assert_eq!(p.len(), 1 + 24);
        // Check creation bytes (LE)
        assert_eq!(&p[1..9], &creation.to_le_bytes());
        // Check access bytes
        assert_eq!(&p[9..17], &access.to_le_bytes());
        // Check modify bytes
        assert_eq!(&p[17..25], &modify.to_le_bytes());
    }

    #[test]
    fn test_encode_uncompressed_size64() {
        let sz: u64 = 0x0102_0304_0506_0708;
        let p = encode_uncompressed_size64(sz);
        assert_eq!(p[0], 0x42);
        assert_eq!(p.len(), 9);
        assert_eq!(&p[1..], &sz.to_le_bytes());
    }

    #[test]
    fn test_encode_compressed_size64() {
        let sz: u64 = 0xDEAD_BEEF_1234_5678;
        let p = encode_compressed_size64(sz);
        assert_eq!(p[0], 0x43);
        assert_eq!(p.len(), 9);
        assert_eq!(&p[1..], &sz.to_le_bytes());
    }

    #[test]
    fn test_encode_comment() {
        let p = encode_comment("hello world");
        assert_eq!(p[0], 0x44);
        assert_eq!(&p[1..], b"hello world");
    }

    #[test]
    fn test_encode_unix_permission() {
        let p = encode_unix_permission(0o755);
        // 0o755 = 0x01ED
        assert_eq!(p, vec![0x46, 0xED, 0x01]);
    }

    #[test]
    fn test_encode_unix_owner_names() {
        let p = encode_unix_owner_names("alice", "users");
        assert_eq!(p[0], 0x50);
        assert_eq!(&p[1..7], b"alice\0");
        assert_eq!(&p[7..], b"users");
    }

    #[test]
    fn test_encode_unix_owner_ids() {
        // uid=1000, gid=100
        let p = encode_unix_owner_ids(1000, 100);
        assert_eq!(p[0], 0x51);
        assert_eq!(&p[1..5], &1000u32.to_le_bytes());
        assert_eq!(&p[5..9], &100u32.to_le_bytes());
    }

    #[test]
    fn test_encode_unix_mtime() {
        let p = encode_unix_mtime(0x1234_5678);
        assert_eq!(p, vec![0x54, 0x78, 0x56, 0x34, 0x12]);
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
            dos_attr: Some(0x0020),
            windows_creation: Some(1),
            windows_access: Some(2),
            windows_modify: Some(3),
            uncompressed_size64: Some(1_000_000_000_000),
            compressed_size64: Some(500_000_000_000),
            comment: Some("note".into()),
            unix_permission: Some(0o644),
            unix_owner_user: Some("alice".into()),
            unix_owner_group: Some("staff".into()),
            unix_uid: Some(1000),
            unix_gid: Some(100),
            unix_mtime: Some(0x1234_5678),
        };
        let payloads = encode_metadata_payloads(&m);
        assert_eq!(payloads.len(), 9);
        // Check canonical order by inspecting the type byte in each payload
        let types: Vec<u8> = payloads.iter().map(|p| p[0]).collect();
        assert_eq!(
            types,
            vec![0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50, 0x51, 0x54]
        );
    }

    #[test]
    fn test_encode_metadata_partial_owner_ids() {
        // Only uid supplied — gid zero-filled
        let m = LzhExtensionMetadata {
            unix_uid: Some(1000),
            ..Default::default()
        };
        let payloads = encode_metadata_payloads(&m);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0][0], 0x51);
        assert_eq!(&payloads[0][1..5], &1000u32.to_le_bytes());
        assert_eq!(&payloads[0][5..9], &0u32.to_le_bytes());
    }

    #[test]
    fn test_encode_metadata_partial_windows_timestamps() {
        // Only modify set — creation and access zero-filled
        let m = LzhExtensionMetadata {
            windows_modify: Some(0xABCD_EF01_2345_6789),
            ..Default::default()
        };
        let payloads = encode_metadata_payloads(&m);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0][0], 0x41);
        // creation = 0
        assert_eq!(&payloads[0][1..9], &0u64.to_le_bytes());
        // access = 0
        assert_eq!(&payloads[0][9..17], &0u64.to_le_bytes());
        // modify = supplied value
        assert_eq!(
            &payloads[0][17..25],
            &0xABCD_EF01_2345_6789u64.to_le_bytes()
        );
    }
}
