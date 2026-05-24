//! Protocol message definitions.

use crate::core::NormalizedPath;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A request from client to daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Health check.
    Ping,
    /// Request daemon shutdown.
    Shutdown,
    /// Request daemon status/statistics.
    Status,
    /// Look up a cached artifact by cache key.
    Lookup {
        /// Hex-encoded cache key.
        cache_key: String,
    },
    /// Store a compilation artifact.
    Store {
        /// Hex-encoded cache key.
        cache_key: String,
        /// The artifact data to store.
        artifact: ArtifactData,
    },
    /// Start a new session with the daemon.
    SessionStart {
        /// Client process ID.
        client_pid: u32,
        /// Client working directory.
        working_dir: NormalizedPath,
        /// Optional path to a log file for this session.
        log_file: Option<NormalizedPath>,
        /// Whether to track per-session statistics.
        track_stats: bool,
        /// Path for per-session JSONL compile journal (must end in .jsonl).
        journal_path: Option<NormalizedPath>,
        /// Issue #256: opt in to the extended journal schema. When true,
        /// the daemon populates crate_name, crate_type, output_ext, and
        /// self_profile_ns on every compile journal line for the duration
        /// of this session. When false, behavior is identical to releases
        /// before the flag existed (no new allocations, no new fields).
        profile: bool,
    },
    /// Compile a source file within an existing session.
    Compile {
        /// Session ID from a prior SessionStart (UUID string).
        session_id: String,
        /// Compiler arguments (e.g., ["-c", "hello.cpp", "-o", "hello.o"]).
        args: Vec<String>,
        /// Working directory for the compilation.
        cwd: NormalizedPath,
        /// Path to the compiler executable (required).
        compiler: NormalizedPath,
        /// Client environment variables to pass to the compiler process.
        /// If `None`, the daemon's own environment is inherited (backward compat).
        /// If `Some`, the compiler process uses exactly these env vars.
        env: Option<Vec<(String, String)>>,
        /// Bytes the wrapper read from its own stdin, ferried to the compiler
        /// child's stdin over IPC. Empty = no stdin (`Stdio::null` on the
        /// daemon side). cargo's RUSTC_WRAPPER path normally yields zero
        /// bytes here; the field exists so that `rustc -` and similar
        /// stdin-consuming invocations work transparently.
        stdin: Vec<u8>,
    },
    /// End a session.
    SessionEnd {
        /// Session ID to end (UUID string).
        session_id: String,
    },
    /// Clear all caches (artifacts, metadata, dep graph).
    Clear,
    /// Single-roundtrip ephemeral compile: session start + compile + session end
    /// in one message. Used by the CLI in drop-in wrapper mode to avoid 3 IPC
    /// roundtrips per invocation.
    CompileEphemeral {
        /// Client process ID.
        client_pid: u32,
        /// Client working directory.
        working_dir: NormalizedPath,
        /// Path to the compiler executable.
        compiler: NormalizedPath,
        /// Compiler arguments (e.g., ["-c", "hello.cpp", "-o", "hello.o"]).
        args: Vec<String>,
        /// Working directory for the compilation.
        cwd: NormalizedPath,
        /// Client environment variables to pass to the compiler process.
        env: Option<Vec<(String, String)>>,
        /// Bytes the wrapper read from its own stdin, ferried to the compiler
        /// child's stdin over IPC. Empty = `Stdio::null` on the daemon side.
        /// See `Request::Compile` for context.
        stdin: Vec<u8>,
    },
    /// Single-roundtrip ephemeral link/archive: used for `zccache ar ...` or
    /// `zccache ld ...` in drop-in wrapper mode.
    LinkEphemeral {
        /// Client process ID.
        client_pid: u32,
        /// Path to the linker/archiver tool (ar, ld, lib.exe, link.exe, etc.).
        tool: NormalizedPath,
        /// Tool arguments (e.g., ["rcs", "libfoo.a", "a.o", "b.o"]).
        args: Vec<String>,
        /// Working directory for the link operation.
        cwd: NormalizedPath,
        /// Client environment variables.
        env: Option<Vec<(String, String)>>,
    },
    /// Query per-session statistics without ending the session.
    /// NOTE: Appended at end to preserve bincode variant indices.
    SessionStats {
        /// Session ID to query (UUID string).
        session_id: String,
    },
    /// Check if files have changed since last successful fingerprint.
    /// NOTE: Appended at end to preserve bincode variant indices.
    FingerprintCheck {
        /// Path to the cache file (e.g., .cache/lint.json).
        cache_file: NormalizedPath,
        /// Cache algorithm: "hash" or "two-layer".
        cache_type: String,
        /// Root directory to scan.
        root: NormalizedPath,
        /// File extensions to include (without dot, e.g., "rs", "cpp").
        /// Empty = all files. Conflicts with `include_globs`.
        extensions: Vec<String>,
        /// Glob patterns for files to include (e.g., "**/*.rs").
        /// Empty = use extensions filter.
        include_globs: Vec<String>,
        /// Patterns or directory names to exclude.
        exclude: Vec<String>,
    },
    /// Mark the previous fingerprint check as successful.
    FingerprintMarkSuccess {
        /// Path to the cache file.
        cache_file: NormalizedPath,
    },
    /// Mark the previous fingerprint check as failed.
    FingerprintMarkFailure {
        /// Path to the cache file.
        cache_file: NormalizedPath,
    },
    /// Invalidate a fingerprint cache (delete all state).
    FingerprintInvalidate {
        /// Path to the cache file.
        cache_file: NormalizedPath,
    },
    /// List all cached Rust artifacts with their output paths.
    /// NOTE: Appended at end to preserve bincode variant indices.
    ListRustArtifacts,
}

