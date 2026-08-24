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
- `cross_compile` — build from a Linux x86 host. When true, setup-soldr is
  given `cross-targets: <target>` and owns the whole toolchain lifecycle:
  Rust std, compiler, linker, SDK/sysroot, and target-scoped environment.
- `prebuild_deps` — setup-soldr prebuild dependency mode.

## Outputs

- `staging_dir` — staged binaries plus any Python artifacts.
- `standalone_dir` — packaged standalone archives.

## One toolchain owner (#1497)

There is exactly one provider of the Rust toolchain here: **setup-soldr**.
`dtolnay/rust-toolchain`, `mlugg/setup-zig`, `cargo-zigbuild`, and
`cargo-xwin` were all removed, along with the `cross_driver` input.

This is not tidying. Two providers is what broke the 1.13.6 release: dtolnay
installed the toolchain into a repo-local `RUSTUP_HOME` before setup-soldr
ran, so setup-soldr had no toolchain to cache but still wrote its own
6-file, 2.5 MB state under the shared `solo-toolchain-v2-<host>-…` key that
other jobs populate with 146–219 MB. GitHub restores the newest entry per
key, so the small one shadowed the good one and every cross target failed
with `error[E0463]: can't find crate for core`. It took four release
attempts and three manual `gh cache delete` calls to ship 1.13.6.

Cross builds now go through `soldr build --target <triple>`, the blessed
surface, which prepares the sysroot and compiler/linker itself — for
`*-pc-windows-msvc` it uses the managed xwin cache with clang/lld directly
rather than routing through `cargo xwin`. `soldr cargo …` remains the
explicit legacy passthrough and is deliberately not used for cross lanes.

`dsymutil` is still installed for `*-apple-darwin`, now keyed on the target
rather than the deleted driver, because the debug-sidecar staging step
consumes it.
