"""Regression coverage for exact-main-push cache pre-pruning."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import yaml

from ci import check_cache_footprint as footprint

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


def test_transition_pre_prune_retires_dead_generations_first_then_forecasts_and_fails_closed() -> None:
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
    # #1850: superseded generations are deleted BEFORE the forecast verdict,
    # the inventory is re-converged, and the post-delete peak is re-forecast.
    retire = script.index("plan.retireFirstIds")
    reconverge = script.index("convergeInventory(dead.map((cache) => cache.id))")
    verdict = script.index("if (!plan.ok)")
    assert retire < script.index("deleteCache(cache, \"superseded") < reconverge < verdict
    assert reconverge < script.index("plan = planFor(before)", reconverge) < verdict
    # The forecast-dependent transition deletes stay behind the fail-closed verdict.
    assert verdict < script.index("deleteCache(cache, \"forecasted old-lock cache\")")
    assert script.count("deleteActionsCacheById") == 1  # one guarded helper
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


def test_transition_forecast_ignores_required_profiles_that_no_cache_carries() -> None:
    """A hardcoded profile digest with no listed cache is an orphan from an older
    toolchain; charging its minimum on every push failed pre-prune closed."""
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
    # Only the native-Python reserve remains.
    assert plan["estimatedNewBytes"] == 1_120_000_000


def test_no_lock_change_deletes_orphaned_old_lock_build_caches() -> None:
    """Live 2026-09-29 shape: the lock is unchanged, but toolchain digests moved.
    Old-lock build caches whose family has a current-lock generation under a new
    digest are orphans: delete them and forecast no re-seed for them."""
    caches = [
        {"id": 1, "key": "setup-soldr-buildcache-v2-linux-x64-67ddfadb5b3c0042-check-linux-x86-musl-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 222_818_750, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 2, "key": "setup-soldr-buildcache-v2-linux-x64-f87c9084b4c91b6a-check-linux-x86-musl-2222222222222222", "ref": "refs/heads/main", "size_in_bytes": 205_000_000, "created_at": "2026-01-02T00:00:00Z"},
        {"id": 3, "key": "setup-soldr-buildcache-v2-linux-x64-73ae636dd0fd865d-dylint-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 240_411_914, "created_at": "2026-01-01T00:00:00Z"},
        {"id": 4, "key": "setup-soldr-buildcache-v2-linux-x64-223016ae85f4db41-dylint-2222222222222222", "ref": "refs/heads/main", "size_in_bytes": 229_000_000, "created_at": "2026-01-02T00:00:00Z"},
        # Old-lock with no current generation in its family: not an orphan.
        {"id": 5, "key": "setup-soldr-buildcache-v2-linux-arm64-aaaaaaaaaaaaaaaa-solo-1111111111111111", "ref": "refs/heads/main", "size_in_bytes": 50, "created_at": "2026-01-01T00:00:00Z"},
    ]
    total = sum(cache["size_in_bytes"] for cache in caches)
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches,total}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,{linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']},total,total)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches, "total": total}),
        check=True,
        capture_output=True,
        text=True,
    )
    plan = json.loads(result.stdout)
    assert plan["ok"] is True
    assert plan["deleteIds"] == [1, 3]
    # Survivors, plus id 5's re-seed and the native-Python reserve.
    assert plan["projectedPeakBytes"] == 205_000_000 + 229_000_000 + 50 + 50 + 1_120_000_000


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
    # 100 B cook re-seed + native-Python reserve. No build-profile minimums:
    # none of their shapes is listed, so none will be re-seeded.
    assert plan["estimatedNewBytes"] == 1_120_000_100


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
    # The CRLF-keyed current build supersedes the LF-keyed row 3. The Windows
    # cook base (1) is also collected: cook runs on Linux only (#1758).
    assert {cache["id"] for cache in plans["post"]["stale"]} == {1, 3}
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
    """Every durable first-party writer is gated; non-push refs are read-only.

    setup-soldr steps use SAVE_CACHE_POLICY: main-push-only on GitHub, full
    saves under nektos/act (local bosn runs, zackees/bosn#309).
    """
    expression = footprint.SAVE_CACHE_POLICY
    for event_name, ref in (
        ("pull_request", "refs/pull/1/merge"),
        ("push", "refs/heads/feature"),
    ):
        assert footprint.evaluate_save_policy(expression, event_name=event_name, ref=ref) == "false"
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
    assert f"save-cache: {footprint.COMPOSITE_SAVE_CACHE_POLICY}" in source
    assert "default: \"false\"" in source
    release = yaml.safe_load((ROOT / ".github/workflows/release-auto.yml").read_text(encoding="utf-8"))
    release_steps = [step for job in release["jobs"].values() for step in job.get("steps", [])]
    release_target = next(step for step in release_steps if step.get("uses") == "./.github/actions/build-target")
    # Callers pass the GitHub rule; the composite step adds the act prefix once.
    assert release_target["with"]["save_cache"] == footprint.MAIN_PUSH_ONLY_SAVE
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


def _plan_live_lock_transition() -> dict:
    fixture = json.loads(
        (ROOT / "ci/tests/fixtures/cache_lock_transition_1758.json").read_text(encoding="utf-8")
    )
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const f=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const lock={linux:f.lock_hashes.lf,macos:f.lock_hashes.lf,windows:[f.lock_hashes.crlf]};"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(f.caches,lock,f.usage_bytes,f.usage_bytes)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(fixture),
        check=True,
        capture_output=True,
        text=True,
    )
    return {"plan": json.loads(result.stdout), "caches": fixture["caches"]}


def test_live_lock_transition_forecast_fits_with_linux_only_cook() -> None:
    """zccache#1758: the 2026-09-28 lock transition failed closed on every push.

    Re-seeding every profile at its old size projected 9.95 GB against the
    9.2 GB target, so producers never ran and the gate could never open.
    Cook bases run on Linux only (zackees/ci.yml#5, RUST-010); the macOS and
    Windows generations (~3.0 GB) are retired rather than forecast.
    """
    result = _plan_live_lock_transition()
    plan = result["plan"]
    non_linux_cooks = {
        cache["id"]
        for cache in result["caches"]
        if cache["ref"] == "refs/heads/main"
        and cache["key"].startswith(("cook-base-v2-macos-", "cook-base-v2-windows-"))
    }

    assert plan["ok"] is True, plan.get("reason")
    assert plan["projectedPeakBytes"] <= 9_200_000_000
    assert len(non_linux_cooks) == 4
    assert non_linux_cooks <= set(plan["deleteIds"])


def test_non_linux_cook_bases_are_retired_main_keys() -> None:
    script = (
        "const {isRetiredMainKey}=require(process.argv[1]);"
        "process.stdout.write(JSON.stringify(["
        "'cook-base-v2-windows-x64-msvc-rustc1.95.0-fnone-l373d63beb34d7b6e-soldrv0.9.23',"
        "'cook-base-v2-macos-arm64-darwin-rustc1.95.0-f9e7e4902-l27ed8f2e0ba4ca9d-soldrv0.9.23',"
        "'cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l27ed8f2e0ba4ca9d-soldrv0.9.23',"
        "].map(isRetiredMainKey)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        check=True,
        capture_output=True,
        text=True,
    )
    assert json.loads(result.stdout) == [True, True, False]


def test_cache_barrier_github_scripts_retry_transient_api_errors() -> None:
    """#1785: one 5xx from the GitHub API failed the pre-prune barrier and
    blocked every main producer. Every github-script step in the barrier,
    its waiter and the cleanup must retry transient errors; 404 stays exempt
    (github-script's default) so explicit 404 handling still applies."""
    paths = [
        ".github/workflows/cache-pre-prune.yml",
        ".github/workflows/cache-cleanup.yml",
        ".github/actions/wait-cache-pre-prune/action.yml",
    ]
    seen = 0
    for relative in paths:
        doc = yaml.safe_load((ROOT / relative).read_text(encoding="utf-8"))
        jobs = doc.get("jobs") or {"composite": {"steps": (doc.get("runs") or {}).get("steps", [])}}
        for job in jobs.values():
            for step in job.get("steps") or []:
                if not str(step.get("uses", "")).startswith("actions/github-script@"):
                    continue
                seen += 1
                options = step.get("with") or {}
                assert int(options.get("retries", 0)) >= 3, f"{relative}: {step.get('name')}"
                assert "retry-exempt-status-codes" not in options, relative
    assert seen >= 10


def _barrier_skip(caches: list[dict[str, object]], usage: int, target: int | None = None) -> dict:
    """Run the planner then planWriterBarrierSkip on one inventory (#1822)."""
    args = "" if target is None else f",{target}"
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune,planWriterBarrierSkip}=require(process.argv[1]);"
        "const {caches,usage}=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const lock={linux:'2222222222222222',macos:'2222222222222222',windows:['3333333333333333']};"
        f"const plan=planLockTransitionPrePrune(caches,lock,usage,usage{args});"
        "process.stdout.write(JSON.stringify({plan,decision:planWriterBarrierSkip(caches,plan)}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches, "usage": usage}),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


_SHA = "a" * 40
_CURRENT_COOK = "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l2222222222222222-soldrv0.9.23"
_OLD_COOK = "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l1111111111111111-soldrv0.9.23"
# The native-Python release cook; the planner reserves a re-seed when absent.
_F9_COOK = _CURRENT_COOK.replace("-fnone-", "-f9e7e4902-")


def _row(cache_id: int, key: str, size: int) -> dict[str, object]:
    return {
        "id": cache_id,
        "key": key,
        "ref": "refs/heads/main",
        "size_in_bytes": size,
        "created_at": "2026-01-01T00:00:00Z",
    }


def test_writer_barrier_is_skipped_when_no_deletes_and_reserve_fits() -> None:
    """#1822: 7.05 GB + the 1.6 GB per-SHA reserve is under the 9.2 GB target."""
    caches = [
        _row(1, _CURRENT_COOK, 3_000_000_000),
        _row(2, f"zccache-Linux-X64-test-x86_64-unknown-linux-gnu-{_SHA}", 1_600_000_000),
        _row(3, _F9_COOK, 1_090_000_000),
    ]
    out = _barrier_skip(caches, 7_050_000_000)
    assert out["plan"]["ok"] is True and out["plan"]["deleteIds"] == []
    assert out["decision"]["skip"] is True
    assert out["decision"]["reserveBytes"] == 1_600_000_000


def test_writer_barrier_waits_when_deletes_are_planned() -> None:
    caches = [
        _row(1, _OLD_COOK, 3_000_000_000),
        _row(2, _CURRENT_COOK, 100),
    ]
    out = _barrier_skip(caches, 3_000_000_100)
    assert out["plan"]["deleteIds"] == [1]
    assert out["decision"]["skip"] is False
    assert "deletes planned" in out["decision"]["reason"]


def test_writer_barrier_waits_when_peak_plus_reserve_exceeds_target() -> None:
    # Peak alone (7.8 GB) fits 9.2 GB, but the 1.6 GB reserve does not.
    caches = [_row(1, _CURRENT_COOK, 6_000_000_000), _row(2, _F9_COOK, 1_800_000_000)]
    out = _barrier_skip(caches, 7_800_000_000)
    assert out["plan"]["ok"] is True and out["plan"]["deleteIds"] == []
    assert out["decision"]["skip"] is False
    assert "in-flight reserve" in out["decision"]["reason"]


def test_writer_barrier_waits_when_forecast_fails_closed() -> None:
    out = _barrier_skip([_row(1, _CURRENT_COOK, 9_100_000_000)], 9_100_000_000, 9_000_000_000)
    assert out["plan"]["ok"] is False
    assert out["decision"]["skip"] is False


def test_in_flight_reserve_counts_each_per_sha_shape_once_at_its_largest() -> None:
    caches = [
        _row(1, f"zccache-Linux-X64-test-x86_64-unknown-linux-gnu-{_SHA}", 1_000_000_000),
        _row(2, f"zccache-Linux-X64-test-x86_64-unknown-linux-gnu-{'b' * 40}", 1_500_000_000),
        _row(3, f"zccache-Windows-X64-test-x86_64-pc-windows-msvc-{_SHA}", 900_000_000),
        _row(4, _CURRENT_COOK, 5_000_000_000),  # lock-keyed, not a per-SHA shape
    ]
    script = (
        "const fs=require('node:fs');"
        "const {inFlightWriterReserveBytes}=require(process.argv[1]);"
        "process.stdout.write(String(inFlightWriterReserveBytes(JSON.parse(fs.readFileSync(0,'utf8')))));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(caches),
        check=True,
        capture_output=True,
        text=True,
    )
    assert int(result.stdout) == 1_500_000_000 + 900_000_000


def test_pre_prune_workflow_forecasts_before_the_barrier_and_gates_on_skip() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    steps = workflow["jobs"]["pre-prune"]["steps"]
    ids = [step.get("id") for step in steps]
    assert ids.index("lock-hashes") < ids.index("early-forecast") < ids.index("writer-barrier")
    gate = "steps.early-forecast.outputs.skip != 'true'"
    barrier = steps[ids.index("writer-barrier")]
    assert barrier["if"] == gate
    retire = next(s for s in steps if s.get("name") == "Forecast and retire old-lock cache generations")
    assert gate in retire["if"] and "writer-barrier.outputs.ready == 'true'" in retire["if"]
    early = steps[ids.index("early-forecast")]["with"]["script"]
    assert "planWriterBarrierSkip" in early
    # The early step is read-only and never deletes.
    assert "deleteActionsCacheById" not in early


# The standalone `zackees/setup-soldr/cook` sub-action (the Linux "Cook test
# dependencies" step) writes the soldr token without the `v` that the in-action
# cook writes: `-soldr0.9.25` instead of `-soldrv0.9.25`. Auto-Release
# 36690156553 failed closed on it; both spellings are one v2 layout.
_COOK_NO_V = "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25"
_COOK_WITH_V = "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldrv0.9.25"
_CURRENT_LOCKS = {"linux": "ec2a428e3983361d", "macos": "ec2a428e3983361d", "windows": ["d2ef11473c3c1d13"]}


def _node_json(expr: str, payload: dict) -> dict:
    script = (
        "const fs=require('node:fs');"
        "const m=require(process.argv[1]);"
        "const d=JSON.parse(fs.readFileSync(0,'utf8'));"
        f"process.stdout.write(JSON.stringify({expr}));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(payload),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def _row(cache_id: int, key: str, size: int) -> dict[str, object]:
    return {"id": cache_id, "key": key, "ref": "refs/heads/main", "size_in_bytes": size, "created_at": "2026-09-30T00:00:00Z"}


def test_cook_key_parser_accepts_sub_action_soldr_token_without_v() -> None:
    parts = _node_json("m.parseCookBaseKey(d.key)", {"key": _COOK_NO_V})
    assert parts == {
        "os": "linux",
        "arch": "x64",
        "libc": "glibc",
        "rustc": "1.95.0",
        "flags": "6cafa616",
        "lockHash": "ec2a428e3983361d",
        "soldr": "0.9.25",
        "suffix": "",
    }
    # Same soldr release, other spelling: identical classification.
    assert _node_json("m.parseCookBaseKey(d.key)", {"key": _COOK_WITH_V}) == parts


def test_cook_key_parser_still_rejects_malformed_soldr_tokens() -> None:
    base = "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d"
    for bad in (f"{base}-soldr", f"{base}-soldrv", f"{base}-soldrx0.9.25", f"{base}-soldr-0.9.25", f"{base}-0.9.25", f"{base}-soldrvv0.9.25"):
        assert _node_json("m.parseCookBaseKey(d.key)", {"key": bad}) is None, bad


def test_lock_transition_forecast_accepts_live_inventory_with_sub_action_cook_key() -> None:
    caches = [
        _row(1, "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.25", 261_662_028),
        _row(2, "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-lec2a428e3983361d-soldrv0.9.25", 645_526_116),
        _row(3, _COOK_NO_V, 279_507_264),
        _row(4, "cook-base-v2-linux-arm64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25", 276_840_157),
    ]
    plan = _node_json(
        "m.planLockTransitionPrePrune(d.caches,d.hashes,5_000_000_000,5_000_000_000)",
        {"caches": caches, "hashes": _CURRENT_LOCKS},
    )
    assert plan["ok"] is True, plan.get("reason")
    # Every generation is on the current lock: nothing is deleted or re-seeded.
    assert plan["deleteIds"] == []
    assert plan["staleCookIds"] == []
    assert plan["estimatedNewBytes"] == 0


def test_sub_action_cook_key_is_retired_by_lock_hash_and_kept_when_current() -> None:
    old = "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-l1111111111111111-soldr0.9.25"
    caches = [_row(1, old, 100), _row(2, _COOK_NO_V, 200)]
    plan = _node_json("(()=>{const p=m.planCookBasePrune(d.caches,d.hashes);return {stale:p.stale.map(c=>c.id),keep:p.keep.map(c=>c.id)}})()", {"caches": caches, "hashes": _CURRENT_LOCKS})
    assert plan == {"stale": [1], "keep": [2]}


def test_v_and_non_v_soldr_spellings_share_one_reseed_profile() -> None:
    # Old-lock generations of the same shape written by the two producers are
    # one slot: the forecast books the larger, not the sum.
    caches = [
        _row(1, "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-l1111111111111111-soldr0.9.25", 300),
        _row(2, "cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-l1111111111111111-soldrv0.9.25", 500),
    ]
    plan = _node_json(
        "m.planLockTransitionPrePrune(d.caches,d.hashes,1_000,1_000,2_000_000_000)",
        {"caches": caches, "hashes": _CURRENT_LOCKS},
    )
    assert plan["ok"] is True, plan.get("reason")
    assert plan["staleCookIds"] == [1, 2]
    assert plan["estimatedNewBytes"] == 500 + 1_120_000_000


# --- #1850: live inventory of 2026-09-30 (227 entries, 10.04 GB) -----------
# Every entry is keyed to the previous lock; main moved to a new lock, so the
# whole inventory is one lock transition. `S:` abbreviates `setup-soldr-`.
OLD_LOCK = "ec2a428e3983361d"
NEW_LOCK = "01fdce755221f8f4"
LIVE_1850_INVENTORY: list[tuple[str, int]] = [
    ("cook-base-v2-linux-arm64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25", 276_840_157),
    ("cook-base-v2-linux-arm64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.25", 422_935_409),
    ("cook-base-v2-linux-arm64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.26", 422_986_355),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25", 279_507_264),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-lec2a428e3983361d-soldrv0.9.25", 645_526_116),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-lec2a428e3983361d-soldrv0.9.26", 831_800_389),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.25", 261_662_028),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.26", 679_752_798),
    ("S:buildcache-v2-linux-arm64-85229e05b0612895-ec2a428e3983361d", 19_980_156),
    ("S:buildcache-v2-linux-x64-223016ae85f4db41-dylint-ec2a428e3983361d", 546_146_899),
    ("S:buildcache-v2-linux-x64-27deaf711481c4fc-check-linux-arm-musl-ec2a428e3983361d", 214_747_857),
    ("S:buildcache-v2-linux-x64-6fef49df0527cdf1-ec2a428e3983361d", 226_727_779),
    ("S:buildcache-v2-linux-x64-85229e05b0612895-ec2a428e3983361d", 6_075_299),
    ("S:buildcache-v2-linux-x64-f87c9084b4c91b6a-check-linux-x86-musl-ec2a428e3983361d", 214_713_894),
    ("S:buildcache-v2-windows-arm64-4962042243664ed1-d2ef11473c3c1d13", 251),
    ("S:buildcache-v2-windows-x64-4962042243664ed1-d2ef11473c3c1d13", 243),
    ("S:buildcache-v2-windows-x64-9de1f1526ac8e278-d2ef11473c3c1d13", 418_706_703),
    ("S:cargoregistry-v1-linux-arm64-ec2a428e3983361d-5dd7376e6a94b983", 112_116_553),
    ("S:cargoregistry-v1-linux-arm64-ec2a428e3983361d-85229e05b0612895", 112_234_523),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-27deaf711481c4fc", 112_275_245),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-5dd7376e6a94b983", 117_336_295),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-5f6e18fb2e53935b", 117_315_391),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-6fef49df0527cdf1", 117_459_355),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-76c7b3f7e670f4a2", 117_342_570),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-85229e05b0612895", 110_679_362),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-f87c9084b4c91b6a", 112_299_637),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-ff7d446a118a3e0b", 117_302_342),
    ("S:cargoregistry-v1-macos-arm64-ec2a428e3983361d-6fef49df0527cdf1", 113_881_292),
    ("S:cargoregistry-v1-macos-arm64-ec2a428e3983361d-76c7b3f7e670f4a2", 113_900_263),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-031df19e2fdf1f1e", 159_220_635),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-4962042243664ed1", 159_218_745),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-697e5b4122d90f00", 159_224_202),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-9de1f1526ac8e278", 159_302_901),
    ("S:dylint-output-v2-linux-x64-0b91abf489b62909-ec2a428e3983361d", 862_402_320),
    ("S:dylint-v2-linux-x64-x86_64-unknown-linux-gnu-d153183e2b438407-dylint", 549_203_070),
    ("S:prepare-v3-linux-x64-aarch64-unknown-linux-musl-re162f3a368305035-s129892c5510eebd1-xcheck-linux-arm-musl", 93_165_348),
    ("S:prepare-v3-linux-x64-aarch64-unknown-linux-musl-re162f3a368305035-s3361bb806daddd47-xcheck-linux-arm-musl", 93_161_476),
    ("S:prepare-v3-linux-x64-x86_64-unknown-linux-musl-re162f3a368305035-s129892c5510eebd1-xcheck-linux-x86-musl", 99_859_555),
    ("S:prepare-v3-linux-x64-x86_64-unknown-linux-musl-re162f3a368305035-s3361bb806daddd47-xcheck-linux-x86-musl", 99_895_669),
]
LIVE_1850_EXTRA_BYTES = 280_000_000  # soldr-mini, sccache, setup-uv leftovers
TARGET = 9_200_000_000


