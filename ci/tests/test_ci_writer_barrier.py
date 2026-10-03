"""Every declared writer keeps the exact-main barrier and remote permission."""

import json
from dataclasses import dataclass
from typing import TypeAlias

import yaml

from ci import check_cache_footprint as guard
from ci.local_gate import run_captured

ROOT = guard.ROOT
YamlDocument: TypeAlias = dict[str | bool, guard.YamlValue]
EXPECTED_WRITERS = frozenset(
    {
        "CI",
        "Linux",
        "macOS",
        "Windows",
        "Wrapper end-to-end",
        "Clippy",
        "Integration",
        "Coverage",
        "Python Tests",
        "Filesystem Matrix",
        "Soldr Broker Stress",
        "Test zccache-action",
        "Feature Matrix Check",
        "Perf Guard",
        "Auto-Release",
    }
)


@dataclass(frozen=True)
class WriterEvent:
    event: str
    ref: str


@dataclass(frozen=True)
class WaitEvidence:
    positions: tuple[int, ...]
    dependency: bool
    actions_read: bool
    workflow_actions_read: bool


def _wait_evidence(
    workflow: YamlDocument, job: YamlDocument, has_wait_job: bool
) -> WaitEvidence:
    needs = job.get("needs", [])
    if isinstance(needs, str):
        needs = [needs]
    positions = tuple(
        index
        for index, step in enumerate(job.get("steps", []))
        if step.get("uses") == "./.github/actions/wait-cache-pre-prune"
    )
    permission = (workflow.get("permissions", {}) or {}).get("actions")
    job_permission = (job.get("permissions", {}) or {}).get("actions")
    return WaitEvidence(
        positions,
        has_wait_job and "cache-pre-prune" in needs,
        permission in {"read", "write"} or job_permission in {"read", "write"},
        permission in {"read", "write"},
    )


def _require_wait(wait: WaitEvidence, label: str, index: int | None = None) -> None:
    assert wait.dependency or (
        index is not None and any(position < index for position in wait.positions)
    ), label
    if index is None:
        assert wait.workflow_actions_read, label
    assert wait.actions_read, (
        f"{label} does not grant actions:read to its pre-prune waiter"
    )


def _check_job_writers(
    workflow: YamlDocument, job: YamlDocument, label: str, has_wait_job: bool
) -> bool:
    triggers = workflow.get(True, workflow.get("on", {})) or {}
    pushes_main = triggers.get("push", {}).get("branches") == ["main"]
    wait = _wait_evidence(workflow, job, has_wait_job)
    requires_wait = pushes_main or "workflow_call" in triggers
    observed_writer = False
    if (
        str(job.get("uses", "")).startswith("./.github/workflows/ci-check")
        and pushes_main
    ):
        _require_wait(wait, label)
        observed_writer = True
    for index, step in enumerate(job.get("steps", [])):
        uses = step.get("uses", "")
        values = step.get("with", {}) or {}
        writes_cache = uses.startswith("zackees/setup-soldr@")
        writes_uv = (
            uses == "astral-sh/setup-uv@v6" and values.get("enable-cache") is True
        )
        writes_action = uses == "./" and values.get("save-cache") not in {None, "false"}
        direct_api = uses.startswith("actions/cache@") or "gha-cache save" in str(
            step.get("run", "")
        )
        if writes_cache:
            assert values.get("save-cache") in {"false", "auto"}, label
            assert (
                values.get("save-cache") == "false"
                or values.get("save-cache-remote") == guard.REMOTE_SAVE_CACHE_POLICY
            ), label
        if writes_cache or writes_uv or writes_action or direct_api:
            if requires_wait:
                _require_wait(wait, label, index)
            if pushes_main:
                observed_writer = True
        if uses == "./.github/actions/build-target" and pushes_main:
            observed_writer = True
    return observed_writer


