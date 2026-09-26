"""The ci/tests suite must stay wired into pull-request CI (#1474).

For months no workflow ran this suite, and it rotted: two tests failed on a
clean `main` and nobody noticed because nothing executed them. #1493 added the
job. This guard keeps it from being dropped, narrowed, gated behind a paths
filter, or made non-blocking again without a test failing.

It does not care *which* workflow hosts the job, so the consolidation of
standalone PR workflows into `ci.yml` (#1639) can move it freely: a workflow
counts if a `pull_request` trigger reaches it directly or through an
unconditional `uses: ./.github/workflows/<file>` call chain.
"""

from __future__ import annotations

import re
import shlex
from pathlib import Path
from typing import Any

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"

# Flags that run less than the whole suite, or hide its failures.
NARROWING_FLAGS = ("-k", "--deselect", "--ignore", "--ignore-glob", "-m")
PYTEST_CI_TESTS = re.compile(r"\bpytest\b.*(?<![\w/.-])ci/tests(?![\w/.-])")


def _load(path: Path) -> dict[str, Any]:
    return yaml.safe_load(path.read_text(encoding="utf-8")) or {}


def _triggers(workflow: dict[str, Any]) -> dict[str, Any]:
    # PyYAML (YAML 1.1) parses the bare key `on` as the boolean True.
    raw = workflow.get("on", workflow.get(True, {}))
    if isinstance(raw, str):
        return {raw: None}
    if isinstance(raw, list):
        return {name: None for name in raw}
    return raw or {}


def _unfiltered_pull_request(workflow: dict[str, Any]) -> bool:
    if "pull_request" not in (triggers := _triggers(workflow)):
        return False
    config = triggers["pull_request"] or {}
    return "paths" not in config and "paths-ignore" not in config


def pr_reachable_workflows(workflows_dir: Path) -> set[str]:
    """File names of workflows that every pull request runs."""
    loaded = {p.name: _load(p) for p in sorted(workflows_dir.glob("*.y*ml"))}
    reachable = {name for name, wf in loaded.items() if _unfiltered_pull_request(wf)}
    frontier = list(reachable)
    while frontier:
        caller = loaded[frontier.pop()]
        for job in (caller.get("jobs") or {}).values():
            uses = job.get("uses", "")
            if not uses.startswith("./.github/workflows/") or "if" in job:
                continue
            callee = uses.rsplit("/", 1)[1]
            if (
                callee in loaded
                and callee not in reachable
                and "workflow_call" in _triggers(loaded[callee])
            ):
                reachable.add(callee)
                frontier.append(callee)
    return reachable


def ci_tests_commands(workflows_dir: Path) -> list[tuple[str, str, str]]:
    """(workflow, job, command) for every blocking, unconditional step that runs
    the whole ci/tests suite from a workflow every pull request reaches."""
    found = []
    for name in sorted(pr_reachable_workflows(workflows_dir)):
        for job_id, job in (_load(workflows_dir / name).get("jobs") or {}).items():
            if "if" in job or job.get("continue-on-error"):
                continue
            for step in job.get("steps") or []:
                if "if" in step or step.get("continue-on-error"):
                    continue
                command = " ".join(str(step.get("run", "")).split("\\\n"))
                for line in command.splitlines():
                    if not PYTEST_CI_TESTS.search(line):
                        continue
                    argv = shlex.split(line)
                    # Only pytest's own arguments: `python -m pytest` must not
                    # read as the `-m` marker filter.
                    args = argv[len(argv) - argv[::-1].index("pytest") :]
                    if any(a.split("=")[0] in NARROWING_FLAGS for a in args):
                        continue
                    if "|| true" in line or line.rstrip().endswith("|| :"):
                        continue
                    found.append((name, job_id, line.strip()))
    return found


def test_every_pull_request_runs_the_whole_ci_tests_suite() -> None:
    commands = ci_tests_commands(WORKFLOWS)
    assert commands, (
        "no workflow reached by every pull request runs `pytest ci/tests` "
        "unconditionally and without narrowing flags; the suite rots when "
        "nothing runs it (#1474)"
    )


def _write(tmp: Path, name: str, body: str) -> None:
    (tmp / name).write_text(body, encoding="utf-8")


PYTEST_JOB = """
jobs:
  pytest:
    runs-on: ubuntu-latest
    steps:
      - run: |
          uv run --no-project --with pytest \\
            python -m pytest ci/tests -q{extra}
"""


def test_guard_rejects_a_repo_where_nothing_runs_the_suite(tmp_path: Path) -> None:
    _write(
        tmp_path,
        "ci.yml",
        "on: [pull_request]\njobs:\n  a:\n    steps:\n      - run: soldr cargo test\n",
    )
    assert ci_tests_commands(tmp_path) == []


@pytest.mark.parametrize(
    ("trigger", "extra", "runs"),
    [
        ("on:\n  pull_request:\n", "", True),
        ("on:\n  pull_request:\n    paths-ignore: ['**/*.md']\n", "", False),
        ("on:\n  push:\n    branches: [main]\n", "", False),
        ("on:\n  pull_request:\n", " -k 'not slow'", False),
        ("on:\n  pull_request:\n", " --deselect ci/tests/test_lint.py::t", False),
        ("on:\n  pull_request:\n", " || true", False),
    ],
)
def test_guard_classifies_direct_workflows(
    tmp_path: Path, trigger: str, extra: str, runs: bool
) -> None:
    _write(tmp_path, "python-tests.yml", trigger + PYTEST_JOB.format(extra=extra))
    assert bool(ci_tests_commands(tmp_path)) is runs


@pytest.mark.parametrize(
    ("condition", "runs"), [("", True), ("    if: false\n", False)]
)
def test_guard_follows_reusable_workflow_calls(
    tmp_path: Path, condition: str, runs: bool
) -> None:
    _write(
        tmp_path,
        "ci.yml",
        "on:\n  pull_request:\njobs:\n  python:\n"
        + condition
        + "    uses: ./.github/workflows/python-tests.yml\n",
    )
    _write(
        tmp_path,
        "python-tests.yml",
        "on:\n  workflow_call:\n" + PYTEST_JOB.format(extra=""),
    )
    assert bool(ci_tests_commands(tmp_path)) is runs


def test_guard_ignores_a_single_file_run(tmp_path: Path) -> None:
    job = PYTEST_JOB.replace("ci/tests -q", "ci/tests/test_lint.py -q")
    _write(tmp_path, "python-tests.yml", "on: pull_request\n" + job.format(extra=""))
    assert ci_tests_commands(tmp_path) == []
