## depgraph/

Dependency graph for include-aware cache invalidation. Tracks `#include`
relationships between source and header files, resolves include paths against
search directories, and determines whether a compilation can use a cached
artifact.

Modules:
- `context/` — `CompileContext` (C/C++) and `RustcCompileContext`; the
  cache-key computation (`compute_context_key`, `compute_artifact_key`,
  `compute_rustc_artifact_key`) — split into `mod.rs` + `tests/`.
- `args` / `msvc_args` / `rustc_args` — per-family argv parsers
  (`ParsedArgs`, `RustcParsedArgs`).
- `compile_commands` — parser for `compile_commands.json`.
- `depfile` — depfile rewriting (`prepare_depfile`).
- `graph` — `DepGraph`, `CacheVerdict`, freshness state machine.
- `scanner` / `search_paths` — include resolution; `scanner/lex.rs` is the
  single-pass `#include` lexer (zccache#1670).
- `session` — per-session bookkeeping and stats.
- `show_includes` — MSVC `/showIncludes` parsing.
- `snapshot/` — disk persistence via bincode 1 (serde), bounded by a 7-day wall-clock TTL and a 256 MiB LRU size budget (zccache#1661).
- `system_includes` — discovery of compiler-default include dirs.
- `watcher_support` — `WatchSet` glue for the file watcher.
