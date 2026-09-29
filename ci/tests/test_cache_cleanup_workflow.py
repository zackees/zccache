"""Keep automated cache retention scoped to the default branch."""

from __future__ import annotations

import json
import subprocess
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
    assert triggers["workflow_run"] == {
        "workflows": ["Integration"],
        "types": ["completed"],
    }
    assert not {"push", "pull_request", "pull_request_target"} & set(triggers)
    job = workflow["jobs"]["prune-per-commit-caches"]
    assert "github.ref == 'refs/heads/main'" in job["if"]
    assert "github.event.workflow_run.head_branch == 'main'" in job["if"]
    assert "github.event.workflow_run.event == 'push'" in job["if"]


def test_producer_completion_cleanup_follows_trailing_main_head_and_waits_for_quiescence() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )
    steps = workflow["jobs"]["prune-per-commit-caches"]["steps"]
    barrier = next(step for step in steps if step.get("id") == "producer-barrier")
    script = barrier["with"]["script"]
    assert "listWorkflowRunsForRepo" in script
    assert 'workflow_id: "cache-pre-prune.yml"' in script
    assert "prePruneReady" in script
    assert "MAIN_CACHE_WRITER_WORKFLOW_NAMES" in script
    assert 'run.head_branch === "main"' in script
    assert "run.head_sha === expectedSha" in script
    assert 'run.event === "push"' in script
    assert 'run.status !== "completed"' in script
    assert "quietSince && Date.now() - quietSince >= 120_000" in script
    assert "Workflow-run delivery can lag" in script
    assert "maxAttempts = 120" in script
    assert "head !== expectedSha" in script
    assert "syncCheckout(expectedSha)" in script
    assert "branchHead() !== expectedSha" in script
    assert "expected-main-sha" in script
    assert "skipping this stale inventory" not in script
    assert '"Cache Cleanup"' not in script
    for step_id in ("gate", "count-prune", "hard-cap"):
        step = next(step for step in steps if step.get("id") == step_id)
        assert "EXPECTED_MAIN_SHA" in step.get("env", {})
    count_prune = next(step for step in steps if step.get("id") == "count-prune")
    hard_cap = next(step for step in steps if step.get("id") == "hard-cap")
    assert "stopping stale deletes" in count_prune["with"]["script"]
    assert "stopping stale deletes" in hard_cap["with"]["script"]


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
    lock_hash_step = next(
        step for step in steps if step.get("id") == "cargo-lock-hashes"
    )
    assert "cargoLockHashes" in lock_hash_step["with"]["script"]
    count_env = next(step for step in steps if step.get("id") == "count-prune")["env"]
    assert "ROOT_LOCK_HASH_LF" in count_env
    assert "ROOT_LOCK_HASH_CRLF" in count_env