/// A response from daemon to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// Response to Ping.
    Pong,
    /// Shutdown acknowledged.
    ShuttingDown,
    /// Daemon status information.
    Status(DaemonStatus),
    /// Cache lookup result.
    LookupResult(LookupResult),
    /// Store result.
    StoreResult(StoreResult),
    /// Session successfully started.
    SessionStarted {
        /// Assigned session ID (UUID string).
        session_id: String,
        /// Path to the per-session JSONL journal file (if journal was requested).
        journal_path: Option<NormalizedPath>,
    },
    /// Result of a compilation request.
    CompileResult {
        /// Compiler exit code.
        exit_code: i32,
        /// Captured stdout (Arc-wrapped to avoid copies on cache hits).
        stdout: Arc<Vec<u8>>,
        /// Captured stderr (Arc-wrapped to avoid copies on cache hits).
        stderr: Arc<Vec<u8>>,
        /// Whether this was served from cache.
        cached: bool,
    },
    /// Session ended successfully.
    SessionEnded {
        /// Per-session stats, if the session opted in to tracking.
        stats: Option<SessionStats>,
    },
    /// Result of a link/archive request.
    LinkResult {
        /// Tool exit code.
        exit_code: i32,
        /// Captured stdout (Arc-wrapped to avoid copies on cache hits).
        stdout: Arc<Vec<u8>>,
        /// Captured stderr (Arc-wrapped to avoid copies on cache hits).
        stderr: Arc<Vec<u8>>,
        /// Whether this was served from cache.
        cached: bool,
        /// Non-determinism warning (if tool invocation uses non-deterministic flags).
        warning: Option<String>,
    },
    /// An error occurred processing the request.
    Error {
        /// Human-readable error message.
        message: String,
    },
    /// Cache cleared successfully.
    Cleared {
        /// Number of in-memory artifacts removed.
        artifacts_removed: u64,
        /// Number of metadata cache entries cleared.
        metadata_cleared: u64,
        /// Number of dep graph contexts cleared.
        dep_graph_contexts_cleared: u64,
        /// Bytes freed from on-disk artifact cache.
        on_disk_bytes_freed: u64,
    },
    /// Mid-session statistics snapshot.
    /// NOTE: Appended at end to preserve bincode variant indices.
    SessionStatsResult {
        /// Per-session stats, if the session exists and opted in to tracking.
        stats: Option<SessionStats>,
    },
    /// Result of a fingerprint check.
    /// NOTE: Appended at end to preserve bincode variant indices.
    FingerprintCheckResult {
        /// "skip" or "run".
        decision: String,
        /// Reason for run (e.g., "no cache file", "content changed").
        reason: Option<String>,
        /// Files that changed (if available).
        changed_files: Vec<String>,
    },
    /// Fingerprint mark/invalidate acknowledged.
    FingerprintAck,
    /// List of cached Rust artifacts.
    /// NOTE: Appended at end to preserve bincode variant indices.
    RustArtifactList { artifacts: Vec<RustArtifactInfo> },
}

