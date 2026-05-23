# zccache-protocol

Wire protocol: `Request`/`Response` enums, length-prefixed bincode framing.

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
