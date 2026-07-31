//! Daemon lifecycle event log — shared writer for CLI + daemon.
//!
//! Synchronous, file-only sink for events that happen at process
//! boundaries. One JSONL line per event so the file is grep- and
//! `jq`-friendly.
//!
//! Lives in `zccache-core` so both the CLI (when it spawns or replaces
//! a daemon) and the daemon (when it starts up or exits) write to the
//! same file at `{cache_dir}/logs/daemon-lifecycle.log`. With both
//! sides writing, one parse of the file reconstructs the full daemon
//! lineage of a session — which is the diagnostic gap that zccache
//! issue #323 (multiple daemon spawns within one build) flagged and
//! issue #755 finished closing.
//!
//! Each `write_event` call opens the file, writes one short JSONL
//! line, and closes it. On Linux/macOS, POSIX guarantees atomic
//! appends for writes smaller than `PIPE_BUF` (4 KiB) so concurrent
//! writers from CLI + daemon do not interleave lines. On Windows,
//! `OpenOptions::append(true)` translates to `FILE_APPEND_DATA` which
//! seeks to EOF atomically on each write — also safe under contention.
//!
//! All failures are silent (`tracing::warn`) — lifecycle logging is
//! diagnostics and must never block or crash a process.
//!
//! # Event schema (additive across releases)
//!
//! Every row carries an envelope: `ts_ms`, `event`, `pid`,
//! `daemon_namespace`. Per-event extras stack on top — the writer
//! preserves any key the caller passes, so a row with extras gets
//! more fields, never fewer. Tools that parse on `event` continue to
//! work across new event types being added; downstream consumers
//! should treat unknown fields as forward-compatible.
//!
//! | event | who writes it | meaning | key fields |
//! |---|---|---|---|
//! | `spawn-attempt` | CLI | won the single-flight spawn slot and is spawning a daemon (#952) | `reason`, `endpoint`, `client_pid`, `client_version`, `client_binary_path` |
//! | `spawn-parked` | CLI | wanted a daemon but another client holds the spawn slot; parking on ready-wait (#952) | `reason`, `endpoint`, `client_pid`, `client_version` |
//! | `spawn` | daemon | bound the endpoint, server up | `endpoint`, `version`, `idle_timeout`, `client_version`, `client_binary_path` |
//! | `died-idle` | daemon | exiting after idle timeout | `reason`, `idle_secs`, `idle_timeout_secs` |
//! | `died-shutdown` | daemon | got a `Shutdown` request | `reason`, `uptime_secs` |
//! | `daemon-died` (#755) | CLI (on takeover) or daemon (self-reported) | predecessor was killed and replaced — see `reason` for the cause class | `pid` (the dying daemon), `endpoint`, `reason`, `replaced_by_pid`, `replaced_by_version`, `client_pid` |
//! | `pipe-handover` (#755) | CLI (on takeover) | new daemon claimed the endpoint a previous daemon held | `pid` (the new daemon), `inbound_pid`, `inbound_version`, `outbound_pid`, `reason`, `client_pid` |
//! | `client-disconnected` (#755) | client | IPC connection broke mid-request | `endpoint`, `client_pid`, `client_version`, `client_binary_path`, `cause`, `detail` |
//! | `wrapper-local-fallback` | CLI wrapper | retired by #1170; kept so historical logs still resolve and the audit rule can assert it never reappears | `reason`, tool fields |
//! | `daemon_spawn_breaker_open` (#1170) | CLI wrapper | daemon recovery was exhausted; later invocations fail immediately until the cool-down expires | `reason`, `cooldown_ms`, `consecutive_failures` |
//! | `wrapper-daemon-unavailable` (#1170) | CLI wrapper | the daemon could not be reached before request dispatch and the wrapper refused to run the tool uncached | `tool`, `cwd`, `endpoint`, `reason`, `phase`, `route`, `exit_code` |
//! | `version_mismatch` | daemon | client / daemon protocol versions disagree | `daemon_protocol_version`, `client_protocol_version`, `reason` |
//! | `state_corrupt` (#1157) | daemon | persisted state did not parse and was dropped rather than reconciled; consequence is a full cold recompile | `subsystem`, `consequence`, `message`, `path`, `bytes` |
//! | `insecure_socket_dir` (#1171) | daemon | the directory holding the IPC endpoint was group/other-writable; another local user could substitute the socket | `path`, `outcome`, `detail` |
//! | `insecure_deploy_dir` (#1172) | CLI | the directory the daemon binary is deployed into was group/other-writable; another local user could have replaced the binary the CLI executes | `path`, `outcome`, `detail` |
//! | `ipc_peer_rejected` (#1171) | daemon | an accepted IPC connection was refused because the peer is not this user, or its credentials were unavailable | `reason`, `detail` |
//! | `sessions_reaped` (#1165) | daemon | periodic maintenance reclaimed session state whose owning client is gone or whose tombstone aged out | `expired`, `dead_client`, `tombstones`, `remaining`, `tombstones_remaining` |
//! | `stale_depfile_dirs_swept` (#1165) | daemon | periodic maintenance reclaimed depfile directories from dead daemon instances | `cleaned` |
//! | `staged_publication_conflict` | daemon | one cache key produced two different valid generations; first generation retained | `cache_key`, `existing_generation`, `candidate_generation`, `elapsed_ns` |
//! | `staged_publication_replaces_invalid_generation` | daemon | a corrupt/incomplete selected generation was replaced by a validated compile result | `cache_key`, `invalid_generation`, `replacement_generation` |
//! | `staged_salvage_started` | daemon | publication/index failed after a successful compile; requested outputs are being recovered | `reason`, `output_count`, `copied_bytes`, `elapsed_ns` |
//! | `staged_salvage_complete` | daemon | every requested output was independently recovered | `reason`, `output_count`, `copied_bytes`, `elapsed_ns` |
//! | `staged_salvage_failed` | daemon | recovery stopped before the complete requested output set existed | `reason`, `output_count`, `copied_bytes`, `elapsed_ns` |
//! | `staged_materialization_failed` | daemon | a published miss/hit could not fulfill every requested output path | `reason`, `output_count`, `copied_bytes`, `elapsed_ns` |
//! | `child_wait_watchdog_fired` | daemon | child drain or progress watchdog released a wedged compile slot | `stage`, `pid`, `reason` |
//! | `client_cancelled` | daemon | the client disconnected before its child result could be returned | `kind`, `reason` |
//! | `died-private-owner-exit` | daemon | a private daemon exited after all registered owners disappeared | `reason`, `uptime_secs`, `removed_pids` |
//! | `embedded_flush_step_timeout` | daemon | one embedded flush/save step exceeded its shutdown budget | `step`, `timeout_ms`, `reason` |
//! | `in_flight_exec_wait_timeout` | daemon | an exec-cache waiter stopped coalescing and ran its own copy | `key`, `budget_ms`, `reason` |
//! | `rustc_identity_probe_timeout` | daemon | the rustc identity probe timed out and hashing fell back to file bytes | `compiler`, `timeout_ms`, `reason` |
//! | `compiler_identity_fallback_flavor` | daemon | a rustc identity probe fell back to file bytes for one request; the fallback is not persisted | `compiler`, `compiler_family`, `reason` |
//! | `staged_publication_failed` | daemon | a successful compile could not publish its staged generation | `reason`, `error`, `artifact_key` |
//! | `persist_failed` | daemon | an asynchronous artifact persist failed, so no index entry was published | `artifact_key`, `paths`, `errno`, `error`, `gap_ms` |
//! | `cow_unregistered_blob_evicted` | daemon | an unregistered COW blob was rejected and removed | `blob_path`, `link_count` |
//! | `cow_blob_corruption_detected` | daemon | a registered COW blob no longer matched its expected digest | blob/hash/link fields |
//! | `destination_write_failed` | daemon | cached artifact could not be written to its requested output | `artifact_key`, `path`, `errno` |
//! | `legacy_artifact_path_accessed` | daemon | legacy artifact layout was reached (Phase 0 audit failure) | path-specific fields |
//! | `miss_reason_unknown` | daemon | miss classification was not recognized (Phase 0 audit failure) | miss-specific fields |
//! | `native_flag_host_salted` | daemon | a `native` CPU-selection flag made the compile key host-specific | `compiler_family` |
//! | `time_macro_noncacheable` | daemon | a C/C++ time macro bypassed the compile cache | `source`, `input`, `macro_name` |
//! | `system_include_discovery_empty` | daemon | a C/C++ include probe returned no paths, so its compile bypassed the cache | `compiler`, `reason` |
//! | `index_writer_gone` (#1177) | daemon | an index insert could not be handed to the index writer; published artifacts are no longer durably recorded | `artifact_key`, `consequence` |
//! | `background_task_died` (#1177) | daemon | a supervised background loop stopped unexpectedly; periodic work it owned has stopped happening | `task`, `reason`, `restarts`, `restartable` |
//! | `background_task_restarted` (#1177) | daemon | a supervised background loop was brought back after an unexpected stop | `task`, `restarts` |
//! | `watcher_degraded` | daemon | the file watcher could not be armed, disabling both fast hit tiers | `reason`, `degradations` |
//! | `watcher_rearmed` | daemon | a degraded daemon re-armed its file watcher | `attempt` |
//! | `watcher_overflow` | daemon | the watcher event queue saturated; hardlink registry re-verified by stat signature | `links_unchanged`, `links_suspect` |
//! | `embedded_dropped_without_shutdown` | daemon | a host dropped an embedded service without `shutdown()`; a best-effort checkpoint was written | `persisted`, `pid` |
//!
//! ## Forensic walkthrough: the two-versions-on-one-pipe wedge
//!
//! The repro from #755 — fbuild's bundled 1.12.4 daemon being replaced
//! by a PyPI 1.12.5 daemon on the same Windows named pipe — looks
//! like this with the #755 events in place (one row per JSONL line,
//! abbreviated):
//!
//! ```text
//! spawn          {pid: 82880, version: 1.12.4, endpoint: \\.\pipe\zc-…, client_binary_path: …/zccache-1.12.4/zccache.exe}
//! client-disconnected {client_pid: 19748, client_version: 1.12.5, cause: comm-error, endpoint: \\.\pipe\zc-…}
//! spawn-attempt  {client_pid: 19748, client_version: 1.12.5, reason: replaced-comm-error, …}
//! daemon-died    {pid: 82880, reason: takeover, replaced_by_pid: 86096, replaced_by_version: 1.12.5, client_pid: 19748}
//! pipe-handover  {pid: 86096, inbound_pid: 86096, inbound_version: 1.12.5, outbound_pid: 82880, reason: previous-died}
//! spawn          {pid: 86096, version: 1.12.5, endpoint: \\.\pipe\zc-…, client_binary_path: …/site-packages/zccache.exe}
//! ```
//!
//! The lineage `82880 (1.12.4, fbuild-managed) -> 86096 (1.12.5, PyPI)`
//! on `\\.\pipe\zc-…` is reconstructable from those six rows alone —
//! no `argv[0]` introspection of dead PIDs, no timestamp guessing.
//! The `outbound_pid` / `inbound_pid` join is the structural key that
//! tools can grep against. See module-level constants for the
//! authoritative `reason` / `cause` strings the schema commits to.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::NormalizedPath;

