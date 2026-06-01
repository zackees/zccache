# zccache GitHub Action

Unified Rust compilation + dependency caching for GitHub Actions. Replaces both `mozilla-actions/sccache-action` and `Swatinem/rust-cache` with a single action.

## Usage

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: zackees/zccache@v1
    with:
      shared-key: ${{ matrix.target }}  # optional, for matrix isolation

  # ... your build/test steps ...

  # REQUIRED: cleanup at end of every job
  - if: always()
    uses: zackees/zccache/action/cleanup@v1
```

## What it replaces

| Before (2 actions) | After (1 action) |
|---|---|
| `mozilla-actions/sccache-action@v0.0.9` | `zackees/zccache@v1` |
| `Swatinem/rust-cache@v2` | (included) |

## Inputs

| Input | Default | Description |
|---|---|---|
| `cache-cargo-registry` | `true` | Cache `~/.cargo/registry/` and `~/.cargo/git/` |
| `cache-compilation` | `true` | Cache compilation units via zccache daemon |
| `cache-target` | `false` | Cache target snapshot + run `zccache warm`; opt in only for workflows where target snapshots are worth the disk budget |
| `target-snapshot-max-size` | `2GiB` | Skip or fail target snapshot save when the pruned snapshot exceeds this size; use `0` for unlimited |
| `target-snapshot-too-large` | `skip` | `skip` oversized target snapshots or `fail` cleanup |
| `target-prune-incremental` | `true` | Remove `target/**/incremental` before creating a snapshot |
| `target-prune-build-script-out` | `false` | Remove `target/**/build/*/out` before creating a snapshot |
| `compilation-restore-fallback` | `true` | Allow prefix fallback for compilation cache restores |
| `target-restore-fallback` | `false` | Allow prefix fallback for target snapshot restores |
| `target-dir` | `target` | Path to the cargo target directory |
| `shared-key` | `""` | Extra cache key for matrix isolation |
| `zccache-version` | `latest` | Version to install; use `source` to build the checked-out repo |
| `save-cache` | `true` | Set `false` for PR builds (restore-only) |

## Outputs

| Output | Description |
|---|---|
| `cache-hit-compilation` | Whether zccache cache was restored |
| `cache-hit-registry` | Whether cargo registry cache was restored |
| `cache-hit-target` | Whether target snapshot cache was restored |

## Architecture

The action has two parts because composite actions lack `post` steps:

1. **`zackees/zccache`** (setup) installs zccache, restores caches through the native GHA cache backend when available, and starts the daemon. When `cache-target: true` is set, it also restores the target snapshot and runs `zccache warm`.
2. **`zackees/zccache/action/cleanup`** (teardown) stops the daemon and saves caches through the same backend.

The cleanup action must be called with `if: always()` to ensure caches are saved even on failure.

### Cache layers

| Layer | What | Replaces |
|---|---|---|
| zccache compilation | Per-unit `.o`/`.rlib` files (~1ms hit) | sccache (~170ms hit) |
| Cargo registry | `~/.cargo/registry/` + `~/.cargo/git/` | Swatinem/rust-cache |
| Target snapshot | `target/` tarball excluding `incremental/` | Cargo fingerprint recomputation |

State is passed from setup to cleanup via `~/.zccache-action-state/`.
On GitHub Actions, the action prefers `zccache gha-cache restore/save` so users do
not need to add `actions/cache` steps themselves. It falls back to
`actions/cache` when the runner does not expose the GHA cache runtime, and it
still uses `actions/cache` for prefix restore-key fallback when those inputs are
enabled.

Target snapshots are disabled by default because Cargo does not garbage collect
`target/`. Most CI should keep the compilation and registry caches enabled and
leave `cache-target: false`. Enable target snapshots only for jobs where skipping
Cargo fingerprint work matters enough to spend extra cache and runner disk.

Target snapshots are a legacy action-only compatibility layer. soldr and
setup-soldr integrations should use `zccache rust-plan` for target artifact
restore/save behavior; see `../docs/architecture/target-cache.md` for the
ownership boundary.

### Restore policy

- `compilation-restore-fallback: true` keeps the speed-first behavior for incremental CI.
- `target-restore-fallback: false` is the default because stale Cargo target snapshots are not safe to prefix-restore across different source trees.
- Target snapshot saves prune `target/**/incremental` by default and skip saving when the snapshot is larger than `target-snapshot-max-size`.
- For release-hardened builds, keep `cache-target: false` and use exact-key-only restores.
