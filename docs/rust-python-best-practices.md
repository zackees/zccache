# Best Practices for a Hybrid Rust + Python Repo

Memorializes the CI + repo-hygiene decisions made during the mid-2026 CI
consolidation effort (PR #832, PR #833, and the `ci/unified-macos-validation`
branch). Filed so future maintainers — and the next agent that lands here — can
see the *why* without re-deriving it from commit archaeology. Tracked by issue
[#835](https://github.com/zackees/zccache/issues/835).

**Context for reading this document:** zccache is a hybrid repo where the Rust
workspace is the product and Python tooling owns CI orchestration, packaging,
and agent hooks. The same checks need to run identically in GitHub Actions and
on a developer laptop.

**Status caveat:** PR #832 (the unified job-per-platform CI with a `./ci.sh`
gate dispatcher) was **closed without merging**. Main today runs the hook-based
setup described in [CLAUDE.md](../CLAUDE.md) plus per-concern workflow files.
Each rule below is annotated with its status: `on main` (implemented today),
or `target pattern` (validated on the unmerged branch; adopt it if/when the
consolidation is revived). The *decisions* stand either way — they encode
costs that were measured, not preferences.

## 1. LOC budget belongs in a lint gate, not only a per-edit hook

*Status: hook on main (`ci/hooks/loc_guard.py`); gate is the target pattern.*

**Rule.** A workspace-wide line-count budget (warn > 1,000, error > 1,500)
should run as a CI lint gate on every push, not only as a `PostToolUse` hook
scoped to the current agent session.

**Why.** A `PostToolUse` hook only fires when Claude/Codex makes an edit. A
`git push` from a developer terminal, a rebase, a generated-code drop — none of
those trigger the hook, so a file can grow past budget silently. A lint gate
walks the workspace on every CI run, so the budget is enforced regardless of
how the bytes got there.

**Split convention** (printed on every violation): `foo.rs` → `foo/mod.rs` +
per-domain files, `pub use` re-exports in `mod.rs` so public paths are
unchanged. Precedents: PRs #355–#363.

## 2. Linters must NOT trigger a full build

*Status: target pattern (load-bearing wherever `uv run` scripts exist).*

**Rule.** Routine lint / gate invocations use `uv run --no-project --script`
so the surrounding `pyproject.toml` is never loaded.

**Why.** With a maturin-backed Python project (zccache ships a Python wheel
built from Rust), a bare `uv run` walks up the tree, discovers
`pyproject.toml`, and triggers the maturin build *before* running anything. A
fmt-check that takes 200 ms blows up into a 5-minute cold build. Lint gates
only need a single-purpose venv, never a wheel install.

**Implementation.** PEP 723 inline-deps in the dispatcher script:

```python
#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml>=6"]
# ///
```

Both flags are load-bearing:

- `--no-project`: suppresses `pyproject.toml` discovery → no maturin build.
- `--script`: reads PEP 723 inline deps, provisions an isolated venv for them.

## 3. One bash wrapper for the dispatcher

*Status: target pattern (`./ci.sh` existed only on the unmerged branch).*

**Rule.** A single wrapper (`./ci.sh <gate>`) is the canonical invocation in
GHA workflows AND local dev — no copy-pasted
`uv run --no-project --script ci.py X` strings.

**Why.** The flag combo from rule 2 is non-obvious; documenting it once in a
wrapper means every consumer gets it right by construction. When uv's defaults
change, one file changes.

**Naming.** Use a `.sh` suffix to disambiguate from the `ci/` package
directory at the repo root; a bare `./ci` collides with the package on
case-insensitive filesystems.

## 4. Ban footgun command shapes at the PreToolUse choke point

*Status: on main for `cargo`/`rustc`/`python`/`pip` (`ci/hooks/tool_guard.py`);
the uv-run flag check was a TODO contingent on rules 2–3 landing.*

**Rule.** A `PreToolUse` hook reads the about-to-run command and refuses known
footgun shapes with the canonical replacement in the error message: bare
`cargo`/`rustc` (must use `soldr`), bare `python`/`pip` (must use `uv`), and —
once a gate dispatcher exists — `uv run` without `--no-project --script`.

**Why.** A wrapper only helps if it is actually used. The hook is the choke
point that catches an agent (or a developer copy-pasting from chat) quietly
invoking the expensive or wrong-toolchain path.

