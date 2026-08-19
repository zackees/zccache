# Concurrency, Correctness & Crash Recovery

Runtime behavior of the daemon: task topology, synchronization, correctness guarantees, failure modes, and crash recovery.

For component details see [overview.md](overview.md). For platform differences see [portability.md](portability.md).

---

## Concurrency Model

### Task Topology

```
Main thread:       daemon startup, signal handling
Tokio runtime:     multi-threaded (default thread count)
  Task per IPC connection:
    - reads request
    - computes cache key (may stat/hash files)
    - looks up artifact store
    - on miss: spawns compiler, stores result
    - sends response
  Background disk maintenance task (startup + 5-minute pressure checks)
    - every pass: reaps sessions whose client died and `ended_sessions`
      tombstones past their TTL (`sessions_reaped` when it reclaims anything)
    - full passes only: sweeps `tmp/depfiles/<pid>-<instance>/` belonging to
      dead daemon instances (`stale_depfile_dirs_swept`). Standalone mode also
      sweeps once at startup; the periodic pass is what bounds growth *within*
      one daemon lifetime and is the only sweep an embedded host ever gets.
  Watcher event processing task

Dedicated OS thread:  file watcher (notify)
Dedicated OS thread:  event log writer (daemon.log)
Dedicated OS thread:  compile journal writer (compile_journal.jsonl + per-session journals)
```

For the on-disk record shape and the closed `miss_reason` enum, see
[journal-schema.md](../journal-schema.md).

### Synchronization Points

| Resource | Mechanism | Contention |
|---|---|---|
| Metadata cache | DashMap (sharded concurrent map) | Low — per-shard locks, short critical sections |
| Artifact store on disk | Atomic rename, no locks | None — each artifact has unique path |
| Artifact index | DashMap (sharded concurrent map); disk I/O only in the background WAL writer's `flush()` | Low — per-shard locks, no fsync on the mutation path |
| File watcher event channel | tokio mpsc (bounded, 4096) | Low — single producer, single consumer |
| Event log channel | tokio mpsc (unbounded) | None — lock-free send, single consumer thread |
| Compile journal channel | tokio mpsc (unbounded) | None — lock-free send, single consumer thread (writes global + per-session files) |

### Lock Ordering

There is no nested locking. The design avoids situations where one lock is held while acquiring another:
- DashMap lookups are point operations. The shard lock is released before any I/O.
- The index writer's `flush()` does not hold DashMap shard locks across disk I/O.
- The watcher thread never acquires DashMap locks directly; it sends events through a channel.

This eliminates deadlock by design.

---

## Async / process bridge (watchdogs, cancellation & timeouts)

The daemon is async internally, but compiler/linker/tool execution, some IPC,
and cache persistence are fundamentally **blocking or OS-handle driven**. Rather
than let each call site decide whether to block, spawn, or await unbounded, all
such work goes through one narrow, watchdogged bridge. The governing invariant:

> **No daemon code path may block a Tokio worker on an unbounded external wait.**
> Every process/pipe/IPC/disk wait that can hang indefinitely is either bounded
> or made cancellable, and every bound that fires is logged loudly and durably.

This is the async-bridge design tracked under meta #889. Its pieces:

### 1. The one spawn API

`daemon::process::tokio_command_output_with_priority_stdin` (and its
`_priority` / `_timeout` wrappers) is the single entry point async daemon code
uses to run a child process — compile (`compile_exec`), link (`handle_link`),
multi-compile (`handle_compile_multi`), generic exec (`handle_exec`), and the
system-include probe. It always spawns with piped stdio + `kill_on_drop(true)`.
Daemon-process death is covered separately on every supported host: Linux uses
`PR_SET_PDEATHSIG`, macOS uses running-process's transactional kqueue
supervisor, and Windows retains zccache's process-wide **job object**
(`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`). Thus the compiler child is reaped both
when a request future is cancelled and when the daemon process itself dies.

### 2. Child-wait watchdog (`daemon::child_watchdog`)

The naive `Child::wait_with_output()` can hang forever. `wait_with_output_watchdog`
replaces it with a concurrent-drain loop guarding two independent wedge modes —
**progress-based, never a wall-clock cap on the compile itself** (a large link
legitimately runs for minutes):

