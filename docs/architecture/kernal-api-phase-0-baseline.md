# kernal-api Phase-0 Baseline Record

This is the authoritative collection record for zccache#1519. Its machine-checked
metadata is in `kernal-api-migration.toml` under `[baseline]`.

## Status

**Pending capture.** The migration inventory and its current dependency ownership
are checked in, but this repository does not commit host-specific timing or
dependency-tree output. No build-time improvement is claimed here.

Collect raw evidence under
`docs/evidence/kernal-api-migration/phase-0/<host>/<timestamp>/`, then update the
status to `captured` only after every named result is present in that untracked
location. Record the host triple, toolchain, Git revision, and each command's
exit status in the accompanying collection summary.

## Comparable collection plan

Use the exact feature sets and command order in the `[baseline]` manifest:

1. Run the first `soldr cargo build --workspace --timings` from a clean target
   directory and save its report as `clean-build-timing.html`.
2. Run the same command without cleaning and save
   `incremental-build-timing.html`.
3. Save duplicate dependency output as `duplicates.txt`, then reverse-feature
   reports for Tokio and `running-process` as
   `tokio-reverse-features.txt` and `running-process-reverse-features.txt`.

The feature sets cover the workspace default graph, CLI download client, and
embedded service. The report and manifest are deliberately structural until the
raw result files exist; later kernal-api slices must compare the same host,
toolchain, revision family, feature set, and command order.
