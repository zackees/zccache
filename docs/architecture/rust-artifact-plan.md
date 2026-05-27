# Rust Artifact Plan Contract

This document defines the implemented zccache-side contract for Rust artifact
plan execution. `soldr` produces a versioned Rust build plan, and `zccache`
validates that plan, restores or saves target artifacts, owns the bundle format,
integrates with cache backends, and reports machine-readable diagnostics.

---

## Scope and Ownership

### zccache owns

- Artifact persistence.
- Restore and save mechanics.
- Artifact archive / bundle format.
- Cache backend behavior, including local disk and GitHub Actions integration.
- Session stats, journals, cache hit / miss reporting, and miss diagnostics.
- Validation and execution of `thin` and `full` Rust cache plans.

### soldr owns

- Cargo invocation context.
- Cargo metadata and workspace interpretation.
- Producing the versioned Rust cache plan.

### setup-soldr owns

- Public action inputs.
- CI presentation and user-facing wiring.

The important boundary is that zccache consumes a structured plan. It validates
the plan, executes it, and reports what happened. It does not infer the full
Cargo workspace model by itself.

### zccache module map

The Rust plan implementation is split by ownership under
`crates/zccache/src/artifact/rust_plan/`:

- `rust_plan.rs` is a thin facade that preserves the existing public artifact
  API and CLI-facing names.
- `schema.rs` owns the v1 plan schema, schema/cache version validation, JSON
  and protobuf plan loading, and cache-key defaults.
- `proto.rs` owns protobuf structs and conversion for plans and bundle
  manifests.
- `manifest.rs` owns bundle manifest IO, manifest compatibility checks, and
  safe relative-path joins.
- `selection.rs` owns target-file walking inputs, artifact classification, and
  plan-driven thin/full artifact selection.
- `local.rs` owns local, delta, and layered save/restore execution.
- `summary.rs` owns JSON summaries, backend identity fields, and miss
  classification diagnostics consumed by soldr/setup-soldr.
- `threads.rs` owns tar/thread-count environment parsing for bundle save
  operations.
- `tests.rs` keeps the rust-plan contract tests colocated with the facade while
  the production responsibilities stay in smaller modules.

---

## Plan Inputs

The v1 plan carries the minimum information needed for deterministic
restore/save decisions. zccache accepts compact protobuf plans and legacy JSON
plans during migration:

- selected mode: `thin` or `full`
- workspace root
- target directory
- `rustc`, `cargo`, and toolchain identity
- target triple
- profile
- feature, `rustflags`, and environment inputs that affect outputs
- lockfile, config, and manifest hashes
- selected package IDs
- workspace and path dependency exclusions
- allowed artifact classes
- cache schema version
- optional journal/log path

The implemented top-level shape is:

```json
{
  "schema_version": 1,
  "mode": "thin",
  "workspace_root": "/repo",
  "target_dir": "/repo/target",
  "toolchain": {
    "rustc": "rustc 1.94.1 ...",
    "cargo": "cargo 1.94.1 ...",
    "channel": "1.94.1",
    "host": "x86_64-unknown-linux-gnu"
  },
  "target_triple": "x86_64-unknown-linux-gnu",
  "profile": "debug",
  "inputs": {
    "features_hash": "...",
    "rustflags_hash": "...",
    "env_hash": "...",
    "lockfile_hash": "...",
    "cargo_config_hash": "...",
    "manifest_hashes": []
  },
  "packages": {
    "selected_package_ids": [],
    "workspace_package_ids": [],
    "excluded_path_package_ids": []
  },
  "allowed_artifact_classes": [
    "rlib",
    "rmeta",
    "dep_info",
    "cargo_fingerprint",
    "build_script_metadata",
    "build_script_output"
  ],
  "cache_schema_version": 1,
  "journal_log_path": "/repo/.zccache/session.jsonl"
}
```

zccache treats these fields as the source of truth for compatibility checks and
plan execution. Unsupported `schema_version` or `cache_schema_version` values
fail before filesystem mutation with a JSON compatibility error when `--json`
is supplied.

---

## Thin vs Full

### `thin`

`thin` is the bounded dependency-artifact mode and is the integration default
expected from `soldr` and `setup-soldr`. zccache still requires the plan to say
`"mode": "thin"` explicitly; the default is applied by the producer/user-facing
integration layer, not by guessing inside zccache.

It is intended to restore and save the subset of artifacts needed to make dependency crates fresh without recreating unsafe transient state. In practice, that means:

- only the artifact classes explicitly allowed by the plan are eligible
- `rlib`, `rmeta`, `.d`, shared library outputs, likely proc-macro dylibs,
  Cargo
  `.fingerprint/**`, and selected build-script metadata/output can be cached
  when allowed
- workspace and path dependency outputs named by the plan are excluded
- `target/**/incremental/**` and other transient state stay out of the bundle
- restore is conservative when a field is missing or mismatched; cache-key,
  mode, or plan-identity mismatches short-circuit before restoring anything
- save only persists what the plan says is safe to reuse

`thin` is the mode that supports the common CI flow: restore dependency artifacts, rebuild only the workspace crates that actually changed, and then save the updated reusable state.

The proc-macro classification is heuristic and follows Cargo target names when
they make the crate type obvious, such as `libproc_macro2-...` dylibs.

### `full`

`full` is explicit whole-target caching and must be requested by the plan.

It is the mode for a plan that wants the target artifact set restored and saved
as a unit. Unlike `thin`, `full` does not try to stay narrow. It still remains
plan-driven: zccache saves and restores exactly what the plan describes, while
pruning transient `incremental` state.

### Shared rule

Both modes are bounded by the plan. zccache does not guess at Cargo semantics
beyond the inputs it is given.

---

## CLI Contract

The stable CLI surface for `soldr` and `setup-soldr` is:

```text
zccache rust-plan validate --plan <plan.pb> [--json] [--cache-dir <dir>] [--session-id <id>] [--endpoint <ipc>] [--journal <path.jsonl>]
zccache rust-plan restore  --plan <plan.pb> [--json] [--backend auto|local|gha] [--cache-dir <dir>] [--session-id <id>] [--endpoint <ipc>] [--journal <path.jsonl>]
zccache rust-plan save     --plan <plan.pb> [--json] [--backend auto|local|gha] [--cache-dir <dir>] [--session-id <id>] [--endpoint <ipc>] [--journal <path.jsonl>]
```

Command behavior:

- `validate` checks that a versioned Rust plan is supported and internally
  consistent, but makes no cache changes.
- `restore` applies the plan against the selected backend and restores eligible
  artifacts into `target_dir`.
- `save` captures eligible artifacts from `target_dir` and persists them through
  the selected backend.
- `--json` prints the machine-readable summary or compatibility failure shape.
- `--backend auto` is the default for `restore` and `save`; it uses GHA cache
  when the GitHub Actions cache runtime is available and otherwise falls back to
  local.
- `--backend local` uses the zccache local bundle directory.
- `--backend gha` uses the GitHub Actions cache backend with the same bundle
  contract.
- `--cache-dir` selects the local bundle root; the bundle is stored under
  `<cache-dir>/rust-plan/<cache_key>/`.
- `--session-id` asks the daemon for compile-cache stats for that session.
- `--endpoint` selects the daemon IPC endpoint used for session stats lookup.
- `--journal` reports a JSONL journal/log path in the summary, overriding the
  path carried by the plan.

---

## Bundle and Backends

### Local backend

The local backend is the reference backend. It writes a zccache-owned bundle at:

```text
<cache-dir>/rust-plan/<cache_key>/
  manifest.pb
  files/<normalized target-relative artifact paths>
```

`manifest.pb` is a compact protobuf manifest. It records the manifest schema
version, plan schema version, cache schema version, mode, cache key, creation
time, plan identity hash, bundle layer kind, optional base cache key, optional
deleted paths, and artifact entries. Each artifact entry records the normalized
target-relative path, class, size, content hash, and mtime in Unix nanoseconds.

Restore-critical metadata and soldr-to-zccache interop files use protobuf.
JSON plan files are still accepted for migration, but new plan files, bundle
manifests, and layer metadata are not JSON.

Restore validates manifest compatibility, prevents path traversal, verifies
payload size and content hash, recreates parent directories, and restores file
mtimes from manifest metadata so Cargo sees a coherent restored tree. zccache
still accepts legacy local `manifest.json` bundles for migration, but new
bundles write `manifest.pb`.

### Layered local restore

The local backend also supports a base-plus-delta workflow for soldr cook
caches:

```text
zccache rust-plan save-delta \
  --plan <plan.pb> \
  --base-cache-dir <base-cache-dir> \
  --delta-cache-dir <delta-cache-dir>

zccache rust-plan restore-layered \
  --plan <plan.pb> \
  --base-cache-dir <base-cache-dir> \
  --delta-cache-dir <delta-cache-dir>
```

