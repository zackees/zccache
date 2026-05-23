# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

zccache is a local-first compiler cache (21 crates) for C/C++/Rust/Emscripten, inspired by sccache but optimized for warm-hit latency. Architecture: a persistent `zccache-daemon` holds an in-memory metadata cache and a filesystem watcher; the `zccache` CLI (binary in `zccache-cli`) shells out per compile but talks to the daemon over a single length-prefixed bincode IPC roundtrip (Unix sockets / Windows named pipes via `zccache-ipc`). The daemon is lazily started by the CLI when not running. See @docs/CLAUDE.md for which architecture doc to read based on what you're working on, and where to document new features.

> [!IMPORTANT]
> ## Performance work → read [PERF.md](PERF.md) FIRST
>
> **The `perf-rust-cluster` GitHub Action is the only sanctioned path for zccache perf work.**
> If you are testing, measuring, optimizing, or regressing zccache's performance —
> read **[PERF.md](PERF.md)** before doing anything else.
>
> Branch naming (`perf/<plat>-<fix>-<scen>`) controls exactly which cells of the matrix
> run. Pushing to a wrong branch name silently runs the full sweep — costly and slow.
> **Do not guess.** PERF.md has the complete 48-pattern scope table.
>
> Do not invent ad-hoc benchmarks (`criterion`, `divan`, `hyperfine` in a one-off
> script). The perf cluster is the regression-blocking measurement; everything else
> is diagnostic.
>
> **When iterating on a perf problem: reproduce locally first.** One GHA cycle is
> 5–17 minutes; one local cycle is seconds. Use the cluster to confirm a fix you
> already validated locally — not to test hypotheses you haven't tried yet. See
> PERF.md → "Iterating on a perf problem — local-first, GHA last."
>
> **Every perf fix lands with a perf unit test.** Without a test, the bug comes
> back. Either extend `crates/zccache-daemon/tests/perf_bench_test.rs` + add a
> threshold row in `ci/perf_guard.py`, or add a `#[test]` `Duration` budget
> assertion in the crate where the regression lived. See PERF.md →
> "Preventing regressions — add a perf unit test."

## Essential Rules

- **Always use `soldr <tool>` directly** to execute Rust commands. Bare cargo/rustc, legacy root trampolines, and `uv run cargo` are blocked by hook. soldr resolves repo-local `.cargo` / `.rustup` homes and the rustup-managed toolchain pinned by `rust-toolchain.toml`.
- **Always use `uv` for Python.** Bare `python`/`pip` are blocked by hook. Use `uv run ...` or `uv pip ...`.
- MSRV: 1.94.1 | Edition: 2021 | Toolchain: 1.94.1 (clippy + rustfmt)
- CI: Linux, macOS, Windows. All warnings denied (`RUSTFLAGS="-D warnings"`)
- Every directory with files must have a README.md (enforced by hook)

## Commands

```bash
./test                      # unit tests only (fast, no compiler needed)
./test --integration        # integration tests only (need clang on PATH)
./test --full               # unit + integration + stress + perf tests
./test -p <crate> -- <test_name>
soldr cargo check --workspace --all-targets
soldr cargo clippy --workspace --all-targets -- -D warnings
soldr cargo fmt --all
RUSTDOCFLAGS="-D warnings" soldr cargo doc --workspace --no-deps
soldr cargo bench -p zccache-hash
./perf.sh                   # performance benchmark (zccache vs sccache vs bare clang)
```

See [PERF.md](PERF.md) for the scenario-driven `perf-rust-cluster.yml` workflow (cold-tar-untar-warm and friends).

## Distribution

Native binaries are built via GitHub Actions and downloaded locally for packaging. PyPI is the distribution channel - no Python in the runtime hot path.

```bash
# Build all platforms (triggers GH Actions, waits, downloads to dist/)
uv run python ci/build_dist.py --ref main

# Download from a specific run
uv run python ci/build_dist.py --run-id <run_id>

# Re-download latest successful build (no new build)
uv run python ci/build_dist.py --skip-build
```

- **Workflow**: `.github/workflows/build.yml` (workflow_dispatch, 8 targets)
- **Script**: `ci/build_dist.py` - orchestrates `gh` CLI to trigger, wait, download, organize
- **Output**: `dist/` with per-platform subdirs + `manifest.json` (gitignored)
- **Targets**: linux-x86_64, linux-aarch64, macos-x86_64, macos-aarch64, windows-x86_64, windows-arm64

### Publishing

- **Automation**: `.github/workflows/release.yml` is the only supported release entrypoint. It validates release metadata, fails fast when the current version is already fully published on PyPI/crates.io, builds wheel/release artifacts, publishes PyPI wheels, publishes Rust crates, and creates the GitHub release.
- **Helper module**: `ci/release_workflow.py` contains workflow-only Python helpers for preflight checks, wheel assembly, and crates.io publish order. It does not dispatch other GitHub workflows.
- **Tag rule**: Push `1.3.0` or `v1.3.0`; the workflow normalizes the tag and requires it to match `[workspace.package].version` in `Cargo.toml`.
- **Manual runs**: `Run workflow` may leave `tag` empty. The workflow then uses the current workspace version from the selected branch, prefers an existing matching tag, and fails early if that version already has a published GitHub release.
- **PyPI setup**: Prefer Trusted Publishing. Configure PyPI to trust repo `zackees/zccache`, workflow `.github/workflows/release.yml`, environment `pypi`.
- **crates.io setup**: Add GitHub Actions secret `CARGO_REGISTRY_TOKEN` from https://crates.io/me.
- **Marketplace**: GitHub Marketplace publishing is not API-automated. After the workflow creates the GitHub release, open that release in GitHub, select `Publish this action to the GitHub Marketplace`, choose categories, and publish.

