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

pub mod decision;
pub mod error;
pub mod file_lock;
pub mod hash_cache;
pub mod persist;
pub mod scan;
pub mod two_layer;

#[cfg(feature = "python")]
mod python;

pub use decision::{CacheDecision, RunReason};
pub use error::{FingerprintError, Result};
pub use hash_cache::{compute_aggregate_hash, HashCache};
pub use persist::detect_pending_type;
pub use scan::{walk_files, walk_files_glob, ScannedFile};
pub use two_layer::TwoLayerCache;
