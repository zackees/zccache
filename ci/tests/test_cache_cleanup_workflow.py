"""Keep automated cache retention scoped to the default branch."""

from __future__ import annotations

from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]


def test_cache_cleanup_runs_daily_on_main_before_budget_check() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )
    triggers = workflow.get(True, workflow.get("on"))
    budget_workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-budget.yml").read_text(encoding="utf-8")
    )
    budget_triggers = budget_workflow.get(True, budget_workflow.get("on"))

    assert triggers["schedule"] == [{"cron": "0 6 * * *"}]
    cleanup_minute, cleanup_hour, *_ = triggers["schedule"][0]["cron"].split()
    budget_minute, budget_hour, *_ = budget_triggers["schedule"][0]["cron"].split()
    cleanup_hour, cleanup_minute = int(cleanup_hour), int(cleanup_minute)
    budget_hour, budget_minute = int(budget_hour), int(budget_minute)
    assert cleanup_hour * 60 + cleanup_minute < budget_hour * 60 + budget_minute
    assert "workflow_dispatch" in triggers
    assert not {"push", "pull_request", "pull_request_target"} & set(triggers)
    assert workflow["jobs"]["prune-per-commit-caches"]["if"] == (
        "github.ref == 'refs/heads/main'"
    )


def test_cache_cleanup_uses_a_scoped_cache_write_permission() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )

    assert workflow["jobs"]["prune-per-commit-caches"]["permissions"] == {
        "actions": "write"
    }
    assert workflow.get("permissions") is None