## Hooks (enforced automatically)

Hooks are in `ci/hooks/` (Python) and `crates/zccache-ci` (Rust):

- **PreToolUse**: `ci/hooks/tool_guard.py` blocks bare Rust commands (must use `soldr`) and bare `python`/`pip` (must use `uv`)
- **PostToolUse**: `ci/hooks/lint.py` auto-formats + runs clippy on edited `.rs` files
- **PostToolUse**: `ci/hooks/readme_guard.py` errors if directory lacks README.md
- **PostToolUse**: `ci/hooks/loc_guard.py` warns when an edited source file exceeds 1,000 LOC and hard-blocks (exit 2) above 1,500 LOC — split into focused submodules before the file crosses the threshold
- **SessionStart**: `ci/hooks/check-on-start.py` captures git fingerprint
- **Stop**: `soldr cargo run -p zccache-ci` runs lint + unit tests in parallel (skips if no changes)

## Language Policy

- **Python is only for CI scripts, packaging, and hooks.** All tests, benchmarks, and application logic must be written in Rust.
- soldr is required for Rust commands because hooks enforce it and soldr owns toolchain discovery. This is not an endorsement of Python for project code.
- When in doubt, write it in Rust.

## Development Philosophy: TDD

- **Red -> Green -> Refactor.** Write failing tests first, then implement the minimum code to make them pass, then refactor.
- Tests are the spec. If the test suite passes, the feature works. If behavior isn't tested, it doesn't exist.
- Comprehensive tests over comprehensive docs. Tests are executable documentation.
- Test real behavior: use `tempfile` for filesystem tests, not mocks. Test the contract, not the implementation.

## Conventions

- **Timing: always use nanoseconds.** All internal timing fields, variables, and phase profiling use `_ns` suffix and `as_nanos()`. Display code converts to human-readable units (ns/us/ms/s). Never use `as_micros()`.
- **Protocol version bump required on wire format changes.** When changing `Request`, `Response`, or any struct serialized over IPC, bump `PROTOCOL_VERSION` in `zccache-protocol`. See DD-018.
- **Zero extra roundtrips.** Never add a separate handshake, version check, or metadata query that requires its own IPC roundtrip. Piggyback on existing messages instead. Example: protocol version is embedded in every message frame, not fetched via a separate Status request. If you need new metadata exchanged between CLI and daemon, add it to the framing layer or to an existing request/response - never introduce a new preliminary exchange.
- **Avoid gratuitous `clone()`.** Do not clone to placate the borrow checker - restructure code instead. Prefer: moves over clones for single-use values, `&str`/`&Path` over owned types in function signatures that only read, `Arc::clone(&x)` over cloning the inner data then wrapping. Cloning is acceptable when data genuinely needs to exist in two places (e.g., inserting into a map while retaining a copy, or moving into a spawned task). Every `clone()` on a `Vec`, `String`, or `PathBuf` should be justified - if you can't explain why both the original and the copy are needed, eliminate it.
- **No source file over 1,000 LOC.** Enforced by the `loc_guard.py` PostToolUse hook (warns >1K, blocks >1.5K). The split pattern is "convert `foo.rs` → `foo/mod.rs` + per-domain files alongside, with tests in a `tests/` subdirectory". PRs #355–#363 are the precedents (server.rs, cli/main.rs, perf_bench_test.rs, compiler/lib.rs, server/{tests,mod}.rs, compile_journal.rs, depgraph/snapshot.rs). Re-export `pub` items from `mod.rs` so the public path is unchanged.
- **Preserve cache-file mtime on hits — never stamp `now()`.** Materializing a cached artifact (`write_cached_output`, `write_cached_file`, `write_cached_payload`, `write_payloads_par`) must leave the resulting file's mtime equal to the cache file's stored mtime. The hardlink fast path already inherits the cache mtime; do not add a `touch_mtime` / `set_file_mtime(_, now())` after the link. **Why:** cargo's incremental fingerprint records the artifact's mtime at first compile and treats a later "newer" mtime as evidence the artifact was externally modified — invalidating the downstream graph and paying re-link / re-fingerprint cost that fully cancels the cache savings. Measured in iter7 of the cold-tar-untar-warm OODA loop: switching `touch_mtime` to a no-op cut per-hit overhead from 5.9 ms to 2.8 ms and recovered the bin-caching win (warm 11.6 s → 9.8 s on the same code). The named `touch_mtime` seam is kept as a marker so the rule is greppable; if a future cc/cpp consumer needs `mtime = now()` semantics for make/ninja, gate it on the consumer rather than re-globalizing the behavior.

## Core Principles

- Simplicity first. Minimal code impact. No over-engineering.
- No laziness. Root causes only. Senior developer standards.
- Speed above all. Ship fast, capture failures in unit tests, fix as they arise.
- Plan non-trivial work in `tasks/todo.md`. Capture lessons in `tasks/lessons.md`.