/// Daemon status information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version (e.g. "1.0.8"). Used by CLI to detect stale daemons.
    pub version: String,
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

/// Result of a cache lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LookupResult {
    /// Cache hit.
    Hit {
        /// The cached artifact data.
        artifact: ArtifactData,
    },
    /// Cache miss.
    Miss,
}

/// Result of storing an artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StoreResult {
    /// Successfully stored.
    Stored,
    /// Already existed in cache.
    AlreadyExists,
}

/// Artifact data exchanged over the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactData {
    /// The output files (filename to contents).
    pub outputs: Vec<ArtifactOutput>,
    /// Captured stdout from the compiler.
    pub stdout: Arc<Vec<u8>>,
    /// Captured stderr from the compiler.
    pub stderr: Arc<Vec<u8>>,
    /// Compiler exit code.
    pub exit_code: i32,
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

/// Where an artifact output's bytes live on the daemon's filesystem at the
/// moment a request is built.
///
/// `Bytes` is the only variant any current client emits — `Path` is reserved
/// for future sccache-emulation paths where the client already has the bytes
/// on disk and the daemon can hardlink directly via `persist_artifact_file`
/// (falling back to copy on cross-volume failure).
///
/// The variant was introduced pre-emptively in PR for issue #296 so that
/// landing the eventual `Request::Store` handler won't require a second
/// `PROTOCOL_VERSION` bump. See `crates/zccache-daemon/src/server.rs` —
/// `CachedPayload` is the internal sibling of this type and predates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ArtifactPayload {
    /// Bytes shipped inline in the IPC message. Used by every current call
    /// site; future remote-daemon scenarios may also need this.
    Bytes(Arc<Vec<u8>>),
    /// Path on the daemon's filesystem. The daemon hardlinks from this path
    /// into the cache (falling back to copy on cross-volume failure). Path
    /// must be absolute and readable by the daemon process (same user).
    /// No current client emits this variant.
    Path(NormalizedPath),
}

impl ArtifactPayload {
    /// Size in bytes of the underlying output. For `Path`, stats the file;
    /// returns 0 on I/O error (matches the prior `unwrap_or_default()`
    /// semantics elsewhere in the daemon for missing-output cases).
    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::Path(p) => std::fs::metadata(p.as_path()).map(|m| m.len()).unwrap_or(0),
        }
    }

    /// Returns `Some` of the inline bytes when this is the `Bytes` variant.
    /// Useful for daemon-internal sites that still want the byte path —
    /// `None` signals "the bytes live on disk; route through a hardlink/read
    /// helper instead."
    #[must_use]
    pub fn as_bytes(&self) -> Option<&Arc<Vec<u8>>> {
        match self {
            Self::Bytes(b) => Some(b),
            Self::Path(_) => None,
        }
    }
}

/// A single output file from compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactOutput {
    /// Relative filename (e.g., "foo.o").
    pub name: String,
    /// Where the bytes live — inline in the message or on disk for hardlink.
    /// See `ArtifactPayload` for the variant rationale.
    pub payload: ArtifactPayload,
}