/// Standard event names. Free-form strings also work — these
/// constants exist for the call sites we ship today.
pub const EVENT_SPAWN: &str = "spawn";
pub const EVENT_SPAWN_ATTEMPT: &str = "spawn-attempt";
/// Issue #952: the client wanted a daemon but another client already
/// holds the single-flight spawn slot; this one parks on the
/// ready-wait instead of spawning. Pairs with exactly one
/// `spawn-attempt` per cold start.
pub const EVENT_SPAWN_PARKED: &str = "spawn-parked";
pub const EVENT_DIED_IDLE: &str = "died-idle";
pub const EVENT_DIED_SHUTDOWN: &str = "died-shutdown";
pub const EVENT_VERSION_MISMATCH: &str = "version_mismatch";
/// Persisted state was unreadable and was dropped rather than reconciled
/// (#1157). Distinct from [`EVENT_VERSION_MISMATCH`], which is an expected
/// schema bump; this one means the bytes on disk did not parse at all.
///
/// Both consequences are the same for the operator — a full cold recompile —
/// so the event carries `subsystem` and `consequence` and is what makes a
/// fleet-wide "everyone recompiled today" incident attributable after the fact.
pub const EVENT_STATE_CORRUPT: &str = "state_corrupt";
/// The directory holding the IPC endpoint was group/other-writable (#1171).
///
/// Reaching that socket is arbitrary process execution as the daemon user, so
/// another local user who can create entries in this directory can substitute
/// the endpoint. Emitted whether the mode was repaired or the daemon refused
/// to serve, because "this was exposed until now" is the operator's business
/// either way.
pub const EVENT_INSECURE_SOCKET_DIR: &str = "insecure_socket_dir";
/// An accepted IPC connection was refused because the peer is not the daemon's
/// own user, or because the kernel would not report the peer's identity (#1171).
///
/// Carries `reason` (`foreign-uid` / `peer-cred-unavailable`) and a `detail`
/// string. Any occurrence means something other than this user's toolchain
/// reached the endpoint — the `0700` directory that is supposed to make that
/// impossible has been bypassed or was never applied.
pub const EVENT_IPC_PEER_REJECTED: &str = "ipc_peer_rejected";
/// The directory the daemon binary is deployed into was group/other-writable
/// (#1172).
///
/// Whoever can write there chooses what the CLI executes as the daemon.
/// Emitted whether the mode was repaired or the deploy refused, because "this
/// was exposed until now" is the operator's business either way — the same
/// contract as `insecure_socket_dir`.
pub const EVENT_INSECURE_DEPLOY_DIR: &str = "insecure_deploy_dir";
/// Periodic maintenance reclaimed session state (#1165).
///
/// Session registries grow with every client that dies without an explicit
/// `SessionEnd`, so "how much is being reclaimed, how often" is the signal that
/// separates a healthy daemon from one leaking a session per compile. Emitted
/// only when something was actually reclaimed.
pub const EVENT_SESSIONS_REAPED: &str = "sessions_reaped";
/// Periodic maintenance reclaimed depfile directories belonging to dead daemon
/// instances (#1165). Emitted only when something was actually reclaimed.
pub const EVENT_STALE_DEPFILE_DIRS_SWEPT: &str = "stale_depfile_dirs_swept";

