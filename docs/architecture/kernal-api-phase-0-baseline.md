# kernal-api Phase-0 Baseline Record

This is the authoritative collection record for zccache#1519. Its machine-checked
metadata is in `kernal-api-migration.toml` under `[baseline]`.

## Status

**Captured.** This accepted phase-0 record is the checked-in Windows baseline;
it makes no build-time improvement claim.

- Capture: `docs/evidence/kernal-api-migration/phase-0/windows-x86_64/20260826T194847Z`
- Captured at: `2026-08-26T19:48:47Z`
- Host: `x86_64-pc-windows-msvc`
- Revision: `5ad45b835008093b6699c01007823976fef86ee9`
- Toolchain: `1.95.0 (59807616e 2026-04-14)`
- Feature set: workspace default

The capture directory contains the two timing reports plus duplicate and
reverse-feature trees named by `[baseline]` in `kernal-api-migration.toml`.
Failed or partial collection attempts are not baseline evidence and must not be
committed.

The evidence README uses exact `Revision:` and `Toolchain:` provenance labels.
The latter may retain the compiler command's standard prefix from raw output
(the literal `r` immediately followed by `ustc `). The TOML record omits exactly
that prefix, which is the only normalization the inventory checker permits.

## Comparable collection plan

Use the exact feature sets and command order in the `[baseline]` manifest:

1. Run the first `soldr cargo build --workspace --timings` from a clean target
   directory and save its report as `clean-build-timing.html`.
2. Run the same command without cleaning and save
   `incremental-build-timing.html`.
3. Save duplicate dependency output as `duplicates.txt`, then reverse-feature
   reports for Tokio and `running-process` as
   `tokio-reverse-features.txt` and `running-process-reverse-features.txt`.

The accepted capture covers the workspace-default graph. Later kernal-api slices
must compare the same host, toolchain, revision family, feature set, and command
order; any new feature-set baseline needs its own accepted evidence record.
