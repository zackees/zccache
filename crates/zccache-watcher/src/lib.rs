//! File watcher for zccache.
//!
//! Provides cross-platform file watching with settle/coalesce buffering,
//! directory ignore filtering, and integration with the metadata cache's
//! clock-based change tracking.
//!
//! Architecture:
//! ```text
//! notify (OS thread) → mpsc → SettleBuffer (tokio task) → CacheSystem
//! ```

#![allow(clippy::missing_errors_doc)]

pub mod ignore;
pub mod notify_watcher;
pub mod polling_watcher;
pub mod recovery;
pub mod settle;

#[cfg(feature = "python")]
mod python;

use zccache_core::NormalizedPath;

pub use ignore::IgnoreFilter;
pub use notify_watcher::NotifyWatcher;
pub use polling_watcher::{
    PollWatchBatch, PollWatchObserver, PollingWatcher, PollingWatcherConfig,
};
#[cfg(feature = "python")]
pub use python::{NativeWatcher, WatchBatch};
pub use recovery::OverflowRecovery;
pub use settle::{SettleBuffer, SettledEvent};

/// Events produced by the file watcher.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A file was modified.
    Modified(NormalizedPath),
    /// A file was created.
    Created(NormalizedPath),
    /// A file was removed.
    Removed(NormalizedPath),
    /// A file was renamed (from, to).
    Renamed {
        from: NormalizedPath,
        to: NormalizedPath,
    },
    /// The watcher's event buffer overflowed. All watched paths
    /// should be considered stale.
    Overflow,
    /// An error occurred in the watcher backend.
    Error(String),
}

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Settle window in milliseconds.
    pub settle_window_ms: u64,
    /// Directory patterns to ignore.
    pub ignore_patterns: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            settle_window_ms: 50,
            ignore_patterns: IgnoreFilter::default_patterns(),
        }
    }
}
