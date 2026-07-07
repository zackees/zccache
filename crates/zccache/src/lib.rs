//! `zccache` — transitional absorber crate. See README and issue #365.
//!
//! Each `pub mod` below corresponds to a former workspace crate of the same
//! name (`zccache-core` → [`core`], `zccache-hash` → [`hash`], etc.). New code
//! should `use crate::<module>::*` instead of the legacy
//! `zccache_<module>::*` paths, which are being deleted wave by wave.

pub mod artifact;
pub mod audit;
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
pub mod compile_trace;
pub mod compiler;
pub mod core;
pub mod daemon;
pub mod depgraph;
#[cfg(feature = "download")]
pub mod download;
#[cfg(feature = "download-client")]
pub mod download_client;
#[cfg(feature = "download-daemon")]
pub mod download_daemon;
#[cfg(feature = "download-protocol")]
pub mod download_protocol;
pub mod embedded;
pub mod fingerprint;
pub mod fscache;
#[cfg(feature = "gha")]
pub mod gha;
pub mod hash;
pub mod ipc;
pub mod protocol;
#[cfg(feature = "symbols")]
pub mod symbols;
pub mod watcher;

#[cfg(feature = "test-support")]
pub mod test_support;
