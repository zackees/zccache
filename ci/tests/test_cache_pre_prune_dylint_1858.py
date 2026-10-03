"""Forecast coverage for lock-keyed families the transition planner ignored (#1858).

``setup-soldr-dylint-output-v2-*`` (~862 MB) and the zccache-action bench
``cargo-registry`` / ``cargo-target`` entries are keyed by the Cargo.lock hash,
so a lock change re-seeds them. The planner charged only cook / build-cache /
registry re-seeds, which understated the real peak by about 1.2 GB.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "ci/tests/fixtures/cache_inventory_live_1858.json"
TARGET = 9_200_000_000
NEXT_LOCK = "ffffffffffffffff"  # a simulated next Cargo.lock change
OLD_LOCK = "ec2a428e3983361d"
DYLINT_OUTPUT_OLD = "setup-soldr-dylint-output-v2-linux-x64-0b91abf489b62909-ec2a428e3983361d"
DYLINT_OUTPUT_CUR = "setup-soldr-dylint-output-v2-linux-x64-b58d3e55eb9d7747-01fdce755221f8f4"
DYLINT_BYTES = 862_838_957
BENCH_REGISTRY_BYTES = 59_914_043
BENCH_TARGET_BYTES = 239_375_644
# projectedPeakBytes the planner reported for this exact inventory + NEXT_LOCK
# before #1858 (it ignored all three families above).
OLD_PLANNER_FORECAST = 8_574_929_973


def _live() -> list[dict[str, object]]:
    return json.loads(FIXTURE.read_text(encoding="utf-8"))


def _row(i: int, key: str, size: int, created: str = "2026-09-30T00:00:00Z") -> dict[str, object]:
    return {"id": i, "key": key, "ref": "refs/heads/main", "size_in_bytes": size, "created_at": created}


def _plan(caches: list[dict[str, object]], lock: str = NEXT_LOCK, extra: int = 0) -> dict:
    total = sum(int(c["size_in_bytes"]) for c in caches) + extra
    script = (
        "const fs=require('node:fs');"
        "const {planLockTransitionPrePrune}=require(process.argv[1]);"
        "const {caches,total}=JSON.parse(fs.readFileSync(0,'utf8'));"
        f"const h={{linux:'{lock}',macos:'{lock}',windows:['{lock}']}};"
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


def _f9(lock: str) -> dict[str, object]:
    """A current-lock native-Python cook, so its fixed reserve is not charged."""
    return _row(
        9_100, f"cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l{lock}-soldrv0.9.26", 1_000
    )


def _ids(caches: list[dict[str, object]], *keys: str) -> set[int]:
    return {int(c["id"]) for c in caches if c["key"] in keys}


def test_live_inventory_forecast_accounts_for_dylint_output_and_bench_reseeds() -> None:
    """RED before #1858: the forecast equalled OLD_PLANNER_FORECAST while the
    real peak (survivors plus the three unmodelled re-seeds) was ~9.7 GB."""
    caches = _live()
    plan = _plan(caches)
    assert plan["ok"] is True, plan.get("reason")

    sizes = {int(c["id"]): int(c["size_in_bytes"]) for c in caches}
    deleted = sum(sizes[i] for i in plan["deleteIds"])
    survivors = plan["currentBytes"] - deleted
    # Independent floor for the true peak: what survives the deletes plus the
    # lock-keyed re-seeds that the old planner never charged.
    honest_floor = survivors + DYLINT_BYTES + BENCH_REGISTRY_BYTES + BENCH_TARGET_BYTES
    assert plan["projectedPeakBytes"] >= honest_floor, (
        plan["projectedPeakBytes"], honest_floor)
    assert plan["projectedPeakBytes"] > OLD_PLANNER_FORECAST or deleted > 0
    assert plan["projectedPeakBytes"] <= TARGET

    # Both dylint-output generations are retired at the transition (one
    # generation per family, never old plus new); the older shape is retire-first.
    both = _ids(caches, DYLINT_OUTPUT_OLD, DYLINT_OUTPUT_CUR)
    assert len(both) == 2 and both <= set(plan["deleteIds"])
    assert _ids(caches, DYLINT_OUTPUT_OLD) <= set(plan["retireFirstIds"]) or \
        _ids(caches, DYLINT_OUTPUT_CUR) <= set(plan["retireFirstIds"])
    # The lock-independent dylint foundation is never touched.
    foundation = [
        c["id"] for c in caches if str(c["key"]).startswith("setup-soldr-dylint-v2-")
    ]
    assert foundation and not set(foundation) & set(plan["deleteIds"])


def test_dylint_output_reseed_is_charged_even_when_it_is_the_only_generation() -> None:
    caches = [
        _row(1, DYLINT_OUTPUT_OLD, DYLINT_BYTES),
        _row(2, "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l" + OLD_LOCK + "-soldrv0.9.26", 600_000_000),
    ]
    plan = _plan(caches, lock="01fdce755221f8f4")
    assert plan["ok"] is True, plan.get("reason")
    assert plan["estimatedNewBytes"] >= DYLINT_BYTES + 600_000_000
    assert 1 in plan["deleteIds"]


def test_old_lock_dylint_output_next_to_a_current_generation_is_orphaned_and_uncharged() -> None:
    caches = [
        _row(1, DYLINT_OUTPUT_OLD, DYLINT_BYTES, "2026-09-29T00:00:00Z"),
        _row(2, DYLINT_OUTPUT_CUR, DYLINT_BYTES, "2026-09-30T00:00:00Z"),
        _f9("01fdce755221f8f4"),
    ]
    plan = _plan(caches, lock="01fdce755221f8f4")
    assert plan["ok"] is True, plan.get("reason")
    assert plan["retireFirstIds"] == [1]
    assert plan["deleteIds"] == [1]  # the current generation is never deleted
    assert plan["estimatedNewBytes"] == 1_200_000_000  # Integration bootstrap


def test_older_dylint_output_shape_is_retire_first_when_no_generation_is_current() -> None:
    caches = [
        _row(1, DYLINT_OUTPUT_OLD, DYLINT_BYTES, "2026-09-29T00:00:00Z"),
        _row(2, DYLINT_OUTPUT_CUR, DYLINT_BYTES, "2026-09-30T00:00:00Z"),
        _f9(NEXT_LOCK),
    ]
    plan = _plan(caches)
    assert plan["retireFirstIds"] == [1]
    assert plan["estimatedNewBytes"] == DYLINT_BYTES + 1_200_000_000  # one dylint re-seed + Integration


def test_bench_target_old_lock_is_dead_weight_and_reseed_is_charged() -> None:
    old = "cargo-target-Linux-X64-bench-" + OLD_LOCK + "-" + "a" * 40
    reg = "cargo-registry-Linux-X64-bench-" + OLD_LOCK
    caches = [_row(1, old, BENCH_TARGET_BYTES), _row(2, reg, BENCH_REGISTRY_BYTES), _f9(NEXT_LOCK)]
    plan = _plan(caches)
    assert plan["ok"] is True, plan.get("reason")
    assert set(plan["deleteIds"]) == {1, 2}
    # The target restore key embeds the lock, so an old-lock target is never restorable.
    assert 1 in plan["retireFirstIds"]
    assert plan["estimatedNewBytes"] == BENCH_TARGET_BYTES + BENCH_REGISTRY_BYTES + 1_200_000_000


def test_genuinely_over_budget_dylint_reseed_still_fails_closed() -> None:
    # A newest dylint-output shape far larger than the family measured today:
    # the honest re-seed charge alone pushes the peak past the target.
    caches = _live() + [
        _row(9_001, "setup-soldr-dylint-output-v2-linux-x64-1111111111111111-" + OLD_LOCK,
             4_000_000_000, "2026-10-01T00:00:00Z"),
    ]
    plan = _plan(caches)
    assert plan["ok"] is False
    assert plan["deleteIds"] == []
    assert "exceeds transition target" in plan["reason"]
    # Superseded generations are still reported for deletion.
    assert _ids(caches, DYLINT_OUTPUT_OLD, DYLINT_OUTPUT_CUR) <= set(plan["retireFirstIds"])


@pytest.mark.parametrize(
    "key",
    [
        "setup-soldr-dylint-output-v2-linux-x64-not-a-hash",
        "setup-soldr-dylint-output-v3-linux-x64-0b91abf489b62909-ec2a428e3983361d",
        "setup-soldr-dylint-v9-linux-x64-x86_64-unknown-linux-gnu-d153183e2b438407-dylint",
        "setup-soldr-prepare-v3-garbage",
        "setup-soldr-mystery-v1-linux-x64-abc",
        "soldr-mini-v2-linux",
        "cook-mystery-v1-linux-x64",
        "cargo-registry-Linux-X64-bench-nothex",
        "cargo-target-Linux-X64-bench-" + OLD_LOCK,
    ],
)
def test_unknown_key_formats_in_owned_namespaces_fail_closed(key: str) -> None:
    caches = _live() + [_row(9_002, key, 1_000)]
    plan = _plan(caches)
    assert plan["ok"] is False
    assert key in plan["reason"]
    assert plan["deleteIds"] == []


def test_lock_independent_and_content_addressed_families_are_ignored() -> None:
    caches = _live()
    plan = _plan(caches)
    ignored_prefixes = ("sccache/", "setup-uv-1-", "setup-soldr-dylint-v2-", "zccache-Linux-X64-bench-")
    hit = [
        c["key"] for c in caches
        if c["id"] in plan["deleteIds"] and str(c["key"]).startswith(ignored_prefixes)
    ]
    assert hit == []