`save-delta` compares selected target artifacts against the base manifest by
path, size, content hash, and mtime. Only changed or new artifacts are copied
into the delta bundle. Paths present in the base but absent from the current
selected set are recorded as protobuf tombstones.

`restore-layered` restores the base bundle first, then overlays the delta
bundle. Delta artifacts win, missing artifacts fall back to the base bundle,
and tombstones remove stale base artifacts.

### GitHub Actions backend

The GHA backend is a transport/persistence adapter around the same artifact
contract. It uses the plan cache key plus the GHA cache version hash for backend
identity, imports/export bundles through the local bundle path, and reports
backend misses as diagnostics. Backend choice is orthogonal to Rust plan
semantics: zccache owns the format, and the backend only determines where the
bundle lives.

---

## Diagnostics

zccache is the authoritative source for whether plan reuse worked. The JSON
summary is emitted for all operations when `--json` is supplied. Important
fields include:

- `operation`: `validate`, `restore`, or `save`
- `mode`: `thin` or `full`
- `plan_schema_version` and `cache_schema_version`
- `compatibility.status` and `compatibility.errors`
- `restored_file_count` and `restored_bytes`
- `saved_file_count` and `saved_bytes`
- `skipped_count`, `skipped_reasons`, and `skipped_samples`
- `miss_classifications`
- `key_input_mismatches`
- `backend`: `local`, `gha`, or `unknown` for early compatibility failures
- `cache_key`
- `backend_cache_key` and `backend_cache_version`
- `archive_path`
- `journal_log_path`
- `target_artifact_effectiveness.eligible_file_count`
- `target_artifact_effectiveness.restored_file_count`
- `target_artifact_effectiveness.reuse_ratio`
- `compile_cache_stats`

`miss_classifications` folds skip reasons, key mismatches, backend misses,
corrupt restored payloads, and compile-cache misses into stable issue-vocabulary
diagnostics. Common classifications include:

- `artifact_absent_from_restored_plan`
- `artifact_class_disallowed_by_plan`
- `workspace_or_path_dependency_excluded_by_plan`
- `toolchain_profile_rustflags_target_mismatch`
- `lockfile_config_manifest_hash_mismatch`
- `restored_payload_missing_or_corrupt`
- `backend_cache_miss`
- `zccache_compile_cache_miss_despite_equivalent_rustc_command`

Raw skip reasons remain in `skipped_reasons`; common raw values include
`transient_state`, `outside_target_dir`, and `path_traversal`.

`key_input_mismatches` reports bundle/plan mismatches such as cache key, mode,
or input hash differences. Unsupported plan or cache schema versions appear as
compatibility errors.

`compile_cache_stats` is populated only when `--session-id` is supplied and the
daemon can return that session. It is intentionally separate from
`target_artifact_effectiveness`: a successful thin restore can make dependency
`rustc` invocations disappear entirely because Cargo considers those
dependencies fresh. That is an artifact-cache success even if compile-cache hit
rate is low or unchanged.

---

## Integration Flow

Expected CI flow:

1. `setup-soldr` exposes a user-facing cache mode. `thin` is the default;
   `full` is explicit.
2. `soldr` resolves Cargo metadata/workspace intent and writes a v1 plan with
   `mode` set.
3. `soldr` runs `zccache rust-plan restore --plan <plan> --json --backend auto`.
4. Cargo builds through the zccache session/wrapper.
5. `soldr` runs `zccache rust-plan save --plan <plan> --json --backend auto`.
6. `setup-soldr` presents the zccache JSON summaries in CI output.

For cook-cache delta mode, soldr/setup-soldr provide separate base and delta
local cache directories and use `restore-layered` before the build and
`save-delta` after the build.

---

## Acceptance Shape

The implemented zccache side satisfies the contract when:

- zccache accepts and validates a versioned Rust artifact plan
- `thin` is the soldr/setup-soldr default mode and `full` is explicit
- `thin` and `full` are distinct execution modes
- local and GHA behavior share the same artifact format
- zccache owns the bundle/cache format and backend behavior
- restore/save outcomes are visible in machine-readable summaries
- artifact restore effectiveness is reported separately from compile-cache stats
- miss diagnostics explain whether reuse failed because of the plan, the bundle,
  the backend, or the underlying Cargo/`rustc` inputs