/// Issue #755 events — daemon death + handover + client disconnect.
/// Additive: existing tooling that filters on `event` continues to
/// work; consumers that want lineage forensics opt into these names.
///
/// `daemon-died` is the generalised companion to `died-idle` /
/// `died-shutdown`: those two report a *self-observed* exit while
/// `daemon-died` is emitted by the CLI side after it kills a daemon
/// via `stop_stale_daemon`. Carries `reason: takeover`
/// (`REASON_TAKEOVER`) plus `replaced_by_pid` + `replaced_by_version`
/// so the lineage `<old pid, old version> → <new pid, new version>` on
/// one endpoint is reconstructable from a single `grep`.
pub const EVENT_DAEMON_DIED: &str = "daemon-died";

/// Emitted by the daemon that just claimed an endpoint a previous
/// daemon held. Pairs with `daemon-died{reason: takeover}` from the
/// CLI side — `outbound_pid` / `outbound_version` on this event match
/// the old daemon's `pid` / `version` from the original `spawn` line.
pub const EVENT_PIPE_HANDOVER: &str = "pipe-handover";

/// Emitted by the client side at the point its IPC connection breaks
/// mid-request. Pre-#755 this failure was only visible one round-trip
/// later as the *next* `spawn-attempt`'s `reason: replaced-comm-error`
/// — splitting it out as its own event makes the dropout observable
/// at the timestamp where it actually happened.
pub const EVENT_CLIENT_DISCONNECTED: &str = "client-disconnected";
/// Emitted when a wrapper invocation runs the real tool locally after a
/// daemon failure that occurred before request dispatch.
pub const EVENT_WRAPPER_LOCAL_FALLBACK: &str = "wrapper-local-fallback";
/// The wrapper refused to run a tool because the daemon was unreachable
/// before dispatch (#1170).
///
/// Replaces the hot-path `wrapper-local-fallback` emission. The distinction
/// matters: the old event recorded that a build had silently degraded to an
/// uncached local run and still exited 0, while this one records a build that
/// failed loudly with a dedicated exit code. Nothing emits
/// `wrapper-local-fallback` any more; its audit rule stays as the guard that
/// it does not come back.
pub const EVENT_WRAPPER_DAEMON_UNAVAILABLE: &str = "wrapper-daemon-unavailable";
/// The bounded recovery ladder was exhausted and the wrapper opened its
/// cross-invocation breaker (#1170).
///
/// Emitted once per opening, not once per translation unit: the breaker
/// exists precisely so a 1000-TU build against a dead daemon fails in
/// seconds rather than paying the ladder 1000 times, and an event per TU
/// would reintroduce the cost it removes in the log.
pub const EVENT_DAEMON_SPAWN_BREAKER_OPEN: &str = "daemon_spawn_breaker_open";
/// Phase 0 log-audit event names. These are emitted by the owning cache paths;
/// the audit catalog consumes the stable spellings rather than source regexes.
pub const EVENT_LEGACY_ARTIFACT_PATH_ACCESSED: &str = "legacy_artifact_path_accessed";
pub const EVENT_DESTINATION_WRITE_FAILED: &str = "destination_write_failed";
pub const EVENT_MISS_REASON_UNKNOWN: &str = "miss_reason_unknown";
pub const EVENT_STAGED_PUBLICATION_CONFLICT: &str = "staged_publication_conflict";
pub const EVENT_STAGED_PUBLICATION_REPLACES_INVALID_GENERATION: &str =
    "staged_publication_replaces_invalid_generation";
