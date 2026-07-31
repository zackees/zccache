//! zccache daemon library.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

#![allow(clippy::missing_errors_doc)]

pub(crate) mod child_watchdog;
pub mod compile_journal;
pub(crate) mod compile_output;
pub mod crash;
/// Startup classification / quarantine policy for the persisted depgraph
/// snapshot (#1157). Gated with `entry` because the standalone daemon is its
/// only production caller; `test` keeps it available to the server tests that
/// drive a synthetic restart.
#[cfg(any(feature = "daemon-entry", test))]
pub(crate) mod depgraph_load;
/// Standalone daemon process entry point (issue #997), gated so it only
/// compiles when a binary that hosts it (`daemon-bin`, or the `cli`/`zccache`
/// binary via argv[0] dispatch) pulls in clap + tracing-subscriber.
#[cfg(feature = "daemon-entry")]
pub mod entry;
pub mod eviction;
pub mod fingerprint;
pub mod jobserver;
pub mod lifecycle;
pub mod lineage;
/// Bounded opt-in tracing file sink (#1165). Gated with `entry` because it is
/// a `tracing_subscriber` layer and that dependency is optional — the daemon
/// library is also built by hosts that install their own subscriber.
#[cfg(feature = "daemon-entry")]
pub mod log_sink;
pub(crate) mod process;
pub mod server;
pub mod side_effect;
pub(crate) mod staged_stats;
pub mod stats;
pub mod trampoline;

pub use server::{DaemonServer, DepGraphSetter};
pub use stats::{PhaseProfiler, ProfileSnapshot, StatsCollector};
