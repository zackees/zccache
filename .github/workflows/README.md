# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** - Runs fmt, Dylint, MSRV, and doc builds on main and pull requests.
- **ci-check.yml** - Reusable check/test workflow used by the OS-specific CI workflows.
- **fs-matrix.yml** - Requires real ReFS/FAT, btrfs/ext4/vfat, and macOS fixtures on PRs; scheduled/manual runs also execute >4 GiB ReFS and btrfs COW acceptance.
- **clippy.yml** - Runs Clippy on pushes to main for the README status badge.
- **benchmark-stats.yml** - Manual/scheduled zccache vs bare compiler vs sccache benchmark publisher for the README images and rendered stats page.
- **perf-guard.yml** - Main-only Rust, C, and C++ perf-regression guard that runs language jobs in parallel, fails below the zccache vs bare compiler or pinned-sccache speed floors, and uploads Markdown/JSON run artifacts.

Normal build/test workflows use `zackees/setup-soldr` for Rust build acceleration, excluding `release-auto.yml`. These setup-soldr steps enable strict zccache seeding so missing managed zccache releases fail immediately instead of falling back to `cargo install`, and they set `linker: fast` explicitly to keep the intended fast-linker behavior without warning noise. Jobs that run zccache self-tests stop the setup-soldr builder daemon before the test phase, run tests with a fresh `SOLDR_CACHE_DIR`, and request `SOLDR_CACHE_LIFECYCLE=command` for the isolated test cache when supported by soldr.

Exceptions:

- **test-action.yml** exercises this repository's own zccache action and must keep using that action directly.
- **bench-action.yml** and **bench-fingerprint.yml** compare bare Cargo, sccache, and zccache behavior, so setup-soldr would invalidate the control rows.
- **perf-rust-cluster.yml** builds pinned benchmark binaries and cross-repo perf fixtures with explicit cache topology; it remains on its purpose-built cache stack.
