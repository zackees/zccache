# protocol/messages

Wire enums and per-domain payload structs.

`mod.rs` owns the append-only `Request` and `Response` enums — bincode encodes
variants by declaration order, so new variants must be appended at the end and
require a `PROTOCOL_VERSION` bump (see `../mod.rs`). Domain payload structs
live in sibling files so new fields land next to related types instead of
interleaving every helper in one monolithic file.

| File | Owns |
|---|---|
| `mod.rs` | `Request`, `Response`, `PrivateDaemonSessionOptions` |
| `status.rs` | `DaemonStatus`, `SessionStats`, `PhaseProfileSummary`, private-daemon diagnostics |
| `artifact.rs` | `ArtifactData`, `ArtifactOutput`, `ArtifactPayload`, `LookupResult`, `RustArtifactInfo`, `StoreResult` |
| `exec.rs` | `ExecCachePolicy`, `ExecOutputStreams` (for `Request::GenericToolExec`) |
| `compat.rs` | Bincode roundtrip + variant-index regression tests (`cfg(test)` only) |
