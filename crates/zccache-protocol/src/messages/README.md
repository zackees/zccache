# protocol/messages

Wire enums and per-domain payload structs.

`mod.rs` owns the `Request` and `Response` enums. Domain payload structs live
in sibling files so new fields land next to related types instead of
interleaving every helper in one monolithic file. Direct IPC uses the prost
schema; update its version and converters when the wire changes.

| File | Owns |
|---|---|
| `mod.rs` | `Request`, `Response`, `PrivateDaemonSessionOptions` |
| `status.rs` | `DaemonStatus`, `SessionStats`, `PhaseProfileSummary`, private-daemon diagnostics |
| `artifact.rs` | `ArtifactData`, `ArtifactOutput`, `ArtifactPayload`, `LookupResult`, `RustArtifactInfo`, `StoreResult` |
| `exec.rs` | `ExecCachePolicy`, `ExecOutputStreams` (for `Request::GenericToolExec`) |
