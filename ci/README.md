# CI & Development Tools

Python scripts for development tooling. Rust commands go through `soldr <tool>` so repo-local toolchain homes and the pinned rustup-managed toolchain are resolved in one place.

## Top-Level Scripts

- **`uv run --no-project python ci/incremental_build.py --samples 5 --output incremental-build.json`** - Measures warm rebuilds after touching compile, link, exec, connection, and shared-state surfaces. Records timing distributions, rebuilt packages, and aggregate process-tree RSS.
- **`uv run --no-project python ci/compile_boundary.py`** - Reports compile-handler references to server-private symbols and rejects new glob imports. Pass `--deny-all-globs` after the remaining legacy imports are removed.
- **`ci/save_cache_policy.py`** - Resolves the zccache Action's `save-cache: auto|true|false` (default `auto`) once per job: skip on GitHub-hosted `pull_request`, always save on local runners (act/bosn). See `action/README.md` (#1783)
- **`ci/runner_identity.sh begin|end`** / **`ci/runner_identity.py`** - Perf Guard observability (#1861, #1807): `begin` logs one `runner-identity` block (`runner.cpu_model`, cores, memory, kernel, `ImageVersion`, steal/load baseline) and mirrors it to `$GITHUB_STEP_SUMMARY`; `end` logs a `runner-identity-delta` line (steal ticks/percent, load averages). Run as separate steps outside the timed benchmarks. `runner_identity.parse_runner_identity` (also `perf_guard.parse_runner_identity`) parses the block from a raw job log so attempts can be grouped by CPU model
- **`uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run`** - The local gate (`local-gate.toml`, zackees/ci.yml GATE-001..010): runs `ci/local_gate.py` on a clean committed tree and stamps HEAD with `Local-Gate:`/`Ci-Attestation:` trailers. Lanes: `lint` (exactly the CI Formatting job, which runs `ci/local_gate.py --lane lint` and nothing else), `py-tests` (`ci/tests`), `check` (the real MSRV job, including workspace check and nested Dylint tests, through Bosn Actions), `docs` (rustdoc), `tests` (the real Integration (Linux) workflow through `bosn ci run`, never on the host; successful receipts must match the clean workspace/HEAD and executed required steps). An attested, in-policy PR head skips the remote Formatting, Documentation, MSRV and Integration (Linux) jobs; `main` pushes always run them
- **`ci/nextest_isolation_guard.sh`** - nextest run-wrapper (`.config/nextest.toml`): refuses to run a test unless `CI=true` or `ZCCACHE_TEST_ISOLATED=1` (GATE-005), so the suite cannot start daemons beside the host's live zccache
- **`./lint`** - Workspace linting (rustfmt + clippy), supports single-file mode
- **`./test`** - Workspace tests, supports per-crate filtering
- **`./perf.sh`** - Performance benchmarks (zccache vs sccache vs bare clang)
- **`uv run python -m ci.benchmark_stats`** - Generates `index.html`, `latest.json`, and per-language benchmark JPGs from validated perf output for the published benchmark report
- **`python -m ci.perf_guard`** - Fails CI when Rust, C, or C++ zccache benchmark rows fall below the bare-compiler or pinned-sccache speed floors. Cold-vs-bare floors whose zccache cost is a fixed added time are re-based per runner by `ci/perf_floor.py` (#1445): a runner faster than the calibration runner keeps the same added-time budget instead of a smaller one. Sub-millisecond warm hits (`c-static-library-link`, `cpp-driver-link`) are gated on an absolute zccache hit budget from `WARM_HIT_BUDGET_SECONDS`, using the nanosecond metric record (`ci/perf_precision.py`), because their ratio against a runner-speed-dependent baseline is noise (#1807); the sample provenance is in `ci/perf_threshold_history.json`
- **`uv run --with pyyaml python ci/render_feature_matrix.py`** - Renders the zccache vs sccache feature matrix from `docs/feature-matrix.yaml` into the README headline/full tables and `docs/FEATURE-MATRIX.md`. Pass `--check` to verify outputs are up-to-date (CI gate)
- **`uv run --no-project python ci/clear_runtime_telemetry.py --cache-root <dir>`** - Deletes the telemetry files `zccache-ci audit-logs` reads (journals, lifecycle and daemon logs) while keeping cache artifacts; the Integration workflow runs it around every seeded or intentional-failure phase. `ci/log_audit_source_fixture.json` is the shared Rust/Python contract for which files count (#1523)
- **`uv run python -m ci.host_diag`** - single-mode host validation diagnostic (issue #1186): timestamped streamed gates, per-gate compile-journal miss-reason summaries, overlapping-session detection, JSON report under `.cache/host-diag/`

The benchmark tests keep human-readable Markdown in `benchmark.log` and also emit one prefixed JSON record per measured row. `ci.benchmark_metrics` validates the complete 32-record v1 contract into immutable dataclasses before the publisher writes schema-v3 `latest.json`. Each record identifies its test, scenario, mode, methodology, exact nanosecond durations, mode-specific cache bytes, and sccache counters. A missing, duplicate, malformed, or unexpected record fails publication; unavailable cache size is `null`, never zero. Warm sccache speedup is reported only for a verified cache-hit run with no misses or non-cacheable requests. Historical schema-v2 reports and Markdown-based `ci.perf_guard` remain readable; new `history.jsonl` rows include `schema_version` and additive exact fields without rewriting old rows.

## Release Automation

- **Canonical workflow** - `.github/workflows/release-auto.yml` is the only supported release entrypoint
- **Workflow helper** - `ci/release_workflow.py` provides preflight checks, wheel assembly, and crates publish helpers for the release workflow only
- **Fast fail** - preflight checks PyPI and crates.io before any build fan-out and skips registry jobs that are already complete
- **Trigger** - push a tag matching the workspace version (`1.3.6` or `v1.3.6`)
- **Manual recovery** - `Run workflow` can leave `tag` empty; the workflow derives the current workspace version from the selected branch. Existing GitHub Releases are updated. A first same-version push stays quiet when that version is already released, while an unreleased version warns that it published nothing. Any rerun skips before bump/tag/release checks and cannot resume a prior release: use `workflow_dispatch` with the existing `tag` and `dry-run` inputs to resume PyPI/crates.io from the GitHub Release checkpoint.
- **PyPI** - use Trusted Publishing with GitHub environment `pypi` and workflow `.github/workflows/release-auto.yml`
- **crates.io** - add repository secret `CARGO_REGISTRY_TOKEN`
- **GitHub Release** - created automatically with standalone archives, installer scripts, and `SHA256SUMS`
- **Marketplace** - still manual in the GitHub UI; edit the generated GitHub release and check `Publish this action to the GitHub Marketplace`

## Hooks (`ci/hooks/`)

Claude Code and Codex hooks that enforce project conventions:

- **tool_guard.py** - PreToolUse: blocks bare `cargo`/`rustc`, legacy root trampolines, and `uv run cargo`/`uv run rustc` (must use `soldr`) and bare `python`/`pip` (must use `uv`)
- **lint.py** - PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** - PostToolUse: ensures every directory has a `README.md`
