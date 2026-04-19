# CI & Development Tools

Python scripts for development tooling. Project-root trampolines (`_cargo`, `_rustc`, `_rustfmt`) prepend the shared rustup proxy location to PATH so the pinned toolchain is always used.

## Top-level scripts

- **`./_cargo`** — Execute cargo with the correct Rust toolchain (via trampoline PATH normalization)
- **`./_rustc`** — Execute rustc with the correct toolchain
- **`./_rustfmt`** — Execute rustfmt with the correct toolchain
- **`./lint`** — Workspace linting (rustfmt + clippy), supports single-file mode
- **`./test`** — Workspace tests, supports per-crate filtering
- **`./perf`** — Performance benchmarks (zccache vs sccache vs bare clang)

## Release Automation

- **`./publish`** â€” Local release operator path; can also build wheels from pre-downloaded `binaries-*` artifacts via `--artifact-download-dir`
- **Canonical workflow** â€” `.github/workflows/release.yml`
- **Trigger** â€” push a tag matching the workspace version (`1.2.15` or `v1.2.15`)
- **Manual runs** â€” `Run workflow` can leave `tag` empty; the workflow derives the current workspace version from the selected branch and fails if that version already has a published GitHub release
- **PyPI** â€” use Trusted Publishing with GitHub environment `pypi` and workflow `.github/workflows/release.yml`
- **crates.io** â€” add repository secret `CARGO_REGISTRY_TOKEN`
- **GitHub Release** â€” created automatically with standalone archives, installer scripts, and `SHA256SUMS`
- **Marketplace** â€” still manual in the GitHub UI; edit the generated GitHub release and check `Publish this action to the GitHub Marketplace`

## Hooks (`ci/hooks/`)

Claude Code hooks that enforce project conventions:

- **tool_guard.py** — PreToolUse: blocks bare `cargo`/`rustc` and `uv run cargo`/`uv run rustc` (must use trampolines) and bare `python`/`pip` (must use `uv`)
- **lint.py** — PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** — PostToolUse: ensures every directory has a `README.md`
