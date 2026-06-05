//! Protocol wire enums and domain type re-exports.
//!
//! `Request` and `Response` stay in this module because bincode encodes enum
//! variants by declaration order. Domain structs live in sibling modules so new
//! soldr-facing protocol fields have an obvious home without interleaving all
//! helper types in one append-only file.

use crate::core::NormalizedPath;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

mod artifact;
mod exec;
mod status;

#[cfg(test)]
mod compat;

pub use artifact::{
    ArtifactData, ArtifactOutput, ArtifactPayload, LookupResult, RustArtifactInfo, StoreResult,
};
pub use exec::{ExecCachePolicy, ExecOutputStreams};
pub use status::{
    DaemonStatus, PhaseProfileSummary, PrivateDaemonOwnerStatus, PrivateDaemonStatus, SessionStats,
};

/// Private daemon options carried by `SessionStart`.
///
/// The daemon is already bound to its endpoint before this request arrives;
/// these fields let the client assert that it reached the intended private
/// daemon, register owner PIDs, and attach private session-scoped env.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivateDaemonSessionOptions {
    /// Portable daemon name requested by the client, if one was used.
    pub daemon_name: Option<String>,
    /// Endpoint the client intended to reach. Must match daemon status when set.
    pub endpoint: Option<String>,
    /// Cache root the client intended to use. Must match daemon status when set.
    pub cache_dir: Option<NormalizedPath>,
    /// PIDs that keep this daemon alive. Empty means `client_pid` owns it.
    pub owner_pids: Vec<u32>,
    /// Private session env vars. Values are applied to this session and redacted in status.
    pub env: Vec<(String, String)>,
}

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
        /// Private daemon ownership/env options for soldr-style isolated sessions.
        private_daemon: Option<PrivateDaemonSessionOptions>,
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
    /// Generic tool exec — cache an arbitrary tool's run by declared inputs.
    ///
    /// Issue #272: lets tools without a compiler-style CLI use the daemon's
    /// artifact cache. Inputs are explicit (`input_files`, `input_env`,
    /// `input_extra`); on hit the tool process is NOT spawned and the cached
    /// stdout/stderr/exit code/output files are replayed.
    ///
    /// NOTE: Appended at end to preserve bincode variant indices.
    GenericToolExec {
        /// Path to the tool executable. Must be absolute (CLI resolves PATH).
        tool: NormalizedPath,
        /// Tool arguments (the part after `--` on the CLI).
        args: Vec<String>,
        /// Working directory for the tool invocation.
        cwd: NormalizedPath,
        /// Selected env vars (name + value pairs). Sorted by the daemon for
        /// determinism. Only these vars enter the cache key.
        env: Vec<(String, String)>,
        /// Input files whose content feeds the cache key.
        input_files: Vec<NormalizedPath>,
        /// Opaque caller-supplied bytes mixed into the cache key.
        input_extra: Arc<Vec<u8>>,
        /// Output streams to capture and cache.
        output_streams: ExecOutputStreams,
        /// Files the tool writes; snapshot post-run, restore on hit.
        output_files: Vec<NormalizedPath>,
        /// Caller-supplied tool identity hash. `None` = daemon hashes the
        /// tool binary itself (cached by `(path, mtime, size)`).
        tool_hash: Option<[u8; 32]>,
        /// Cache lookup/store policy.
        cache_policy: ExecCachePolicy,
        /// Whether the CWD contributes to the cache key. False ⇒ tool output
        /// is treated as CWD-independent (callable from any directory).
        cwd_in_key: bool,
        /// Path A (#272): C/C++-style files to scan for `#include`
        /// directives. The daemon walks them transitively using
        /// `include_dirs` / `system_include_dirs` / `iquote_dirs` and mixes
        /// every resolved header's content into the cache key.
        include_scan_files: Vec<NormalizedPath>,
        /// `-I` user include directories (used for both quoted and angle
        /// includes during the include scan).
        include_dirs: Vec<NormalizedPath>,
        /// `-isystem` directories (system includes, after `-I`).
        system_include_dirs: Vec<NormalizedPath>,
        /// `-iquote` directories (quoted-only, before `-I`).
        iquote_dirs: Vec<NormalizedPath>,
        /// Path B (#272): make-style depfile the tool emits at this path.
        /// First invocation: daemon runs the tool, parses the emitted
        /// depfile, stores the dep set under the primary cache key as a
        /// `.deps` sidecar. Subsequent invocations: load the sidecar, hash
        /// each listed dep, compose the *full* key, look up.
        depfile: Option<NormalizedPath>,
        /// When true the daemon never caches this run (passthrough only).
        /// Intended for tools that emit timestamps, PIDs, or other
        /// non-reproducible content. Implies a forced Bypass.
        non_deterministic: bool,
        /// Regex patterns that drop matching args from the cache key (but
        /// not from the spawned tool's argv). Lets callers exclude purely
        /// runtime flags like `--verbose` or `--no-color` from the key.
        key_args_filter: Vec<String>,
    },
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
    /// Result of a `GenericToolExec` request.
    ///
    /// NOTE: Appended at end to preserve bincode variant indices.
    GenericToolExecResult {
        /// Tool exit code (cached on miss, replayed on hit).
        exit_code: i32,
        /// Captured stdout (empty if `output_streams.stdout` was false).
        stdout: Arc<Vec<u8>>,
        /// Captured stderr (empty if `output_streams.stderr` was false).
        stderr: Arc<Vec<u8>>,
        /// Snapshotted output files keyed by their declared relative path.
        output_files: Vec<ArtifactOutput>,
        /// True when the response was served from cache (tool not spawned).
        cached: bool,
        /// Cache key, hex-encoded. Useful for diagnostics.
        cache_key_hex: String,
    },
    /// Daemon is healthy but under sufficient internal pressure (queue depth,
    /// lock contention, etc.) that it has chosen not to dispatch this request
    /// right now. The client should sleep `retry_after_ms` and retry against
    /// the same daemon — this is **not** a wedge signal, and the existing
    /// Layer A/B/C wedge-recovery path should not fire on this response.
    ///
    /// Lets the daemon express overload in-band rather than letting the
    /// transport layer alias overload with "daemon dead" via
    /// `ERROR_PIPE_BUSY` or recv-timeout. See issue tracker for the
    /// back-pressure design doc.
    ///
    /// NOTE: Appended at end to preserve bincode variant indices.
    Backpressure {
        /// Daemon's current queue depth at the moment of decision.
        /// Diagnostic — the client treats this as advisory only.
        queue_depth: u32,
        /// How long the client should sleep before retrying, in milliseconds.
        /// Includes server-side jitter; client may add its own.
        retry_after_ms: u32,
        /// Why the daemon back-pressured. Diagnostic — known reasons:
        /// `"compile_queue_full"`, `"fp_lock_contention"`,
        /// `"depgraph_lock_contention"`, `"resident_memory_high"`.
        reason: String,
    },
}