def test_writer_matrix_gates_main_push_and_disables_other_main_ref_saves() -> None:
    for case in (
        WriterEvent("pull_request", "refs/pull/1/merge"),
        WriterEvent("push", "refs/heads/feature"),
    ):
        assert (
            guard.evaluate_save_policy(
                guard.REMOTE_SAVE_CACHE_POLICY, event_name=case.event, ref=case.ref
            )
            == "false"
        )
    script = (
        "const {MAIN_CACHE_WRITER_WORKFLOW_NAMES}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(MAIN_CACHE_WRITER_WORKFLOW_NAMES));"
    )
    result = run_captured(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")]
    )
    assert result.returncode == 0, result.output
    registered: list[str] = json.loads(result.output)
    assert set(registered) == EXPECTED_WRITERS
    observed: set[str] = set()
    for path in (ROOT / ".github/workflows").glob("*.yml"):
        workflow: YamlDocument = yaml.safe_load(path.read_text(encoding="utf-8"))
        jobs = workflow.get("jobs", {})
        has_wait_job = (
            jobs.get("cache-pre-prune", {}).get("uses")
            == "./.github/workflows/wait-cache-pre-prune.yml"
        )
        for job in jobs.values():
            if isinstance(job, dict) and _check_job_writers(
                workflow, job, path.name, has_wait_job
            ):
                observed.add(str(workflow["name"]))
    assert EXPECTED_WRITERS <= observed | {"Auto-Release"}


def test_perf_guard_preserves_independent_build_cache_writer_barrier() -> None:
    document: YamlDocument = yaml.safe_load(
        (ROOT / ".github/workflows/perf-guard.yml").read_text(encoding="utf-8")
    )
    steps = [step for job in document["jobs"].values() for step in job.get("steps", [])]
    setup = [
        step
        for step in steps
        if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    ]
    assert setup and all(step.get("with", {}).get("cache") is False for step in setup)
    assert all(
        step.get("with", {}).get("save-cache") == "auto"
        and step.get("with", {}).get("save-cache-remote")
        == guard.REMOTE_SAVE_CACHE_POLICY
        for step in setup
    )
    # Cache=false disables setup/cook and registry, but build-cache is independent.
    assert any(
        step.get("with", {}).get("build-cache", True) is not False for step in setup
    )
    assert not any(
        str(step.get("uses", "")).startswith("actions/cache@")
        or "gha-cache save" in str(step.get("run", ""))
        for step in steps
    )


def test_release_and_build_preserve_explicit_global_read_only_policy() -> None:
    source = (ROOT / ".github/actions/build-target/action.yml").read_text(
        encoding="utf-8"
    )
    assert "wait-cache-pre-prune" in source
    assert f"save-cache: {guard.COMPOSITE_SAVE_CACHE_POLICY}" in source
    assert 'default: "false"' in source
    release = yaml.safe_load(
        (ROOT / ".github/workflows/release-auto.yml").read_text(encoding="utf-8")
    )
    release_steps = [
        step for job in release["jobs"].values() for step in job.get("steps", [])
    ]
    release_target = next(
        step
        for step in release_steps
        if step.get("uses") == "./.github/actions/build-target"
    )
    # Release builds save nothing (test_cache_release_saves.py); the composite
    # step forwards that explicit disable unchanged on every runner.
    assert release_target["with"]["save_cache"] == "false"
    release_job = next(
        job
        for job in release["jobs"].values()
        if any(
            step.get("uses") == "./.github/actions/build-target"
            for step in job.get("steps", [])
        )
    )
    assert "cache-pre-prune" in release_job["needs"]
    build = yaml.safe_load(
        (ROOT / ".github/workflows/build.yml").read_text(encoding="utf-8")
    )
    build_steps = [
        step for job in build["jobs"].values() for step in job.get("steps", [])
    ]
    build_target = next(
        step
        for step in build_steps
        if step.get("uses") == "./.github/actions/build-target"
    )
    assert build_target["with"]["save_cache"] == "false"
    test_action = (ROOT / ".github/workflows/test-action.yml").read_text(
        encoding="utf-8"
    )
    assert (
        "save-cache: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && env.ZCCACHE_CACHE_WRITES == 'true' }}"
        in test_action
    )
    assert (
        test_action.count(
            "if: github.event_name == 'push' && github.ref == 'refs/heads/main'"
        )
        >= 3
    )
    assert "zccache gha-cache save" in test_action
    test_workflow = yaml.safe_load(test_action)
    gha_steps = test_workflow["jobs"]["test-gha-cache"]["steps"]
    primer = next(
        step for step in gha_steps if step.get("uses", "").startswith("actions/cache@")
    )
    cli_save = next(
        step for step in gha_steps if "gha-cache save" in str(step.get("run", ""))
    )
    expected_push_if = "github.event_name == 'push' && github.ref == 'refs/heads/main'"
    assert primer.get("if") == expected_push_if
    assert cli_save.get("if") == expected_push_if
