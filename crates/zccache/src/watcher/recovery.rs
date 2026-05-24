//! Deferred overflow recovery.
//!
//! After a watcher overflow, schedules a background filesystem re-scan
//! with a configurable delay. If a build event arrives before the delay
//! elapses, the re-scan runs immediately — so the cache is warm when
//! it matters.
//!
//! ```text
//! overflow → wait(delay OR build_event) → rescan_entries → loop
//! ```

use crate::fscache::CacheSystem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Deferred overflow recovery.
///
/// Sits between the watcher (which detects overflows) and the cache
/// (which needs re-verification). Schedules a background re-stat with
/// a long delay, but triggers immediately if a compilation request
/// arrives — because that's when cache accuracy actually matters.
#[derive(Debug)]
pub struct OverflowRecovery {
    overflow: Notify,
    build_trigger: Notify,
    pending: AtomicBool,
    delay: Duration,
}

impl OverflowRecovery {
    /// Create a recovery handler with the given rescan delay.
    #[must_use]
    pub fn new(delay: Duration) -> Self {
        Self {
            overflow: Notify::new(),
            build_trigger: Notify::new(),
            pending: AtomicBool::new(false),
            delay,
        }
    }

    /// Create a recovery handler with the default 30-second delay.
    #[must_use]
    pub fn default_delay() -> Self {
        Self::new(Duration::from_secs(30))
    }

    /// Returns whether a rescan is currently pending.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Signal that a watcher overflow occurred.
    ///
    /// The daemon calls this after `CacheSystem::apply_overflow()`.
    /// Wakes the background task to start the rescan delay.
    pub fn on_overflow(&self) {
        tracing::info!("overflow recovery: overflow detected, scheduling deferred rescan");
        self.pending.store(true, Ordering::Release);
        self.overflow.notify_one();
    }

    /// Signal that a build event (compilation request) arrived.
    ///
    /// If an overflow rescan is pending, this cancels the deferred
    /// timer and triggers the rescan immediately. If no overflow is
    /// pending, this is a no-op.
    pub fn on_build_event(&self) {
        if self.pending.load(Ordering::Acquire) {
            tracing::info!(
                "overflow recovery: build event received, \
                 cancelling deferred rescan and triggering immediately"
            );
            self.build_trigger.notify_one();
        }
    }

    /// Background recovery loop.
    ///
    /// Waits for overflow events, then rescans after either the delay
    /// elapses or a build event arrives (whichever comes first).
    /// The losing branch of the select is cancelled — no duplicate
    /// rescans.
    ///
    /// This task runs for the lifetime of the daemon.
    pub async fn run(&self, cache: &CacheSystem) {
        loop {
            // Block until an overflow occurs.
            self.overflow.notified().await;

            tracing::info!(
                delay_secs = self.delay.as_secs(),
                "overflow recovery: rescan scheduled"
            );

            // Wait for either the delay or a build event.
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {
                    tracing::info!(
                        "overflow recovery: delay elapsed, starting deferred rescan"
                    );
                }
                () = self.build_trigger.notified() => {
                    tracing::info!(
                        "overflow recovery: build event triggered immediate rescan, \
                         deferred timer cancelled"
                    );
                }
            }

            let promoted = cache.rescan_entries();
            self.pending.store(false, Ordering::Release);

            tracing::info!(
                promoted,
                "overflow recovery: rescan complete, deferred task removed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::NormalizedPath;
    use crate::fscache::clock::Clock;
    use crate::fscache::Confidence;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn create_file(dir: &TempDir, name: &str, content: &str) -> NormalizedPath {
        let path = dir.path().join(name);
        fs::write(&path, content).expect("failed to create test file");
        path.into()
    }

    #[tokio::test]
    async fn recovery_rescans_after_delay() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_overflow();

        assert_eq!(
            cache.metadata().get(&path).unwrap().confidence,
            Confidence::Low
        );

        let recovery = Arc::new(OverflowRecovery::new(Duration::from_millis(50)));
        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        recovery.on_overflow();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(
            cache.metadata().get(&path).unwrap().confidence,
            Confidence::High
        );

        handle.abort();
    }

    #[tokio::test]
    async fn recovery_rescans_immediately_on_build_event() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_overflow();

        // Long delay — should NOT wait this long.
        let recovery = Arc::new(OverflowRecovery::new(Duration::from_secs(60)));
        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        recovery.on_overflow();

        // Retry build_trigger until the spawned task has entered the select!
        // and registered its notified() future. Two yield_now() calls are not
        // enough under CI load — the notification is lost if sent before the
        // listener awaits. Retrying is safe: on_build_event is a no-op once
        // pending becomes false after a successful rescan.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            // Give the spawned task a chance to progress.
            tokio::time::sleep(Duration::from_millis(5)).await;
            recovery.on_build_event();

            if cache.metadata().get(&path).unwrap().confidence == Confidence::High {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for build-triggered rescan"
            );
        }

        handle.abort();
    }

    #[tokio::test]
    async fn no_overflow_means_no_rescan() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.metadata().downgrade_all();

        let recovery = Arc::new(OverflowRecovery::new(Duration::from_millis(10)));
        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        // Send build event without overflow — no rescan should happen.
        recovery.on_build_event();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Still Low — no overflow was signaled.
        assert_eq!(
            cache.metadata().get(&path).unwrap().confidence,
            Confidence::Low
        );

        handle.abort();
    }

    #[tokio::test]
    async fn default_delay_creates_recovery() {
        let recovery = OverflowRecovery::default_delay();
        assert_eq!(recovery.delay, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn pending_flag_tracks_state() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_overflow();

        let recovery = Arc::new(OverflowRecovery::new(Duration::from_millis(50)));
        assert!(!recovery.is_pending());

        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        recovery.on_overflow();
        assert!(recovery.is_pending());

        // Wait for rescan to complete.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!recovery.is_pending());

        handle.abort();
    }

    #[tokio::test]
    async fn build_event_is_noop_without_pending_overflow() {
        // Ensure stale build_trigger permits don't cause spurious rescans.
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.metadata().downgrade_all();

        let recovery = Arc::new(OverflowRecovery::new(Duration::from_millis(10)));
        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        // Build events without overflow: should not store a trigger permit.
        recovery.on_build_event();
        recovery.on_build_event();
        recovery.on_build_event();
        assert!(!recovery.is_pending());

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Still Low — no overflow, no rescan.
        assert_eq!(
            cache.metadata().get(&path).unwrap().confidence,
            Confidence::Low
        );

        handle.abort();
    }

    #[tokio::test]
    async fn multiple_overflows_coalesce() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.h", "content");

        let cache = Arc::new(CacheSystem::new());
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_overflow();

        let recovery = Arc::new(OverflowRecovery::new(Duration::from_millis(30)));
        let r = Arc::clone(&recovery);
        let c = Arc::clone(&cache);
        let handle = tokio::spawn(async move { r.run(&c).await });

        // Fire multiple overflows rapidly.
        recovery.on_overflow();
        recovery.on_overflow();
        recovery.on_overflow();

        // Poll until the rescan completes — fixed sleeps are racy under CI load.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if cache.metadata().get(&path).unwrap().confidence == Confidence::High {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for overflow recovery rescan"
            );
        }

        handle.abort();
    }
}
