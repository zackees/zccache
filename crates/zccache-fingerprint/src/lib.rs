//! Lightweight fingerprint cache for CI and tooling.
//!
//! Answers "has this set of files changed since the last successful operation?"
//! without the full machinery of the artifact store or metadata cache.
//!
//! # Cache Types
//!
//! - [`TwoLayerCache`] - Per-file mtime->blake3 fingerprinting. Skips hashing
//!   when mtime is unchanged (Layer 1). When mtime differs but content hasn't,
//!   updates the cached mtime silently (Layer 2, smart touch handling).
//!
//! - [`HashCache`] - Single aggregate blake3 hash of an entire file set.
//!   Suited for all-or-nothing decisions like "run all tests".
//!
//! Both use the pending pattern for crash safety: `check()` pre-computes
//! the fingerprint, then `mark_success()`/`mark_failure()` promotes it.
//! A cache value promotes the snapshot its own `check()` took, so cycles
//! interleaved on one cache file each commit their own result; a fresh value
//! (a separate `mark-*` process) falls back to the on-disk `.pending`.
//!
//! - [`mtime_replay`] - Content-verified mtime snapshot/replay (zccache#1595).
//!   Records each source file's mtime alongside its blake3 hash so a later
//!   checkout can restore the original mtime, but only where the restored
//!   content still verifies — never blanket-touching a tree or trusting a
//!   stale timestamp on changed content. Independent of the two caches
//!   above: no pending/promote step, just `snapshot()` then `replay()`.

pub mod decision;
pub mod error;
pub mod file_lock;
pub mod hash_cache;
pub mod mtime_replay;
pub mod persist;
pub mod scan;
pub mod two_layer;

pub use decision::{CacheDecision, RunReason};
pub use error::{FingerprintError, Result};
pub use hash_cache::{compute_aggregate_hash, HashCache};
pub use mtime_replay::{MtimeEntry, MtimeManifest, ReplayOutcome, ReplayReport, MANIFEST_VERSION};
pub use persist::detect_pending_type;
pub use scan::{walk_files, walk_files_glob, ScannedFile};
pub use two_layer::TwoLayerCache;
