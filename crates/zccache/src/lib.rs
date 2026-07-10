//! `zccache` — transitional absorber crate. See README and issue #365.
//!
//! Each `pub mod` below corresponds to a former workspace crate of the same
//! name (`zccache-core` → [`core`], `zccache-hash` → [`hash`], etc.). New code
//! should `use crate::<module>::*` instead of the legacy
//! `zccache_<module>::*` paths, which are being deleted wave by wave.

pub use zccache_artifact as artifact;
pub use zccache_audit as audit;
/// Issue zccache#926 — durable audit JSONL writer for the embedded service.
/// Moved to `zccache-daemon-core` (#1018); re-exported so the public path
/// `zccache::audit_writer` is unchanged.
pub use zccache_daemon_core::audit_writer;
#[cfg(feature = "ci")]
pub mod ci;
/// The CLI subsystem, moved to `zccache-cli-core` (#1022 Split A). Re-exported
/// so the public path `zccache::cli::…` (used by the bins and integration
/// tests) is unchanged.
#[cfg(feature = "cli")]
pub use zccache_cli_core::cli;
/// The download-cache client, moved to `zccache-cli-core` (#1022 Split A).
/// Re-exported so `zccache::download_client::…` is unchanged.
#[cfg(feature = "download-client")]
pub use zccache_cli_core::download_client;
/// The download-cache daemon logic, moved to `zccache-cli-core` (#1022 Split A).
/// Re-exported so `zccache::download_daemon::…` is unchanged.
#[cfg(feature = "download-daemon")]
pub use zccache_cli_core::download_daemon;
/// zccache#940 — per-sub-phase JSONL trace for the embedded compile
/// path. Diagnostic-only, gated by the `ZCCACHE_INNER_TRACE` env var.
/// See module doc for the wire format and why it exists.
pub use zccache_compile_trace as compile_trace;
pub use zccache_compiler as compiler;
pub use zccache_core as core;
/// The daemon subsystem, moved to `zccache-daemon-core` (#1018). Re-exported so
/// the public path `zccache::daemon::…` (used by the bins, the CLI, and
/// integration tests) is unchanged.
pub use zccache_daemon_core::daemon;
/// The embedded `ZccacheService` API (soldr/fbuild), moved to
/// `zccache-daemon-core` (#1018). Re-exported so `zccache::embedded::…` is
/// unchanged.
pub use zccache_daemon_core::embedded;
pub use zccache_depgraph as depgraph;
#[cfg(feature = "download")]
pub use zccache_download as download;
#[cfg(feature = "download-protocol")]
pub use zccache_download_protocol as download_protocol;
pub use zccache_fingerprint as fingerprint;
pub use zccache_fscache as fscache;
#[cfg(feature = "gha")]
pub use zccache_gha as gha;
pub use zccache_hash as hash;
pub use zccache_ipc as ipc;
pub use zccache_protocol as protocol;
#[cfg(feature = "symbols")]
pub use zccache_symbols as symbols;
pub use zccache_watcher as watcher;

/// Dev-only test helpers, moved to `zccache-daemon-core` (#1018). Re-exported
/// so `zccache::test_support` is unchanged for integration tests.
#[cfg(feature = "test-support")]
pub use zccache_daemon_core::test_support;
