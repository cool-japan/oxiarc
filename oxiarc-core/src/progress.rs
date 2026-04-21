//! Progress reporting infrastructure for archive operations.

use std::sync::Arc;

/// Trait for receiving progress notifications during archive operations.
pub trait ProgressSink: Send + Sync {
    /// Called when bytes are processed.
    fn on_progress(&self, processed: u64, total: Option<u64>);

    /// Called when a new entry is about to be processed.
    fn on_entry(&self, _name: &str, _index: u64) {}

    /// Called when the operation is complete.
    fn on_finish(&self) {}
}

/// A no-op progress sink that discards all notifications.
pub struct NoopProgress;

impl ProgressSink for NoopProgress {
    fn on_progress(&self, _processed: u64, _total: Option<u64>) {}
}

/// A shared, type-erased progress sink handle.
pub type ProgressHandle = Arc<dyn ProgressSink>;

/// Returns a `ProgressHandle` that discards all progress notifications.
pub fn noop_progress() -> ProgressHandle {
    Arc::new(NoopProgress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingSink {
        calls: AtomicU64,
        last_processed: AtomicU64,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: AtomicU64::new(0),
                last_processed: AtomicU64::new(0),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ProgressSink for CountingSink {
        fn on_progress(&self, processed: u64, _total: Option<u64>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_processed.store(processed, Ordering::SeqCst);
        }
    }

    #[test]
    fn noop_progress_compiles_and_callable() {
        let h = noop_progress();
        h.on_progress(100, Some(200));
        h.on_entry("foo.txt", 0);
        h.on_finish();
    }

    #[test]
    fn progress_handle_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProgressHandle>();
    }

    #[test]
    fn counting_sink_receives_calls() {
        let sink = Arc::new(CountingSink::new());
        let handle: ProgressHandle = sink.clone();
        handle.on_progress(1024, None);
        handle.on_progress(2048, None);
        assert_eq!(sink.call_count(), 2);
    }
}
