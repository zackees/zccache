"""The local gate stays in step with the remote jobs it stands in for.

An attested PR head skips the remote Formatting, Documentation, MSRV and
Integration (Linux) jobs (local-gate.toml [gate.trust], ci-attestations.yml),
so the gate must keep running what those jobs run. These checks catch the
drift a reviewer would miss: a ci-lint pin bumped in one place only, a step
added to the mirrored job instead of the lane, or an Integration test command
the isolated lane never learned about.
"""

from __future__ import annotations

import re
from pathlib import Path

from ci import local_gate

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
GATE_TESTS = ROOT / "ci" / "docker" / "gate" / "run_gate_tests.sh"


def _job_block(workflow: str, job: str) -> str:
    text = (WORKFLOWS / workflow).read_text(encoding="utf-8")
    start = text.index(f"\n  {job}:\n")
    following = re.search(r"\n  [a-z0-9-]+:\n", text[start + 1 :])
    end = start + 1 + following.start() if following else len(text)
    return text[start:end]


def test_ci_lint_pin_is_the_same_everywhere() -> None:
    ref = local_gate.CI_LINT_REF
    assert re.fullmatch(r"[0-9a-f]{40}", ref)
    for workflow in ("ci.yml", "integration.yml"):
        block = _job_block(workflow, "ci-mode")
        assert f"ref: {ref}" in block, workflow
    assert f"ci.yml@{ref}" in (ROOT / "local-gate.toml").read_text(encoding="utf-8")


def test_formatting_job_runs_only_the_lint_lane() -> None:
    """GATE-001: every check of the mirrored job lives in the gate."""
    block = _job_block("ci.yml", "fmt")
    runs = re.findall(r"^\s+run: (.+)$", block, re.MULTILINE)
    assert runs == ["uv run --no-project --python 3.13 python ci/local_gate.py --lane lint"]
    assert {c.lane for c in local_gate.checks()} == set(local_gate.LANES)


def test_skippable_jobs_consume_their_ci_mode_decision() -> None:
    for workflow, job in (
        ("ci.yml", "fmt"),
        ("ci.yml", "docs"),
        ("ci.yml", "msrv"),
        ("integration.yml", "integration"),
    ):
        block = _job_block(workflow, job)
        assert f"needs.ci-mode.outputs.skip_{job} != 'true'" in block, (workflow, job)
        assert "ci-mode" in block.split("if:", 1)[0], (workflow, job)


def test_isolated_lane_runs_every_integration_test_command() -> None:
    """Each `soldr cargo test|nextest` command of the Integration (Linux) job
    on pull requests, and the MSRV job's ignored test, runs in the lane."""
    lane = GATE_TESTS.read_text(encoding="utf-8").replace('"$FEATURES"', "FEATURES")
    integration = _job_block("integration.yml", "integration")
    # The ignored/stress suite runs only on schedule and dispatch.
    integration = integration.split("- name: Test ignored integration and stress suite", 1)[0]
    msrv = _job_block("ci.yml", "msrv")
    commands = re.findall(r"soldr cargo (?:test|nextest run)[^\n|]*", integration + msrv)
    assert len(commands) >= 5, commands
    for command in commands:
        normalized = command.replace('"$ZCCACHE_INTEGRATION_FEATURES"', "FEATURES").strip()
        normalized = re.sub(r"\s*2>&1$", "", normalized).rstrip('"').strip()
        assert normalized in lane, normalized


def test_isolated_lane_proves_its_tree_first() -> None:
    """GATE-009: the nonce echo precedes every command."""
    lines = [ln for ln in GATE_TESTS.read_text(encoding="utf-8").splitlines() if ln and not ln.startswith("#")]
    first_command = next(ln for ln in lines if not ln.startswith(("set ", "cd ")))
    assert first_command.startswith('echo "gate-nonce: ')
