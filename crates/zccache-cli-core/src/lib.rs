//! `zccache-cli-core` — the zccache CLI subsystem, extracted from the `zccache`
//! facade (#1022 Phase 2, Split A) to cut incremental recompile time.
//!
//! The subsystem crates are re-aliased at this lib root under the same short
//! names the moved code uses (`core`, `ipc`, `protocol`, …), and `daemon` is
//! re-exported from `zccache-daemon-core`, so internal `crate::<module>::…`
//! paths inside `cli`, `download_client`, and `download_daemon` resolve
//! unchanged — no mass rewrite.

// Subsystem crate aliases — keep the `crate::<name>` paths in the moved modules
// resolving without edits.
pub use zccache_artifact as artifact;
pub use zccache_compiler as compiler;
pub use zccache_core as core;
/// The daemon subsystem (from `zccache-daemon-core`). The CLI's only reference
/// is the `daemon-run` escape hatch (`daemon::entry::run_from`).
pub use zccache_daemon_core::daemon;
pub use zccache_depgraph as depgraph;
#[cfg(feature = "download")]
pub use zccache_download as download;
#[cfg(feature = "download-protocol")]
pub use zccache_download_protocol as download_protocol;
#[cfg(feature = "gha")]
pub use zccache_gha as gha;
pub use zccache_hash as hash;
pub use zccache_ipc as ipc;
pub use zccache_protocol as protocol;
#[cfg(feature = "symbols")]
pub use zccache_symbols as symbols;

// The CLI subsystem modules, moved here from `zccache`.
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "download-client")]
pub mod download_client;
#[cfg(feature = "download-daemon")]
pub mod download_daemon;
