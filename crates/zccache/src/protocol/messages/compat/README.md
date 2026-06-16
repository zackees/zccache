# compat

Bincode compatibility and protocol roundtrip tests, split from the
original `compat.rs` to stay under the 1,000 LOC per-file cap.

`mod.rs` holds shared test helpers (`roundtrip`, `variant_index`,
`sample_*`) and the `PROTOCOL_VERSION` compile-time assertions.
Submodules group related tests by domain:

| Submodule | Covers |
|---|---|
| `variant_indices.rs` | Append-only bincode variant indices for `Request` / `Response` |
| `session_stats.rs` | `SessionStats` roundtrip + `PhaseProfileSummary` JSON back-compat |
| `daemon_status.rs` | `DaemonStatus` expanded + `version` field roundtrips |
| `session_lifecycle.rs` | `SessionStart` / `SessionStarted` / `SessionEnded` / `SessionStats[Result]` |
| `clear.rs` | `Request::Clear` + `Response::Cleared` |
| `ephemeral.rs` | `CompileEphemeral` / `LinkEphemeral` / `LinkResult` + legacy-variant guards |
| `fingerprint.rs` | All four `Fingerprint*` requests + `FingerprintCheckResult` / `FingerprintAck` |
| `rust_artifacts.rs` | `ListRustArtifacts` + `RustArtifactList` + `RustArtifactInfo` |
| `generic_exec.rs` | `GenericToolExec` (issue #272) + `ExecOutputStreams` / `ExecCachePolicy` defaults |
| `artifact_payload.rs` | `ArtifactPayload::Bytes` / `Path` size + clone Arc-sharing + raw bincode |
