# Server

IPC server entry point and request handlers for the daemon. `mod.rs` is a thin
glue file: it defines `DaemonServer`, declares every submodule, and re-exports
their items so every sibling can `use super::*;` to see the same vocabulary.

Topic-focused submodules:

- `lifecycle.rs` — `DaemonServer::{bind, bind_with_cache_dir}`, `new_shared_state`, accessors, test seams
- `embedded.rs` — `EmbeddedDaemon` (in-process host integration): construction, background cache loads, compile entrypoint, flush/shutdown
- `loaders.rs` — deferred cache-load handles (`DepGraphSetter`, the four `*Loader`s) + the `DaemonServer` factory methods that hand them out (bind-first / load-in-background, #640/#784)
- `run.rs` — `DaemonServer::run` main loop + watcher pipeline initializer
- `state.rs` — `SharedState`, the daemon's central state object
- `cached_artifact.rs` — `CachedArtifact`, `CachedPayload`, payload materialization, legacy `.meta` migration
- `compiler_hash.rs` — compiler-binary hash memoization keyed by `(mtime, size)`
- `request_cache.rs` — request-level fast-path records (`RequestCacheEntry`, `CachedRequestPath`, etc.)
- `rsp_cache.rs` — response-file (`@file`) expansion + caching
- `cache_trim.rs` — time-based + size-capped trimmers for the ephemeral caches
- `in_flight.rs` — RAII guard for `state.in_flight_bytes`
- `pch.rs` — PCH source-header resolution
- `client_env.rs` — replay client env into compiler children
- `session.rs` — `Request::SessionStart` handler + session log writer
- `handle_clear.rs` — `Request::Clear` handler
- `handle_compile_ephemeral.rs` — single-roundtrip ephemeral compile + direct (uncached) compiler invocation
- `watch.rs` — `watch_directory` / `watch_directories` helpers
- `util.rs` — `persist_workers_default`, `hash_file`, `context_files_fresh`, `lookup_artifact_with_disk_fallback`

Pre-existing splits: `connection.rs`, `handle_compile.rs`, `handle_compile_multi.rs`,
`handle_link.rs`, `keys.rs`, `link_helpers.rs`, `persist.rs`, `rustc.rs`, `wal.rs`,
plus the `tests/` directory.
