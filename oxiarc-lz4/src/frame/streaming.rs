//! LZ4 streaming compressor and decompressor implementing core traits.

use super::compress::compress_with_options;
use super::decompress::decompress;
use super::types::FrameDescriptor;
use oxiarc_core::error::Result;
use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};

/// LZ4 compressor implementing the Compressor trait.
#[derive(Debug)]
pub struct Lz4Compressor {
    buffer: Vec<u8>,
    desc: FrameDescriptor,
    finished: bool,
}

impl Lz4Compressor {
    /// Create a new LZ4 compressor with default options.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            desc: FrameDescriptor::new(),
            finished: false,
        }
    }

    /// Create a new LZ4 compressor with custom options.
    pub fn with_options(desc: FrameDescriptor) -> Self {
        Self {
            buffer: Vec::new(),
            desc,
            finished: false,
        }
    }
}

impl Default for Lz4Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor for Lz4Compressor {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        if matches!(flush, FlushMode::Finish) {
            // Compress the buffer using the configured descriptor
            let desc = self.desc.with_content_size(self.buffer.len() as u64);
            let compressed = compress_with_options(&self.buffer, desc)?;

            if compressed.len() <= output.len() {
                output[..compressed.len()].copy_from_slice(&compressed);
                self.finished = true;
                Ok((input.len(), compressed.len(), CompressStatus::Done))
            } else {
                Ok((input.len(), 0, CompressStatus::NeedsOutput))
            }
        } else {
            Ok((input.len(), 0, CompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// LZ4 decompressor implementing the Decompressor trait.
#[derive(Debug)]
pub struct Lz4Decompressor {
    buffer: Vec<u8>,
    finished: bool,
}

impl Lz4Decompressor {
    /// Create a new LZ4 decompressor.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            finished: false,
        }
    }
}

impl Default for Lz4Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Lz4Decompressor {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        // Try to decompress if we have enough data
        if self.buffer.len() >= 7 {
            // Minimum frame size
            match decompress(&self.buffer, 64 * 1024 * 1024) {
                Ok(decompressed) => {
                    let to_copy = decompressed.len().min(output.len());
                    output[..to_copy].copy_from_slice(&decompressed[..to_copy]);
                    self.finished = true;
                    Ok((input.len(), to_copy, DecompressStatus::Done))
                }
                Err(_) => {
                    // Need more data
                    Ok((input.len(), 0, DecompressStatus::NeedsInput))
                }
            }
        } else {
            Ok((input.len(), 0, DecompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}
