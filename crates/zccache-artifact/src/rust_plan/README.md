# rust_plan

Plan-driven Rust target artifact save/restore for `zccache-artifact`.

A `RustArtifactPlanV1` describes a Cargo target directory (workspace root, target
dir, toolchain, inputs, packages). This module bundles the selected artifacts,
computes a stable cache key, and saves/restores them against a backend.

## Modules

- `schema` — `RustArtifactPlanV1` and related plan/config types; schema versions.
- `selection` — classify and select which target-dir files belong in a bundle.
- `manifest` — bundle manifest (`RustArtifactBundleManifest`) read/write + safe join.
- `local` — local, delta, and layered save/restore execution + cache-key/identity hashing.
- `summary` — `RustPlanSummary` operation result (counts, backend identity, skips).
- `threads` — tar thread-count resolution for bundling.
- `targz` — synchronous tar+gzip codec used by the GHA backend (single home; the
  CLI's async `targz` wrappers re-export these).
- `gha` — in-process GitHub Actions cache save/restore (`save_rust_plan_gha` /
  `restore_rust_plan_gha`) and `RustPlanGhaError`. Consumed by soldr#1368.

## Public entry points

Re-exported from the crate root: `save_rust_plan_local`, `restore_rust_plan_local`,
`save_rust_plan_delta_local`, `restore_rust_plan_layered_local`, `rust_plan_cache_key`,
`rust_plan_bundle_dir`, `save_rust_plan_gha`, `restore_rust_plan_gha`,
`rust_plan_gha_version`, plus the schema/manifest/summary types.