def _live_1850_caches() -> list[dict[str, object]]:
    return [
        {
            "id": index + 1,
            "key": key.replace("S:", "setup-soldr-"),
            "ref": "refs/heads/main",
            "size_in_bytes": size,
            "created_at": f"2026-09-{28 + index % 3:02d}T00:00:00Z",
        }
        for index, (key, size) in enumerate(LIVE_1850_INVENTORY)
    ]


def _run_transition(caches: list[dict[str, object]], extra: int = 0) -> dict:
    total = sum(int(c["size_in_bytes"]) for c in caches) + extra
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches,total}=JSON.parse(fs.readFileSync(0,'utf8'));"
        f"const h={{linux:'{NEW_LOCK}',macos:'{NEW_LOCK}',windows:['{NEW_LOCK}']}};"
        "process.stdout.write(JSON.stringify(planLockTransitionPrePrune(caches,h,total,total)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps({"caches": caches, "total": total}),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def test_live_1850_inventory_is_forecast_to_fit_after_retiring_dead_generations() -> None:
    """RED before #1850: peak 11.5 GB > 9.2 GB because superseded soldr
    generations were charged a re-seed and the digest-pinned build-cache
    fallbacks selected nothing."""
    caches = _live_1850_caches()
    plan = _run_transition(caches, LIVE_1850_EXTRA_BYTES)

    assert plan["ok"] is True, plan.get("reason")
    assert plan["projectedPeakBytes"] <= TARGET
    by_id = {c["id"]: c["key"] for c in caches}
    # Only the soldr 0.9.25 generations that a 0.9.26 sibling supersedes.
    assert sorted(by_id[i] for i in plan["retireFirstIds"]) == [
        "cook-base-v2-linux-arm64-glibc-rustc1.95.0-fnone-l" + OLD_LOCK + "-soldrv0.9.25",
        "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l" + OLD_LOCK + "-soldrv0.9.25",
        "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l" + OLD_LOCK + "-soldrv0.9.25",
    ]
    # The lone soldr0.9.25 f6cafa616 (#1838) family has no newer sibling.
    assert not any("f6cafa616" in by_id[i] for i in plan["retireFirstIds"])
    # Retire-first is a subset of the transition deletes, never current keys.
    assert set(plan["retireFirstIds"]) <= set(plan["deleteIds"])
    assert all(OLD_LOCK in by_id[i] for i in plan["deleteIds"] if "cook-base" in by_id[i])


