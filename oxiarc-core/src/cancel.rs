//! Cooperative cancellation primitive for archive operations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::error::OxiArcError;

/// A cooperative cancellation token that can be shared across threads.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates a new, uncancelled token.
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signals cancellation to all holders of this token (or any of its clones).
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Returns `true` if cancellation has been signalled.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Returns `Err(OxiArcError::Cancelled)` if cancellation has been signalled,
    /// or `Ok(())` otherwise.
    pub fn check(&self) -> Result<(), OxiArcError> {
        if self.is_cancelled() {
            Err(OxiArcError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};

    #[test]
    fn token_is_send_sync_clone() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<CancellationToken>();
    }

    #[test]
    fn cancel_is_observed() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn check_returns_error_after_cancel() {
        let token = CancellationToken::new();
        assert!(token.check().is_ok());
        token.cancel();
        assert!(matches!(token.check(), Err(OxiArcError::Cancelled)));
    }

    #[test]
    fn clone_shares_flag() {
        let token = CancellationToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn cross_thread_cancel() {
        let token = CancellationToken::new();
        let clone = token.clone();
        let observed = Arc::new(StdAtomicBool::new(false));
        let obs_clone = observed.clone();
        let handle = std::thread::spawn(move || {
            while !clone.is_cancelled() {
                std::thread::yield_now();
            }
            obs_clone.store(true, StdOrdering::Release);
        });
        token.cancel();
        handle.join().unwrap();
        assert!(observed.load(StdOrdering::Acquire));
    }
}