## 5. Reserve full `uv run` (with project) for explicit build entry points

*Status: on main by convention (`./test`, `ci/build_dist.py`).*

**Rule.** The maturin build IS necessary somewhere — running the test suite,
building the wheel for release, validating PyPI install. Those live in named
scripts that explicitly opt in to the full project context. Everything else
opts out (rule 2).

**Why.** Not every uv invocation is wrong; keeping the boundary explicit (one
place opts IN to the build, everything else opts OUT) makes the cost
predictable.

## 6. Workflow YAML is thin orchestration; logic lives in Python

*Status: target pattern; main still embeds multi-line shell in several
workflows.*

**Rule.** Every CI step should be a one-liner dispatching to a Python gate
(`run: ./ci.sh <gate>`); the logic lives in `ci/gates/<name>.py` with a
`run() -> int` signature.

**Why.** Embedded YAML shell is untestable, unlintable, and unreviewable.
Python gates are lintable (pyright/ruff understand `def run() -> int`),
testable against a fixture worktree, locally reproducible byte-for-byte, and
replaceable in isolation.

## 7. One runner per platform; continue-past-failure with a single fatal gate

*Status: target pattern (the core of closed PR #832).*

**Rule.** One GHA matrix entry per platform. Every gate runs even if earlier
ones fail, EXCEPT the `build` gate: if `cargo check` fails, every downstream
gate is noise, so the job halts and reports only `build`.

**Why.** The macOS runner pool is the wall-clock bottleneck on every PR.
Splitting fmt/clippy/dylint/build/test into separate matrix entries makes each
queue for its own runner slot AND pay the toolchain cold cost separately.
Folding them into one runner per platform amortizes both; continue-past-failure
shows a developer ALL failures in one CI cycle instead of fix-push-wait per
failure.

## 8. README guard with a minimum-size floor

*Status: presence check on main (`ci/hooks/readme_guard.py`); the 50-line
floor is the target refinement.*

**Rule.** Every directory has a `README.md`; a minimum-size floor (~50 lines)
prevents the presence check from being satisfied by one-line placeholders.

**Why this one IS a hook** (not a gate, deliberately): it fires at the moment
a new directory gets its first file, which is the right moment to demand the
README. A workspace walk would have to reconstruct which directories were
newly populated — exactly what the agent session context provides for free.

## 9. Hooks vs gates — the split

*Status: the decision rule; main currently implements the hook column.*

Anything that checks **repo state** is a CI lint gate (runs on every push).
Anything that checks **agent intent** at the moment of an action is a hook
(PreToolUse / PostToolUse, runs only during a Claude/Codex session).

| Concern | Home |
|---|---|
| File size budget (workspace-wide) | gate (`loc` gate; hook-only on main today) |
| README presence at dir creation | hook (`ci/hooks/readme_guard.py`) |
| Bare cargo / rustc / uv-flag command shape | hook (`ci/hooks/tool_guard.py`) |
| Workspace-wide fmt, clippy, dylint | gates (workflow steps on main today) |
| Session-start git fingerprint snapshot | hook (`ci/hooks/check-on-start.py`) |

Litmus test: if a rule would fire equally well on a `git push` from a plain
terminal, it's a gate. If it needs to know what tool is about to run, what
file was just written, or what session just started, it's a hook.

## 10. action.yml self-test is cheap, not a full re-build

*Status: target pattern.*

**Rule.** The composite-action contract (`action.yml` + cleanup action) is
validated by two cheap checks: parse the YAML and assert structural shape, and
run the just-built binary to confirm every subcommand the action's shell
snippets call is present in `--help`.

**Why.** A full `uses: ./` end-to-end rebuilds zccache from source per
platform — minutes per matrix entry. The contract downstream consumers of
`zackees/zccache@v1` actually depend on is "does the binary surface match what
action.yml calls?" — that is a <5 s test against the already-built binary.

## Disposition of the original TODO items

Issue #835 listed five TODO items (extend `tool_guard.py` with the uv-run flag
check, document the build entry-point list, add `tests/test_gates.py`, backfill
sub-floor READMEs, drop `--no-project` when uv changes defaults). All five were
contingent on the `ci/gates/` + `./ci.sh` architecture from PR #832, which was
closed without merging. They are recorded here as part of the target pattern
rather than tracked as open work; if the consolidation is revived, this
document is the spec to revive them from.
