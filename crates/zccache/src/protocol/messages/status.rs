//! Daemon status, session statistics, and timing-profile protocol payloads.

use crate::core::NormalizedPath;
use serde::{Deserialize, Serialize};
/// Daemon status information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version (e.g. "1.0.8"). Used by CLI to detect stale daemons.
    pub version: String,
    /// Active daemon/socket namespace. `default` means no explicit namespace
    /// was configured and all endpoint/path names use the historical layout.
    pub daemon_namespace: String,
    /// IPC endpoint this daemon bound and serves.
    pub endpoint: String,
    /// Number of artifacts in cache.
    pub artifact_count: u64,
    /// Total size of cached artifacts in bytes.
    pub cache_size_bytes: u64,
    /// Number of entries in the metadata cache.
    pub metadata_entries: u64,
    /// Daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Total cache hits since startup.
    pub cache_hits: u64,
    /// Total cache misses since startup.
    pub cache_misses: u64,
    /// Total compile requests received.
    pub total_compilations: u64,
    /// Non-cacheable invocations (linking, preprocessing, etc.).
    pub non_cacheable: u64,
    /// Compilations that exited with non-zero status.
    pub compile_errors: u64,
    /// Non-zero compile results served from cache.
    pub compile_errors_cached: u64,
    /// Estimated wall-clock time saved in milliseconds.
    pub time_saved_ms: u64,
    /// Total link/archive requests received.
    pub total_links: u64,
    /// Link cache hits.
    pub link_hits: u64,
    /// Link cache misses.
    pub link_misses: u64,
    /// Non-cacheable link invocations.
    pub link_non_cacheable: u64,
    /// Number of compilation contexts in the dependency graph.
    pub dep_graph_contexts: u64,
    /// Number of tracked files in the dependency graph.
    pub dep_graph_files: u64,
    /// Total sessions created since daemon start.
    pub sessions_total: u64,
    /// Currently active sessions.
    pub sessions_active: u64,
    /// Path to the cache directory.
    pub cache_dir: NormalizedPath,
    /// On-disk depgraph snapshot format version.
    pub dep_graph_version: u32,
    /// Size of the depgraph snapshot file on disk (0 = not persisted).
    pub dep_graph_disk_size: u64,
    /// Whether the in-memory dep graph is backed by a persisted snapshot.
    ///
    /// `true` if the graph was loaded from disk on startup OR has been
    /// successfully written to disk since startup (periodic save or shutdown).
    /// `false` on a fresh daemon that has not yet flushed its first snapshot.
    pub dep_graph_persisted: bool,
}

/// Per-session statistics, returned when the session opted in to tracking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// Wall-clock duration of the session in milliseconds.
    pub duration_ms: u64,
    /// Total compile requests in this session.
    pub compilations: u64,
    /// Cache hits in this session.
    pub hits: u64,
    /// Cache misses (cold compiles) in this session.
    pub misses: u64,
    /// Non-cacheable invocations (linking, preprocessing, etc.).
    pub non_cacheable: u64,
    /// Compilations that exited with non-zero status.
    pub errors: u64,
    /// Non-zero compile results served from cache.
    #[serde(default)]
    pub errors_cached: u64,
    /// Estimated wall-clock time saved in milliseconds.
    pub time_saved_ms: u64,
    /// Distinct source files compiled.
    pub unique_sources: u64,
    /// Total artifact bytes served from cache.
    pub bytes_read: u64,
    /// Total artifact bytes stored into cache.
    pub bytes_written: u64,
    /// Daemon-wide phase-timing aggregate. `None` from older daemons that
    /// don't populate the field; `Some` from PROTOCOL_VERSION >= 9 daemons.
    ///
    /// Aggregate is daemon-wide totals since the last
    /// `PhaseProfiler::reset()` (which is called on `Request::Clear`). For
    /// fresh-daemon perf scenarios this is equivalent to "this session's
    /// phase totals". For long-lived daemons handling overlapping sessions,
    /// totals cross-contaminate — that's acceptable for v1 and revisited if
    /// a real consumer needs per-session isolation.
    pub phase_profile: Option<PhaseProfileSummary>,
}

/// Aggregate phase-timing totals from the daemon's PhaseProfiler.
///
/// Totals are in nanoseconds. Divide hit-path totals by `hit_count` and
/// miss-path totals by `miss_count` to derive per-compile averages.
///
/// Use case: a perf harness collects this from a warm-rebuild session to
/// identify which phase dominates the warm-side wall time (e.g.
/// `write_output_ns` for artifact materialization vs `depgraph_check_ns`
/// for depgraph lookups).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseProfileSummary {
    /// Number of cache-hit compiles that contributed to the hit-path totals.
    pub hit_count: u64,
    /// Number of cache-miss compiles that contributed to the miss-path totals.
    pub miss_count: u64,
    // ── Hit-path totals (ns) ──
    /// Argument parsing.
    pub parse_args_ns: u64,
    /// Build compile context + register with depgraph.
    pub build_context_ns: u64,
    /// Source-file hash via the metadata cache fast path.
    pub hash_source_ns: u64,
    /// Header hashes via the metadata cache fast path.
    pub hash_headers_ns: u64,
    /// Depgraph verdict lookup.
    pub depgraph_check_ns: u64,
    /// Request-level cache lookup.
    pub request_cache_lookup_ns: u64,
    /// Cross-root request validation.
    pub cross_root_validate_ns: u64,
    /// In-memory artifact-store lookup.
    pub artifact_lookup_ns: u64,
    /// Write cached outputs to disk (hardlink-first, copy fallback).
    pub write_output_ns: u64,
    /// Stats recording + session bookkeeping.
    pub bookkeeping_ns: u64,
    /// Wall-clock total of the hit path.
    pub total_hit_ns: u64,
    // ── Miss-path totals (ns) ──
    /// Run the actual compiler subprocess.
    pub compiler_exec_ns: u64,
    /// Scan included files post-compile.
    pub include_scan_ns: u64,
    /// Hash all inputs for the artifact key.
    pub hash_all_ns: u64,
    /// Persist the new artifact to disk.
    pub artifact_store_ns: u64,
    /// Wall-clock total of the miss path.
    pub total_miss_ns: u64,
}
