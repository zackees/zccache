//! `zccache-daemon-core` — the zccache daemon subsystem (#1018 crate split).
//!
//! Extracted from the monolithic `zccache` crate. The subsystem crates are
//! re-exported under the same short aliases the daemon code uses (`core`,
//! `ipc`, …) and the `daemon` / `embedded` / `audit_writer` / `test_support`
//! module structure is preserved, so internal `crate::daemon::…` paths resolve
//! unchanged. The `zccache` facade re-exports these modules to keep the public
//! `zccache::daemon::…` / `zccache::embedded::…` paths stable.

// Subsystem-crate aliases (mirrors the former `zccache` facade so the moved
// daemon code's `crate::core` / `crate::ipc` / … paths keep resolving).
pub use zccache_artifact as artifact;
pub use zccache_audit as audit;
pub use zccache_compile_trace as compile_trace;
pub use zccache_compiler as compiler;
pub use zccache_core as core;
pub use zccache_depgraph as depgraph;
pub use zccache_fingerprint as fingerprint;
pub use zccache_fscache as fscache;
pub use zccache_hash as hash;
pub use zccache_ipc as ipc;
pub use zccache_protocol as protocol;
pub use zccache_watcher as watcher;

pub mod audit_writer;
pub mod daemon;
pub mod embedded;

#[cfg(feature = "test-support")]
pub mod test_support;