pub const EVENT_STAGED_PUBLICATION_FAILED: &str = "staged_publication_failed";
pub const EVENT_PERSIST_FAILED: &str = "persist_failed";
pub const EVENT_STAGED_SALVAGE_STARTED: &str = "staged_salvage_started";
pub const EVENT_STAGED_SALVAGE_COMPLETE: &str = "staged_salvage_complete";
pub const EVENT_STAGED_SALVAGE_FAILED: &str = "staged_salvage_failed";
pub const EVENT_STAGED_MATERIALIZATION_FAILED: &str = "staged_materialization_failed";
pub const EVENT_CHILD_WAIT_WATCHDOG_FIRED: &str = "child_wait_watchdog_fired";
pub const EVENT_CLIENT_CANCELLED: &str = "client_cancelled";
pub const EVENT_DIED_PRIVATE_OWNER_EXIT: &str = "died-private-owner-exit";
pub const EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT: &str = "embedded_flush_step_timeout";
pub const EVENT_IN_FLIGHT_EXEC_WAIT_TIMEOUT: &str = "in_flight_exec_wait_timeout";
pub const EVENT_RUSTC_IDENTITY_PROBE_TIMEOUT: &str = "rustc_identity_probe_timeout";
pub const EVENT_COMPILER_IDENTITY_FALLBACK_FLAVOR: &str = "compiler_identity_fallback_flavor";
pub const EVENT_COW_UNREGISTERED_BLOB_EVICTED: &str = "cow_unregistered_blob_evicted";
pub const EVENT_COW_BLOB_CORRUPTION_DETECTED: &str = "cow_blob_corruption_detected";
pub const EVENT_NATIVE_FLAG_HOST_SALTED: &str = "native_flag_host_salted";
pub const EVENT_TIME_MACRO_NONCACHEABLE: &str = "time_macro_noncacheable";
pub const EVENT_SYSTEM_INCLUDE_DISCOVERY_EMPTY: &str = "system_include_discovery_empty";
/// The index-writer task is gone, so published artifacts are no longer being
/// recorded durably (#1177).
///
/// The cache is effectively write-only from this point: every artifact it
/// publishes is invisible to the next daemon start. Emitted once per daemon —
/// the condition is permanent, so an event per compile would turn one fault
/// into an unbounded log.
pub const EVENT_INDEX_WRITER_GONE: &str = "index_writer_gone";
/// A supervised background loop stopped unexpectedly (#1177).
///
/// Every periodic task used to be a bare `tokio::spawn` with a dropped
/// handle, so a panic inside one was completely silent — the task vanished,
/// the daemon kept serving, and the only symptom was that something stopped
/// happening. This is what makes "eviction stopped running three days ago"
/// attributable instead of a mystery.
pub const EVENT_BACKGROUND_TASK_DIED: &str = "background_task_died";
/// A supervised background loop was restarted after an unexpected stop (#1177).
pub const EVENT_BACKGROUND_TASK_RESTARTED: &str = "background_task_restarted";
/// The daemon is running without a file watcher: both fast hit tiers are
/// disabled and shared cache blobs are re-hashed on every read.
pub const EVENT_WATCHER_DEGRADED: &str = "watcher_degraded";
/// A degraded daemon successfully re-armed its file watcher.
pub const EVENT_WATCHER_REARMED: &str = "watcher_rearmed";
/// The watcher event queue saturated; records how much of the hardlink
/// registry the stat-signature sweep kept trusted versus marked suspect.
pub const EVENT_WATCHER_OVERFLOW: &str = "watcher_overflow";
/// A host dropped an embedded service without calling `shutdown()`. The state
/// was salvaged by a best-effort checkpoint, but the host is misusing the API
/// — this is the signal that says so, and log-audit rules can bound it.
pub const EVENT_EMBEDDED_DROPPED_WITHOUT_SHUTDOWN: &str = "embedded_dropped_without_shutdown";

/// Complete lifecycle-event catalog. Keep this additive and update the module
/// schema table with every new event; log-audit and operator docs depend on it.
pub const EVENT_ALL: &[&str] = &[
    EVENT_SPAWN,
    EVENT_SPAWN_ATTEMPT,
    EVENT_SPAWN_PARKED,
    EVENT_DIED_IDLE,
    EVENT_DIED_SHUTDOWN,
    EVENT_DIED_PRIVATE_OWNER_EXIT,
    EVENT_VERSION_MISMATCH,
    EVENT_STATE_CORRUPT,
    EVENT_INSECURE_SOCKET_DIR,
    EVENT_IPC_PEER_REJECTED,
    EVENT_INSECURE_DEPLOY_DIR,
    EVENT_SESSIONS_REAPED,
    EVENT_STALE_DEPFILE_DIRS_SWEPT,
    EVENT_DAEMON_DIED,
    EVENT_PIPE_HANDOVER,
    EVENT_CLIENT_DISCONNECTED,
    EVENT_CLIENT_CANCELLED,
    EVENT_WRAPPER_LOCAL_FALLBACK,
    EVENT_WRAPPER_DAEMON_UNAVAILABLE,
    EVENT_DAEMON_SPAWN_BREAKER_OPEN,
    EVENT_STAGED_PUBLICATION_CONFLICT,
    EVENT_STAGED_PUBLICATION_REPLACES_INVALID_GENERATION,
    EVENT_STAGED_PUBLICATION_FAILED,
    EVENT_PERSIST_FAILED,
    EVENT_STAGED_SALVAGE_STARTED,
    EVENT_STAGED_SALVAGE_COMPLETE,
    EVENT_STAGED_SALVAGE_FAILED,
    EVENT_STAGED_MATERIALIZATION_FAILED,
    EVENT_CHILD_WAIT_WATCHDOG_FIRED,
    EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
    EVENT_IN_FLIGHT_EXEC_WAIT_TIMEOUT,
    EVENT_RUSTC_IDENTITY_PROBE_TIMEOUT,
    EVENT_COMPILER_IDENTITY_FALLBACK_FLAVOR,
    EVENT_COW_UNREGISTERED_BLOB_EVICTED,
    EVENT_COW_BLOB_CORRUPTION_DETECTED,
    EVENT_NATIVE_FLAG_HOST_SALTED,
    EVENT_TIME_MACRO_NONCACHEABLE,
    EVENT_SYSTEM_INCLUDE_DISCOVERY_EMPTY,
    EVENT_INDEX_WRITER_GONE,
    EVENT_BACKGROUND_TASK_DIED,
    EVENT_BACKGROUND_TASK_RESTARTED,
    EVENT_WATCHER_DEGRADED,
    EVENT_WATCHER_REARMED,
    EVENT_WATCHER_OVERFLOW,
    EVENT_EMBEDDED_DROPPED_WITHOUT_SHUTDOWN,
    EVENT_LEGACY_ARTIFACT_PATH_ACCESSED,
    EVENT_DESTINATION_WRITE_FAILED,
    EVENT_MISS_REASON_UNKNOWN,
];

/// Reasons the CLI emits with `spawn-attempt`. Matches the branches
/// in `ensure_daemon` — keep in sync.
pub const REASON_INITIAL_START: &str = "initial-start";
pub const REASON_REPLACED_STALE_VERSION: &str = "replaced-stale-version";
pub const REASON_REPLACED_COMM_ERROR: &str = "replaced-comm-error";
pub const REASON_REPLACED_UNREACHABLE: &str = "replaced-unreachable";

/// Reasons the daemon emits with `died-*` events.
pub const REASON_GRACEFUL_SHUTDOWN: &str = "graceful-shutdown";
pub const REASON_IDLE_TIMEOUT: &str = "idle-timeout";

/// Reasons that ride on `daemon-died` (CLI-emitted) and
/// `pipe-handover` (daemon-emitted). #755.
pub const REASON_TAKEOVER: &str = "takeover";
pub const REASON_PREVIOUS_DIED: &str = "previous-died";
pub const REASON_FORCED_REPLACE: &str = "forced-replace";
pub const REASON_PIPE_STALE: &str = "pipe-stale";

