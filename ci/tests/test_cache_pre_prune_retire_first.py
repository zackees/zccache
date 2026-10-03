"""Retire-first cache pre-prune coverage for the 2026-09-30 overrun (#1850)."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

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


def test_live_1850_inventory_cannot_also_bootstrap_integration() -> None:
    """RED before #1850: peak 11.5 GB > 9.2 GB because superseded soldr
    generations were charged a re-seed and the digest-pinned build-cache
    fallbacks selected nothing."""
    caches = _live_1850_caches()
    plan = _run_transition(caches, LIVE_1850_EXTRA_BYTES)

    # Keep the historical evidence unchanged: the new Integration producer
    # no longer fits this old footprint, even after known dead generations.
    assert plan["ok"] is False
    assert plan["projectedPeakBytes"] > TARGET
    assert plan["deleteIds"] == []
    by_id = {c["id"]: c["key"] for c in caches}
    # The soldr 0.9.25 cook generations that a 0.9.26 sibling supersedes (#1852
    # adds superseded mini/prepare generations and the retired arm64 f6caf leg).
    cooks = sorted(
        by_id[i] for i in plan["retireFirstIds"]
        if by_id[i].startswith("cook-base") and "f6cafa616" not in by_id[i]
    )
    assert cooks == [
        "cook-base-v2-linux-arm64-glibc-rustc1.95.0-fnone-l" + OLD_LOCK + "-soldrv0.9.25",
        "cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l" + OLD_LOCK + "-soldrv0.9.25",
        "cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l" + OLD_LOCK + "-soldrv0.9.25",
    ]
    # The x64 soldr0.9.25 f6cafa616 (#1838) family has no newer sibling.
    assert not any(
        "f6cafa616" in by_id[i] and "linux-x64" in by_id[i] for i in plan["retireFirstIds"]
    )
    # Dead generations are still identified, but a failed forecast withholds
    # all forecast-dependent deletes.
    assert all(OLD_LOCK in by_id[i] for i in plan["retireFirstIds"] if "cook-base" in by_id[i])


def test_live_1850_prune_then_forecast_is_stable_after_retire_first_deletes() -> None:
    caches = _live_1850_caches()
    first = _run_transition(caches, LIVE_1850_EXTRA_BYTES)
    dead = set(first["retireFirstIds"])
    assert dead
    survivors = [c for c in caches if c["id"] not in dead]
    freed = sum(int(c["size_in_bytes"]) for c in caches if c["id"] in dead)
    second = _run_transition(survivors, LIVE_1850_EXTRA_BYTES)
    assert second["ok"] is False  # Integration still cannot fit after dead-only deletes
    assert second["deleteIds"] == []
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
    assert len(plan["retireFirstIds"]) >= 3  # dead generations still retirable


def test_superseded_soldr_cook_bases_retire_first_even_on_the_current_lock() -> None:
    def cook(cache_id: int, lock: str, soldr: str) -> dict[str, object]:
        return {
            "id": cache_id,
            "key": f"cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l{lock}-soldrv{soldr}",
            "ref": "refs/heads/main",
            "size_in_bytes": 100,
            "created_at": "2026-09-30T00:00:00Z",
        }

    # #1875: a current-lock 0.9.25 next to a current-lock 0.9.26 is a dead
    # generation too. Every workflow resolves one soldr version
    # (check_cache_footprint.py), so no producer restores or re-seeds it, and
    # keeping it is what deadlocked the 0.9.25 -> 0.9.27 transition.
    plan = _run_transition([cook(1, NEW_LOCK, "0.9.25"), cook(2, NEW_LOCK, "0.9.26"), cook(3, OLD_LOCK, "0.9.25")])
    assert plan["ok"] is True
    assert plan["retireFirstIds"] == [1, 3]
    assert 2 not in plan["deleteIds"]
