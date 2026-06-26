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
pub mod ci;
pub mod cli;
pub mod compiler;
pub mod core;
pub mod daemon;
pub mod depgraph;
pub mod download;
pub mod download_client;
pub mod download_daemon;
pub mod download_protocol;
pub mod embedded;
pub mod fingerprint;
pub mod fscache;
pub mod gha;
pub mod hash;
pub mod ipc;
pub mod protocol;
pub mod symbols;
pub mod watcher;

#[cfg(feature = "test-support")]
pub mod test_support;
