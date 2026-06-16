# `rust_plan::tests`

Unit tests for the `rust_plan` module, grouped by topic into submodules.

`mod.rs` owns the shared test fixtures (`sample_plan`, `synthetic_target*`,
manifest helpers, mtime helpers) consumed by every submodule via
`super::*`.

## Submodules

- `schema_validation` — JSON/protobuf schema and cache-schema acceptance/rejection.
- `save_restore` — Thin/full save and restore happy paths, mtime preservation, allowed-class gating.
- `delta` — Delta save and layered restore (base + delta overlay with tombstones).
- `classes_and_packages` — Allowed-class filtering, proc-macro vs shared-lib heuristics, package-id-based exclusions, `package_name_from_id` parsing.
- `restore_errors` — Bundle/key mismatches, corrupt payloads, path-traversal entries, `safe_join`.
- `summary_tests` — `RustPlanSummary` backend identity, miss classifications, serialization.
- `tar_threads` — Tar-thread resolver parser and parallel-vs-sequential bundling equivalence.
- `thin_v2` — soldr#461 thin-v2 wire-format support (cache_profile, drop list, fingerprint split).

Split from a single 1271-LOC `tests.rs` to keep each file under the
1000-LOC `loc_guard.py` threshold.
