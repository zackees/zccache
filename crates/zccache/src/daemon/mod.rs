//! zccache daemon library.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

#![allow(clippy::missing_errors_doc)]

pub(crate) mod child_watchdog;
pub mod compile_journal;
pub mod crash;
/// Standalone daemon process entry point (issue #997), gated so it only
/// compiles when a binary that hosts it (`daemon-bin`, or the `cli`/`zccache`
/// binary via argv[0] dispatch) pulls in clap + tracing-subscriber.
#[cfg(feature = "daemon-entry")]
pub mod entry;
pub mod event_log;
pub mod eviction;
pub mod fingerprint;
pub mod jobserver;
pub mod lifecycle;
pub mod lineage;
pub(crate) mod process;
pub mod server;
pub mod side_effect;
pub mod stats;
pub mod trampoline;

pub use event_log::EventLogger;
pub use server::{DaemonServer, DepGraphSetter};
pub use stats::{PhaseProfiler, ProfileSnapshot, StatsCollector};
