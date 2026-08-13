# enforce_platform_boundary

This Dylint enforces the source boundary from zccache#1365: **host-platform
selection and native OS APIs may only appear inside the `zccache-platform`
leaf crate.**

```
Allowed:
  crates/zccache-platform/src/lib.rs          exactly one cfg_select! host selector
  src/platform_win.rs + src/platform_win/**   host cfg and native APIs allowed
  src/platform_linux.rs + src/platform_linux/**
  src/platform_macos.rs + src/platform_macos/**

Denied everywhere else in production Rust sources:
  - #[cfg(...)] / #[cfg_attr(...)] / cfg!(...) referencing host predicates
    (windows, unix, target_os, target_family, target_arch, target_env,
    target_abi, target_vendor, target_endian, target_pointer_width)
  - paths importing std::os::{windows,unix}, libc, or windows_sys
  - references to the concrete module names (platform_win, platform_linux,
    platform_macos) and to platform_imp outside zccache-platform

Allowed everywhere:
  - cfg(test) and cfg(feature = "...") and other host-independent predicates
  - neutral facade files may bridge through crate::platform_imp (never the
    concrete names) — see src/README.md for the exact classification.

Out of scope: vendor/, perf fixtures, Dylint/UI fixtures, benches,
integration tests, *_tests.rs, and the dev-only zccache-test-support crate.
```

## The ratcheting baseline

The workspace still contains pre-migration host code (~400 exact
occurrences). `src/baseline.txt` records each occurrence as a
`path<TAB>kind<TAB>normalized<TAB>ordinal` row:

- an existing exact occurrence is accepted (ordinal matches a baseline row);
- any **new** occurrence fails the lint immediately — even in a file that
  already has grandfathered entries;
- a row is deleted in the same PR that migrates its code;
- a stale row (its occurrence migrated away) fails the lint at runtime;
- the count may only stay equal or decrease, never increase;
- no wildcard, directory, crate, or whole-file exemptions.

Phase 6 of #1365 deletes the baseline at zero.

## Running

```bash
# lint the workspace (Linux; CI does this on Ubuntu)
uv run python -m ci.lint --dylint-only

# unit/UI tests for this lint
soldr rustup run nightly-2026-05-26 cargo test --manifest-path dylints/enforce_platform_boundary/Cargo.toml
```

To regenerate `src/baseline.txt` from the current workspace (migration
bootstrap only; requires dylint to run on this host):

```bash
ZCCACHE_PLATFORM_BOUNDARY_DUMP=/tmp/baseline.dump uv run python -m ci.lint --dylint-only || true
sort -u /tmp/baseline.dump > dylints/enforce_platform_boundary/src/baseline.txt
# then update the "# total" line and bump the lint crate version
```
