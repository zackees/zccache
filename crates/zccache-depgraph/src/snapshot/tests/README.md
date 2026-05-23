# `snapshot/tests/`

`#[cfg(test)] mod tests` for `crates/zccache-depgraph/src/snapshot/`,
split per concern so every file stays well under 1,000 LOC.

## Layout

- `mod.rs` — declares submodules + shared test helpers (`test_path`,
  `make_ctx`, `dummy_hash`, `always_fresh`).
- `round_trip.rs` — focused snapshot ↔ load roundtrip tests for each
  field of `DepGraphSnapshot` / `ContextEntrySnapshot`: empty graph,
  populated graph, `last_file_hashes`, `artifact_key` (Some/None),
  `unresolved_includes`, `has_computed_includes`, `IncludeKind::Computed`
  inner string, empty strings, exact byte equality for hashes and
  artifact keys, stats reset.
- `persistence.rs` — file I/O and on-disk format edge cases:
  version mismatch, bad magic, truncated payload, missing file,
  atomic tmp cleanup, overwrite, zero-length payload, header too short,
  trailing garbage, payload-length overflow, plus all `classify_load`
  variants.
- `behavioral.rs` — cross-cutting behavior across save/load that does
  not fit cleanly into a single field: cache-hit recovery, context-key
  consistency, unicode paths, double-roundtrip idempotency, overlapping
  contexts, all `ContextState` variants, bit-flip detection,
  re-register after load, GC behavior on save, concurrent save/load,
  large-graph stress.