def test_live_1850_prune_then_forecast_is_stable_after_retire_first_deletes() -> None:
    caches = _live_1850_caches()
    first = _run_transition(caches, LIVE_1850_EXTRA_BYTES)
    dead = set(first["retireFirstIds"])
    assert dead
    survivors = [c for c in caches if c["id"] not in dead]
    freed = sum(int(c["size_in_bytes"]) for c in caches if c["id"] in dead)
    second = _run_transition(survivors, LIVE_1850_EXTRA_BYTES)
    assert second["ok"] is True
    assert second["retireFirstIds"] == []
    assert second["currentBytes"] == first["currentBytes"] - freed
    assert second["projectedPeakBytes"] == first["projectedPeakBytes"]


def test_build_cache_fallbacks_are_digest_agnostic_and_keep_musl_check_profiles() -> None:
    caches = [
        {"id": 1, "key": f"setup-soldr-buildcache-v2-linux-x64-6fef49df0527cdf1-{OLD_LOCK}", "ref": "refs/heads/main", "size_in_bytes": 227, "created_at": "2026-09-30T00:00:00Z"},
        {"id": 2, "key": f"setup-soldr-buildcache-v2-windows-x64-9de1f1526ac8e278-{OLD_LOCK}", "ref": "refs/heads/main", "size_in_bytes": 419, "created_at": "2026-09-30T00:00:00Z"},
        {"id": 3, "key": f"setup-soldr-buildcache-v2-linux-x64-223016ae85f4db41-dylint-{OLD_LOCK}", "ref": "refs/heads/main", "size_in_bytes": 546, "created_at": "2026-09-30T00:00:00Z"},
        {"id": 4, "key": f"setup-soldr-buildcache-v2-linux-x64-f87c9084b4c91b6a-check-linux-x86-musl-{OLD_LOCK}", "ref": "refs/heads/main", "size_in_bytes": 215, "created_at": "2026-09-30T00:00:00Z"},
        {"id": 5, "key": f"setup-soldr-buildcache-v2-linux-x64-27deaf711481c4fc-check-linux-arm-musl-{OLD_LOCK}", "ref": "refs/heads/main", "size_in_bytes": 215, "created_at": "2026-09-30T00:00:00Z"},
    ]
    plan = _run_transition(caches)
    assert plan["ok"] is True
    assert plan["selectedBuildCacheIds"] == [1, 2, 3]


