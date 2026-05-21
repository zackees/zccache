# CI & Development Tools

Python scripts for development tooling. Rust commands go through `soldr <tool>` so repo-local toolchain homes and the pinned rustup-managed toolchain are resolved in one place.

## Top-Level Scripts

- **`./lint`** - Workspace linting (rustfmt + clippy), supports single-file mode
- **`./test`** - Workspace tests, supports per-crate filtering
- **`./perf.sh`** - Performance benchmarks (zccache vs sccache vs bare clang)
- **`python -m ci.benchmark_stats`** - Generates `index.html`, `latest.json`, and per-language benchmark JPGs from perf output for the published benchmark report
- **`python -m ci.perf_guard`** - Fails CI when Rust, C, or C++ zccache benchmark rows fall below the bare-compiler or pinned-sccache speed floors

## Release Automation

- **Canonical workflow** - `.github/workflows/release-auto.yml` is the only supported release entrypoint
- **Workflow helper** - `ci/release_workflow.py` provides preflight checks, wheel assembly, and crates publish helpers for the release workflow only
- **Fast fail** - preflight checks PyPI and crates.io before any build fan-out and stops if the current version is already published
- **Trigger** - push a tag matching the workspace version (`1.3.6` or `v1.3.6`)
- **Manual runs** - `Run workflow` can leave `tag` empty; the workflow derives the current workspace version from the selected branch and fails if that version already has a published GitHub release
- **PyPI** - use Trusted Publishing with GitHub environment `pypi` and workflow `.github/workflows/release-auto.yml`
- **crates.io** - add repository secret `CARGO_REGISTRY_TOKEN`
- **GitHub Release** - created automatically with standalone archives, installer scripts, and `SHA256SUMS`
- **Marketplace** - still manual in the GitHub UI; edit the generated GitHub release and check `Publish this action to the GitHub Marketplace`

## Hooks (`ci/hooks/`)

Claude Code and Codex hooks that enforce project conventions:

- **tool_guard.py** - PreToolUse: blocks bare `cargo`/`rustc`, legacy root trampolines, and `uv run cargo`/`uv run rustc` (must use `soldr`) and bare `python`/`pip` (must use `uv`)
- **lint.py** - PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** - PostToolUse: ensures every directory has a `README.md`
