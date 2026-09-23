# CI/CD Workflows

GitHub Actions workflow definitions.

## CI modes

`ci/ci_mode.py` is the coverage selector. Ordinary pull requests and `main`
pushes select the minimal contract: formatting, Linux x86 check/test, Python
CI hygiene, wire stability, and feature-matrix consistency. A `ci-test` PR
label adds Linux integration and wrapper end-to-end checks. `ci-full` selects
all registered workflows, including macOS, Windows, filesystem, broker,
performance, action, coverage, and Clippy checks. Label changes rerun the
selector for the current PR head. Unknown `ci-*` labels fail selection.
The wrapper smoke stays on Linux and Windows in `ci-test`; native Intel and
Apple Silicon hosts run in `ci-full` and release validation only. The weekly
filesystem run keeps Linux and Windows coverage without allocating Apple hosts.

The selector's `FULL` set in `ci/ci_mode.py` is the coverage inventory; keep
it synchronized with workflow jobs. Before creating a release tag, dispatch
every workflow in that set on a branch whose tip is the exact release commit. The
release preflight checks the Actions API for a completed, successful
`workflow_dispatch` run of every listed workflow on that SHA and successful
conclusions for every required full-mode job in the same run. A normal push,
PR run, or successful build on a different commit cannot satisfy the gate.
The release workflow creates no tag until this preflight passes. Dispatching
the matrix is currently manual; a single dispatcher is still to be added.

For new pull-request tests, add a job to `ci.yml` or a reusable workflow
invoked by `ci.yml`. Never add a new PR-triggered workflow/category for a
test. Keep docs-only checks reachable when editing CI path filters. Existing
standalone PR workflows are legacy until the consolidation tracked in #1639;
this rule applies to all new tests now.

- **ci.yml** - Runs fmt on ordinary PRs and `main`; Dylint, MSRV, and docs run in full mode.
- **python-tests.yml** - Runs the fast `ci/tests/` pytest suite, including Markdown guards, on every PR and `main` update. Its Linux-native PyO3 source suite runs in full mode.
- **ci-check.yml** - Reusable check/test workflow used by the OS-specific CI workflows.
- **integration.yml** - Runs Linux workspace integration in extended/full mode; full mode also executes ignored integration/stress tests.
- **fs-matrix.yml** - Checks real ReFS/FAT, btrfs/ext4/vfat, and macOS fixtures in full PR/manual mode; the weekly run covers Linux and Windows without hosted macOS and also executes >4 GiB ReFS and btrfs COW acceptance.
- **clippy.yml** - Runs Clippy on pushes to main for the README status badge.
- **benchmark-stats.yml** - Manual/scheduled zccache vs bare compiler vs sccache benchmark publisher for the README images and rendered stats page.
- **perf-guard.yml** - Main-only Rust, C, and C++ perf-regression guard that runs language jobs in parallel, fails below the zccache vs bare compiler or pinned-sccache speed floors, and uploads Markdown/JSON run artifacts.
- **broker-stress.yml** - Exercises concurrent soldr clients against the shared broker, requires the daemon path, and rejects broker-error and uncached-fallback telemetry.

Normal build/test workflows use `zackees/setup-soldr` for Rust build acceleration, excluding `release-auto.yml`. These setup-soldr steps enable strict zccache seeding so missing managed zccache releases fail immediately instead of falling back to `cargo install`, and they set `linker: fast` explicitly to keep the intended fast-linker behavior without warning noise. Jobs that run zccache self-tests stop the setup-soldr builder daemon before the test phase, run tests with a fresh `SOLDR_CACHE_DIR`, and request `SOLDR_CACHE_LIFECYCLE=command` for the isolated test cache when supported by soldr.

Exceptions:

- **test-action.yml** exercises this repository's own zccache action and must keep using that action directly.
- **bench-action.yml** and **bench-fingerprint.yml** compare bare Cargo, sccache, and zccache behavior, so setup-soldr would invalidate the control rows.
- **perf-rust-cluster.yml** builds pinned benchmark binaries and cross-repo perf fixtures with explicit cache topology; it remains on its purpose-built cache stack.
