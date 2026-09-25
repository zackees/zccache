"""Static CI contract for native Intel and Apple Silicon runner coverage."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
MACOS_WORKFLOWS = {
    "ci-macos.yml": ["macos (macos-15-intel) / Test"],
    "fs-matrix.yml": ["matrix (macos-15-intel)"],
    "wrapper-e2e.yml": ["wrapper-e2e (macos-15-intel)"],
    "test-action.yml": ["macOS x86_64"],
    "release-auto.yml": ["Test PyPI Wheel (macosx_10_12_x86_64)"],
}


def _workflow(name: str) -> str:
    return (WORKFLOWS / name).read_text(encoding="utf-8")


def _job(text: str, name: str) -> str:
    marker = f"  {name}:\n"
    start = text.index(marker)
    following = re.search(r"^  [a-z][a-z0-9-]*:\n", text[start + len(marker) :], re.MULTILINE)
    end = start + len(marker) + following.start() if following else len(text)
    return text[start:end]


def test_all_load_bearing_macos_lanes_observe_the_intel_runner() -> None:
    """The scarcer native Intel host has a bounded queue observer."""
    for workflow_name, expected_jobs in MACOS_WORKFLOWS.items():
        workflow = _workflow(workflow_name)
        assert "macos-14" not in workflow, workflow_name
        assert "permissions:" in workflow and "actions: read" in workflow, workflow_name
        observer = _job(workflow, "observe-macos-queue")
        assert "astral-sh/setup-uv@v6" in observer, workflow_name
        assert "uv run --no-project --python 3.13 python ci/observe_runner_queue.py" in observer, workflow_name
        assert "--runner macos-15-intel" in observer, workflow_name
        for job in expected_jobs:
            assert f'--job "{job}"' in observer, (workflow_name, job)


def test_intel_wheel_is_smoked_on_a_native_intel_host() -> None:
    """The release smoke runs its Intel wheel on an Intel machine."""
    workflow = _workflow("release-auto.yml")
    ungated = _job(workflow, "test-wheels-ungated")
    assert "- os: macos-15-intel" in ungated
    assert "architecture: x64" in ungated
    assert "arch -x86_64" not in ungated
    assert "python -m pip install" in ungated
    assert "python ci/test_exec_cached_wheel.py" in ungated


def test_intel_wheel_smoke_remains_outside_every_publish_needs_edge() -> None:
    """#1538's release-dependency repair must survive the migration."""
    workflow = _workflow("release-auto.yml")
    publish_jobs = ("publish-pypi", "publish-crates")
    for job in publish_jobs:
        assert "test-wheels-ungated" not in _job(workflow, job)
