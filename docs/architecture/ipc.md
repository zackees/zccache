# IPC Model

Transport abstraction, socket discovery, connection lifecycle, and error handling for CLI-daemon communication.

For the protocol message types see [overview.md](overview.md) (section 2.4). For platform differences see [portability.md](portability.md).

---

## Transport Abstraction

The `Transport` trait (see overview.md section 2.3) abstracts over Unix domain sockets and Windows named pipes. The daemon and CLI code are written against the trait; platform selection happens at build time via conditional compilation:

```rust
#[cfg(unix)]
type PlatformTransport = UnixTransport;

#[cfg(windows)]
type PlatformTransport = NamedPipeTransport;
```

## Socket Discovery

**Unix (Linux / macOS):**
- Socket path: `$XDG_RUNTIME_DIR/zccache/sock`
- Fallback if `$XDG_RUNTIME_DIR` is unset: `/tmp/zccache-{uid}/sock`
- With `ZCCACHE_DAEMON_NAMESPACE=<ns>`: `sock-<ns>`
- Lock file: adjacent to socket as `daemon.lock` or `daemon-<ns>.lock`
- Directory created with mode 0700.

**Windows:**
- Named pipe: `\\.\pipe\zccache-{username}`
- With `ZCCACHE_DAEMON_NAMESPACE=<ns>`: `\\.\pipe\zccache-{username}-<ns>`
- Lock file: `~/.zccache/daemon.lock` or `~/.zccache/daemon-<ns>.lock`
- Username obtained via `GetUserNameW`.

When `ZCCACHE_CACHE_DIR` is set, the default compile daemon endpoint is derived
from that cache root. Unix uses `<cache>/daemon.sock` or
`<cache>/daemon-<ns>.sock`; Windows uses a stable cache-root ID in the named
pipe and appends `-<ns>` when a daemon namespace is configured. Explicit
`ZCCACHE_ENDPOINT` still overrides the derived endpoint.

## Connection Lifecycle

**CLI side (drop-in wrapper mode):**
1. Compute socket address.
2. Ensure daemon is running (auto-start if needed).
3. Connect and send a single `Request::CompileEphemeral` message.
4. Read one `Response::CompileResult`, relay stdout/stderr, exit.

This single-roundtrip flow replaced an earlier 3-message sequence
(SessionStart → Compile → SessionEnd) that added ~10-20ms overhead
per invocation.

**CLI side (session mode, `ZCCACHE_SESSION_ID` set):**
1. Connect and send `Request::Compile` with the existing session ID.
2. Read `Response::CompileResult`, relay output, exit.

**CLI side (session lifecycle):**
1. `zccache session-start [--stats] [--log FILE]` → `Request::SessionStart` → `Response::SessionStarted { session_id }`.
2. Build system sets `ZCCACHE_SESSION_ID=<uuid>`. Each compiler invocation sends `Request::Compile`.
3. `zccache session-stats <id>` → `Request::SessionStats` → `Response::SessionStatsResult`. Non-destructive; session stays active. Returns `Option<SessionStats>` (`None` if `--stats` was not used at start).
4. `zccache session-end <id>` → `Request::SessionEnd` → `Response::SessionEnded { stats }`. Removes the session. Idempotent: ending a UUID the daemon does not know returns `SessionEnded { stats: None }` rather than an error so wrappers can safely call `session-end` after a daemon restart (e.g. zccache-ci killing the daemon to unlock target binaries on Windows).

**Daemon side:**
1. Acquire lock file (write PID).
2. Bind transport listener.
3. Loop: accept connections, spawn a tokio task per connection.
4. Each task: read requests in a loop, process them, send responses.
   A connection may carry multiple requests (session mode) or a single
   `CompileEphemeral` (drop-in mode).

## Compile progress heartbeats (issue #1216)

A compile can legitimately park for minutes inside the daemon's global
compile-concurrency gate (`server/compile_concurrency.rs`). That wait used to
be entirely silent, so the wrapper's single blocking `recv` could not tell
"queued behind 40 other compiles" from "daemon hung" — it tripped its wedge
budget (`ZCCACHE_WEDGE_RECV_TIMEOUT_SECS`, default 180 s) and applied the
#753/#955 recovery, throwing away in-progress daemon work or killing the
daemon outright.

`Response::CompileProgress { queue_position, queue_depth, in_flight, phase }`
closes that gap **without any extra roundtrip**: it is an interim,
non-terminal frame pushed on the connection that already carries the request.

- **Daemon.** `connection::guarded_dispatch_with_progress` wraps the
  `Compile` / `CompileEphemeral` handler in a ticker
  (`COMPILE_PROGRESS_INTERVAL`, 5 s; `ZCCACHE_COMPILE_PROGRESS_INTERVAL_MS=0`
  disables). Each tick emits one frame plus a structured
  `event="compile_progress"` log line. Counters come from
  `server/compile_progress.rs`: `tokio::sync::Semaphore` reports available
  permits but neither its waiter count nor its capacity, so the gate keeps its
  own `CompileQueueGauge`. The *per-request* queue ticket reaches the
  connection layer through a task-local slot rather than a progress handle
  threaded through `handle_compile` → `pipeline` → `compile_exec` — sound
  because the handler is awaited inline by `guarded_dispatch`, never spawned.
- **Wrapper.** `wrap/ipc.rs::compile_recv_with_wedge_detection` is a recv
  *loop*: a `CompileProgress` frame prints a `zccache[info][Q]` status line
  and restarts the budget. Wedge detection therefore measures **daemon
  silence**, not compile duration — a queued-but-progressing compile keeps its
  original connection and cached-path result, while a daemon that emits
  nothing for a full budget still trips the existing wedge handling.
- **Not covered: the embedded lane.** `server/embedded.rs` calls
  `handle_compile_ephemeral` directly and has no `IpcConnection`, so an
  in-process host (soldr/fbuild via `ZccacheService`) sees no heartbeats and
  its own dispatch budget (30 s, soldr#1657) is unaffected. The
  `CompileQueueGauge` counters *are* maintained on that path, since the gate
  itself is shared — so exposing the same queue view to an embedded host is a
  cheap follow-up (a callback or a gauge accessor), not a redesign.
- **Compatibility.** Heartbeats are only emitted after the request has been
  decoded, so the client's wire version is already known. Both lanes were
  bumped in #1216 (bincode 18 → 20, prost 19 → 21, skipping 19 so the header
  byte that selects the lane never re-uses a value the other lane shipped), so
  a client too old to decode `CompileProgress` fails version negotiation long
  before a heartbeat could reach it.

## Error Handling

- If the daemon crashes mid-request, the CLI receives a broken-pipe error. The CLI falls back to running the compiler directly (non-cached) and prints a warning to stderr.
- If serialization/deserialization fails, the daemon sends `Response::Error` if possible, otherwise drops the connection. The CLI falls back.
- Timeouts: the CLI imposes a 60-second timeout on the full IPC round-trip. On timeout, it kills the request and falls back to direct compilation.
