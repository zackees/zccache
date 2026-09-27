"""Regression coverage for exact-main-push cache pre-pruning."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]


def _plan_cook_prune(caches: list[dict[str, object]], lock_hashes: dict[str, object]) -> dict:
    script = (
        "const fs=require('node:fs');"
        "const {planCookBasePrune}=require(process.argv[1]);"
        "const {caches,hashes}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const p=planCookBasePrune(caches,hashes);"
        "process.stdout.write(JSON.stringify({stale:p.stale.map(c=>c.id),keep:p.keep.map(c=>c.id)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches, "hashes": lock_hashes}),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def test_cook_prune_retires_only_old_main_lock_keys() -> None:
    caches = [
        {
            "id": 1,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 100,
            "created_at": "2026-01-01T00:00:00Z",
        },
        {
            "id": 2,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l2222222222222222-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 200,
            "created_at": "2026-01-02T00:00:00Z",
        },
        {
            "id": 3,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l2222222222222222-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 300,
            "created_at": "2026-01-03T00:00:00Z",
        },
        {
            "id": 4,
            "key": "cook-base-v2-windows-x64-msvc-rustc1.95.0-fnone-l3333333333333333-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 400,
            "created_at": "2026-01-04T00:00:00Z",
        },
        {
            "id": 5,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23",
            "ref": "refs/pull/123/merge",
            "size_in_bytes": 500,
            "created_at": "2026-01-05T00:00:00Z",
        },
        {
            "id": 6,
            "key": "cook-base-v2-unknown-shape-l1111111111111111-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 600,
            "created_at": "2026-01-06T00:00:00Z",
        },
        {
            "id": 7,
            "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l3333333333333333-soldrv0.9.23",
            "ref": "refs/heads/main",
            "size_in_bytes": 700,
            "created_at": "2026-01-07T00:00:00Z",
        },
    ]

    plan = _plan_cook_prune(
        caches,
        {"linux": "2222222222222222", "macos": "2222222222222222", "windows": ["2222222222222222", "3333333333333333"]},
    )

    assert set(plan["stale"]) == {1, 7}
    assert {2, 3, 4, 5, 6}.issubset(set(plan["keep"]))


def test_cook_prune_fails_closed_when_a_platform_hash_is_missing() -> None:
    plan = _plan_cook_prune(
        [
            {
                "id": 1,
                "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23",
                "ref": "refs/heads/main",
                "size_in_bytes": 100,
                "created_at": "2026-01-01T00:00:00Z",
            }
        ],
        {"linux": "", "macos": "2222222222222222", "windows": []},
    )

    assert plan["stale"] == []
    assert plan["keep"] == [1]


def test_transition_pre_prune_uses_live_usage_and_fails_before_any_delete_on_overrun() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    script = next(
        step["with"]["script"]
        for step in workflow["jobs"]["pre-prune"]["steps"]
        if step.get("name") == "Forecast and retire old-lock cache generations"
    )
    assert "before.usageBytes" in script and "before.listedBytes" in script
    assert "planLockTransitionPrePrune" in script
    assert "if (!plan.ok)" in script
    assert script.index("if (!plan.ok)") < script.index("deleteActionsCacheById")
    assert "convergeInventory(stale.map((cache) => cache.id))" in script
    # The planner's fixed profile allowlist, rather than row count, bounds
    # selection: one profile may have several stale lock generations.
    assert "plan.selectedBuildCacheIds.length > 4" not in script


def test_debug_cook_producers_use_fnone_and_native_python_keeps_release_profile() -> None:
    ci = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8"))
    miss_steps = ci["jobs"]["miss-overhead"]["steps"]
    miss_setup = next(
        step for step in miss_steps if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    )
    assert miss_setup["with"]["prebuild-deps"] == "soldr-cook"
    assert miss_setup["with"]["prebuild-deps-flags"] == ""

    reflink = yaml.safe_load(
        (ROOT / ".github/workflows/reflink-e2e.yml").read_text(encoding="utf-8")
    )
    reflink_setup = next(
        step for step in reflink["jobs"]["btrfs"]["steps"]
        if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    )
    assert reflink_setup["with"]["prebuild-deps"] == "soldr-cook"
    assert reflink_setup["with"]["prebuild-deps-flags"] == ""

    python = yaml.safe_load(
        (ROOT / ".github/workflows/python-tests.yml").read_text(encoding="utf-8")
    )
    native_setup = next(
        step for step in python["jobs"]["native-pytest"]["steps"]
        if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    )
    assert native_setup["with"]["prebuild-deps"] == "soldr-cook"
    assert native_setup["with"]["prebuild-deps-flags"] == "--release"
    planner = (ROOT / "ci/cache_cleanup_plan.js").read_text(encoding="utf-8")
    assert r"^cook-base-v2-linux-x64-glibc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23$/i" not in planner


def test_lock_transition_forecast_selects_only_measured_buildcache_fallbacks() -> None:
    caches = [
        {"id": 1, "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23", "ref": "refs/heads/main", "size_in_bytes": 3_000_000_000, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 2, "key": "setup-soldr-cargoregistry-v1-linux-x64-1111111111111111-aaaaaaaaaaaaaaaa", "ref": "refs/heads/main", "size_in_bytes": 1_000_000_000, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 3, "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 695_272_185, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 4, "key": "setup-soldr-buildcache-v2-windows-x64-0a12db972fd789a0-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 563_360_821, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 5, "key": "setup-soldr-buildcache-v2-linux-arm64-6d40444a3fc5e4d0-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 326_918_007, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 6, "key": "setup-soldr-buildcache-v2-linux-x64-67ddfadb5b3c0042-check-linux-x86-musl-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 222_818_750, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 7, "key": "setup-soldr-buildcache-v2-linux-x64-f19152cfbd9e4599-check-linux-arm-musl-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 214_577_067, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 8, "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-2222222222222222", "ref": "refs/pull/123/merge", "size_in_bytes": 695_272_185, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 9, "key": "setup-soldr-buildcache-v2-linux-x64-032744c531163905-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 366_602_514, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 10, "key": "setup-soldr-buildcache-v2-linux-x64-032744c531163905-4444444444444444", "ref": "refs/heads/main", "size_in_bytes": 366_602_514, "created_at": "2026-01-02T00:00:00Z"},
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,{linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']},7_509_058_050,7_509_058_050)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches}),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)

    assert plan["ok"] is True
    assert set(plan["deleteIds"]) == {1, 2, 3, 4, 5, 9, 10}
    assert plan["selectedBuildCacheIds"] == [3, 4, 5, 9, 10]
    assert plan["projectedPeakBytes"] <= 9_200_000_000
    assert plan["projectedPeakBytes"] == 8_733_248_839


def test_transition_forecast_reserves_perf_guard_profile_when_row_is_absent() -> None:
    script = (
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const p=planLockTransitionPrePrune([],{linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']},1_000_000_000,1_000_000_000);"
        "process.stdout.write(JSON.stringify(p));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is True
    assert plan["estimatedNewBytes"] >= 400_000_000


def test_transition_forecast_does_not_double_reserve_existing_native_python_f9() -> None:
    cache = {
        "id": 1,
        "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l27ed8f2e0ba4ca9d-soldrv0.9.23",
        "ref": "refs/heads/main",
        "size_in_bytes": 824_323_345,
        "created_at": "2026-09-27T00:00:00Z",
    }
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const p=planLockTransitionPrePrune(caches,{linux:'27ed8f2e0ba4ca9d',macos:'27ed8f2e0ba4ca9d',windows:['373d63beb34d7b6e']},1_000_000_000,1_000_000_000);"
        "process.stdout.write(JSON.stringify(p));"
    )

    def plan(caches: list[dict[str, object]]) -> dict:
        result = subprocess.run(
            ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
            input=json.dumps({"caches": caches}),
            check=True,
            capture_output=True,
            text=True,
        )
        return json.loads(result.stdout)

    with_current = plan([cache])
    without_current = plan([])

    assert with_current["ok"] is True
    assert without_current["ok"] is True
    assert without_current["estimatedNewBytes"] - with_current["estimatedNewBytes"] == 1_120_000_000


def test_lock_transition_forecast_fails_closed_for_unknown_buildcache_key() -> None:
    caches = [
        {"id": 1, "key": "setup-soldr-buildcache-v3-linux-x64-unknown", "ref": "refs/heads/main", "size_in_bytes": 100, "created_at": "2026-01-01T00:00:00Z"}
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,{linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']},100,100)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches}),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is False
    assert plan["deleteIds"] == []


def test_lock_transition_forecast_fails_closed_above_peak_target() -> None:
    caches = [
        {"id": 1, "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23", "ref": "refs/heads/main", "size_in_bytes": 3_000_000_000, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 2, "key": "setup-soldr-cargoregistry-v1-linux-x64-1111111111111111-aaaaaaaaaaaaaaaa", "ref": "refs/heads/main", "size_in_bytes": 1_000_000_000, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 3, "key": "setup-soldr-buildcache-v2-linux-x64-6d40444a3fc5e4d0-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 695_272_185, "created_at": "2026-01-01T00:00:00Z"},
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,{linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']},9_100_000_000,9_100_000_000,9_000_000_000)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches}),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is False
    assert plan["deleteIds"] == []
    assert plan["projectedPeakBytes"] > plan["targetBytes"]


def test_transition_forecast_uses_maximum_live_cache_usage_view() -> None:
    script = (
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const p=planLockTransitionPrePrune([],{linux:'1111111111111111',macos:'1111111111111111',windows:['1111111111111111']},9_200_000_000,7_500_000_000);"
        "process.stdout.write(JSON.stringify(p));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is False
    assert plan["currentBytes"] == 9_200_000_000
    assert plan["deleteIds"] == []


def test_transition_forecast_keeps_case_sensitive_cook_suffix_profiles_distinct() -> None:
    caches = [
        {"id": 1, "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23-MyJob", "ref": "refs/heads/main", "size_in_bytes": 100, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 2, "key": "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l2222222222222222-soldrv0.9.23-myjob", "ref": "refs/heads/main", "size_in_bytes": 200, "created_at": "2026-01-02T00:00:00Z"},
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,{linux:'2222222222222222',macos:'2222222222222222',windows:['2222222222222222']},1000,1000)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches}),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is True
    assert plan["staleCookIds"] == [1]
    assert plan["estimatedNewBytes"] == 3_542_946_930


def test_windows_lf_cache_is_stale_when_main_writers_normalize_to_crlf() -> None:
    caches = [
        {"id": 1, "key": "cook-base-v2-windows-x64-msvc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23", "ref": "refs/heads/main", "size_in_bytes": 100, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 3, "key": "setup-soldr-buildcache-v2-windows-x64-0a12db972fd789a0-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 300, "created_at": "2026-01-01T00:00:00Z"},
    ]
    script = (
        "const fs=require('node:fs');"
        "const {planCountPrune,planCookBasePrune,planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const current={linux:'3333333333333333',macos:'3333333333333333',windows:['2222222222222222']};"
        "const currentBuild={id:4,key:'setup-soldr-buildcache-v2-windows-x64-0a12db972fd789a0-2222222222222222',ref:'refs/heads/main',size_in_bytes:400,created_at:'2026-01-02T00:00:00Z'};"
        "process.stdout.write(JSON.stringify({post:planCountPrune([...caches,currentBuild],1,current),cook:planCookBasePrune(caches,current),transition:planLockTransitionPrePrune(caches,current,1000,1000)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches}),
        check=True,
        capture_output=True,
        text=True,
    )
    plans = json.loads(result.stdout)
    assert {cache["id"] for cache in plans["post"]["stale"]} == {3}
    assert {cache["id"] for cache in plans["cook"]["stale"]} == {1}
    assert plans["transition"]["staleCookIds"] == [1]
    assert plans["transition"]["deleteIds"] == [1, 3]
    assert plans["transition"]["estimatedNewBytes"] > 700_000_000


def test_windows_main_cache_writer_normalizes_lock_after_waiter_before_writes() -> None:
    action = yaml.safe_load(
        (ROOT / ".github/actions/wait-cache-pre-prune/action.yml").read_text(
            encoding="utf-8"
        )
    )
    steps = action["runs"]["steps"]
    wait_index = next(
        i for i, step in enumerate(steps)
        if step.get("name") == "Wait for the exact-SHA cache pre-prune run"
    )
    normalize_index = next(
        i for i, step in enumerate(steps)
        if step.get("name") == "Normalize Cargo.lock to the Windows cache identity"
    )
    normalize = steps[normalize_index]
    assert wait_index < normalize_index
    assert normalize["if"] == (
        "runner.os == 'Windows' && github.event_name == 'push' "
        "&& github.ref == 'refs/heads/main'"
    )
    assert "git config --local core.autocrlf true" in normalize["run"]
    assert "git checkout-index --force -- Cargo.lock" in normalize["run"]
    assert '"(?<!`r)`n"' in normalize["run"]
    assert 'git diff --exit-code -- Cargo.lock' in normalize["run"]

    pre_prune = yaml.safe_load(
        (ROOT / ".github/workflows/cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    cleanup = yaml.safe_load(
        (ROOT / ".github/workflows/cache-cleanup.yml").read_text(encoding="utf-8")
    )
    forecast_step = next(
        step for step in pre_prune["jobs"]["pre-prune"]["steps"]
        if step.get("name") == "Forecast and retire old-lock cache generations"
    )
    cleanup_steps = [step for job in cleanup["jobs"].values() for step in job.get("steps", [])]
    planner_step = next(
        step for step in cleanup_steps
        if step.get("name") == "Prune stale cache generations (keep newest per shape)"
    )
    assert "windows: [process.env.ROOT_LOCK_HASH_CRLF]" in forecast_step["with"]["script"]
    assert "windows: [process.env.ROOT_LOCK_HASH_CRLF]" in planner_step["with"]["script"]



def test_pre_prune_is_push_only_and_waiter_is_read_only_and_exact_sha() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    triggers = workflow.get(True, workflow.get("on"))
    assert set(triggers) == {"push"}
    assert triggers["push"]["branches"] == ["main"]
    assert workflow["jobs"]["pre-prune"]["permissions"] == {
        "actions": "write",
        "contents": "read",
    }
    reusable_wait = yaml.safe_load(
        (ROOT / ".github/workflows/wait-cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    reusable_triggers = reusable_wait.get(True, reusable_wait.get("on"))
    assert set(reusable_triggers) == {"workflow_call"}
    assert reusable_wait["permissions"] == {"actions": "read", "contents": "read"}
    wait_job = reusable_wait["jobs"]["wait"]
    assert wait_job["timeout-minutes"] == 360
    assert all(
        step.get("if") == "github.event_name == 'push' && github.ref == 'refs/heads/main'"
        for step in wait_job["steps"]
    )

    action = yaml.safe_load(
        (ROOT / ".github/actions/wait-cache-pre-prune/action.yml").read_text(
            encoding="utf-8"
        )
    )
    assert action["runs"]["using"] == "composite"
    scripts = "\n".join(
        step.get("with", {}).get("script", "") for step in action["runs"]["steps"]
    )
    assert "cache-pre-prune.yml" in scripts
    assert "expectedSha" in scripts
    assert "head_sha" in scripts
    assert "conclusion !== \"success\"" in scripts
    assert "branchHead() !== expectedSha" in scripts
    assert "maxAttempts" in scripts
    assert "actions: write" not in scripts
    pre_prune = workflow["jobs"]["pre-prune"]["steps"]
    barrier = next(step for step in pre_prune if step.get("id") == "writer-barrier")
    assert 'run.event === "push"' in barrier["with"]["script"]
    assert "MAIN_CACHE_WRITER_WORKFLOW_NAMES" in barrier["with"]["script"]
    assert "maxAttempts = 640" in barrier["with"]["script"]
    assert 'const activeStatuses = ["queued", "in_progress", "requested", "waiting", "pending"]' in barrier["with"]["script"]
    assert "if (!activeStatusSet.has(run.status))" in barrier["with"]["script"]
    assert '"completed"' not in barrier["with"]["script"]
    assert "completed runs cannot race" in barrier["with"]["script"]
    assert "const active = olderWriters.filter" not in barrier["with"]["script"]
    assert workflow["jobs"]["pre-prune"]["timeout-minutes"] == 360
    assert "maxAttempts = 1420" in scripts
    # 320m barrier + 20m max convergence + 15m API/delete allowance stays
    # within the 360m job cap; the exact-SHA waiter also exits by 355m.
    assert 640 * 30 / 60 + 2 * 10 + 15 <= 360
    assert 1420 * 15 / 60 < 360
    retire = next(step for step in pre_prune if step.get("name") == "Forecast and retire old-lock cache generations")
    assert "error.status !== 404" in retire["with"]["script"]
    assert "convergeInventory(stale.map((cache) => cache.id))" in retire["with"]["script"]


def test_writer_matrix_gates_main_push_and_disables_other_main_ref_saves() -> None:
    """Every durable first-party writer is gated; non-push refs are read-only."""
    expression = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && 'auto' || 'false' }}"
    writer_names = {
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
    script = (
        "const {MAIN_CACHE_WRITER_WORKFLOW_NAMES}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(MAIN_CACHE_WRITER_WORKFLOW_NAMES));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert set(json.loads(result.stdout)) == writer_names

    perf_guard = yaml.safe_load(
        (ROOT / ".github/workflows/perf-guard.yml").read_text(encoding="utf-8")
    )
    perf_steps = [
        step
        for job in perf_guard["jobs"].values()
        for step in job.get("steps", [])
    ]
    setup_steps = [
        step
        for step in perf_steps
        if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    ]
    assert setup_steps and all(step.get("with", {}).get("cache") is False for step in setup_steps)
    assert all(
        step.get("with", {}).get("save-cache") == expression for step in setup_steps
    )
    # `cache: false` disables setup/cook and registry, but build-cache is an
    # independent layer. The benchmark builder still writes this key and must
    # stay inside the writer barrier.
    assert any(
        step.get("with", {}).get("build-cache", True) is not False
        for step in setup_steps
    )
    assert not any(
        str(step.get("uses", "")).startswith("actions/cache@")
        or "gha-cache save" in str(step.get("run", ""))
        for step in perf_steps
    )

    workflow_files = [path for path in (ROOT / ".github/workflows").glob("*.yml")]
    observed_push_writers: set[str] = set()
    for path in workflow_files:
        workflow = yaml.safe_load(path.read_text(encoding="utf-8"))
        triggers = workflow.get(True, workflow.get("on", {})) or {}
        jobs = workflow.get("jobs", {})
        workflow_permissions = workflow.get("permissions", {}) or {}
        has_wait_job = (
            jobs.get("cache-pre-prune", {}).get("uses")
            == "./.github/workflows/wait-cache-pre-prune.yml"
        )
        for job in jobs.values():
            if (
                isinstance(job, dict)
                and str(job.get("uses", "")).startswith("./.github/workflows/ci-check")
                and triggers.get("push", {}).get("branches") == ["main"]
            ):
                observed_push_writers.add(str(workflow["name"]))
                assert workflow_permissions.get("actions") in {"read", "write"}
                dependencies = job.get("needs", [])
                if isinstance(dependencies, str):
                    dependencies = [dependencies]
                assert has_wait_job and "cache-pre-prune" in dependencies
            steps = job.get("steps", []) if isinstance(job, dict) else []
            if not steps:
                continue
            gate_indexes = [
                index
                for index, step in enumerate(steps)
                if step.get("uses") == "./.github/actions/wait-cache-pre-prune"
            ]
            dependencies = job.get("needs", []) if isinstance(job, dict) else []
            if isinstance(dependencies, str):
                dependencies = [dependencies]
            waits_by_dependency = has_wait_job and "cache-pre-prune" in dependencies
            for index, step in enumerate(steps):
                uses = step.get("uses", "")
                with_values = step.get("with", {}) or {}
                writes_cache = uses.startswith("zackees/setup-soldr@")
                writes_uv = uses == "astral-sh/setup-uv@v6" and with_values.get("enable-cache") is True
                writes_composite = uses == "./.github/actions/build-target"
                writes_action_under_test = (
                    uses == "./" and with_values.get("save-cache") not in {None, "false"}
                )
                direct_cache_api = uses.startswith("actions/cache@") or (
                    "gha-cache save" in str(step.get("run", ""))
                )
                if writes_cache:
                    assert with_values.get("save-cache") in {"false", expression}, (
                        f"{path.name} setup-soldr save policy is not main-push-only"
                    )
                if writes_cache or writes_uv or writes_action_under_test or direct_cache_api:
                    if triggers.get("push", {}).get("branches") == ["main"] or "workflow_call" in triggers:
                        assert waits_by_dependency or any(gate_index < index for gate_index in gate_indexes), (
                            f"{path.name} cache writer has no earlier main-push gate"
                        )
                        job_permissions = job.get("permissions", {}) or {}
                        assert workflow_permissions.get("actions") in {"read", "write"} or (
                            job_permissions.get("actions") in {"read", "write"}
                        ), f"{path.name} does not grant actions:read to its pre-prune waiter"
                    if writes_uv and triggers.get("push", {}).get("branches") == ["main"]:
                        observed_push_writers.add(str(workflow["name"]))
                    if triggers.get("push", {}).get("branches") == ["main"]:
                        observed_push_writers.add(str(workflow["name"]))
                if writes_composite and triggers.get("push", {}).get("branches") == ["main"]:
                    observed_push_writers.add(str(workflow["name"]))

    assert writer_names <= observed_push_writers | {"Auto-Release"}

    source = (ROOT / ".github/actions/build-target/action.yml").read_text(
        encoding="utf-8"
    )
    assert "wait-cache-pre-prune" in source
    assert "save-cache: ${{ inputs.save_cache }}" in source
    assert "default: \"false\"" in source
    release = yaml.safe_load((ROOT / ".github/workflows/release-auto.yml").read_text(encoding="utf-8"))
    release_steps = [step for job in release["jobs"].values() for step in job.get("steps", [])]
    release_target = next(step for step in release_steps if step.get("uses") == "./.github/actions/build-target")
    assert release_target["with"]["save_cache"] == expression
    release_job = next(job for job in release["jobs"].values() if any(
        step.get("uses") == "./.github/actions/build-target" for step in job.get("steps", [])
    ))
    assert "cache-pre-prune" in release_job["needs"]
    build = yaml.safe_load((ROOT / ".github/workflows/build.yml").read_text(encoding="utf-8"))
    build_steps = [step for job in build["jobs"].values() for step in job.get("steps", [])]
    build_target = next(step for step in build_steps if step.get("uses") == "./.github/actions/build-target")
    assert build_target["with"]["save_cache"] == "false"
    test_action = (ROOT / ".github/workflows/test-action.yml").read_text(
        encoding="utf-8"
    )
    assert "save-cache: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}" in test_action
    assert test_action.count("if: github.event_name == 'push' && github.ref == 'refs/heads/main'") >= 3
    assert "zccache gha-cache save" in test_action
    test_workflow = yaml.safe_load(test_action)
    gha_steps = test_workflow["jobs"]["test-gha-cache"]["steps"]
    primer = next(step for step in gha_steps if step.get("uses", "").startswith("actions/cache@"))
    cli_save = next(step for step in gha_steps if "gha-cache save" in str(step.get("run", "")))
    expected_push_if = "github.event_name == 'push' && github.ref == 'refs/heads/main'"
    assert primer.get("if") == expected_push_if
    assert cli_save.get("if") == expected_push_if
