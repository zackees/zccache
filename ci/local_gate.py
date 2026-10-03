#!/usr/bin/env python3
"""zccache's local gate (zackees/ci.yml#166, GATE-001..010).

One command that is a superset of the remote Linux quick gate, so a PR passes
CI on its first push and an attested head can skip the remote jobs it already
ran (local-gate.toml [gate.trust], ci-attestations.yml):

    uv run --no-project --python 3.13 python ci/local_gate.py             # every lane
    uv run --no-project --python 3.13 python ci/local_gate.py --lane lint # = the remote Formatting job
    uv run --no-project --python 3.13 python ci/local_gate.py --list

Do not call it directly before pushing; call it through the attesting
wrapper, which runs it on a clean tree and stamps HEAD with a tree-bound
`Local-Gate:` trailer plus one `Ci-Attestation:` trailer per proven gate:

    uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run

Lanes (GATE-007: each is cached on its own input set):

- `lint`: every check the remote `Formatting` job (ci.yml `fmt`) runs. That
  job runs exactly `ci/local_gate.py --lane lint` and nothing else
  (GATE-001), so this list is the single source of truth for it.
- `py-tests`: the `ci/tests` pytest suite (python-tests.yml `ci/tests`).
- `check`: `soldr cargo check --workspace` on the pinned toolchain, which is
  the MSRV (ci.yml `msrv`).
- `docs`: rustdoc with warnings denied (ci.yml `docs`).
- `tests`: zccache's own test suite -- the Integration (Linux) job's
  commands and the MSRV job's nested Dylint cache contract -- in bosn's
  isolated container (`bosn run --task gate-test`), never on the host
  (GATE-005): zccache is the live compiler cache of every soldr build here.

Remote-only (not attested): native macOS and Windows runs, the btrfs reflink
e2e, the miss-overhead timing budget, Dylint, and every other workflow.

Checks in a lane run in parallel; each one's output is shown only when it
fails, then a timing table. Exit 1 when any check fails.
"""

from __future__ import annotations

import argparse
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# The zackees/ci.yml commit whose ci_lint this repository uses. The `ci-mode`
# jobs in ci.yml and integration.yml check out the same SHA;
# ci/tests/test_local_gate.py keeps them in step.
CI_LINT_REF = "7edeb8dc318c1530ee3bf770029cb2e8b639e6f5"
LANES = ("lint", "py-tests", "check", "docs", "tests")
PY = ("uv", "run", "--no-project", "--python", "3.13")
# ci.yml's and integration.yml's workflow-level env: warnings are errors. It
# also replaces .cargo/config.toml's build.rustflags, exactly as on CI.
RUST_ENV = (("RUSTFLAGS", "-D warnings"),)


@dataclass(frozen=True)
class Check:
    name: str
    argv: tuple[str, ...]
    lane: str
    env: tuple[tuple[str, str], ...] = ()
    # Run after every parallel check, alone: it saturates every core.
    exclusive: bool = False
    # Oldest version of argv[0] that runs this check correctly.
    min_version: tuple[int, ...] | None = None
    # zackees/ci.yml#196 (GATE-009): the isolated runner must prove it saw
    # THIS worktree. A fresh nonce goes to NONCE_FILE; the runner echoes it.
    tree_nonce: bool = False


def checks() -> list[Check]:
    lint = [
        Check("rustfmt", ("soldr", "cargo", "fmt", "--all", "--", "--check"), "lint", RUST_ENV),
        Check(
            "Strict benchmark metric type check (pyright)",
            ("uvx", "--from", "pyright==1.1.411", "pyright"),
            "lint",
        ),
        Check(
            "Benchmark report contract tests",
            (
                *PY,
                "--with",
                "pytest",
                "--with",
                "pillow",
                "pytest",
                "-q",
                "ci/tests/test_benchmark_metrics.py",
                "ci/tests/test_benchmark_stats.py",
            ),
            "lint",
            (("PYTHONPATH", "."),),
        ),
        Check(
            "Log-audit registry snapshot",
            ("uv", "run", "python", "ci/check_log_audit_registry.py"),
            "lint",
        ),
        Check(
            "setup-soldr cache footprint",
            ("uv", "run", "--no-project", "--with", "pyyaml", "python", "ci/check_cache_footprint.py"),
            "lint",
        ),
        Check(
            "kernal-api migration inventory",
            ("uv", "run", "--no-project", "python", "ci/check_kernal_api_baseline.py"),
            "lint",
        ),
        Check(
            "Local gate wiring (GATE-001..010)",
            (
                "uvx",
                "--from",
                f"git+https://github.com/zackees/ci.yml@{CI_LINT_REF}",
                "--with",
                "pyyaml",
                "ci-lint",
                "local-gate",
                "lint",
                "--repo",
                ".",
            ),
            "lint",
        ),
    ]
    py_tests = [
        Check(
            "ci/tests",
            (
                *PY,
                "--with",
                "pytest",
                "--with",
                "pillow",
                "--with",
                "pyyaml",
                "python",
                "-m",
                "pytest",
                "ci/tests",
                "-q",
                "-rs",
            ),
            "py-tests",
            (("PYTHONPATH", "."),),
        )
    ]
    rust = [
        Check(
            "cargo check (MSRV toolchain)",
            ("soldr", "cargo", "check", "--workspace"),
            "check",
            RUST_ENV,
            exclusive=True,
        ),
        Check(
            "rustdoc",
            ("soldr", "cargo", "doc", "--workspace", "--no-deps"),
            "docs",
            (*RUST_ENV, ("RUSTDOCFLAGS", "-D warnings")),
            exclusive=True,
        ),
    ]
    # zackees/ci.yml#168 (GATE-005): never on the host. The nextest run
    # wrapper refuses unless CI=true or ZCCACHE_TEST_ISOLATED=1; the bosn
    # image sets the marker.
    tests = [
        Check(
            "zccache tests (isolated, bosn)",
            ("bosn", "run", "--task", "gate-test", "--deadline-ms", "7200000", "--output-limit", "67108864"),
            "tests",
            exclusive=True,
            # bosn 0.1.7 binds a fresh setup container to this worktree
            # instead of reusing one bound to another (zackees/bosn#314).
            min_version=(0, 1, 7),
            tree_nonce=True,
        )
    ]
    return lint + py_tests + rust + tests