| Mode | Wedge | Signal | Action |
|---|---|---|---|
| **A** (#962) | Child exited, but an orphaned grandchild still holds the stdout/stderr pipe → EOF never arrives | pipe not EOF `POST_EXIT_DRAIN` after exit | abandon drain, return captured output + real status |
| **B** (#891) | Child never exits and makes no progress | no output **and** no CPU for `STALL_WINDOW` | kill the child, return |

Mode B samples per-process CPU (`GetProcessTimes` on Windows, `/proc/<pid>/stat`
on Linux, `proc_pid_rusage` on macOS) every `STALL_TICK`; a child that is silent
but CPU-bound (rustc mid-codegen) or chatty but slow is never touched.

### 3. Request cancellation (#967)

The IPC dispatch loop races each compile/link/exec handler against **client
disconnect** (`IpcConnection::wait_for_disconnect`). If the client vanishes, the
handler future is dropped at its `wait_with_output().await` point; `kill_on_drop`
reaps the child and the compile-concurrency permit is released. The embedded
service does the same via `ZccacheConfig::cancellation` (#923).

### 4. Other bounded waits

| Path | Bound | Env knob | Issue |
|---|---|---|---|
| Post-exit pipe drain (Mode A) | 2 s after exit | `ZCCACHE_POST_EXIT_DRAIN_MS` (`0`=off) | #962 |
| Alive-hung stall (Mode B) | 300 s no-progress | `ZCCACHE_STALL_WINDOW_MS` (`0`=off) | #891 |
| `<compiler> -vV` identity probe | 30 s | `ZCCACHE_RUSTC_PROBE_TIMEOUT_MS` | #972 |
| Embedded flush disk-save steps | 30 s each | — | #973 |
| `zccache exec` coalesce wait | 60 s, then run own copy | `ZCCACHE_EXEC_COALESCE_WAIT_MS` | #971 |
| Client recv (server) | 600 s | — | — |
| Windows named-pipe connect | 5 s + `ERROR_PIPE_BUSY` backoff | `ZCCACHE_PIPE_POOL_SIZE` | #666/#774 |
| Watcher consumer loop | wakes on shutdown `Notify` | — | #974 |

### 5. Sync work off the worker threads (#955)

CPU/IO-heavy synchronous sections (rayon hashing of large extern sets, big
`.rlib` persists) run under `run_cpu_blocking` — `tokio::task::block_in_place`
on the multi-thread daemon runtime (spins up a replacement worker), inline on a
current-thread runtime. Disk saves that must not stall a worker use
`spawn_blocking`.

### 6. Diagnostics (forensics)

Every watchdog/timeout/cancellation fire emits **both** a `tracing::warn!`
(`event = "child_wait_watchdog_fired"`, `"client_cancelled"`,
`"embedded_flush_step_timeout"`, `"rustc_identity_probe_timeout"`,
`"in_flight_exec_wait_timeout"`, …) **and** a durable
`core::lifecycle::write_event` record — with the stage, command, elapsed time,
and captured byte counts — so a wedge is investigable after a detached run where
daemon stderr is redirected. A silent timeout is forbidden.

---

## Standalone daemon identity, deployment & lifecycle

This section is the single source of truth for how a **standalone** zccache
daemon is named, deployed, discovered, and torn down. It is the end state of
the #1007 burn-down (#997–#1005). **Embedded mode is out of scope**: soldr and
fbuild run the daemon in-process via `ZccacheService` on synthetic `embedded:`
endpoints, never bind IPC, and are fenced off from standalone spawning by
`ZCCACHE_NO_SPAWN` (#982/#987). Everything below applies only when the `zccache`
CLI lazily spawns a real per-user daemon.

### One binary, dispatched by `argv[0]` (#998)

Only `zccache` (plus the independent `zccache-fp` utility) is shipped. It is a
**multi-call binary**: it reads its own `argv[0]` file stem and dispatches
`zccache-daemon` to the compile daemon (`daemon::entry::run`) and
`zccache-download-daemon` to the download daemon. Any other or empty name falls
through to the CLI. Both daemon entry points live in library code so the CLI
and transitional, non-shipped daemon bin targets can host them. A hidden
`zccache daemon-run <flags…>` subcommand is the `argv[0]`-independent escape
hatch (debugging, `noexec` cache dirs, unreliable `argv[0]`). The name check is
`cli::multicall::stem_matches`: Windows is case-insensitive and drops `.exe`;
Unix is exact; a teardown orphan `zccache-daemon.old.<rand>.exe` does **not**
dispatch as the daemon (`file_stem` strips only the final extension).

### Version-rooted self-materialization (#999, fixes #760)

When the daemon is needed, the CLI **copies itself** (`current_exe()`) to a
stable, version-rooted path using the daemon's own name:

```
~/.zccache/v<VERSION>/zccache-daemon[.exe]
~/.zccache/v<VERSION>/zccache-download-daemon[.exe]
```

`materialize_daemon_exe` is **idempotent** (an existing dest whose size matches
the source is reused — N concurrent same-version CLIs converge on one file, no
repeated multi-MB copies) and **atomic** (temp-in-same-dir + `rename`; a
concurrent winner is tolerated). Because each installed version owns its own
`v<VERSION>/` directory, a stale copy can never masquerade as a newer one — the
structural fix for the #760 "soft-shadow" downgrade the old random-name
`runtime-binaries/` copies allowed. Each own-name copy satisfies its exact
`verify_pid_exe_stem` identity check.

**Windows locked-exe teardown.** Windows locks a running executable's file, so
`rm -rf` of a version dir (during `zccache clear`, a stale-sibling prune, or an
upgrade) cannot delete a live daemon's `zccache-daemon.exe` — but it *can*
rename it. `trampoline::unlock_exe` renames the locked exe to
`zccache-daemon.old.<rand>.exe` to free the path; the orphan is swept once the
daemon exits. The hash/random suffix is a **teardown displacement device only**,
never the daemon's deployed identity.

### Version-aware endpoint, lock & identity (#1004, #1003)

Every front-door name carries the version tag (`v<VERSION>`), folded into the
leaf helpers `socket_name` / `daemon_socket_name` / `pipe_name` /
`lock_file_name`. Two installed versions therefore get **distinct** endpoints,
locks, and backend-identity files and never contend — kill-and-replace becomes a
same-version-only rare path (previously the #755 lifecycle-log herds). There is
**no** unversioned compat alias: binding a shared alias would reintroduce the
collision; the one-time transient extra daemon on the first upgrade (old daemon
idles out) is the accepted trade-off.

When a cache dir is pinned (`ZCCACHE_CACHE_DIR` / `--cache-dir`), the override is
normalized through `effective_cache_root_from_top_level` (`normalized_override_root`,
#1003) before the endpoint/lock/backend-identity are derived, so `--cache-dir
/foo` and `--cache-dir /foo/v<version>` resolve to the **same** daemon — the
endpoint, lock, backend identity, and daemon state all live coherently under
`/foo/v<version>`.

Unstamped development binaries add a content-derived identity to
`ZCCACHE_DAEMON_NAMESPACE` before resolving any of those names. The value is
`<version>-<first-16-hex-of-blake3(current-exe)>`, so two local builds sharing
one home root cannot displace each other's daemon. An inherited non-empty
namespace always wins: managed hosts such as soldr compute it once above a
build and every wrapper plus the spawned daemon reuse that value. Official
binaries carry the release footer and retain the bare, version-only identity
so normal upgrade semantics are unchanged.

### Conflict prevention & the retired broker (#1002)

Standalone conflict prevention is the version-aware endpoint scheme above, not a
control-plane broker. The opt-in `ZCCACHE_BROKER_CONNECT` broker-negotiation
connect lane was **retired** (#1002) — it was never enabled in production. The
daemon still **publishes** its manifest + BackendHandle identity for discovery
tooling; only the client-side broker *connect* path is gone. Clients use the
direct connect.

### Version-namespace hygiene (#1005)

The daemon writes an advisory `<top-level>/last-version.txt` on bind
(diagnostics / warm-start hint, never authoritative). `zccache clear` prunes
stale sibling `v<MAJOR.MINOR.PATCH>` dirs, **skipping** the current version and
any version whose daemon still holds its files (a live-daemon dir's removal
fails on Windows and is retried next prune — `clear` never nukes a running
daemon's state).

### Spawn single-flight & takeover

Discovery + spawn preserve the existing thundering-herd protections: the #952
`.spawn` slot (materialization runs inside it — one arbiter copies + spawns,
losers park on the ready-wait), the #640/#641 pre-bind probe, the #639 bind-race
loser-defer, the #132 recycled-PID `verify_pid_exe_stem` defense, and the
#666/#726 wedge single-payer. `ensure_daemon` kills+replaces only a
same-version *stale* daemon (`DaemonOlder`). All spawn/park/takeover decisions
land as JSONL lifecycle events (#755) for post-mortem correlation.

---

## Correctness Model

### Layered Invalidation

zccache uses a layered approach where each layer is progressively more expensive but more authoritative:

```
Layer 0: File Watcher (free, async, best-effort)
    |
    v
Layer 1: Metadata Cache lookup (in-memory, O(1))
    |
    v
Layer 2: Stat Verification (syscall, ~1us)
    |
    v
Layer 3: Content Hash (read + hash, ~1ms per file)
```

The watcher provides early warning. The metadata cache avoids redundant stats. Stat verification catches changes the watcher missed. Content hashing is the ground truth but is only invoked when cheaper layers indicate a possible change.

### Conservative Bias

When in doubt, zccache assumes the file has changed and re-verifies. Specific policies:

- **No cached hash at any confidence level:** always hash.
- **Watcher overflow:** downgrade everything to Low, stat-verify all.
- **stat race detected (mtime changed during hashing):** retry, then treat as uncacheable.
- **Unknown file ID:** fall back to path + mtime + size (less reliable, but safe because mtime changes on write in all supported filesystems).
- **Compiler binary changed:** re-hash compiler identity on every daemon start and whenever its metadata cache entry is not High.

### Failure Modes and Mitigations

| Failure | Impact | Mitigation |
|---|---|---|
| Watcher misses an event | Stale metadata at Medium | Stat verification on every cache key computation (stat guard in `lookup_since()` catches changes even without watcher) |
| Watcher overflows | Many stale entries | Downgrade all to Low; stat-verify everything |
| File replaced with same mtime/size | Incorrect cache hit | file_id (inode) detection; extremely rare in practice |
| Compiler updated in-place | Incorrect cache hit | Compiler binary is in metadata cache; stat-verified on use |
| Clock skew / mtime unreliable | Incorrect cache hit | file_id provides second signal; Low confidence triggers re-hash |
| Disk full during artifact write | Orphaned temp dir | Temp dir cleaned on startup; write failure returns error, CLI falls back |
| `index.bin` corrupt or truncated | Index lost | `ArtifactStore` reports `started_corrupt`; the daemon rebuilds by scanning artifact directories |

### What zccache Does NOT Cache

- Failed compilations (non-zero exit code).
- Compilations reading from stdin.
- Compilations involving response files that cannot be fully resolved.
- Compilations where the preprocessor output is non-deterministic (detected heuristically: `__TIME__`, `__DATE__` in source — future enhancement).

---

## Generic tool exec (`zccache exec`)

Issue #272: the `Request::GenericToolExec` handler lets arbitrary tools — linters, codegen, formatters, ad-hoc analyzers — use the daemon's artifact cache without zccache having to know their CLI. The caller declares every input (`input_files`, `input_env`, `input_extra`) and every captured output (`output_files`, `output_streams`); on a hit the tool process is NOT spawned and the cached stdout/stderr/exit-code/output-files are replayed.

Source: `crates/zccache/src/daemon/server/handle_exec.rs` + `crates/zccache/src/cli/commands/exec.rs`.

Cache-key composition has two layers, both domain-separated:

**Primary key** (domain tag `zccache-exec-key-v2`) — everything known before the tool runs:
- tool identity hash (caller `--tool-hash` override or daemon blake3 of the binary cached by `(path, mtime, size)`)
- args in argv order, after `--key-args-filter` regex drops (filtered args still reach the tool's argv)
- sorted (name=value) env subset declared via `--input-env`
- cwd (when `cwd_in_key=true`; suppressible via `--no-cwd-in-key`)
- sorted (path, content-hash) input file pairs (content via the two-layer fingerprint), declared via repeated `--input-file` and/or the bulk forms below
- sorted (path, content-hash) **Path A** transitive headers — every file reached from `--include-scan` seeds resolved against `--include-dir` / `--system-include` / `--iquote-dir` using the existing `depgraph::scanner`
- declared output_file names (so changing the capture set invalidates)
- `input_extra` opaque bytes

**Full key** (domain tag `zccache-exec-full-key-v2`) — extends the primary with Path B depfile-derived deps:
- **First invocation**: full = primary; tool runs, the emitted `--depfile` is parsed, each listed file's content-hash is recorded in a `<primary>.deps` sidecar alongside the artifact.
- **Subsequent invocations**: the sidecar is read before lookup, dep contents are re-hashed (via two-layer), and the full key is composed; lookup happens under the full key. Stale sidecars (referencing vanished files) force a fresh miss; a non-zero tool exit skips writing the sidecar so the next call cleanly bootstraps.

**Bulk input declaration** (issue #837) — for high-fanout callers (e.g. every file under `src/**`, ~1500–2000 paths) that would blow the OS argv limit spelling out `--input-file` per path:
- `--input-file-list <PATH>` — read newline-delimited paths from a file.
- `--input-file-stdin` — read newline-delimited paths from this process's stdin. Safe alongside the wrapped tool, which exec runs with a null stdin.

Both are pure delivery mechanisms: `cli/commands/exec.rs::parse_input_path_lines` trims trailing whitespace/`\r` and drops blank lines (no comment syntax — a literal `#path` is preserved), then the entries join the `--input-file` set before absolutization. The cache key is byte-identical to spelling every path on the command line. `--input-glob` (daemon-side walking) was deferred as a design question; see #837.

Cache policies (`ExecCachePolicy`):
- `Normal` (default) — look up + store
- `ReadOnly` — look up, never store
- `Bypass` (`--no-cache`) — never consult, never store
- `--non-deterministic` forces a passthrough regardless of policy

The daemon runs the tool with `env_clear()` and only the declared env subset, so the cache key is the exact functional input of the run. Concurrent callers with the same full key coalesce on `state.in_flight_exec` — the first inserter spawns the tool; the rest wait on a shared `tokio::sync::Notify` and re-attempt the cache lookup once it fires, guaranteeing exactly one tool spawn per herd.

**Measured warm-hit latency**: ~190 µs / request on Windows NTFS (criterion `benches/exec.rs::exec_warm_hit`), versus ~15 ms / cold-miss request (dominated by tool spawn cost). The IPC roundtrip + cache-key compose + artifact-replay path lands well under the issue's "sub-millisecond" warm-hit target.

The integration suite covering this handler spans two files:
- `tests/daemon_generic_exec_test.rs` (12 tests): baseline shape — warm hit, input change, mtime touch, env, cwd, no-cache, output-file capture/restore, daemon-restart persistence, output-stream toggles.
- `tests/daemon_generic_exec_advanced_test.rs` (15 tests): Path A (3), Path B (4), hybrid A+B, non-determinism, key-args filter, concurrent coalescing, tool-binary hash override, tool-touch with content unchanged, tar-restore with normalized mtimes, missing-input diagnostic.

The test fixture is the `exec_test_tool` binary (built under `--features test-support`); the criterion benchmark is `benches/exec.rs` (`exec_warm_hit`, `exec_cold_miss`, `exec_one_input_changed`).

---

## Daemon unavailable is a hard error (issue #1170)

When the daemon/cache pipeline fails **before request dispatch** (daemon spawn failure, connect timeout, `ZCCACHE_NO_SPAWN` refusal), the wrapper **refuses to run the tool**. There is no uncached fallback.

The removed fallback mirrored the tool's exit code, so a daemon outage that happened to compile fine exited `0` — the build stayed green and the outage was invisible. #1039's read-only hardlinked artifacts made it worse than a lost cache: a direct compiler run cannot overwrite them, so the fallback frequently failed or corrupted the build. `ZCCACHE_FALLBACK` (the #1211 policy gate, which had already defaulted to `Error`) is gone with it.

The refusal is loud on three surfaces (`wrap/unavailable.rs`):

- **exit 125** — wrapper-infrastructure failure, the git/env/docker convention, deliberately outside the 1/2 a compiler uses for diagnostics so CI classifies an infra failure from the code alone. This is part of the stable `zccache cc` / `zccache c++` contract.
- **stderr** — `zccache[err][D]: daemon unavailable at <endpoint> (<reason>); refusing to run <tool> uncached.`
- **a durable event** — `wrapper-daemon-unavailable`, and the `no-daemon-unavailable` audit rule forbids it in perf and integration runs (a run that hit it did not use the cache, so it measured something else).

**The only sanctioned bypasses**, both explicit and opt-in: `ZCCACHE_DISABLE=1` (full passthrough, never contacts the daemon; warns, never silent) and `ZCCACHE_PROBE_BYPASS` (meson-probe TUs exec directly; fully silent because probe callers parse the tool's stderr). Post-dispatch transport failures (`FailurePhase::DeliveryUnknown`) were already fail-fast and are unchanged — the hazard there is a double compile.

### The bounded recovery ladder

Because unavailability is now fatal, recovery has to be robust *and* bounded (`cli/recovery.rs`):

- **Total deadline** — `ZCCACHE_RECOVERY_BUDGET_MS`, default 30 s, `0` disables. Previously the ladder had no overall cap and the worst case was minutes inside one compile.
- **Wedge classification on every arm** — a wedge is probed before any kill (#753). A busy daemon under a `-j16` burst is indistinguishable from a hung one from one client's timeout; the wrapper retries once without killing and kills only when the follow-up probe also fails.
- **Identity-scoped cleanup** — the kill names the instance that failed (#1161), and clears the lock file, `<lock>.spawn`, and the backend identity file together.
- **Cross-invocation breaker** — the wrapper is a fresh process per TU, so ladder exhaustion writes `<daemon-lock>.spawn-failed`. Invocations inside its cool-down (60 s, doubling, capped at 10 min) fail immediately with the *original* reason, so a 1000-TU build fails in seconds rather than paying the ladder 1000 times. Any successful acquisition clears it; `daemon_spawn_breaker_open` fires once per opening, not per TU.

## Daemon log output (issue #1165)

The daemon's `tracing` output goes to **stderr**, and that is the operational contract: whoever supervises the process (systemd, launchd, a Windows service, a CI runner) owns that stream and is **expected to rotate it**. zccache does not rotate a stream it does not own.

For operators with no such supervisor, `ZCCACHE_LOG_FILE=<path>` adds a **size-capped** file sink alongside stderr — additive, never a redirect, so a supervisor already collecting stderr keeps getting everything. It retains one live file plus one archive, so the footprint is bounded at `2 ×` the cap (`ZCCACHE_LOG_FILE_MAX_BYTES`, default 16 MiB), matching the lifecycle log's retention shape. Writes are best-effort: a failing sink degrades to no file output and never blocks or crashes the daemon.


---

## Crash Recovery

### Daemon Crash Recovery

**Stale socket:** The CLI detects a stale socket by attempting to connect. If the connection fails (connection refused or broken pipe), the CLI removes the socket file and lock file, then starts a fresh daemon.

**Lock file:** Contains the daemon PID. The CLI checks whether the PID is alive (`kill(pid, 0)` on Unix, `OpenProcess` on Windows). If the process is dead, the lock file is stale and is removed.

### Metadata Cache Recovery

The in-memory metadata cache is **not persisted**. After a daemon restart, the cache is empty. Entries are rebuilt lazily: the first compilation after restart will stat and hash all referenced files, populating the cache. Subsequent compilations benefit from cached metadata.

This is a deliberate design choice. Persisting the metadata cache would add complexity (serialization, staleness on restart) for marginal benefit — the cache warms up within one full build.

### Dep Graph Recovery

The dep graph **is** persisted across daemon restarts (issue #262). At graceful shutdown, and again every 5 minutes while running, the daemon flushes the current `DepGraph` to `<cache_dir>/depgraph/depgraph.bin` using a rkyv zero-copy snapshot. The on-disk format carries a magic header (`ZCDG`) plus a `DEPGRAPH_VERSION` (currently 4) so old snapshots written by an incompatible build are rejected rather than misread.

On startup, the daemon attempts to load the snapshot:

- **Success:** the in-memory graph is populated from the file and `DaemonStatus.dep_graph_persisted` reports `true`. CI runs that restore `<cache_dir>` from a cache store skip the cold-seed compile entirely.
- **Missing file:** a plain cold start, no warning and no event.
- **`VersionMismatch` / corrupt bytes:** a warning is logged, a durable lifecycle event is written (`version_mismatch` or `state_corrupt`, with `subsystem=depgraph`, `path`, `bytes`, `quarantined_to`, `recovered_from`, `consequence`), and the rejected snapshot is **quarantined** rather than left for the next graceful shutdown to overwrite. See [Depgraph snapshot quarantine](#depgraph-snapshot-quarantine) below.

#### Depgraph snapshot quarantine

Issue #1157 finding 2. An artifact key is `H(logical_context_key, sorted(path → content_hash))` over the source *plus every resolved include*, and that include set exists **only** in the depgraph — `zccache_artifact::ArtifactIndex` records outputs/stdout/stderr/exit-code and nothing about inputs. So an empty graph really does force one recompile per translation unit. What survives is the artifact *store*: the recompile recomputes the identical key and re-adopts the artifact on disk, so a reset costs one recompile, not a cache wipe.

Reinterpreting a foreign-schema snapshot to "keep artifact-key resolution usable" is deliberately **not** done: reading it needs the old type definitions (a versioned migration), and doing it without one is unsafe — a schema bump can be precisely because a new input class started feeding the key (`rustc_env_deps`, #1021), and a resurrected context missing that field could satisfy `check()` and serve an artifact built under different inputs.

What the daemon does instead (`zccache_depgraph::quarantine`, driven by `daemon::depgraph_load::load_for_startup`):

- A version-skewed `depgraph.bin` is **moved** to `depgraph.v<file_version>.bin`; bytes that failed validation go to the single-slot `depgraph.corrupt.bin` (forensics only, never read back).
- A sidecar named for **this build's own** `DEPGRAPH_VERSION` is loaded back through the ordinary `classify_load` path — same magic/version/rkyv validation as the primary — so a cache root shared by two binaries with different schema versions keeps each side warm instead of destroying the other's snapshot on every switch.
- Sidecars are capped (`MAX_QUARANTINED_SNAPSHOTS`, oldest pruned first); the current build's sidecar is never a pruning candidate.

The snapshot load runs in a background blocking task after the IPC endpoint and readiness lockfile are available, so daemon startup stays fast. Compile handlers gate their first depgraph registration/check on that background task completing; otherwise a warm daemon can race the empty default graph and classify the first lookup as `cold_skip` before the persisted graph is installed.

The `dep_graph_persisted` flag is also flipped to `true` when a periodic or shutdown save completes successfully, so a daemon that started cold but has since flushed reports itself as persisted. `zccache status` surfaces this as either `vN, persisted, X.YZ MB on disk` or `vN, not persisted`.

The daemon writes its readiness lock file before the potentially expensive disk
load completes, but compile requests do not register or classify contexts until
startup depgraph classification has finished. This keeps daemon startup
observable quickly while preventing the first warm compile from racing against
the empty default graph and reporting `cold_skip` when a valid persisted graph
is about to be installed (issue #798).

### Crash Dumper (shared with CLI)

Both `zccache-cli` and `zccache-daemon` call `zccache_core::crash::install(<bin-stem>)` at the top of `main`. That call wires up:

1. A Rust panic hook that writes `<cache>/crashes/crash-<ts>-<bin>-panic.txt` (full backtrace; runs in normal context so `Backtrace::force_capture()` is safe).
2. A native signal / SEH handler (via the `crash-handler` crate) that catches SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT on Unix and structured exceptions on Windows. Writes `crash-<ts>-<bin>-<sig>.txt` with siginfo and the OS-supplied register state. No in-handler stack walking — async-signal-unsafe.

Auto-surfacing: every successful `install()` refreshes `<cache>/last_run_<bin>.txt`. The CLI then calls `zccache_core::crash::note_previous_crashes()` which emits one stderr line per CLI invocation if any dump in `<cache>/crashes/` is newer than that marker. The daemon uses `check_previous_crashes()` instead, which logs via `tracing::warn` and writes `.reported` sentinels to suppress duplicates across daemon restarts.

The dumper is intentionally text-only for v1 — minidumps via `MiniDumpWriteDump` / `minidump-writer` are out of scope (see issue #313).

### Artifact Store Recovery

**Orphaned temp directories:** On startup, `{cache_root}/tmp/` is deleted recursively. This removes any incomplete artifact writes from a previous crash.

**Artifact directories:** Intact. Atomic rename ensures an artifact directory is either fully present or absent. If the daemon crashed after creating the temp dir but before renaming, the temp dir is cleaned up and the artifact is simply absent (cache miss; the compilation will re-run).

### Index Recovery

**`index.bin`** is rewritten whole by each `flush()`, so it is never half-updated. A crash *between* flushes loses that delta — the artifact files remain on disk, so the effect is a re-miss on the unflushed keys, not incorrect behavior. A blob that is present but unparseable sets `started_corrupt`, and the daemon rebuilds the index by scanning the artifact directories. Graceful shutdown flushes synchronously.

**Index-artifact divergence:** If the daemon crashed after writing the artifact directory but before the next index flush, the artifact exists on disk but is not in the index. This is a harmless orphan; it wastes disk space but does not cause incorrect behavior. A periodic (or on-demand) maintenance task can scan the artifact directories and reconcile with the index:
- Artifact on disk but not in index: add to index.
- Entry in index but no artifact on disk: remove from index.

---

## Cache root invariants

The "cache root" is the directory resolved by
[`zccache_core::config::resolve_cache_root`][resolve]. Wrappers (notably
[soldr](https://github.com/zackees/soldr)) excludable this single directory
from Windows Defender / on-access scanners and trust that **no zccache
persistent write escapes it**. Issue #275 closes that contract.

[resolve]: ../../crates/zccache-core/src/config.rs

### Maintenance ownership

Each daemon maintains only its resolved effective root. Embedded soldr/fbuild
instances therefore cannot inspect or delete standalone `~/.zccache` state or
another product's root. The daemon is the primary scheduler; persisted daily
catch-up removes the need for an OS scheduler. Budget, pressure, age, and
accounting semantics are defined in
[artifact-store.md](artifact-store.md#daemon-owned-retention-policy).

### Resolution rules

| Source | When it fires | `cache-root --json` value |
|---|---|---|
| `ZCCACHE_CACHE_DIR` | Env var set and non-empty | `env:ZCCACHE_CACHE_DIR` |
| Same-volume colocation | `ZCCACHE_COLOCATE` is truthy *and* CWD is on a different volume from `$HOME` (issue #296) | `colocate:cross_volume` |
| Default | Otherwise | `default:platform_dirs` (`~/.zccache`) |

`zccache cache-root` (default) prints the resolved absolute path; `--json`
adds the `source`, `daemon_namespace`, and derived `daemon_endpoint` fields so
wrappers can verify at runtime that their redirect and daemon identity were
honored.

### Daemon namespace rules

`ZCCACHE_DAEMON_NAMESPACE` selects a daemon/socket namespace without changing
the cache root. This is the soldr development isolation knob: soldr can set
`ZCCACHE_DAEMON_NAMESPACE=soldr-dev` before invoking zccache so zccache
development builds do not attach to, replace, or stop the daemon used by normal
app builds on the same machine.

Unset or empty means the default namespace and keeps all historical names. A
non-empty value is trimmed, sanitized to an ASCII path component, and folded
into:

- Unix sockets: `sock-<namespace>` for runtime-dir sockets, or
  `<cache>/daemon-<namespace>.sock` when `ZCCACHE_CACHE_DIR` is set.
- Windows named pipes: `\\.\pipe\zccache-<base>-<namespace>`.
- Lock files: `daemon-<namespace>.lock`.
- Lifecycle logs: `logs/daemon-lifecycle-<namespace>.log`.

The conventional development namespace is `dev`. The old
`zccache-daemon-dev` idea is codified as namespace mode rather than a separate
shipped binary; callers should set `ZCCACHE_DAEMON_NAMESPACE=dev` (or a more
specific soldr namespace) and then use the normal `zccache` / `zccache-daemon`
entrypoints.

### Host no-spawn guard (`ZCCACHE_NO_SPAWN`)

Embedding hosts that serve compiles through the in-process embedded service
(see [embedded-service.md](embedded-service.md)) set `ZCCACHE_NO_SPAWN=1`
(or case-insensitive `true`) to forbid the CLI from ever spawning a
standalone `zccache-daemon` or `zccache-download-daemon` process
(issue #982). soldr's compiled-in `zccache` trampoline is the canonical
setter.

Semantics:

- Connecting to an **already-running, version-compatible** daemon is still
  allowed — the guard forbids spawning, not talking.
- Any path that would spawn — including the stale-daemon **replace** paths,
  which would otherwise stop the old daemon first — fails *before* any
  daemon is stopped, killed, or copied, with an error that names
  `ZCCACHE_NO_SPAWN`.
- Enforced at the `ensure_daemon` / `spawn_and_wait` chokepoints in
  `cli/runtime.rs` and `cli/commands/daemon.rs`, as a backstop inside
  `runtime::spawn_daemon` (before `prepare_daemon_exe` materializes a
  runtime-binaries copy), and on the `download_client` spawn path.
- This is different from `ZCCACHE_DISABLE=1`, which puts the compiler
  wrapper into passthrough (run the compiler, skip the cache): the no-spawn
  guard keeps every subcommand's cache semantics but turns lazy daemon
  spawns into hard errors.

### Persistent writes — exhaustive table

Every persistent write the daemon and CLI perform lands under the resolved
cache root via one of the helpers in `zccache::core::config`:

| Subpath | Owner | Resolver |
|---|---|---|
| `artifacts/` | daemon — content-addressed artifact store + sibling tmp files for atomic rename | `artifacts_dir_from_cache_dir` |
| `.disk-maintenance-last-full-v1` | daemon — timestamp of the last successful full-age retention pass | exact effective cache root |
| `tmp/` | daemon — recursively wiped on startup (orphaned in-progress writes) | `tmp_dir_from_cache_dir` |
| `tmp/depfiles/<pid>-<instance>/` | daemon — compiler-injected depfiles and Windows response files (`*.rsp`) | `depfile_dir_from_cache_dir` |
| `depgraph/depgraph.bin` | daemon — rkyv snapshot of the dep graph | `depgraph_file_path` |
| `logs/daemon.log[.<ts>]` | daemon — rolling event log | `log_dir_from_cache_dir` |
| `logs/daemon-lifecycle[--namespace].log[.1]` | daemon + CLI — JSONL lifecycle events (spawn / shutdown / version mismatch) | `lifecycle::log_file_path` |
| `logs/compile_journal.jsonl` + per-session `*.jsonl` | daemon — compile decisions | derives from `log_dir_from_cache_dir` |
| `crashes/crash-*.{txt,dmp}` + `.reported` | daemon — panic & signal dumps | `crash_dump_dir_from_cache_dir` |
| `symbols/<version>-<triple>/` + `.symref` sidecars next to dumps | CLI — `zccache symbols install` + `symbolicate` | `symbols_cache_dir_from_cache_dir` |
| `cargo-registry/<key>.tar.gz` | CLI + composite action - compressed cargo registry archive cache used by `zccache cargo-registry` and native GHA cache upload/download | `cargo_registry_cache_dir_from_cache_dir` |
| `index.bin` (+ sibling tmp) | daemon — bincode artifact index, atomic-rename writes | `index_path_from_cache_dir` |
| `metadata.bin` (+ sibling tmp) | daemon - persisted metadata cache snapshot | `metadata_path_from_cache_dir` |
| `ino/<key>.ino.cpp` | CLI — Arduino preprocessor cache | `default_cache_dir().join("ino")` |
| `kv/<namespace>/<hex>.bin` | CLI — namespaced key/value store | derives from `default_cache_dir` |
| `daemon[--namespace].lock` | CLI + daemon — PID lock | `lock_file_path` |
| `daemon[--namespace].sock` (Unix, only when env override is set) | daemon — IPC socket co-located with the cache root | `default_endpoint` |

The cache-root-rooted invariant for the well-known subpaths is asserted in
the unit test `cache_root_invariant_all_subpaths_rooted` in
`crates/zccache/src/core/config.rs`.

### Legitimate exceptions (documented and stable)

A small set of writes is intentionally *outside* the cache root. soldr
excludes these separately if Defender scanning ever becomes an issue:

- **Composite-action target snapshot metadata:** `$HOME/.zccache-target-meta`
  stores `target-meta.tar` for the optional target snapshot cache layer. This
  is action-owned rather than daemon/CLI-owned zccache cache state, and the
  path is kept stable so existing action/cache entries remain compatible. This
  is legacy action-only behavior, not the soldr target artifact interface; see
  [target-cache.md](target-cache.md).
- **Composite-action cleanup handoff state:** `$HOME/.zccache-action-state`
  stores the setup action's cache keys and options until
  `action/cleanup/action.yml` runs. It is ephemeral action state, removed by
  cleanup, and not part of the cache root contract.
- **IPC socket (Unix, no env override):** `$XDG_RUNTIME_DIR/zccache/sock`
  or `/tmp/zccache-$USER/sock`. The socket inode lives in `tmpfs` on Linux
  so it is not a real on-disk write. When `ZCCACHE_CACHE_DIR` is set, the
  socket moves into `<cache>/daemon.sock` (or
  `<cache>/daemon-<namespace>.sock`) automatically — see
  `zccache_ipc::endpoint_for_cache_dir`.
- **Named pipe (Windows):** `\\.\pipe\zccache-<username>` (default) or
  `\\.\pipe\zccache-<stable-id>` (when `ZCCACHE_CACHE_DIR` is set), with
  `-<namespace>` appended when `ZCCACHE_DAEMON_NAMESPACE` is set. Named pipes
  have no filesystem footprint — nothing for Defender to scan.
- **OS-managed working directory (`std::env::set_current_dir(temp_dir())`):**
  the daemon's `trampoline::release_cwd()` chdirs the process to `$TMPDIR`
  *only* to release the inherited CWD handle. No file is written there.
- **`.claude/`, `target/`, scratch tempdirs in dev-mode tests:** test code
  paths and dev tooling. Production runtime never writes here. The
  `ban_unrooted_tempdir` dylint blocks new ad-hoc `tempfile::tempdir()`
  call sites in production code; legacy call sites are listed explicitly
  in `dylints/ban_unrooted_tempdir/src/allowlist.txt`.

Any new persistent write must either pick a helper from the table above or
get its own row plus a one-line justification here. The dylint catches
unrooted `$TMPDIR` writes at compile time, but it cannot catch writes that
hardcode an absolute path outside the cache root — those have to be caught
in review against this section.
