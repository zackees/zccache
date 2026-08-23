# `build-target` composite action

Builds the zccache binaries for **one** Rust target triple and stages the
release artifacts for it. Used by `build.yml` (the 8-target dist matrix) and
by `release-auto.yml`, so both paths produce identically-shaped artifacts
rather than each workflow rolling its own packaging steps.

## Inputs

- `target` (required) — the Rust target triple.
- `cache_key` (required) — cache key suffix passed to setup-soldr.
- `binary_ext` — executable suffix, e.g. `.exe`.
- `use_soldr` — use setup-soldr for setup and caching (default `true`).
- `cross_compile` / `cross_driver` — build from a Linux x86 host with a
  soldr-managed cross driver (`native`, `zigbuild`, or `xwin`).
- `prebuild_deps` — setup-soldr prebuild dependency mode.

## Outputs

- `staging_dir` — staged binaries plus any Python artifacts.
- `standalone_dir` — packaged standalone archives.

The `cross_driver` choice is not cosmetic: the `xwin` driver is how the
aarch64-pc-windows-msvc leg is produced, and a flag set for one crate there
is read by every `cc-rs` build script in the graph — which is what broke the
1.13.6 release (#1440, #1472).
