# `snapshot/`

Disk persistence for the dependency graph via rkyv zero-copy serialization.

The graph is saved to `~/.zccache/depgraph/depgraph.bin` so warm contexts
survive daemon restarts and cache hits resume immediately. Public API is
unchanged from before the split — every previously-`pub` item at
`zccache_depgraph::snapshot::*` remains reachable via `pub use` re-exports.

## Layout

- `mod.rs` — snapshot types (`DepGraphSnapshot`, `FileEntrySnapshot`,
  `IncludeDirectiveSnapshot`, `ContextEntrySnapshot`, `SnapshotStats`),
  `SnapshotError`, magic + version constants, `impl DepGraph`
  conversion methods (`to_snapshot` / `from_snapshot`), and the tiny
  `paths_to_strings` / `strings_to_paths` helpers shared with tests.
- `persistence.rs` — file I/O: `save_to_file`, `load_from_file`,
  `classify_load`, `depgraph_file_path`, and the `DepGraphLoadOutcome`
  enum returned by `classify_load`.
- `tests/` — `#[cfg(test)]` submodules split per concern: roundtrip,
  persistence/error variants, and behavioral.

## On-disk format

Header (16 bytes):

```
magic (4)  | version (LE u32) | payload_len (LE u64)
"ZCDG"     |    DEPGRAPH_     |        rkyv payload bytes
           |    VERSION       |
```

Payload: rkyv-archived `DepGraphSnapshot`, validated via
`rkyv::check_archived_root` on load. Atomic write uses tempfile +
rename (with explicit pre-remove on Windows).