def test_cleanup_allowlist_preserves_foundation_cache_families() -> None:
    planner = (ROOT / "ci/cache_cleanup_plan.js").read_text(encoding="utf-8")
    workflow = (ROOT / ".github/workflows/cache-cleanup.yml").read_text(
        encoding="utf-8"
    )
    assert '"soldr-mini-v2-' not in planner
    script = (
        "const {CACHE_PREFIXES}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(CACHE_PREFIXES));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert "cook-base-v2-" not in json.loads(result.stdout)
    assert '"solo-toolchain-v3-' in planner
    assert '"setup-soldr-buildcache-v2-macos-arm64-6d40444a3fc5e4d0-' in planner
    assert '"setup-soldr-buildcache-v2-macos-arm64-032744c531163905-' in planner
    assert "setup-soldr-buildcache-v2-linux-x64-032744c531163905" not in planner
    assert "no current shape or foundation cache was deleted" in workflow


def test_perf_guard_build_cache_profile_is_not_retired() -> None:
    planner_script = (
        "const {RETIRED_MAIN_PATTERNS}=require(process.argv[1]);"
        "const key='setup-soldr-buildcache-v2-linux-x64-032744c531163905-27ed8f2e0ba4ca9d';"
        "process.stdout.write(String(RETIRED_MAIN_PATTERNS.some((p)=>p.test(key))));"
    )
    result = subprocess.run(
        ["node", "-e", planner_script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert result.stdout == "false"


def test_retired_cache_families_are_main_only_and_exactly_scoped() -> None:
    fixture = ROOT / "ci/tests/fixtures/cache_cleanup_shapes.json"
    script = (
        "const fs=require('node:fs');"
        "const {planCountPrune}=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const plan=planCountPrune(caches);"
        "process.stdout.write(JSON.stringify({"
        "keep:plan.keep.map(c=>c.id),stale:plan.stale.map(c=>c.id)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=fixture.read_text(encoding="utf-8"),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert {30, 31} <= set(plan["stale"])
    assert 32 not in plan["stale"]
    # Windows Test build-cache is live again (#1829 freed the budget).
    assert 33 not in plan["stale"]
    assert 34 not in plan["stale"]


def test_measured_retired_cache_shapes_are_exact_and_main_only() -> None:
    caches = [
        {
            "id": 60,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l27ed8f2e0ba4ca9d-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 824000000,
            "created_at": "2026-09-27T05:18:00Z",
        },
        {
            "id": 61,
            "key": "cook-base-v2-windows-x64-msvc-rustc1.95.0-f9e7e4902-l373d63beb34d7b6e-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 448000000,
            "created_at": "2026-09-27T05:27:00Z",
        },
        {
            "id": 62,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l27ed8f2e0ba4ca9d-soldrv0.9.23-xdylint",
            "ref": "refs/heads/main",
            "size_in_bytes": 599000000,
            "created_at": "2026-09-27T05:29:00Z",
        },
        {
            "id": 63,
            "key": "setup-soldr-buildcache-v2-linux-x64-032744c531163905-27ed8f2e0ba4ca9d",
            "ref": "refs/heads/main",
            "size_in_bytes": 440000000,
            "created_at": "2026-09-27T05:30:00Z",
        },
        {
            "id": 64,
            "key": "setup-soldr-buildcache-v2-windows-x64-9cc0e23f450b04b3-373d63beb34d7b6e",
            "ref": "refs/heads/main",
            "size_in_bytes": 356000000,
            "created_at": "2026-09-27T05:31:00Z",
        },
        {
            "id": 65,
            "key": "setup-soldr-buildcache-v2-windows-arm64-9cc0e23f450b04b3-373d63beb34d7b6e",
            "ref": "refs/heads/main",
            "size_in_bytes": 351000000,
            "created_at": "2026-09-27T05:32:00Z",
        },
        {
            "id": 66,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l27ed8f2e0ba4ca9d-soldrv0.9.23-xdylint",
            "ref": "refs/pull/1739/merge",
            "size_in_bytes": 599000000,
            "created_at": "2026-09-27T05:29:00Z",
        },
        {
            "id": 67,
            "key": "setup-soldr-buildcache-v2-windows-x64-aaaaaaaaaaaaaaaa-f2415904ed1de43f",
            "ref": "refs/heads/main",
            "size_in_bytes": 356000000,
            "created_at": "2026-09-27T05:31:00Z",
        },
        {
            "id": 68,
            "key": "setup-soldr-buildcache-v2-linux-arm64-032744c531163905-27ed8f2e0ba4ca9d",
            "ref": "refs/heads/main",
            "size_in_bytes": 422000000,
            "created_at": "2026-09-27T05:30:00Z",
        },
        {
            "id": 69,
            "key": "setup-soldr-buildcache-v2-windows-x64-9cc0e23f450b04b3-f2415904ed1de43f",
            "ref": "refs/heads/main",
            "size_in_bytes": 350000000,
            "created_at": "2026-09-26T05:31:00Z",
        },
        {
            "id": 70,
            "key": "setup-soldr-buildcache-v2-linux-x64-032744c531163905-1111111111111111",
            "ref": "refs/heads/main",
            "size_in_bytes": 440000000,
            "created_at": "2026-09-25T05:30:00Z",
        },
        {
            "id": 71,
            "key": "setup-soldr-buildcache-v2-windows-x64-9cc0e23f450b04b3-1111111111111111",
            "ref": "refs/heads/main",
            "size_in_bytes": 356000000,
            "created_at": "2026-09-25T05:31:00Z",
        },
    ]
    script = (
        "const {planCountPrune}=require(process.argv[1]);"
        "const p=planCountPrune(JSON.parse(require('node:fs').readFileSync(0,'utf8')));"
        "process.stdout.write(JSON.stringify({stale:p.stale.map(c=>c.id),keep:p.keep.map(c=>c.id)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(caches),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    # Linux x64 f9e7 remains the native-Python release cook profile; only the
    # measured disabled cook/build-cache identities retire automatically.
    assert set(plan["stale"]) == {61, 62}
    assert 60 not in plan["stale"]
    assert {63, 70}.isdisjoint(plan["stale"])
    assert {66, 67, 68}.isdisjoint(plan["stale"])


def test_old_buildcache_fallback_is_pruned_only_after_exact_same_shape_replacement() -> None:
    caches = [
        {
            "id": 1,
            "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-1111111111111111",
            "ref": "refs/heads/main",
            "size_in_bytes": 695_000_000,
            "created_at": "2026-09-01T00:00:00Z",
        },
        {
            "id": 2,
            "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-2222222222222222",
            "ref": "refs/heads/main",
            "size_in_bytes": 700_000_000,
            "created_at": "2026-09-27T00:00:00Z",
        },
        {
            "id": 3,
            "key": "setup-soldr-buildcache-v2-linux-x64-67ddfadb5b3c0042-check-linux-x86-musl-1111111111111111",
            "ref": "refs/heads/main",
            "size_in_bytes": 222_000_000,
            "created_at": "2026-09-01T00:00:00Z",
        },
        {
            "id": 4,
            "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-1111111111111111",
            "ref": "refs/pull/123/merge",
            "size_in_bytes": 695_000_000,
            "created_at": "2026-09-01T00:00:00Z",
        },
        {
            "id": 5,
            "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-MyJob-1111111111111111",
            "ref": "refs/heads/main",
            "size_in_bytes": 100_000_000,
            "created_at": "2026-09-01T00:00:00Z",
        },
        {
            "id": 6,
            "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-myjob-2222222222222222",
            "ref": "refs/heads/main",
            "size_in_bytes": 100_000_000,
            "created_at": "2026-09-27T00:00:00Z",
        },
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planCountPrune}=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const p=planCountPrune(caches,1,{linux:'2222222222222222',macos:'2222222222222222',windows:['2222222222222222','3333333333333333']});"
        "process.stdout.write(JSON.stringify({stale:p.stale.map(c=>c.id),keep:p.keep.map(c=>c.id)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(caches),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["stale"] == [1]
    assert {2, 3, 4, 5, 6}.isdisjoint(plan["stale"])


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
    assert set(plan["stale"]) == {12, 13, 25, 26, 29, 30, 31}
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


def test_cargo_registry_lock_generations_require_exact_same_profile_replacement() -> (
    None
):
    fixture = ROOT / "ci/tests/fixtures/cache_cleanup_shapes.json"
    script = (
        "const fs=require('node:fs');"
        "const {planCountPrune}=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const plan=planCountPrune(caches,1,{"
        "linux:'3bf5d54592e41b8a',macos:'3bf5d54592e41b8a',"
        "windows:['3bf5d54592e41b8a','f2415904ed1de43f']});"
        "process.stdout.write(JSON.stringify({"
        "keep:plan.keep.map(c=>c.id),stale:plan.stale.map(c=>c.id)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=fixture.read_text(encoding="utf-8"),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    stale = set(plan["stale"])
    # All ten currently observed cache profiles remain protected. Old lock
    # generations are retired only when an exact digest/platform replacement
    # exists on main; a different digest, architecture, OS or ref is not proof.
    assert set(range(35, 45)).isdisjoint(stale)
    assert {45, 47, 50, 51} <= stale
    assert {46, 48, 49, 52, 53, 54, 55, 56, 57, 58}.isdisjoint(stale)


def test_cargo_lock_hash_helper_covers_lf_and_crlf_checkouts() -> None:
    script = (
        "const {cargoLockHashes}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(cargoLockHashes('version = 4\\r\\n[[package]]\\n')));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == {
        "lf": "2eb705d50759188a",
        "crlf": "7a6b15d9081fcc9f",
    }


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
