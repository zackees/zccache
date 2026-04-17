# CLAUDE.md

zccache is a local-first compiler cache daemon (11 crates). See @docs/CLAUDE.md for which architecture doc to read based on what you're working on, and where to document new features.

## Essential Rules

- **Always use the project-root trampolines** (`_cargo`, `_rustc`, `_rustfmt`) or `soldr <tool>` directly to execute Rust commands. Bare cargo/rustc and `uv run cargo` are blocked by hook. Both forms dispatch through [soldr](https://github.com/zackees/soldr), which resolves each tool via `rustup which` so the rustup-managed toolchain is always used; the cargo path passes `--no-cache` so the previous bare-cargo semantics are preserved. Usage: `./_cargo check --workspace` or `soldr cargo check --workspace`.
- **Always use `uv` for Python.** Bare `python`/`pip` are blocked by hook. Use `uv run ...` or `uv pip ...`.
- MSRV: 1.75 | Edition: 2021 | Toolchain: stable (clippy + rustfmt)
- CI: Linux, macOS, Windows. All warnings denied (`RUSTFLAGS="-D warnings"`)
- Every directory with files must have a README.md (enforced by hook)

## Commands

```bash
./test                      # unit tests only (fast, no compiler needed)
./test --integration        # integration tests only (need clang on PATH)
./test --full               # unit + integration + stress + perf tests
./test -p <crate> -- <test_name>
./_cargo check --workspace --all-targets
./_cargo clippy --workspace --all-targets -- -D warnings
./_cargo fmt --all
RUSTDOCFLAGS="-D warnings" ./_cargo doc --workspace --no-deps
./_cargo bench -p zccache-hash
./perf                      # performance benchmark (zccache vs sccache vs bare clang)
```

## Distribution

Native binaries are built via GitHub Actions and downloaded locally for packaging. PyPI is the distribution channel — no Python in the runtime hot path.

```bash
# Build all platforms (triggers GH Actions, waits, downloads to dist/)
uv run python ci/build_dist.py --ref main

# Download from a specific run
uv run python ci/build_dist.py --run-id <run_id>

# Re-download latest successful build (no new build)
uv run python ci/build_dist.py --skip-build
```

- **Workflow**: `.github/workflows/build.yml` (workflow_dispatch, 6 targets)
- **Script**: `ci/build_dist.py` — orchestrates `gh` CLI to trigger, wait, download, organize
- **Output**: `dist/` with per-platform subdirs + `manifest.json` (gitignored)
- **Targets**: linux-x86_64, linux-aarch64, macos-x86_64, macos-aarch64, windows-x86_64, windows-arm64

### Publishing

```bash
./publish     # pre-check PyPI → build → download → wheel → upload
```

- **Script**: `./publish` — zero-arg PEP 723 script (runs via `uv run --script`)
- **Pre-check**: Fails fast if version from `pyproject.toml` already exists on PyPI
- **Pipeline**: Triggers GH Actions build → waits → downloads artifacts → builds platform wheels → uploads via `uv publish`
- **Auth**: `UV_PUBLISH_TOKEN`, `~/.pypirc`, or interactive prompt

## Hooks (enforced automatically)

Hooks are in `ci/hooks/` (Python) and `crates/zccache-ci` (Rust):

- **PreToolUse**: `ci/hooks/tool_guard.py` blocks bare Rust commands (must use `uv run`) and bare `python`/`pip` (must use `uv`)
- **PostToolUse**: `ci/hooks/lint.py` auto-formats + runs clippy on edited .rs files
- **PostToolUse**: `ci/hooks/readme_guard.py` errors if directory lacks README.md
- **SessionStart**: `ci/hooks/check-on-start.py` captures git fingerprint
- **Stop**: `cargo run -p zccache-ci` runs lint + unit tests in parallel (skips if no changes)

## Language Policy

- **Python is only for CI scripts, packaging, and hooks.** All tests, benchmarks, and application logic must be written in Rust.
- Trampolines (`_cargo`, `_rustc`, `_rustfmt`) are required for Rust commands because hooks enforce it — they prepend the shared rustup proxy location to PATH ensuring the pinned toolchain. This is not an endorsement of Python for project code.
- When in doubt, write it in Rust.

## Development Philosophy: TDD

- **Red → Green → Refactor.** Write failing tests first, then implement the minimum code to make them pass, then refactor.
- Tests are the spec. If the test suite passes, the feature works. If behavior isn't tested, it doesn't exist.
- Comprehensive tests over comprehensive docs. Tests are executable documentation.
- Test real behavior: use `tempfile` for filesystem tests, not mocks. Test the contract, not the implementation.

## Conventions

- **Timing: always use nanoseconds.** All internal timing fields, variables, and phase profiling use `_ns` suffix and `as_nanos()`. Display code converts to human-readable units (ns/us/ms/s). Never use `as_micros()`.
- **Protocol version bump required on wire format changes.** When changing `Request`, `Response`, or any struct serialized over IPC, bump `PROTOCOL_VERSION` in `zccache-protocol`. See DD-018.
- **Zero extra roundtrips.** Never add a separate handshake, version check, or metadata query that requires its own IPC roundtrip. Piggyback on existing messages instead. Example: protocol version is embedded in every message frame, not fetched via a separate Status request. If you need new metadata exchanged between CLI and daemon, add it to the framing layer or to an existing request/response — never introduce a new preliminary exchange.
- **Avoid gratuitous `clone()`.** Do not clone to placate the borrow checker — restructure code instead. Prefer: moves over clones for single-use values, `&str`/`&Path` over owned types in function signatures that only read, `Arc::clone(&x)` over cloning the inner data then wrapping. Cloning is acceptable when data genuinely needs to exist in two places (e.g., inserting into a map while retaining a copy, or moving into a spawned task). Every `clone()` on a `Vec`, `String`, or `PathBuf` should be justified — if you can't explain why both the original and the copy are needed, eliminate it.

## Core Principles

- Simplicity first. Minimal code impact. No over-engineering.
- No laziness. Root causes only. Senior developer standards.
- Speed above all. Ship fast, capture failures in unit tests, fix as they arise.
- Plan non-trivial work in `tasks/todo.md`. Capture lessons in `tasks/lessons.md`.
