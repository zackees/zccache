# zccache-protocol

Wire protocol: `Request`/`Response` enums over a length-prefixed daemon frame.
Clients prefer the prost lane for the full request family, while the daemon
continues to dispatch both prost and legacy bincode frames during migration.
The schema is generated from `proto/zccache_v1.proto`.

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

`BINCODE_PROTOCOL_VERSION` and `PROST_PROTOCOL_VERSION` version the two lanes
independently; `PROTOCOL_VERSION` remains the bincode compatibility alias for
the legacy encode/decode helpers. Because the header version byte is
what selects the decoding lane, a bump must never re-use a value the *other* lane
has previously shipped — that is why #1216 moved bincode 18 → 20 and prost
19 → 21 rather than 18 → 19. The live daemon receive path dispatches both frame
versions and converts the full request/response family.

Unset or `ZCCACHE_DAEMON_WIRE=auto` clients try prost first and reconnect once
with bincode only after an explicit old-daemon protocol rejection. Explicit
`prost` and `bincode` values force their respective lanes. Ambiguous transport
failures never trigger a replay. Prost status responses include bounded
`bincode_requests_by_type` telemetry plus an availability bit; both fields are
skipped by legacy bincode serialization so the compatibility wire shape remains
unchanged and an unavailable old response cannot be mistaken for a real zero.

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