@dataclass(frozen=True)
class Result:
    check: Check
    code: int
    seconds: float
    output: str


@dataclass(frozen=True)
class Captured:
    returncode: int
    output: str


def run_captured(argv: list[str], env: dict[str, str] | None = None) -> Captured:
    """Run `argv` from the repository root with stdout and stderr captured
    through one temporary file, never a pipe (zackees/ci.yml PY-003): a full
    pipe blocks the child, and a daemon that inherits a pipe keeps the caller
    waiting for an EOF that never comes."""
    with tempfile.TemporaryFile() as out:
        proc = subprocess.run(
            argv,
            cwd=ROOT,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            check=False,
        )
        out.seek(0)
        return Captured(proc.returncode, out.read().decode("utf-8", errors="replace"))


def tool_version(tool: str) -> tuple[int, ...] | None:
    proc = run_captured([tool, "--version"])
    match = re.search(r"(\d+)\.(\d+)\.(\d+)", proc.output)
    return tuple(int(part) for part in match.groups()) if match else None


NONCE_FILE = ".gate-nonce"
NONCE_MARKER = "gate-nonce: "


def _run(check: Check) -> Result:
    if not check.tree_nonce:
        return _run_plain(check)
    nonce = secrets.token_hex(16)
    path = ROOT / NONCE_FILE
    path.write_text(nonce + "\n", encoding="utf-8")
    try:
        result = _run_plain(check)
    finally:
        path.unlink(missing_ok=True)
    if f"{NONCE_MARKER}{nonce}" in result.output:
        return result
    seen = [ln for ln in result.output.splitlines() if ln.startswith(NONCE_MARKER)]
    return Result(
        check,
        result.code or 1,
        result.seconds,
        result.output + f"\nlocal gate: the isolated runner did not see this worktree "
        f"(expected {NONCE_MARKER}{nonce}, saw {seen[-1] if seen else 'no nonce'}). "
        "It ran another checkout's tree (zackees/bosn#314, zackees/ci.yml#196); "
        "stop the bosn container bound to the other worktree, then rerun.",
    )


def _run_plain(check: Check) -> Result:
    start = time.monotonic()
    tool = shutil.which(check.argv[0])
    if tool is None:
        return Result(check, 127, 0.0, f"{check.argv[0]}: not found on PATH")
    if check.min_version is not None:
        found = tool_version(tool)
        if found is None or found < check.min_version:
            want = ".".join(map(str, check.min_version))
            return Result(
                check,
                1,
                0.0,
                f"{check.argv[0]} {found or 'unknown'} is older than {want}; "
                f"upgrade it: uv tool upgrade {check.argv[0]}",
            )
    env = {**os.environ, **dict(check.env)} if check.env else None
    proc = run_captured(list(check.argv), env)
    return Result(check, proc.returncode, time.monotonic() - start, proc.output)


def _save_log(result: Result) -> Path:
    logs = ROOT / "target" / "local-gate-logs"
    logs.mkdir(parents=True, exist_ok=True)
    path = logs / (re.sub(r"[^A-Za-z0-9_.-]+", "-", result.check.name).strip("-") + ".log")
    path.write_text(result.output, encoding="utf-8")
    return path


def _report(result: Result) -> None:
    status = "ok  " if result.code == 0 else "FAIL"
    print(f"{status} {result.seconds:6.1f}s  {result.check.name}", flush=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--lane", choices=("all", *LANES), default="all", help="one GATE-007 lane")
    parser.add_argument("--list", action="store_true", help="print the checks and exit")
    parser.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 2))
    args = parser.parse_args(argv)

    selected = [c for c in checks() if args.lane in ("all", c.lane)]
    if args.list:
        for check in selected:
            env = " ".join(f"{k}={v!r}" for k, v in check.env)
            print(f"[{check.lane}] {check.name}: {env + ' ' if env else ''}{' '.join(check.argv)}")
        return 0

    start = time.monotonic()
    results: list[Result] = []
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(_run, c) for c in selected if not c.exclusive]
        for future in as_completed(futures):
            result = future.result()
            results.append(result)
            _report(result)
    for check in (c for c in selected if c.exclusive):
        result = _run(check)
        results.append(result)
        _report(result)

    failed = [r for r in results if r.code != 0]
    for result in failed:
        print(f"\n===== FAIL: {result.check.name} (exit {result.code}) =====")
        print(f"$ {' '.join(result.check.argv)}")
        print(result.output.rstrip()[-20000:])
        # The tail can miss the cause (nextest prints a failing test's output
        # where it fails, not at the end), so keep the whole log.
        print(f"full output: {_save_log(result)}")
    total = time.monotonic() - start
    print(f"\nlocal gate ({args.lane}): {len(results) - len(failed)}/{len(results)} passed in {total:.0f}s")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
