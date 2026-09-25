"""Release publication requires a successful full run on the exact commit."""

from pathlib import Path

import yaml

from ci.ci_mode import selected_workflows
from ci.full_ci_gate import REQUIRED_JOBS, has_successful_full_run


def test_only_completed_dispatch_on_exact_sha_counts():
    sha = "a" * 40
    valid = {
        "head_sha": sha,
        "event": "workflow_dispatch",
        "status": "completed",
        "conclusion": "success",
    }
    jobs = lambda _run: [{"name": "required", "conclusion": "success"}]
    assert has_successful_full_run([valid], sha, {"required"}, jobs)
    for field, value in (
        ("head_sha", "b" * 40),
        ("event", "push"),
        ("status", "in_progress"),
        ("conclusion", "failure"),
    ):
        assert not has_successful_full_run(
            [{**valid, field: value}], sha, {"required"}, jobs
        )


def test_green_workflow_with_skipped_required_job_cannot_release():
    sha = "a" * 40
    run = {
        "head_sha": sha,
        "event": "workflow_dispatch",
        "status": "completed",
        "conclusion": "success",
    }
    assert not has_successful_full_run(
        [run],
        sha,
        {"required"},
        lambda _run: [{"name": "required", "conclusion": "skipped"}],
    )
    assert not has_successful_full_run([run], sha, {"required"}, lambda _run: [])
    assert not has_successful_full_run(
        [run],
        sha,
        {"required"},
        lambda _run: [
            {"name": "required", "conclusion": "skipped"},
            {"name": "required", "conclusion": "success"},
        ],
    )


def test_jobs_must_pass_in_one_run():
    sha = "a" * 40
    runs = [
        {
            "id": 1,
            "head_sha": sha,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
        },
        {
            "id": 2,
            "head_sha": sha,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "success",
        },
    ]
    jobs = {
        1: [
            {"name": "a", "conclusion": "success"},
            {"name": "b", "conclusion": "skipped"},
        ],
        2: [
            {"name": "a", "conclusion": "skipped"},
            {"name": "b", "conclusion": "success"},
        ],
    }
    assert not has_successful_full_run(
        runs, sha, {"a", "b"}, lambda run: jobs[run["id"]]
    )


def test_release_gate_precedes_all_release_work():
    workflow = yaml.safe_load(
        (
            Path(__file__).resolve().parents[2] / ".github/workflows/release-auto.yml"
        ).read_text()
    )
    steps = workflow["jobs"]["preflight"]["steps"]
    names = [step["name"] for step in steps if "name" in step]
    assert names.index("Require full CI on release commit") < names.index(
        "Check registry publish status"
    )
    gate = next(
        step
        for step in steps
        if step.get("name") == "Require full CI on release commit"
    )
    assert "ci.full_ci_gate" in gate["run"]


def test_full_inventory_is_dispatchable():
    root = Path(__file__).resolve().parents[2] / ".github/workflows"
    for name in selected_workflows("full"):
        workflow = yaml.safe_load((root / name).read_text())
        events = workflow.get("on", workflow.get(True, {})) or {}
        assert "workflow_dispatch" in events, name


def test_release_gate_has_required_jobs_for_every_full_workflow():
    assert set(REQUIRED_JOBS) == selected_workflows("full")
    assert all(REQUIRED_JOBS.values())
