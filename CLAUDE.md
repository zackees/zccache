# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

zccache is a local-first compiler cache (23 crates) for C/C++/Rust/Emscripten, inspired by sccache but optimized for warm-hit latency. Architecture: a persistent `zccache-daemon` holds an in-memory metadata cache and a filesystem watcher; the `zccache` CLI shells out per compile but talks to the daemon over a single length-prefixed IPC roundtrip (Unix sockets / Windows named pipes via `zccache-ipc`). Message bodies are **prost**; unset or `auto` resolves to `ProstV16`, while legacy bincode values are unsupported (#840). The shipped `zccache` binary is a multi-call binary defined in `crates/zccache`; `crates/zccache-cli` is **not** the CLI — it is the PyO3 `cdylib` hosting `zccache._native`, and the CLI subcommand surface lives in `zccache-cli-core`. The daemon is lazily started by the CLI when not running. See @docs/CLAUDE.md for which architecture doc to read based on what you're working on, and where to document new features.

> [!IMPORTANT]
> ## Performance work → read [PERF.md](PERF.md) FIRST
>
> **The local Linux Docker matrix in `ci/perf_local.py --matrix` is the sanctioned path for zccache perf work.**
> If you are testing, measuring, optimizing, or regressing zccache's performance —
> read **[PERF.md](PERF.md)** before doing anything else.
>
> GitHub Actions does not run the large wall-clock fixture matrix. Use narrow
> local cells while iterating, then run all eight cells with `--matrix` before merge.
>
> Do not invent ad-hoc benchmarks (`criterion`, `divan`, `hyperfine` in a one-off
> script). The local Docker matrix is the regression-blocking measurement; everything else
> is diagnostic.
>
> **When iterating on a perf problem: reproduce in local Docker first.** Retain
> `.perf-local/results/<fixture>/<scenario>/` as evidence and never use Actions
> as the timing feedback loop.
>
> **Every perf fix lands with a perf unit test.** Without a test, the bug comes
> back. Either extend `crates/zccache-daemon/tests/perf_bench_test.rs` + add a
> threshold row in `ci/perf_guard.py`, or add a `#[test]` `Duration` budget
> assertion in the crate where the regression lived. See PERF.md →
> "Preventing regressions — add a perf unit test."

## Essential Rules

- **Always use `soldr <tool>` directly** to execute Rust commands. Bare cargo/rustc, legacy root trampolines, and `uv run cargo` are blocked by hook. soldr resolves repo-local `.cargo` / `.rustup` homes and the rustup-managed toolchain pinned by `rust-toolchain.toml`.
- **Always use `uv` for Python.** Bare `python`/`pip` are blocked by hook. Use `uv run ...` or `uv pip ...`.
- MSRV: 1.95.0 | Edition: 2021 | Toolchain: 1.95.0 (clippy + rustfmt)
- CI: Linux, macOS, Windows. All warnings denied (`RUSTFLAGS="-D warnings"`)
- **Workflows get Rust from `zackees/setup-soldr`, never `dtolnay/rust-toolchain`.** soldr materializes the `rust-toolchain.toml` pin; dtolnay ignores it. Reuse the pinned setup-soldr SHA from current `main`, and never copy a setup-soldr block from a stale branch. Every setup-soldr call site sets `solo-toolchain-cache: false` (#1677 retired that archive), `cook-delta: false`, and global `save-cache: auto` with the main-push pre-prune permission in `save-cache-remote`, or explicit global `save-cache: "false"`. The published action owns local automatic saves; do not add ACT-only input overrides. Explicit global false stays read-only on every runner. A new cache family needs a budget before it lands (the repo already runs near the 10 GB quota, #1758). When the job tests something that owns `RUSTC_WRAPPER` (e.g. `test-action.yml`), also turn off `build-cache`, `target-cache` and `cargo-registry-cache`, set `prebuild-deps: none`, and reset the `ZCCACHE_CACHE_DIR`/`ZCCACHE_COMPILE_PRIORITY` values it exports. **After any workflow or action edit, run `uv run --no-project --with pyyaml python ci/check_cache_footprint.py`**; the `workflow_guard.py` PostToolUse hook also runs it on workflow edits (#1760). Existing dtolnay uses in `bench-action.yml`, `bench-fingerprint.yml` and `release-auto.yml` are migration debt, not precedent.
- **Put every new pull-request test in `.github/workflows/ci.yml`**, either as a CI job or in a reusable workflow called by CI. Do not create a new PR-triggered GitHub Actions workflow/category for a test. Preserve docs-only checks when changing CI path filters; existing standalone PR workflows are being consolidated in #1639.
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
soldr cargo bench -p zccache            # criterion benches live in crates/zccache/benches/
./perf.sh                   # performance benchmark (zccache vs sccache vs bare clang)
```

## Local gate (before every push)

When the user explicitly requests remote CI because the local host is busy or
Bosn cannot produce a valid receipt, push the committed branch without local
attestation and require the full remote PR checks before merge. Do not retry
heavy local lanes in that mode or claim a failed local gate passed. Apply the
`ci-full` label to the PR so prior attestations cannot skip remote checks.

```bash
uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run   # pin: ci/local_gate.py CI_LINT_REF
```

Runs `ci/local_gate.py` (lanes in `local-gate.toml`) on a clean committed tree and stamps HEAD with a tree-bound `Local-Gate:` trailer plus `Ci-Attestation:` trailers (`ci-attestations.yml`). The test suite runs only through `bosn ci run` replaying Integration and MSRV, never on the host: the nextest guard refuses outside CI or `ZCCACHE_TEST_ISOLATED=1`. CI's `ci-mode` jobs then skip the Formatting, Documentation, MSRV and Integration (Linux) jobs for an attested, in-policy PR head; `main` pushes always run them. Add a Formatting check to the `lint` lane, never as a workflow step (GATE-001), and keep the recorded workflow selection and required step evidence in step with Integration/MSRV (`ci/tests/test_local_gate.py`).

## Temporary dependent subgit integration

When a cross-repository migration needs an unreleased dependency capability,
keep the editable dependency as a nested git checkout at
`_vender/<dependency>/`; do not copy its source into this repository and do
not leave a host-specific absolute path in a release branch.

1. Ask the coordinating owner before creating or repointing the checkout, so
   every consumer resolves the same edited source.
2. Clone the dependency into `_vender/<dependency>/` as its own git worktree
   on the producer's migration branch. Keep its origin and branch visible in
   that checkout; zccache does not commit the dependency's source changes.
3. During integration only, add a root `[patch]` or exact workspace dependency
   path that resolves the named package from `_vender/<dependency>/...`.
   Enable only the capability features the migrated call sites use.
4. Record the required producer revision and the temporary override in the
   migration handoff. Make contract tests cover the frozen product behavior
   before changing a facade call site.
5. Before a zccache release branch, remove every local path/patch override,
   replace it with the producer's exact published version, regenerate the
   lockfile through the normal validation pass, and verify no `_vender` path
   remains in manifests or the resolved release package.

`_vender` is intentionally spelled this way for migration tooling. It is a
temporary integration boundary, not a vendored-source ownership transfer.

See [PERF.md](PERF.md) for the scenario-driven local Docker gate (cold-tar-untar-warm and friends).

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

- **Automation**: `.github/workflows/release-auto.yml` is the only supported release entrypoint. It validates release metadata, fails fast when the current version is already fully published on PyPI/crates.io, builds wheel/release artifacts, publishes PyPI wheels, publishes Rust crates, and creates the GitHub release.
- **Helper module**: `ci/release_workflow.py` contains workflow-only Python helpers for preflight checks, wheel assembly, and crates.io publish order. It does not dispatch other GitHub workflows.
- **Three trigger paths**, all converge on the same publish pipeline:
  1. **Auto on push-to-main** (the everyday path): the `detect-bump` job reads `[workspace.package].version` from `Cargo.toml`, compares it to the prior commit's version, and proceeds iff the version was bumped. Merge a `chore(release): bump … -> X.Y.Z` PR and the release ships on its own.
  2. **Tag push**: push `1.3.0` or `v1.3.0`; the workflow normalizes the tag and requires it to match `[workspace.package].version` in `Cargo.toml`.
  3. **`workflow_dispatch`**: optionally accepts a `tag` input. Leave it empty to use the current workspace version from the selected branch; prefers an existing matching tag and fails early if that version is already fully published.
- **Recovery after a failed release — re-dispatch is safe, and it resumes.** A failed release run publishes **nothing**: every publish job gates on `always() && <upstream>.result == 'success'`, where `always()` only makes the condition evaluable and the explicit `result` checks still hold the gate. Matrix jobs (`build`, `test-wheels`) report `success` only when every leg passed, so one bad target blocks the whole publish phase. Verified by v1.13.6: the `aarch64-pc-windows-msvc` build failed and `Publish GitHub Release`, `Build PyPI Wheels`, `Test PyPI Wheel`, `Publish PyPI`, and `Publish crates.io` all reported `skipped`.
  - **Auto-release does not retry on its own.** `detect-bump` proceeds only when the version *changed* relative to the parent commit, so once a bump commit's run has failed, every later push correctly skips and reports success. Fixing the underlying bug does **not** re-trigger the release — recovery is always a manual `workflow_dispatch`. A first same-version push stays quiet when the version already has a GitHub Release, but warns if it is unreleased. Any rerun (`github.run_attempt > 1`) skips before bump/tag/release checks, warns that it published nothing, and points to the existing `tag` / `dry-run` inputs for recovery. (#1472, #1537.)
  - **`dry-run` defaults to `true`.** A dispatch that accepts the defaults builds, packages, and wheel-tests every target and publishes nothing — including the GitHub Release, whose publish step is itself `if: inputs['dry-run'] != true`. That is the rehearsal: use it to prove a fix before shipping. **Set `dry-run: false` to actually publish.**
  - **Re-dispatch resumes; it does not republish.** `preflight` reads PyPI and crates.io and exports `pypi_complete` / `crates_complete`; `publish-pypi` and `publish-crates` skip whatever is already live, and `publish-crates` also accepts an already-complete PyPI in place of a fresh `publish-pypi`. `detect-bump` short-circuits to `should_release=true` on `workflow_dispatch`, so a manual run proceeds even when the GitHub Release/tag already exists. A partially published version therefore recovers by dispatching again — it lands on the missing registry only.
  - **The one irreversible boundary** is inside the publish phase, which is sequential (GitHub Release → PyPI → crates.io). "Nothing publishes on failure" is exact for any failure *before* that phase. If `publish-pypi` succeeds and `publish-crates` then fails, the PyPI version is live and cannot be reused — recovery resumes at crates.io rather than bumping. Treat a green `dry-run` as the checkpoint before crossing this line.
- **PyPI setup**: Prefer Trusted Publishing. Configure PyPI to trust repo `zackees/zccache`, workflow `.github/workflows/release-auto.yml`, environment `pypi`.
- **crates.io setup**: Add GitHub Actions secret `CARGO_REGISTRY_TOKEN` from https://crates.io/me.
- **Marketplace**: GitHub Marketplace publishing is not API-automated. After the workflow creates the GitHub release, open that release in GitHub, select `Publish this action to the GitHub Marketplace`, choose categories, and publish.

## Hooks (enforced automatically)

Hooks are in `ci/hooks/` (Python) and `crates/zccache-ci` (Rust):

- **PreToolUse**: `ci/hooks/tool_guard.py` blocks bare Rust commands (must use `soldr`) and bare `python`/`pip` (must use `uv`)
- **PostToolUse**: `ci/hooks/readme_guard.py` errors if directory lacks README.md
- **PostToolUse** (Edit|Write and Bash): `ci/hooks/workflow_guard.py` runs `ci/check_cache_footprint.py` whenever a workflow, composite action, `action.yml` or the cache planner is edited or dirty, and fails the call on a cache-footprint violation (#1760)
- **PostToolUse**: `ci/hooks/loc_guard.py` warns when an edited source file exceeds 1,000 LOC and hard-blocks (exit 2) above 1,500 LOC — split into focused submodules before the file crosses the threshold
- **SessionStart**: `ci/hooks/check-on-start.py` captures git fingerprint
- **Not registered by default:** the `ci/hooks/lint.py` format/clippy pass and the `zccache-ci` Stop hook were removed from `.claude/settings.json` (#309, Windows console pop-ups). Run `./lint` and `soldr cargo clippy --workspace --all-targets -- -D warnings` yourself before reporting Rust changes done, or re-enable them in a gitignored `.claude/settings.local.json`

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
- **`zccache cc` / `zccache c++` are stable public surface.** These wrapper-mode subcommands (issue #391) are how soldr's default-on `CC`/`CXX` injection routes `cc-rs` build-script work through the managed cache (soldr#310). Treat them like `RUSTC_WRAPPER`: argv shape, exit-code contract, and stdout/stderr behavior are part of the contract.
- **Daemon-unavailable is a hard error, exit code 125 (#1170).** When the wrapper cannot reach a daemon *before* dispatch it refuses to run the tool and exits **125** — the git/env/docker convention for "the wrapper could not run the command", deliberately outside the 1/2 a compiler uses for diagnostics, so CI can classify an infrastructure failure from the code alone. It emits `zccache[err][D]:` on stderr and a durable `wrapper-daemon-unavailable` lifecycle event. There is no silent uncached fallback: it exited 0 whenever the tool happened to succeed, which turned a daemon outage into a green build, and read-only hardlinked artifacts (#1038/#1039) made the bypass unsafe anyway. The only sanctioned bypasses are the explicit, opt-in `ZCCACHE_DISABLE=1` and `ZCCACHE_PROBE_BYPASS`.
- **Zero extra roundtrips.** Never add a separate handshake, version check, or metadata query that requires its own IPC roundtrip. Piggyback on existing messages instead. Example: protocol version is embedded in every message frame, not fetched via a separate Status request. If you need new metadata exchanged between CLI and daemon, add it to the framing layer or to an existing request/response - never introduce a new preliminary exchange.
- **Avoid gratuitous `clone()`.** Do not clone to placate the borrow checker - restructure code instead. Prefer: moves over clones for single-use values, `&str`/`&Path` over owned types in function signatures that only read, `Arc::clone(&x)` over cloning the inner data then wrapping. Cloning is acceptable when data genuinely needs to exist in two places (e.g., inserting into a map while retaining a copy, or moving into a spawned task). Every `clone()` on a `Vec`, `String`, or `PathBuf` should be justified - if you can't explain why both the original and the copy are needed, eliminate it.
- **No source file over 1,000 LOC.** Enforced by the `loc_guard.py` PostToolUse hook (warns >1K, blocks >1.5K). The split pattern is "convert `foo.rs` → `foo/mod.rs` + per-domain files alongside, with tests in a `tests/` subdirectory". PRs #355–#363 are the precedents (server.rs, cli/main.rs, perf_bench_test.rs, compiler/lib.rs, server/{tests,mod}.rs, compile_journal.rs, depgraph/snapshot.rs). Re-export `pub` items from `mod.rs` so the public path is unchanged.
- **Preserve cache-file mtime on hits — never stamp `now()`.** Materializing a cached artifact (`write_cached_output`, `write_cached_file`, `write_cached_payload`, `write_payloads_par`) preserves the cache file's stored mtime by default. **Preservation is the fast path** — zero extra syscalls and no cargo-fingerprint regression. The hardlink fast path already inherits the cache mtime; never add `set_file_mtime(_, now())` after the link. **Why:** cargo's incremental fingerprint records the artifact's mtime at first compile and treats a later "newer" mtime as evidence the artifact was externally modified — invalidating the downstream graph and paying re-link / re-fingerprint cost that fully cancels the cache savings. Measured in iter7 of the cold-tar-untar-warm OODA loop: making the per-file mtime touch a no-op cut per-hit overhead from 5.9 ms to 2.8 ms and recovered the bin-caching win (warm 11.6 s → 9.8 s on the same code). **The single allowed exception is the sibling-floor refinement** in `resolve_hit_mtime` (`persist/mtime.rs`, issues #466 / #467): when a cache hit lands next to an *existing* sibling artifact in `target/debug/deps/` whose mtime is already higher (e.g. from out-of-order materialization or parallel cache stores), the artifact's mtime is floored UP to that sibling max so cargo's "dep_mtime ≤ my_mtime" check doesn't misfire and recompile 30+ crates. The floor only ever picks a *stable sibling-derived value*, never `now()` — preserving the iter7 invariant against the fingerprint regression. In isolation (no siblings, or all siblings older) the floor is a no-op and preservation wins. `zccache-core`'s `mtime` module is the single owner of every materialized-output mtime decision (#1771: one mtime across LINK/REFLINK/COPY; a workspace guard test fails on a file-time write anywhere else unless allowlisted; daemon-core re-exports it via `persist/mtime.rs`), so the rule is greppable; if a future cc/cpp consumer needs `mtime = now()` semantics for make/ninja, gate it on the consumer rather than re-globalizing the behavior. Disable both behaviours with `ZCCACHE_DISABLE_MTIME_FLOOR=1` if a specific build system needs strict preservation only. **Known open exception — do not "fix" without measuring (#1158):** the *batch* materializer (`write_payloads_par_*` → `apply_batch_policy`, the `RustcInputFloor` policy) seeds its floor with `now()`, so it stamps every materialized output to ~now() rather than flooring up to a stable sibling. That contradicts the rule above, but it is what a closed regression test asserts — #599 measured the opposite failure (preserve an old mtime on a rustc hit → cargo records a stale output → the *next* no-op build recompiles, 14× slower "warm (target intact)"). The two findings cannot both be fully right for cargo, and the difference (~0.44 s per `medium` warm build) is well under the ~10 s run-to-run noise of a 4-CPU Docker VM, so it needs `--matrix --repeat 5` on a quiet box to settle. See the comment at the `apply_batch_policy` call site and #1158 for the exact A/B. If you resolve it, gate the `now()` seed on the *consumer* per the sentence above — do not re-globalize either behaviour.

## Core Principles

- Simplicity first. Minimal code impact. No over-engineering.
- No laziness. Root causes only. Senior developer standards.
- Speed above all. Ship fast, capture failures in unit tests, fix as they arise.
- Plan non-trivial work in `tasks/todo.md`. Capture lessons in `tasks/lessons.md`.
