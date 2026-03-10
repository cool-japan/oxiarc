//! LZ4 frame format types and constants.

use oxiarc_core::error::{OxiArcError, Result};

/// LZ4 frame magic number.
pub const LZ4_FRAME_MAGIC: u32 = 0x184D2204;

/// LZ4 legacy magic number (simple framing).
pub(super) const LZ4_LEGACY_MAGIC: u32 = 0x184C2102;

/// Block maximum sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum BlockMaxSize {
    /// 64 KB maximum block size.
    Size64KB = 4,
    /// 256 KB maximum block size.
    Size256KB = 5,
    /// 1 MB maximum block size.
    Size1MB = 6,
    /// 4 MB maximum block size (default).
    #[default]
    Size4MB = 7,
}

impl BlockMaxSize {
    /// Get the actual byte size for this block max setting.
    pub fn size_bytes(self) -> usize {
        match self {
            BlockMaxSize::Size64KB => 64 * 1024,
            BlockMaxSize::Size256KB => 256 * 1024,
            BlockMaxSize::Size1MB => 1024 * 1024,
            BlockMaxSize::Size4MB => 4 * 1024 * 1024,
        }
    }

    /// Convert from the 3-bit BD field value.
    pub(super) fn from_bd(bd: u8) -> Option<Self> {
        match (bd >> 4) & 0x07 {
            4 => Some(BlockMaxSize::Size64KB),
            5 => Some(BlockMaxSize::Size256KB),
            6 => Some(BlockMaxSize::Size1MB),
            7 => Some(BlockMaxSize::Size4MB),
            _ => None,
        }
    }

    /// Convert to the BD byte value.
    pub(super) fn to_bd(self) -> u8 {
        (self as u8) << 4
    }
}

/// Frame descriptor flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDescriptor {
    /// Block independence flag (blocks can be decoded independently).
    pub block_independence: bool,
    /// Block checksum flag (each block has XXH32 checksum).
    pub block_checksum: bool,
    /// Content size present in header.
    pub content_size: Option<u64>,
    /// Content checksum flag (frame has XXH32 checksum at end).
    pub content_checksum: bool,
    /// Block maximum size.
    pub block_max_size: BlockMaxSize,
    /// Dictionary ID (when using dictionary compression).
    pub dict_id: Option<u32>,
}

impl FrameDescriptor {
    /// Create default frame descriptor.
    pub fn new() -> Self {
        Self {
            block_independence: true,
            block_checksum: false,
            content_size: None,
            content_checksum: true,
            block_max_size: BlockMaxSize::default(),
            dict_id: None,
        }
    }

    /// Create with content size.
    pub fn with_content_size(mut self, size: u64) -> Self {
        self.content_size = Some(size);
        self
    }

    /// Set block checksum flag.
    pub fn with_block_checksum(mut self, enabled: bool) -> Self {
        self.block_checksum = enabled;
        self
    }

    /// Set content checksum flag.
    pub fn with_content_checksum(mut self, enabled: bool) -> Self {
        self.content_checksum = enabled;
        self
    }

    /// Set block max size.
    pub fn with_block_max_size(mut self, size: BlockMaxSize) -> Self {
        self.block_max_size = size;
        self
    }

    /// Set dictionary ID for dictionary-based compression.
    pub fn with_dict_id(mut self, id: u32) -> Self {
        self.dict_id = Some(id);
        self
    }

    /// Encode FLG byte.
    pub(super) fn flg_byte(&self) -> u8 {
        let mut flg = 0x40; // Version = 01
        if self.block_independence {
            flg |= 0x20;
        }
        if self.block_checksum {
            flg |= 0x10;
        }
        if self.content_size.is_some() {
            flg |= 0x08;
        }
        if self.content_checksum {
            flg |= 0x04;
        }
        if self.dict_id.is_some() {
            flg |= 0x01;
        }
        flg
    }

    /// Parse from FLG and BD bytes.
    pub(super) fn parse(flg: u8, bd: u8) -> Result<Self> {
        // Check version (must be 01)
        if (flg >> 6) != 0x01 {
            return Err(OxiArcError::invalid_header("unsupported LZ4 frame version"));
        }

        // Reserved bit must be 0
        if (flg & 0x02) != 0 {
            return Err(OxiArcError::invalid_header("reserved FLG bit set"));
        }

        // Reserved bits in BD must be 0
        if (bd & 0x8F) != 0 {
            return Err(OxiArcError::invalid_header("reserved BD bits set"));
        }

        let block_max_size = BlockMaxSize::from_bd(bd)
            .ok_or_else(|| OxiArcError::invalid_header("invalid block max size"))?;

        Ok(Self {
            block_independence: (flg & 0x20) != 0,
            block_checksum: (flg & 0x10) != 0,
            content_size: if (flg & 0x08) != 0 { Some(0) } else { None },
            content_checksum: (flg & 0x04) != 0,
            block_max_size,
            dict_id: if (flg & 0x01) != 0 { Some(0) } else { None },
        })
    }
}
