"""Keep automated cache retention scoped to the default branch."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess

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
        "actions": "write",
        "contents": "read",
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
    gate = next(step for step in steps if step.get("id") == "gate")
    checkout = next(step for step in steps if step.get("uses") == "actions/checkout@v6")
    assert checkout["with"]["ref"] == "main"
    for step in (count_prune, hard_cap):
        assert step["env"]["DRY_RUN"] == "${{ inputs['dry-run'] }}"
        assert "deleteActionsCacheById" in step["with"]["script"]
    assert "Would delete ${c.key} (id ${c.id}," in count_prune["with"]["script"]
    assert "Would hard-cap delete ${c.key} (id ${c.id}," in hard_cap["with"]["script"]
    assert "getActionsCacheUsage(repo)" in hard_cap["with"]["script"]
    assert "const maxAttempts = dryRun ? 1 : 6" in hard_cap["with"]["script"]
    assert "effectiveCacheBytes" in gate["with"]["script"]


def test_cleanup_allowlist_preserves_foundation_cache_families() -> None:
    planner = (ROOT / "ci/cache_cleanup_plan.js").read_text(encoding="utf-8")
    workflow = (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    for foundation in (
        '"setup-soldr-buildcache-v2-',
        '"solo-toolchain-v3-',
        '"soldr-mini-v2-',
        '"cook-base-v2-',
    ):
        assert foundation not in planner
    assert "no current shape or foundation cache was deleted" in workflow


def test_cleanup_keeps_one_current_cache_per_target_shape() -> None:
    fixture = ROOT / "ci/tests/fixtures/cache_cleanup_shapes.json"
    script = (
        "const fs=require('node:fs');"
        "const {planCountPrune,planHardCap,isEligible}=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const plan=planCountPrune(caches);"
        "const linux=caches.filter(c=>c.id===16||c.id===26);"
        "const cap=planHardCap(linux,2000000000,1500000000);"
        "process.stdout.write(JSON.stringify({"
        "keep:plan.keep.map(c=>c.id),stale:plan.stale.map(c=>c.id),"
        "registryEligible:caches.filter(c=>[21,22,27,28].includes(c.id)).map(c=>isEligible(c.key)),"
        "capKeep:cap.keep.map(c=>c.id),capSelected:cap.selected.map(c=>c.id),"
        "capBytes:cap.projectedBytes,capWithinBudget:cap.withinBudget}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=fixture.read_text(encoding="utf-8"),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    # Keep the newest generation for each (ref, target shape); main and PR
    # caches are not assumed to be mutually restorable. Foundation caches are
    # not candidates.
    assert set(plan["keep"]) == {10, 11, 14, 15, 16, 17, 18, 19, 20}
    assert set(plan["stale"]) == {12, 13, 25, 26, 29}
    # setup-soldr registry keys encode both Cargo.lock identity and registry
    # archive digest; these are separate content identities, not generations.
    assert plan["registryEligible"] == [False, False, False, False]
    # Regression for the former global-oldest hard cap: it deleted both
    # Linux ARM64 generations, including the current main key. Only the old
    # duplicate may be selected; the current target remains protected.
    assert 16 in plan["capKeep"]
    assert 16 not in plan["capSelected"]
    assert 26 in plan["capSelected"]
    assert plan["capWithinBudget"] is False


def test_hard_cap_fails_when_only_unique_protected_shapes_remain() -> None:
    fixture = ROOT / "ci/tests/fixtures/cache_cleanup_shapes.json"
    script = (
        "const fs=require('node:fs');"
        "const {planHardCap}=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const {stale}=require(process.argv[1]).planCountPrune(caches);"
        "const cap=planHardCap(caches,10000000000,9500000000,stale.map(c=>c.id));"
        "process.stdout.write(JSON.stringify({"
        "keep:cap.keep.map(c=>c.id),selected:cap.selected.map(c=>c.id),"
        "projectedBytes:cap.projectedBytes,withinBudget:cap.withinBudget}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=fixture.read_text(encoding="utf-8"),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert set(plan["keep"]) >= {16, 17, 18}
    assert plan["selected"] == []
    assert plan["projectedBytes"] == 10_000_000_000
    assert plan["withinBudget"] is False

    workflow = (ROOT / ".github/workflows/cache-cleanup.yml").read_text(
        encoding="utf-8"
    )
    assert "if (!withinBudget)" in workflow
    assert "Unique protected cache shapes remain above" in workflow


def test_effective_usage_uses_whichever_inventory_view_is_larger() -> None:
    script = (
        "const {effectiveCacheBytes}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(["
        "effectiveCacheBytes(100,200),effectiveCacheBytes(300,200)]));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == [200, 300]