/// Information about a cached Rust compilation artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RustArtifactInfo {
    /// Cache key hex.
    pub cache_key: String,
    /// Output file names (e.g., ["libfoo-abc123.rlib", "libfoo-abc123.rmeta", "foo-abc123.d"]).
    pub output_names: Vec<String>,
    /// Number of payload files.
    pub payload_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: roundtrip a value through bincode.
    fn roundtrip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        val: &T,
    ) {
        let bytes = bincode::serialize(val).unwrap();
        let decoded: T = bincode::deserialize(&bytes).unwrap();
        assert_eq!(*val, decoded);
    }

    #[test]
    fn session_stats_roundtrip() {
        let stats = SessionStats {
            duration_ms: 12345,
            compilations: 100,
            hits: 80,
            misses: 15,
            non_cacheable: 5,
            errors: 2,
            time_saved_ms: 8000,
            unique_sources: 42,
            bytes_read: 1024 * 1024,
            bytes_written: 512 * 1024,
            phase_profile: None,
        };
        roundtrip(&stats);
    }

    #[test]
    fn session_stats_default_zeros() {
        let stats = SessionStats {
            duration_ms: 0,
            compilations: 0,
            hits: 0,
            misses: 0,
            non_cacheable: 0,
            errors: 0,
            time_saved_ms: 0,
            unique_sources: 0,
            bytes_read: 0,
            bytes_written: 0,
            phase_profile: None,
        };
        roundtrip(&stats);
    }

    #[test]
    fn session_stats_with_phase_profile_roundtrip() {
        // Regression guard for PROTOCOL_VERSION 9 — a populated phase_profile
        // must round-trip both bincode (IPC wire) and serde-json (the form
        // soldr writes to last-session-stats.json).
        let stats = SessionStats {
            duration_ms: 12345,
            compilations: 146,
            hits: 103,
            misses: 12,
            non_cacheable: 31,
            errors: 3,
            time_saved_ms: 223,
            unique_sources: 115,
            bytes_read: 143_812_577,
            bytes_written: 62_500_000,
            phase_profile: Some(PhaseProfileSummary {
                hit_count: 103,
                miss_count: 12,
                parse_args_ns: 4_000_000,
                build_context_ns: 19_000_000,
                hash_source_ns: 6_000_000,
                hash_headers_ns: 11_000_000,
                depgraph_check_ns: 28_000_000,
                request_cache_lookup_ns: 2_500_000,
                cross_root_validate_ns: 1_200_000,
                artifact_lookup_ns: 8_700_000,
                write_output_ns: 540_000_000,
                bookkeeping_ns: 3_300_000,
                total_hit_ns: 623_700_000,
                compiler_exec_ns: 11_400_000_000,
                include_scan_ns: 270_000_000,
                hash_all_ns: 95_000_000,
                artifact_store_ns: 120_000_000,
                total_miss_ns: 11_885_000_000,
            }),
        };
        roundtrip(&stats);

        // serde-json round-trip — written to last-session-stats.json and
        // read by both `zccache analyze` and the perf harness's
        // `perf_local.py render_summary`.
        let json = serde_json::to_string(&stats).expect("serialize");
        let decoded: SessionStats = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(stats, decoded);

        // An old-daemon-style JSON that omits phase_profile must decode to
        // None (back-compat with PROTOCOL_VERSION 8 consumers that haven't
        // upgraded the field expectation).
        let legacy = r#"{
            "duration_ms": 0, "compilations": 0, "hits": 0, "misses": 0,
            "non_cacheable": 0, "errors": 0, "time_saved_ms": 0,
            "unique_sources": 0, "bytes_read": 0, "bytes_written": 0
        }"#;
        let decoded: SessionStats = serde_json::from_str(legacy).expect("legacy decode");
        assert!(decoded.phase_profile.is_none());
    }

    #[test]
    fn daemon_status_expanded_roundtrip() {
        let status = DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            artifact_count: 892,
            cache_size_bytes: 147_000_000,
            metadata_entries: 5430,
            uptime_secs: 8040,
            cache_hits: 1089,
            cache_misses: 143,
            total_compilations: 1247,
            non_cacheable: 15,
            compile_errors: 3,
            time_saved_ms: 750_000,
            total_links: 50,
            link_hits: 38,
            link_misses: 10,
            link_non_cacheable: 2,
            dep_graph_contexts: 892,
            dep_graph_files: 4201,
            sessions_total: 41,
            sessions_active: 3,
            cache_dir: "/home/user/.zccache".into(),
            dep_graph_version: 1,
            dep_graph_disk_size: 2_500_000,
            dep_graph_persisted: true,
        };
        roundtrip(&status);
    }

    #[test]
    fn session_start_with_track_stats_roundtrip() {
        let req = Request::SessionStart {
            client_pid: 1234,
            working_dir: "/home/user/project".into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        };
        roundtrip(&req);

        let req_no_stats = Request::SessionStart {
            client_pid: 1234,
            working_dir: "/home/user/project".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
        };
        roundtrip(&req_no_stats);
    }

    #[test]
    fn session_start_with_journal_path_roundtrip() {
        let req = Request::SessionStart {
            client_pid: 5678,
            working_dir: "/home/user/project".into(),
            log_file: None,
            track_stats: false,
            journal_path: Some("/tmp/build.jsonl".into()),
            profile: false,
        };
        roundtrip(&req);

        let req_no_journal = Request::SessionStart {
            client_pid: 5678,
            working_dir: "/home/user/project".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
        };
        roundtrip(&req_no_journal);
    }

    #[test]
    fn session_started_with_journal_path_roundtrip() {
        let resp = Response::SessionStarted {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            journal_path: Some("/home/user/.zccache/logs/sessions/test.jsonl".into()),
        };
        roundtrip(&resp);

        let resp_no_journal = Response::SessionStarted {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            journal_path: None,
        };
        roundtrip(&resp_no_journal);
    }

    #[test]
    fn session_ended_with_stats_roundtrip() {
        let stats = SessionStats {
            duration_ms: 34000,
            compilations: 32,
            hits: 28,
            misses: 3,
            non_cacheable: 1,
            errors: 0,
            time_saved_ms: 8200,
            unique_sources: 30,
            bytes_read: 2_000_000,
            bytes_written: 500_000,
            phase_profile: None,
        };
        let resp = Response::SessionEnded { stats: Some(stats) };
        roundtrip(&resp);

        let resp_no_stats = Response::SessionEnded { stats: None };
        roundtrip(&resp_no_stats);
    }

    #[test]
    fn clear_request_roundtrip() {
        roundtrip(&Request::Clear);
    }

    #[test]
    fn cleared_response_roundtrip() {
        roundtrip(&Response::Cleared {
            artifacts_removed: 42,
            metadata_cleared: 100,
            dep_graph_contexts_cleared: 25,
            on_disk_bytes_freed: 1024 * 1024,
        });
    }

    #[test]
    fn compile_ephemeral_roundtrip() {
        roundtrip(&Request::CompileEphemeral {
            client_pid: 9876,
            working_dir: "/home/user/project".into(),
            compiler: "/usr/bin/clang++".into(),
            args: vec!["-c".into(), "main.cpp".into(), "-o".into(), "main.o".into()],
            cwd: "/home/user/project/build".into(),
            env: Some(vec![("PATH".into(), "/usr/bin".into())]),
            stdin: Vec::new(),
        });
        // Non-empty stdin payload must round-trip byte-for-byte — including
        // embedded NULs and binary bytes — so `rustc -` style invocations
        // through the wrapper see the same input the parent sent us.
        roundtrip(&Request::CompileEphemeral {
            client_pid: 1,
            working_dir: ".".into(),
            compiler: "gcc".into(),
            args: vec![],
            cwd: ".".into(),
            env: None,
            stdin: b"hello\x00world\nbinary\xff\xfe".to_vec(),
        });
    }

    #[test]
    fn link_ephemeral_roundtrip() {
        roundtrip(&Request::LinkEphemeral {
            client_pid: 5555,
            tool: "/usr/bin/ar".into(),
            args: vec!["rcs".into(), "libfoo.a".into(), "a.o".into(), "b.o".into()],
            cwd: "/home/user/project/build".into(),
            env: Some(vec![("PATH".into(), "/usr/bin".into())]),
        });
        roundtrip(&Request::LinkEphemeral {
            client_pid: 1,
            tool: "lib.exe".into(),
            args: vec!["/OUT:foo.lib".into(), "a.obj".into()],
            cwd: ".".into(),
            env: None,
        });
    }

    #[test]
    fn link_result_roundtrip() {
        roundtrip(&Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
            warning: None,
        });
        roundtrip(&Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(b"some warning".to_vec()),
            cached: false,
            warning: Some("non-deterministic: missing D flag".into()),
        });
    }

    #[test]
    fn session_stats_request_roundtrip() {
        roundtrip(&Request::SessionStats {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        });
    }

    #[test]
    fn session_stats_result_roundtrip() {
        let stats = SessionStats {
            duration_ms: 5000,
            compilations: 10,
            hits: 7,
            misses: 2,
            non_cacheable: 1,
            errors: 0,
            time_saved_ms: 3000,
            unique_sources: 9,
            bytes_read: 50_000,
            bytes_written: 20_000,
            phase_profile: None,
        };
        roundtrip(&Response::SessionStatsResult { stats: Some(stats) });
        roundtrip(&Response::SessionStatsResult { stats: None });
    }

    #[test]
    fn existing_request_variants_still_work() {
        roundtrip(&Request::Ping);
        roundtrip(&Request::Shutdown);
        roundtrip(&Request::Status);
        roundtrip(&Request::SessionEnd {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        });
        roundtrip(&Request::Compile {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            args: vec!["-c".into(), "foo.c".into()],
            cwd: "/tmp".into(),
            compiler: "/usr/bin/gcc".into(),
            env: None,
            stdin: Vec::new(),
        });
    }

    #[test]
    fn existing_response_variants_still_work() {
        roundtrip(&Response::Pong);
        roundtrip(&Response::ShuttingDown);
        roundtrip(&Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        });
        roundtrip(&Response::Error {
            message: "test".into(),
        });
    }

    #[test]
    fn daemon_status_version_field_roundtrips() {
        let with_version = DaemonStatus {
            version: "1.2.3".to_string(),
            artifact_count: 0,
            cache_size_bytes: 0,
            metadata_entries: 0,
            uptime_secs: 0,
            cache_hits: 0,
            cache_misses: 0,
            total_compilations: 0,
            non_cacheable: 0,
            compile_errors: 0,
            time_saved_ms: 0,
            total_links: 0,
            link_hits: 0,
            link_misses: 0,
            link_non_cacheable: 0,
            dep_graph_contexts: 0,
            dep_graph_files: 0,
            sessions_total: 0,
            sessions_active: 0,
            cache_dir: "".into(),
            dep_graph_version: 0,
            dep_graph_disk_size: 0,
            dep_graph_persisted: false,
        };
        roundtrip(&with_version);
    }

    // Compile-time check: PROTOCOL_VERSION must be positive.
    const _: () = assert!(super::super::PROTOCOL_VERSION > 0);
    // Compile-time check: PROTOCOL_VERSION == 9 after SessionStats gained
    // `phase_profile: Option<PhaseProfileSummary>` (perf observability for
    // per-session phase aggregates). v8 was the prior pin after
    // Compile/CompileEphemeral gained `stdin: Vec<u8>` and ArtifactPayload
    // replaced ArtifactOutput.data: Arc<Vec<u8>> (issue #296 Option B).
    const _FINGERPRINT_VERSION: () = assert!(super::super::PROTOCOL_VERSION == 9);

    #[test]
    fn fingerprint_check_roundtrip() {
        roundtrip(&Request::FingerprintCheck {
            cache_file: "/tmp/lint.json".into(),
            cache_type: "two-layer".into(),
            root: "/home/user/project/src".into(),
            extensions: vec!["rs".into(), "toml".into()],
            include_globs: vec![],
            exclude: vec![".git".into(), "target".into()],
        });
        roundtrip(&Request::FingerprintCheck {
            cache_file: "cache.json".into(),
            cache_type: "hash".into(),
            root: ".".into(),
            extensions: vec![],
            include_globs: vec!["**/*.cpp".into(), "**/*.h".into()],
            exclude: vec![],
        });
    }

    #[test]
    fn fingerprint_mark_success_roundtrip() {
        roundtrip(&Request::FingerprintMarkSuccess {
            cache_file: "/tmp/lint.json".into(),
        });
    }

    #[test]
    fn fingerprint_mark_failure_roundtrip() {
        roundtrip(&Request::FingerprintMarkFailure {
            cache_file: "/tmp/lint.json".into(),
        });
    }

    #[test]
    fn fingerprint_invalidate_roundtrip() {
        roundtrip(&Request::FingerprintInvalidate {
            cache_file: "/tmp/lint.json".into(),
        });
    }

    #[test]
    fn fingerprint_check_result_roundtrip() {
        roundtrip(&Response::FingerprintCheckResult {
            decision: "skip".into(),
            reason: None,
            changed_files: vec![],
        });
        roundtrip(&Response::FingerprintCheckResult {
            decision: "run".into(),
            reason: Some("content changed".into()),
            changed_files: vec!["src/main.rs".into(), "src/lib.rs".into()],
        });
        roundtrip(&Response::FingerprintCheckResult {
            decision: "run".into(),
            reason: Some("no cache file".into()),
            changed_files: vec![],
        });
    }

    #[test]
    fn fingerprint_ack_roundtrip() {
        roundtrip(&Response::FingerprintAck);
    }

    #[test]
    fn list_rust_artifacts_request_roundtrip() {
        roundtrip(&Request::ListRustArtifacts);
    }

    #[test]
    fn rust_artifact_list_response_roundtrip() {
        roundtrip(&Response::RustArtifactList {
            artifacts: vec![
                RustArtifactInfo {
                    cache_key: "abc123def456".into(),
                    output_names: vec![
                        "libfoo-abc123.rlib".into(),
                        "libfoo-abc123.rmeta".into(),
                        "foo-abc123.d".into(),
                    ],
                    payload_count: 3,
                },
                RustArtifactInfo {
                    cache_key: "deadbeef".into(),
                    output_names: vec!["libbar-deadbeef.rlib".into()],
                    payload_count: 1,
                },
            ],
        });
        // Empty list
        roundtrip(&Response::RustArtifactList { artifacts: vec![] });
    }

    #[test]
    fn rust_artifact_info_roundtrip() {
        roundtrip(&RustArtifactInfo {
            cache_key: "0123456789abcdef".into(),
            output_names: vec!["test.o".into()],
            payload_count: 1,
        });
    }

    #[test]
    fn artifact_clone_shares_payload_via_arc() {
        let bytes = Arc::new(vec![1u8, 2, 3, 4]);
        let artifact = ArtifactData {
            outputs: vec![ArtifactOutput {
                name: "test.o".into(),
                payload: ArtifactPayload::Bytes(Arc::clone(&bytes)),
            }],
            stdout: Arc::new(vec![5, 6]),
            stderr: Arc::new(vec![7, 8]),
            exit_code: 0,
        };

        let cloned = artifact.clone();

        // Arc::clone bumps refcount — both point to the same allocation.
        let orig_inner = artifact.outputs[0].payload.as_bytes().unwrap();
        let cloned_inner = cloned.outputs[0].payload.as_bytes().unwrap();
        assert!(Arc::ptr_eq(orig_inner, cloned_inner));
        assert!(Arc::ptr_eq(orig_inner, &bytes));
        assert!(Arc::ptr_eq(&artifact.stdout, &cloned.stdout));
        assert!(Arc::ptr_eq(&artifact.stderr, &cloned.stderr));
    }

    #[test]
    fn artifact_payload_size_bytes_for_bytes_variant() {
        let p = ArtifactPayload::Bytes(Arc::new(vec![0u8; 1234]));
        assert_eq!(p.size_bytes(), 1234);
    }

    #[test]
    fn artifact_payload_size_bytes_for_path_variant() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), vec![0u8; 4321]).expect("write");
        let p = ArtifactPayload::Path(NormalizedPath::from(tmp.path()));
        assert_eq!(p.size_bytes(), 4321);
    }

    #[test]
    fn artifact_payload_size_bytes_for_missing_path_is_zero() {
        let p = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
            "/this/path/does/not/exist/zccache",
        )));
        assert_eq!(p.size_bytes(), 0);
    }

    #[test]
    fn artifact_payload_round_trips_through_bincode() {
        let bytes_variant = ArtifactPayload::Bytes(Arc::new(b"hello".to_vec()));
        let encoded = bincode::serialize(&bytes_variant).expect("serialize bytes");
        let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize bytes");
        assert_eq!(decoded, bytes_variant);

        let path_variant = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
            "/tmp/some/place.rlib",
        )));
        let encoded = bincode::serialize(&path_variant).expect("serialize path");
        let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize path");
        assert_eq!(decoded, path_variant);
    }

    #[test]
    fn arc_vec_u8_roundtrip_matches_plain_vec() {
        // Prove Arc<Vec<u8>> serializes identically to Vec<u8>.
        let plain: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let arc_wrapped: Arc<Vec<u8>> = Arc::new(plain.clone());

        let plain_bytes = bincode::serialize(&plain).unwrap();
        let arc_bytes = bincode::serialize(&arc_wrapped).unwrap();
        assert_eq!(
            plain_bytes, arc_bytes,
            "Arc<Vec<u8>> must serialize identically to Vec<u8>"
        );

        // Deserialize Arc bytes back as plain Vec and vice versa.
        let decoded_plain: Vec<u8> = bincode::deserialize(&arc_bytes).unwrap();
        let decoded_arc: Arc<Vec<u8>> = bincode::deserialize(&plain_bytes).unwrap();
        assert_eq!(decoded_plain, plain);
        assert_eq!(*decoded_arc, plain);
    }
}