/// Causes that ride on `client-disconnected`. #755.
pub const CAUSE_COMM_ERROR: &str = "comm-error";
pub const CAUSE_PIPE_CLOSED_MID_WRITE: &str = "pipe-closed-mid-write";
pub const CAUSE_TIMEOUT: &str = "timeout";
pub const CAUSE_REPLACED_BY_OTHER_VERSION: &str = "replaced-by-other-version";

/// Issue #755 acceptance #2: emit the linked
/// `daemon-died{reason: takeover, …}` + `pipe-handover` pair when the
/// CLI's `stop_stale_daemon` + `spawn_and_wait` orchestration has
/// killed a predecessor and confirmed the new daemon is up. Called by
/// the CLI side because that's where both PIDs are known (the dying
/// daemon can't emit a `replaced_by_pid` it doesn't know about, and
/// the new daemon can't emit an `outbound_pid` because the CLI's
/// `stop_stale_daemon` deletes the lock file before it spawns).
///
/// Outbound version is left unset by default — the CLI doesn't know
/// the killed daemon's version, but the operator can correlate
/// `outbound_pid` against the matching `spawn` line a few rows up.
pub fn emit_takeover_lifecycle_events(
    killed_pid: u32,
    new_pid: u32,
    new_version: &str,
    endpoint: &str,
) {
    // Outgoing daemon's death, emitted "by the next spawner" per #755.
    // The envelope's `pid` is overridden with the dying daemon's PID
    // so the row reads as the outgoing daemon's record, not the
    // CLI's. The writer's own PID is still observable via
    // `client_pid` (see the `client_meta` field below).
    write_event(
        EVENT_DAEMON_DIED,
        serde_json::json!({
            "pid": killed_pid,
            "endpoint": endpoint,
            "reason": REASON_TAKEOVER,
            "replaced_by_pid": new_pid,
            "replaced_by_version": new_version,
            // Surface who actually wrote this row so operators can
            // distinguish "daemon self-reported death" from
            // "CLI inferred death" without inspecting source.
            "client_pid": std::process::id(),
        }),
    );
    // Incoming daemon's pipe takeover. Same emitter, but the envelope
    // PID is overridden to the incoming daemon's PID so a grep on
    // `pid == new_pid` picks up the matching spawn + handover pair.
    write_event(
        EVENT_PIPE_HANDOVER,
        serde_json::json!({
            "pid": new_pid,
            "endpoint": endpoint,
            "inbound_pid": new_pid,
            "inbound_version": new_version,
            "outbound_pid": killed_pid,
            "reason": REASON_PREVIOUS_DIED,
            "client_pid": std::process::id(),
        }),
    );
}

/// `{client_version, client_binary_path}` — the zccache binary that
/// is *writing this event line*. Added to every event so the
/// (binary_path, version) tuple a wedge involved is reconstructable
/// from the log alone, without `argv[0]` introspection of dead PIDs.
/// Issue #755 acceptance #4.
///
/// `client_version` is the compile-time `CARGO_PKG_VERSION` of the
/// caller's crate (so the daemon side gets `daemon` semantics, the
/// CLI side gets `cli` semantics — both are the same crate today).
/// `client_binary_path` is `std::env::current_exe()` rendered as a
/// string; on failure (`/proc` not mounted, custom sandbox) it falls
/// back to `"<unknown>"` so the field is always present.
#[must_use]
pub fn client_meta(client_version: &str) -> serde_json::Value {
    let binary_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "<unknown>".to_string());
    serde_json::json!({
        "client_version": client_version,
        "client_binary_path": binary_path,
    })
}

/// Soft cap on the live `daemon-lifecycle.log` size. When the file
/// exceeds this on the next `write_event` call, it is renamed to
/// `daemon-lifecycle.log.1` (replacing the prior archive, if any) and
/// a fresh empty file is opened for the new event. This bounds total
/// disk footprint to `2 × MAX_LOG_SIZE` (current + one archive).
pub const MAX_LOG_SIZE: u64 = 1024 * 1024;

/// Filename of the live lifecycle log (no extension suffix). Exposed
/// so callers that sweep `{cache_dir}/logs/` (notably the CLI's
/// `gc_log_directory`) can skip the file the daemon may be writing
/// to and avoid clobbering in-flight events.
pub const LIVE_LOG_FILENAME: &str = "daemon-lifecycle.log";

/// Returns the live lifecycle filename for the active daemon namespace.
///
/// The unset/default namespace keeps the historical `daemon-lifecycle.log`
/// filename. Explicit namespaces use `daemon-lifecycle-<namespace>.log` so
/// development and app daemons can write lifecycle events independently.
#[must_use]
pub fn live_log_filename() -> String {
    match super::config::daemon_namespace() {
        Some(namespace) => format!("daemon-lifecycle-{namespace}.log"),
        None => LIVE_LOG_FILENAME.to_string(),
    }
}

/// Returns true when `name` is a live lifecycle log filename that GC must not
/// remove merely because it is old. Rotated archives still end with `.log.1`
/// and are intentionally not considered live.
#[must_use]
pub fn is_live_lifecycle_log_name(name: &str) -> bool {
    name == LIVE_LOG_FILENAME || (name.starts_with("daemon-lifecycle-") && name.ends_with(".log"))
}

/// Append a JSONL line describing a daemon lifecycle event.
///
/// `extra` carries event-specific fields (endpoint, idle_secs,
/// reason, etc.). The function adds the standard envelope: `ts_ms`
/// (epoch milliseconds), `event` (the name), and `pid` (the current
/// process). On any failure it logs at `tracing::warn` and returns —
/// lifecycle logging is best-effort.
pub fn write_event(event_name: &str, extra: serde_json::Value) {
    if let Err(e) = try_write_to_path(log_file_path().as_path(), event_name, &extra) {
        tracing::warn!(event = event_name, "failed to write lifecycle event: {e}");
    }
}

/// Filename of the dedicated child-termination log.
///
/// Every process the daemon kills, and every child whose output pipe fails,
/// is recorded here **in addition to** the main lifecycle log.
pub const TERMINATION_LOG_FILENAME: &str = "child-terminations.jsonl";