def test_genuinely_over_budget_generation_still_fails_closed_after_retire_first() -> None:
    """Retire-first must not turn the forecast into a rubber stamp: dead
    generations are still reported, but a peak above target stays a failure
    and withholds the forecast-dependent deletes."""
    caches = _live_1850_caches() + [
        {
            "id": 9_000,
            "key": f"cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l{OLD_LOCK}-soldrv0.9.26-bloat",
            "ref": "refs/heads/main",
            "size_in_bytes": 1_500_000_000,
            "created_at": "2026-09-30T00:00:00Z",
        }
    ]
    plan = _run_transition(caches, LIVE_1850_EXTRA_BYTES)
    assert plan["ok"] is False
    assert "exceeds transition target" in plan["reason"]
    assert plan["deleteIds"] == []
    assert plan["projectedPeakBytes"] > TARGET
    assert len(plan["retireFirstIds"]) == 3  # dead generations still retirable


def test_current_generation_cook_bases_are_never_retire_first() -> None:
    def cook(cache_id: int, lock: str, soldr: str) -> dict[str, object]:
        return {
            "id": cache_id,
            "key": f"cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l{lock}-soldrv{soldr}",
            "ref": "refs/heads/main",
            "size_in_bytes": 100,
            "created_at": "2026-09-30T00:00:00Z",
        }

    # Current-lock 0.9.25 next to a current-lock 0.9.26: not stale, so it is
    # neither retire-first nor deleted (producers may still pin it).
    plan = _run_transition([cook(1, NEW_LOCK, "0.9.25"), cook(2, NEW_LOCK, "0.9.26"), cook(3, OLD_LOCK, "0.9.25")])
    assert plan["ok"] is True
    assert plan["retireFirstIds"] == [3]
    assert plan["deleteIds"] == [3]
