# zccache-protocol

Wire protocol: `Request`/`Response` enums over a length-prefixed daemon frame.
The active compatibility path is v15 bincode. The v16 prost schema is generated
from `proto/zccache_v1.proto` and scaffolded in `wire_prost.rs` so the daemon can
later dispatch both v15 bincode and v16 prost frames during migration.

## Module Layout

`messages/mod.rs` owns the append-only `Request` and `Response` enum order.
Domain payloads live next to it:

- `messages/status.rs`: daemon status, session stats, phase timing.
- `messages/artifact.rs`: artifact cache payloads and Rust artifact listings.
- `messages/exec.rs`: generic tool execution request options.
- `messages/compat.rs`: bincode roundtrip and variant-index guards.
- `wire_prost.rs`: generated protobuf module, v16 frame helpers, and
  `ZCCACHE_DAEMON_WIRE` parsing.
- `decode_wire_message`: migration dispatcher hook that peeks the frame
  protocol-version header and selects v15 bincode or v16 prost decoding.

New protocol payload structs should land in the closest domain module. New
enum variants must still be appended in `messages/mod.rs` and require a
`PROTOCOL_VERSION` bump.

## Wire Migration

`PROTOCOL_VERSION` remains `15` while the public `encode_message` and
`decode_message` helpers emit and accept bincode bodies. `PROST_PROTOCOL_VERSION`
is `16` for the prost path. The live daemon receive path dispatches both frame
versions, but only the control/maintenance request slice (`Ping`, `Status`,
`Shutdown`, `Clear`, `ReleaseWorktreeHandles`) is converted from prost today,
and only the matching control/maintenance response slice (`Pong`, `Status`,
`ShuttingDown`, `Cleared`, `ReleaseWorktreeHandlesResult`, `Error`) is converted
back to prost replies.
`ZCCACHE_DAEMON_WIRE` is honored for that client control slice: unset or `auto`
tries prost first and falls back to v15 bincode on a clear old-daemon protocol
rejection; `bincode` forces the old path. The live daemon can accept a direct
v16 prost `ReleaseWorktreeHandles` request, but the high-level client selector
does not route it yet. Compile, session, artifact lookup/store, fingerprint,
generic-tool, and download-daemon requests remain v15 bincode until their full
protobuf conversion lands.

## Request Variants

| Variant | Description |
|---------|-------------|
| `Ping` | Health check |
| `Shutdown` | Graceful daemon shutdown |
| `Status` | Global daemon statistics (`DaemonStatus`) |
| `SessionStart` | Create a session (`client_pid`, `working_dir`, `log_file`, `track_stats`) |
| `SessionEnd` | End a session, returns final `SessionStats` if tracking was enabled |
| `SessionStats` | Query mid-session stats without ending the session |
| `Compile` | Compile within an existing session |
| `CompileEphemeral` | Single-roundtrip compile (session start + compile + session end) |
| `LinkEphemeral` | Single-roundtrip link/archive |
| `Lookup` / `Store` | Direct artifact cache access |
| `Clear` | Wipe all caches |
| `ReleaseWorktreeHandles` | Drop session-owned handles under a worktree path |

## Response Variants

| Variant | Description |
|---------|-------------|
| `Pong` | Reply to `Ping` |
| `ShuttingDown` | Ack for `Shutdown` |
| `Status(DaemonStatus)` | Global stats snapshot |
| `SessionStarted { session_id }` | UUID session identifier |
| `SessionEnded { stats }` | Final `Option<SessionStats>` |
| `SessionStatsResult { stats }` | Mid-session `Option<SessionStats>` snapshot |
| `CompileResult` | `exit_code`, `stdout`, `stderr`, `cached` flag |
| `LinkResult` | Same as `CompileResult` plus optional `warning` |
| `Error { message }` | Error string |
| `Cleared` | Counts of artifacts/metadata/contexts removed |
| `ReleaseWorktreeHandlesResult` | Worktree-handle release counts and unreleased paths |
