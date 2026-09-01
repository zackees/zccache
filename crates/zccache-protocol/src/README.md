# zccache-protocol

Wire protocol: `Request`/`Response` enums over a length-prefixed prost daemon
frame. The schema is generated from `proto/zccache_v1.proto`.

## Module Layout

`messages/mod.rs` owns `Request` and `Response`. Domain payloads live next to
it:

- `messages/status.rs`: daemon status, session stats, phase timing.
- `messages/artifact.rs`: artifact cache payloads and Rust artifact listings.
- `messages/exec.rs`: generic tool execution request options.
- `wire_prost.rs`: generated protobuf module, v16 frame helpers, and
  `ZCCACHE_DAEMON_WIRE` parsing.
- `decode_wire_message`: dispatcher hook for prost frames and `FrameV1`
  envelopes.

New protocol payload structs should land in the closest domain module. Wire
schema changes require a `PROTOCOL_VERSION` bump.

## Wire selection

Unset or `ZCCACHE_DAEMON_WIRE=auto` selects prost. `prost`, `prost-v16`, and
`v16` are equivalent explicit values; `frame` selects the running-process
envelope. Legacy bincode values return an unsupported-value error. The
retirement follows the elapsed public release soak without asserting a fleet
telemetry sample.

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
