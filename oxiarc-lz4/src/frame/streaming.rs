//! LZ4 streaming compressor and decompressor implementing core traits.

use super::compress::compress_with_options;
use super::decompress::decompress;
use super::types::FrameDescriptor;
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};

/// LZ4 compressor implementing the Compressor trait.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`Lz4Compressor::with_progress`] / [`Lz4Compressor::with_cancel`] builders.
pub struct Lz4Compressor {
    buffer: Vec<u8>,
    desc: FrameDescriptor,
    finished: bool,
    /// Optional progress sink. Notified once after compression completes.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked at the start of each compress call.
    cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for Lz4Compressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4Compressor")
            .field("buffer_len", &self.buffer.len())
            .field("finished", &self.finished)
            .finish()
    }
}

impl Lz4Compressor {
    /// Create a new LZ4 compressor with default options.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            desc: FrameDescriptor::new(),
            finished: false,
            progress: None,
            cancel: None,
        }
    }

    /// Create a new LZ4 compressor with custom options.
    pub fn with_options(desc: FrameDescriptor) -> Self {
        Self {
            buffer: Vec::new(),
            desc,
            finished: false,
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes, None)` is called once after the full
    /// compression is done and the data is placed into the output buffer.
    /// `on_finish()` is called at the same point.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`Compressor::compress`] call.
    /// If cancelled, the call returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
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

        // Cooperative cancellation check before starting.
        if let Some(ref token) = self.cancel {
            token.check()?;
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
                let total_bytes = self.buffer.len() as u64;
                if let Some(ref handle) = self.progress {
                    handle.on_progress(total_bytes, None);
                    handle.on_finish();
                }
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
} // end impl Compressor for Lz4Compressor

/// LZ4 decompressor implementing the Decompressor trait.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`Lz4Decompressor::with_progress`] / [`Lz4Decompressor::with_cancel`] builders.
pub struct Lz4Decompressor {
    buffer: Vec<u8>,
    finished: bool,
    /// Optional progress sink. Notified once after decompression completes.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked at the start of each decompress call.
    cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for Lz4Decompressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4Decompressor")
            .field("buffer_len", &self.buffer.len())
            .field("finished", &self.finished)
            .finish()
    }
}

impl Lz4Decompressor {
    /// Create a new LZ4 decompressor.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            finished: false,
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes, None)` is called once after the full
    /// decompression is done. `on_finish()` is called at the same point.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`Decompressor::decompress`] call.
    /// If cancelled, the call returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
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

        // Cooperative cancellation check before starting.
        if let Some(ref token) = self.cancel {
            token.check()?;
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
                    let total_bytes = decompressed.len() as u64;
                    if let Some(ref handle) = self.progress {
                        handle.on_progress(total_bytes, None);
                        handle.on_finish();
                    }
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
} // end impl Decompressor for Lz4Decompressor

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::progress::ProgressSink;
    use std::sync::{Arc, Mutex};

    type ProgressLog = Arc<Mutex<Vec<(u64, Option<u64>)>>>;

    struct MockSink(ProgressLog);

    impl ProgressSink for MockSink {
        fn on_progress(&self, processed: u64, total: Option<u64>) {
            self.0
                .lock()
                .expect("lock poisoned")
                .push((processed, total));
        }
    }

    fn make_compressible_data(size: usize) -> Vec<u8> {
        let pattern = b"LZ4 streaming test data with repeating pattern ABCDEFGH ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk = &pattern[..remaining.min(pattern.len())];
            data.extend_from_slice(chunk);
        }
        data
    }

    #[test]
    fn test_lz4_compressor_progress_reports() {
        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let data = make_compressible_data(1024 * 1024); // 1 MB
        let mut compressor = Lz4Compressor::new().with_progress(sink as ProgressHandle);

        let mut output = vec![0u8; data.len() * 2];
        let _ = compressor
            .compress(&data, &mut output, FlushMode::None)
            .expect("compress none");
        let _ = compressor
            .compress(&[], &mut output, FlushMode::Finish)
            .expect("compress finish");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
        let (last_processed, _) = *recorded.last().expect("non-empty");
        assert_eq!(
            last_processed,
            data.len() as u64,
            "final processed count must equal input size"
        );
    }

    #[test]
    fn test_lz4_compressor_cancel_aborts() {
        let token = CancellationToken::new();
        let mut compressor = Lz4Compressor::new().with_cancel(token.clone());

        let data = make_compressible_data(1024);
        let mut output = vec![0u8; data.len() * 2];

        token.cancel();

        let result = compressor.compress(&data, &mut output, FlushMode::Finish);
        assert!(result.is_err(), "expected cancellation error");
    }

    #[test]
    fn test_lz4_decompressor_progress_reports() {
        let data = make_compressible_data(1024 * 1024); // 1 MB
        let mut enc = Lz4Compressor::new();
        let mut compressed = vec![0u8; data.len() * 2];
        enc.compress(&data, &mut compressed, FlushMode::None)
            .expect("compress none");
        let (_, written, _) = enc
            .compress(&[], &mut compressed, FlushMode::Finish)
            .expect("compress finish");
        let compressed = compressed[..written].to_vec();

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let mut decompressor = Lz4Decompressor::new().with_progress(sink as ProgressHandle);
        let mut output = vec![0u8; data.len() * 2];
        decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
        let (last_processed, _) = *recorded.last().expect("non-empty");
        assert_eq!(
            last_processed,
            data.len() as u64,
            "final processed count must equal decompressed size"
        );
    }

    #[test]
    fn test_lz4_decompressor_cancel_aborts() {
        let data = make_compressible_data(1024);
        let mut enc = Lz4Compressor::new();
        let mut compressed = vec![0u8; data.len() * 2];
        enc.compress(&data, &mut compressed, FlushMode::None)
            .expect("compress none");
        let (_, written, _) = enc
            .compress(&[], &mut compressed, FlushMode::Finish)
            .expect("compress finish");
        let compressed = compressed[..written].to_vec();

        let token = CancellationToken::new();
        let mut decompressor = Lz4Decompressor::new().with_cancel(token.clone());
        let mut output = vec![0u8; data.len() * 2];

        token.cancel();
        let result = decompressor.decompress(&compressed, &mut output);
        assert!(result.is_err(), "expected cancellation error");
    }
}
