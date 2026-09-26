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


def test_manual_cleanup_defaults_to_dry_run_and_lists_exact_targets() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )
    dispatch = workflow.get(True, workflow.get("on"))["workflow_dispatch"]
    assert dispatch["inputs"]["dry-run"]["type"] == "boolean"
    assert dispatch["inputs"]["dry-run"]["default"] is True

    steps = workflow["jobs"]["prune-per-commit-caches"]["steps"]
    count_prune = next(step for step in steps if step.get("id") == "count-prune")
    hard_cap = next(step for step in steps if step.get("id") == "hard-cap")
    for step in (count_prune, hard_cap):
        assert step["env"]["DRY_RUN"] == "${{ inputs['dry-run'] }}"
        assert "deleteActionsCacheById" in step["with"]["script"]
    assert "Would delete ${c.key} (id ${c.id}," in count_prune["with"]["script"]
    assert "Would hard-cap delete ${c.key} (id ${c.id}," in hard_cap["with"]["script"]


def test_cleanup_allowlist_preserves_foundation_cache_families() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )
    steps = workflow["jobs"]["prune-per-commit-caches"]["steps"]
    hard_cap = next(step for step in steps if step.get("id") == "hard-cap")
    script = hard_cap["with"]["script"]
    start = script.index("const ELIGIBLE_PREFIXES = [")
    end = script.index("];", start)
    eligible_prefixes = script[start:end]

    for foundation in (
        "setup-soldr-buildcache-v2-",
        "setup-soldr-cargoregistry-v1-",
        "solo-toolchain-v3-",
        "soldr-mini-v2-",
    ):
        assert f'"{foundation}"' not in eligible_prefixes