/// Append an event to a dedicated log file beside the main lifecycle log.
///
/// The lifecycle log interleaves every daemon event, so counting how often
/// children are force-terminated means grepping a busy, rotating file and
/// hoping nothing aged out. Terminations are the signal people actually chase
/// when builds die without an explanation (soldr#1857), so they get their own
/// stream — `logs/<file_name>` — which stays readable and countable with a
/// plain `wc -l`.
///
/// Deliberately *additive*: callers still write the lifecycle event too, so
/// existing tooling and the chronological daemon narrative are unaffected.
/// Best-effort like the rest of this module — a failure here never disturbs
/// the caller.
pub fn write_event_to_named_log(file_name: &str, event_name: &str, extra: serde_json::Value) {
    let log_path = super::config::log_dir().join(file_name);
    if let Err(e) = try_write_to_path(log_path.as_path(), event_name, &extra) {
        tracing::warn!(
            event = event_name,
            log = file_name,
            "failed to write dedicated log event: {e}"
        );
    }
}

/// Write an event beneath an explicitly selected daemon-state root.
///
/// Daemons embedded in tests and hosts can use a cache root that differs from
/// the process-global `ZCCACHE_CACHE_DIR`. State-owned events must use this
/// entry point so cache-root audits inspect the same root that produced the
/// artifact or failure.
pub fn write_event_in_cache_root(cache_root: &Path, event_name: &str, extra: serde_json::Value) {
    let log_path = cache_root.join("logs").join(live_log_filename());
    if let Err(e) = try_write_to_path(&log_path, event_name, &extra) {
        tracing::warn!(
            event = event_name,
            cache_root = %cache_root.display(),
            "failed to write lifecycle event: {e}"
        );
    }
}

fn try_write_to_path(
    log_path: &Path,
    event_name: &str,
    extra: &serde_json::Value,
) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Soft-cap rotation. We have no long-lived file handle to juggle
    // (each write opens and closes one), so a plain rename works on
    // every OS without the Windows handle-replacement dance.
    let _ = rotate_if_oversized(log_path);

    let mut envelope = serde_json::Map::new();
    envelope.insert("ts_ms".to_string(), serde_json::Value::from(now_ms()));
    envelope.insert(
        "event".to_string(),
        serde_json::Value::from(event_name.to_string()),
    );
    envelope.insert(
        "pid".to_string(),
        serde_json::Value::from(std::process::id()),
    );
    envelope.insert(
        "daemon_namespace".to_string(),
        serde_json::Value::from(super::config::daemon_namespace_label()),
    );
    if let serde_json::Value::Object(fields) = extra {
        for (k, v) in fields {
            envelope.insert(k.clone(), v.clone());
        }
    }

    let mut line = serde_json::to_string(&serde_json::Value::Object(envelope))?;
    line.push('\n');

    let mut file = open_append(log_path)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Open a file in append mode with sharing flags that allow deletion
/// on Windows. Mirrors the compile journal's `log_io::open_append`;
/// duplicated here so `zccache-core` doesn't need to depend on the
/// daemon crate.
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ (0x1) | FILE_SHARE_WRITE (0x2) | FILE_SHARE_DELETE (0x4)
        opts.share_mode(0x1 | 0x2 | 0x4);
    }
    opts.open(path)
}

/// Absolute path to the live lifecycle log file.
#[must_use]
pub fn log_file_path() -> NormalizedPath {
    log_file_path_in(&super::config::log_dir())
}

/// Same as [`log_file_path`] but rooted at a caller-supplied logs
/// directory. Used by tests and by callers that already resolved a
/// non-default cache root.
#[must_use]
pub fn log_file_path_in(log_dir: &NormalizedPath) -> NormalizedPath {
    log_dir.join(live_log_filename())
}

