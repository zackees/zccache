//! `zccache` — transitional absorber crate. See README and issue #365.
//!
//! Each `pub mod` below corresponds to a former workspace crate of the same
//! name (`zccache-core` → [`core`], `zccache-hash` → [`hash`], etc.). New code
//! should `use crate::<module>::*` instead of the legacy
//! `zccache_<module>::*` paths, which are being deleted wave by wave.

pub use zccache_artifact as artifact;
pub use zccache_audit as audit;
/// Issue zccache#926 — durable audit JSONL writer for the embedded
/// service. Spawned by [`embedded::ZccacheService::start`] when
/// [`audit::AuditConfig::mode`] > `Off`.
pub mod audit_writer;
#[cfg(feature = "ci")]
pub mod ci;
#[cfg(feature = "cli")]
pub mod cli;
/// zccache#940 — per-sub-phase JSONL trace for the embedded compile
/// path. Diagnostic-only, gated by the `ZCCACHE_INNER_TRACE` env var.
/// See module doc for the wire format and why it exists.
pub use zccache_compile_trace as compile_trace;
pub use zccache_compiler as compiler;
pub use zccache_core as core;
pub mod daemon;
pub use zccache_depgraph as depgraph;
#[cfg(feature = "download")]
pub use zccache_download as download;
#[cfg(feature = "download-client")]
pub mod download_client;
#[cfg(feature = "download-daemon")]
pub mod download_daemon;
#[cfg(feature = "download-protocol")]
pub use zccache_download_protocol as download_protocol;
pub mod embedded;
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

#[cfg(feature = "test-support")]
pub mod test_support;
