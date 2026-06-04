//! zccache daemon library.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

#![allow(clippy::missing_errors_doc)]

pub mod compile_journal;
pub mod crash;
pub mod event_log;
pub mod eviction;
pub mod fingerprint;
pub mod lifecycle;
pub mod lineage;
mod process;
pub mod server;
pub mod side_effect;
pub mod stats;
pub mod trampoline;

pub use event_log::EventLogger;
pub use server::{DaemonServer, DepGraphSetter};
pub use stats::{PhaseProfiler, ProfileSnapshot, StatsCollector};