/// If the live log file exceeds `MAX_LOG_SIZE`, rename it to `.1`
/// (replacing any existing archive). No-op if the file is missing or
/// under the threshold.
fn rotate_if_oversized(log_path: &std::path::Path) -> std::io::Result<()> {
    let size = match std::fs::metadata(log_path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if size <= MAX_LOG_SIZE {
        return Ok(());
    }
    let archive = archive_path(log_path);
    std::fs::rename(log_path, &archive)
}

fn archive_path(log_path: &std::path::Path) -> PathBuf {
    let mut name = log_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".1");
    log_path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes the env-var swap critical section so parallel tests
    /// don't observe each other's `ZCCACHE_CACHE_DIR`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous_cache_dir: Option<OsString>,
        previous_namespace: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_cache_dir = std::env::var_os(crate::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::config::DAEMON_NAMESPACE_ENV);
            std::env::set_var("ZCCACHE_CACHE_DIR", value);
            std::env::remove_var(crate::config::DAEMON_NAMESPACE_ENV);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
            }
        }

        fn set_cache_dir_and_namespace(value: &std::path::Path, namespace: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_cache_dir = std::env::var_os(crate::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::config::DAEMON_NAMESPACE_ENV);
            std::env::set_var(crate::config::CACHE_DIR_ENV, value);
            std::env::set_var(crate::config::DAEMON_NAMESPACE_ENV, namespace);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous_cache_dir {
                Some(value) => std::env::set_var(crate::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CACHE_DIR_ENV),
            }
            match &self.previous_namespace {
                Some(value) => std::env::set_var(crate::config::DAEMON_NAMESPACE_ENV, value),
                None => std::env::remove_var(crate::config::DAEMON_NAMESPACE_ENV),
            }
        }
    }

    #[test]
    fn dedicated_log_receives_events_without_disturbing_the_lifecycle_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        write_event_to_named_log(
            TERMINATION_LOG_FILENAME,
            EVENT_CHILD_WAIT_WATCHDOG_FIRED,
            serde_json::json!({"stage": "alive_hung_no_progress", "pid": 4242u64}),
        );

        let dedicated = crate::config::log_dir()
            .as_path()
            .join(TERMINATION_LOG_FILENAME);
        let contents = std::fs::read_to_string(&dedicated).expect("dedicated log written");
        let mine: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("line parses"))
            .filter(|v| v["pid"] == 4242)
            .collect();
        assert_eq!(mine.len(), 1, "expected exactly our event: {contents}");
        assert_eq!(mine[0]["event"], EVENT_CHILD_WAIT_WATCHDOG_FIRED);
        assert_eq!(mine[0]["stage"], "alive_hung_no_progress");
        // The envelope is the same as the main log's, so existing tooling
        // parses this file unchanged.
        assert!(mine[0]["ts_ms"].is_number(), "missing ts_ms envelope field");

        // The whole point is that this is a *separate* stream: writing here
        // must not append to the interleaved lifecycle log.
        let lifecycle = log_file_path().as_path().to_path_buf();
        let lifecycle_has_ours = std::fs::read_to_string(&lifecycle)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|v| v["pid"] == 4242);
        assert!(
            !lifecycle_has_ours,
            "dedicated-log write must not also land in the lifecycle log"
        );
    }

    #[test]
    fn write_event_appends_jsonl_with_envelope_and_extras() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        write_event(
            EVENT_SPAWN,
            serde_json::json!({"endpoint": "test://nowhere"}),
        );
        write_event(
            EVENT_DIED_IDLE,
            serde_json::json!({"idle_secs": 3600u64, "uptime_secs": 7200u64}),
        );

        let log_path = log_file_path().as_path().to_path_buf();
        let contents = std::fs::read_to_string(&log_path).expect("log file written");
        // Concurrent tests elsewhere in the crate legitimately emit their
        // own lifecycle events while this test's cache-dir override is
        // active: `write_event` reads the env at call time, and async
        // tests (e.g. the child-watchdog kill paths) cannot hold ENV_LOCK
        // across their awaits. Filter to the two events this test wrote
        // instead of asserting an exact line count (arm-lane flake seen
        // on zccache#987).
        let mine: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("line parses"))
            .filter(|value| {
                (value["event"] == EVENT_SPAWN && value["endpoint"] == "test://nowhere")
                    || (value["event"] == EVENT_DIED_IDLE && value["idle_secs"] == 3600)
            })
            .collect();
        assert_eq!(
            mine.len(),
            2,
            "expected this test's two events, got: {contents:?}"
        );

        let first = &mine[0];
        assert_eq!(first["event"], EVENT_SPAWN);
        assert!(first["ts_ms"].is_number());
        assert!(first["pid"].is_number());
        assert_eq!(first["endpoint"], "test://nowhere");

        let second = &mine[1];
        assert_eq!(second["event"], EVENT_DIED_IDLE);
        assert_eq!(second["idle_secs"], 3600);
        assert_eq!(second["uptime_secs"], 7200);
    }

    #[test]
    fn daemon_namespace_changes_live_lifecycle_log_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir_and_namespace(tmp.path(), "soldr-dev");

        assert_eq!(live_log_filename(), "daemon-lifecycle-soldr-dev.log");
        // Issue #761 / #762 Phase 0 versions cache state, and #1098 isolates
        // mutable state for an explicit namespace below `daemon-state/`. Use
        // the pure resolver seam so a host-provided ZCCACHE_DAEMON_STATE_DIR
        // cannot make this process-global env test race unrelated config tests.
        let versioned_root =
            crate::NormalizedPath::from(tmp.path().join(crate::config::versioned_subdir()));
        let state_dir = crate::config::daemon_state_dir_from_cache_dir_with_namespace(
            &versioned_root,
            Some("soldr-dev".to_string()),
        );
        assert_eq!(
            log_file_path_in(&state_dir.join("logs")),
            tmp.path()
                .join(crate::config::versioned_subdir())
                .join("daemon-state")
                .join("soldr-dev")
                .join("logs")
                .join("daemon-lifecycle-soldr-dev.log")
        );
    }

    #[test]
    fn live_lifecycle_log_name_matches_default_and_namespaced_logs() {
        assert!(is_live_lifecycle_log_name("daemon-lifecycle.log"));
        assert!(is_live_lifecycle_log_name("daemon-lifecycle-soldr-dev.log"));
        assert!(!is_live_lifecycle_log_name("daemon-lifecycle.log.1"));
        assert!(!is_live_lifecycle_log_name(
            "daemon-lifecycle-soldr-dev.log.1"
        ));
    }

    #[test]
    fn archive_path_appends_dot_one_alongside_parent() {
        let p = std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log");
        assert_eq!(
            archive_path(&p),
            std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log.1")
        );
    }

    #[test]
    fn write_event_rotates_when_live_log_exceeds_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        let log_path = log_file_path().as_path().to_path_buf();
        let archive = log_file_path().as_path().with_extension("log.1");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();

        let padding = vec![b'x'; (MAX_LOG_SIZE + 1024) as usize];
        std::fs::write(&log_path, &padding).expect("seed oversized log");
        assert!(std::fs::metadata(&log_path).unwrap().len() > MAX_LOG_SIZE);

        write_event(EVENT_SPAWN, serde_json::json!({"first": true}));

        let live = std::fs::read_to_string(&log_path).expect("live log readable");
        assert_eq!(live.lines().count(), 1, "live log should hold new event");
        let v: serde_json::Value = serde_json::from_str(live.trim()).expect("jsonl parses");
        assert_eq!(v["first"], true);
        assert!(archive.exists(), "archive should exist");
        assert!(std::fs::metadata(&archive).unwrap().len() > MAX_LOG_SIZE);

        std::fs::write(&log_path, &padding).expect("re-seed oversized log");
        write_event(EVENT_DIED_IDLE, serde_json::json!({"second": true}));

        assert!(archive.exists(), "archive still present");
        assert!(
            !log_file_path().as_path().with_extension("log.2").exists(),
            "no .2 archive — single-rotation policy"
        );
    }

    /// Issue #755: new event constants are present and have the names
    /// the schema doc + downstream `grep` examples reference.
    #[test]
    fn new_event_constants_match_schema() {
        assert_eq!(EVENT_DAEMON_DIED, "daemon-died");
        assert_eq!(EVENT_PIPE_HANDOVER, "pipe-handover");
        assert_eq!(EVENT_CLIENT_DISCONNECTED, "client-disconnected");
    }

    #[test]
    fn event_catalog_contains_phase_zero_and_existing_events_once() {
        let unique: std::collections::BTreeSet<_> = EVENT_ALL.iter().copied().collect();
        assert_eq!(
            unique.len(),
            EVENT_ALL.len(),
            "EVENT_ALL must not duplicate names"
        );
        assert!(unique.contains(EVENT_DESTINATION_WRITE_FAILED));
        assert!(unique.contains(EVENT_LEGACY_ARTIFACT_PATH_ACCESSED));
        assert!(unique.contains(EVENT_MISS_REASON_UNKNOWN));
        assert!(unique.contains(EVENT_CHILD_WAIT_WATCHDOG_FIRED));

        // Every emitted lifecycle event has a named constant. Parse this module's
        // declarations so adding a constant without updating EVENT_ALL fails in CI.
        let declared: std::collections::BTreeSet<_> = include_str!("lifecycle.rs")
            .split("pub const EVENT_")
            .skip(1)
            .filter_map(|tail| {
                let declaration = tail.split_once(';')?.0;
                declaration
                    .contains(": &str")
                    .then(|| declaration.split('"').nth(1))
                    .flatten()
            })
            .collect();
        assert_eq!(
            unique, declared,
            "EVENT_ALL must contain every EVENT_* constant"
        );

        let module_docs = include_str!("lifecycle.rs")
            .split_once("pub const EVENT_SPAWN")
            .map(|(docs, _)| docs)
            .expect("EVENT_SPAWN declaration should delimit the module docs");
        for event in EVENT_ALL {
            assert!(
                module_docs.contains(&format!("| `{event}`")),
                "module event-schema table is missing {event}"
            );
        }
    }

    /// Issue #755: new reason/cause constants pin the wire-visible
    /// string so a regex/dashboard built against the schema doesn't
    /// silently drift when these consts move.
    #[test]
    fn new_reason_and_cause_constants_match_schema() {
        assert_eq!(REASON_TAKEOVER, "takeover");
        assert_eq!(REASON_PREVIOUS_DIED, "previous-died");
        assert_eq!(REASON_FORCED_REPLACE, "forced-replace");
        assert_eq!(REASON_PIPE_STALE, "pipe-stale");
        assert_eq!(CAUSE_COMM_ERROR, "comm-error");
        assert_eq!(CAUSE_PIPE_CLOSED_MID_WRITE, "pipe-closed-mid-write");
        assert_eq!(CAUSE_TIMEOUT, "timeout");
        assert_eq!(CAUSE_REPLACED_BY_OTHER_VERSION, "replaced-by-other-version");
    }

    /// Issue #755 acceptance #4: `client_meta()` returns the
    /// `(client_version, client_binary_path)` pair every event must
    /// carry. Both fields are always present (binary_path falls back
    /// to `"<unknown>"` rather than being absent) so dashboards can
    /// assume the schema.
    #[test]
    fn client_meta_returns_version_and_binary_path() {
        let meta = client_meta("1.2.3");
        assert_eq!(meta["client_version"], "1.2.3");
        let bin = meta["client_binary_path"]
            .as_str()
            .expect("client_binary_path must be a string");
        assert!(
            !bin.is_empty(),
            "binary path must never be the empty string"
        );
    }

    /// `write_event` is generic over the event name (free-form
    /// string), so the new constants compose with the existing writer
    /// without a separate code path. This proves a `daemon-died` line
    /// round-trips through the JSONL writer with its custom fields
    /// intact — the wire shape downstream tools will see.
    #[test]
    fn write_event_round_trips_new_daemon_died_takeover_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        let meta = client_meta("9.9.9");
        write_event(
            EVENT_DAEMON_DIED,
            serde_json::json!({
                "endpoint": "test://endpoint",
                "reason": REASON_TAKEOVER,
                "replaced_by_pid": 4242u32,
                "replaced_by_version": "9.9.9",
                "outbound_pid": 1111u32,
                "outbound_version": "1.0.0",
                "client_version": meta["client_version"],
                "client_binary_path": meta["client_binary_path"],
            }),
        );

        let log_path = log_file_path().as_path().to_path_buf();
        let contents = std::fs::read_to_string(&log_path).expect("log file written");
        let v: serde_json::Value =
            serde_json::from_str(contents.trim()).expect("jsonl line parses");

        assert_eq!(v["event"], EVENT_DAEMON_DIED);
        assert_eq!(v["reason"], REASON_TAKEOVER);
        assert_eq!(v["replaced_by_pid"], 4242);
        assert_eq!(v["replaced_by_version"], "9.9.9");
        assert_eq!(v["outbound_pid"], 1111);
        assert_eq!(v["outbound_version"], "1.0.0");
        assert_eq!(v["client_version"], "9.9.9");
        assert!(v["client_binary_path"].is_string());
        // Envelope unchanged.
        assert!(v["ts_ms"].is_number());
        assert!(v["pid"].is_number());
    }

    /// Issue #755 acceptance #2 — the helper emits both events with
    /// the PIDs that link them: `daemon-died.pid` correlates to
    /// `pipe-handover.outbound_pid`, and `daemon-died.replaced_by_pid`
    /// correlates to `pipe-handover.inbound_pid`. This is exactly the
    /// (`outbound_pid`, `inbound_pid`) join the schema doc instructs
    /// operators to grep for.
    #[test]
    fn emit_takeover_lifecycle_events_writes_linked_pair() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        emit_takeover_lifecycle_events(1111, 2222, "9.9.9", "\\\\.\\pipe\\test");

        let log_path = log_file_path().as_path().to_path_buf();
        let contents = std::fs::read_to_string(&log_path).expect("log file written");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected daemon-died + pipe-handover");

        let died: serde_json::Value = serde_json::from_str(lines[0]).expect("daemon-died parses");
        assert_eq!(died["event"], EVENT_DAEMON_DIED);
        assert_eq!(died["reason"], REASON_TAKEOVER);
        // Envelope PID is overridden to the OUTGOING daemon's PID so
        // the row reads as the outgoing daemon's record.
        assert_eq!(died["pid"], 1111);
        assert_eq!(died["replaced_by_pid"], 2222);
        assert_eq!(died["replaced_by_version"], "9.9.9");
        assert_eq!(died["endpoint"], "\\\\.\\pipe\\test");
        // CLI's own PID is preserved as client_pid so an operator can
        // tell who wrote the row.
        assert!(died["client_pid"].is_number());

        let handover: serde_json::Value =
            serde_json::from_str(lines[1]).expect("pipe-handover parses");
        assert_eq!(handover["event"], EVENT_PIPE_HANDOVER);
        assert_eq!(handover["reason"], REASON_PREVIOUS_DIED);
        // Envelope PID is overridden to the INCOMING daemon's PID so
        // the row reads as the incoming daemon's record.
        assert_eq!(handover["pid"], 2222);
        assert_eq!(handover["inbound_pid"], 2222);
        assert_eq!(handover["inbound_version"], "9.9.9");
        assert_eq!(handover["outbound_pid"], 1111);

        // The join that satisfies acceptance #2.
        assert_eq!(died["pid"], handover["outbound_pid"]);
        assert_eq!(died["replaced_by_pid"], handover["inbound_pid"]);
    }

    #[test]
    fn write_event_does_not_rotate_when_under_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        let log_path = log_file_path().as_path().to_path_buf();
        let archive = log_file_path().as_path().with_extension("log.1");

        write_event(EVENT_SPAWN, serde_json::json!({"only": "event"}));
        write_event(EVENT_SPAWN, serde_json::json!({"another": "event"}));

        assert!(log_path.exists());
        assert!(!archive.exists());
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
