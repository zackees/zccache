"""Same-lock toolchain generations are retired (2026-10-02 pre-prune deadlock).

A soldr release (0.9.25 -> 0.9.27) changes setup-soldr's toolchain-signature
digest and the cook-base ``soldr`` component without touching ``Cargo.lock``.
Every lock-keyed family then holds two generations of the same lock side by
side. The janitor only retired *old-lock* entries, so both generations stayed
(12.41 GB live), the pre-prune forecast failed closed on every main push, the
producers waiting on it failed, and Cache Cleanup refused to run behind the
failed pre-prune: nothing could ever shrink the inventory again.

The fixture is the live listing taken right after pre-prune run 37065004106
failed closed at 12,414,016,686 B.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "ci/tests/fixtures/cache_inventory_live_soldr_generation.json"
LOCK = "01fdce755221f8f4"
LOCK_CRLF = "7ecfb43196b28d31"
TARGET = 9_200_000_000
# zccache#1550: Linux x64 Test's admitted debug test binaries, ~0.9 GB.
TEST_BINARY_HEADROOM = 900_000_000

# Superseded by a newer same-lock generation. Digests: build cache and
# registry share setup-soldr's toolchain-signature digest.
OLD_DIGESTS = (
    "85229e05b0612895", "6fef49df0527cdf1", "223016ae85f4db41", "f87c9084b4c91b6a",
    "27deaf711481c4fc", "9de1f1526ac8e278", "4962042243664ed1",
)
NEW_DIGESTS = (
    "63942eb6ab326045", "95bc1f3919e1cef0", "e2094a1a77f011b7", "572e8c4d0b3f2eaf",
    "abc663cb3d42e76b", "50a8d2a0b0bf0d35", "dea222baf8ddd2a5",
)


def _node(function: str, payload: dict) -> dict:
    script = (
        "const fs=require('node:fs');"
        "const plan=require(process.argv[1]);"
        "const input=JSON.parse(fs.readFileSync(0,'utf8'));"
        f"process.stdout.write(JSON.stringify(({function})(plan,input)));"
    )
    result = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(payload),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def _live() -> list[dict]:
    return json.loads(FIXTURE.read_text(encoding="utf-8"))["actions_caches"]


def _pre_prune(caches: list[dict]) -> dict:
    total = sum(int(c["size_in_bytes"]) for c in caches)
    return _node(
        "(p,i)=>p.planLockTransitionPrePrune(i.caches,"
        f"{{linux:'{LOCK}',macos:'{LOCK}',windows:['{LOCK_CRLF}']}},i.total,i.total)",
        {"caches": caches, "total": total},
    )


def _is_old(key: str) -> bool:
    if key.startswith("cook-base-v2-"):
        return key.endswith(("-soldrv0.9.25", "-soldr0.9.25"))
    if key.startswith("setup-soldr-dylint-output-v2-"):
        return "-b58d3e55eb9d7747-" in key
    if key.startswith("setup-soldr-dylint-v2-"):
        return "-d153183e2b438407-" in key
    return any(f"-{digest}" in key for digest in OLD_DIGESTS)


def test_live_inventory_retires_every_superseded_same_lock_generation() -> None:
    caches = _live()
    plan = _pre_prune(caches)
    keys = {c["id"]: c["key"] for c in caches}
    retired = {keys[i] for i in plan["retireFirstIds"]}
    expected = {c["key"] for c in caches if _is_old(c["key"])}
    assert retired == expected, sorted(retired ^ expected)
    assert not any(digest in key for key in retired for digest in NEW_DIGESTS)


def test_live_inventory_fits_after_retire_first_with_test_binary_headroom() -> None:
    caches = _live()
    retired = set(_pre_prune(caches)["retireFirstIds"])
    plan = _pre_prune([c for c in caches if c["id"] not in retired])
    assert plan["ok"] is True, plan.get("reason")
    assert plan["projectedPeakBytes"] + TEST_BINARY_HEADROOM <= TARGET, plan["projectedPeakBytes"]


def test_cleanup_count_prune_retires_the_same_generations() -> None:
    caches = _live()
    result = _node(
        "(p,i)=>p.planCountPrune(i.caches,1,"
        f"{{linux:'{LOCK}',macos:'{LOCK}',windows:['{LOCK_CRLF}']}}).stale.map((c)=>c.key)",
        {"caches": caches},
    )
    expected = {c["key"] for c in caches if _is_old(c["key"])}
    assert expected <= set(result), sorted(expected - set(result))
    assert not any(digest in key for key in result for digest in NEW_DIGESTS)


def _row(i: int, key: str, created: str) -> dict:
    return {"id": i, "key": key, "ref": "refs/heads/main", "size_in_bytes": 100,
            "created_at": created, "last_accessed_at": created}


def _bc(i: int, digest: str, created: str, suffix: str = "") -> dict:
    mid = f"-{suffix}" if suffix else ""
    return _row(i, f"setup-soldr-buildcache-v2-linux-x64-{digest}{mid}-{LOCK}", created)


def _registry(i: int, digest: str, created: str) -> dict:
    return _row(i, f"setup-soldr-cargoregistry-v1-linux-x64-{LOCK}-{digest}", created)


def _minis() -> list[dict]:
    return [
        _row(90, "soldr-mini-v2-linux-x64-glibc-v0.9.25", "2026-09-28T00:00:00Z"),
        _row(91, "soldr-mini-v2-windows-x64-msvc-v0.9.27", "2026-10-02T21:02:00Z"),
        _row(92, "soldr-mini-v2-linux-x64-glibc-v0.9.27", "2026-10-02T20:55:00Z"),
    ]


def test_concurrent_current_generation_shapes_are_kept() -> None:
    # Two jobs share a family with distinct toolchain shapes, both saved in
    # the current soldr generation (one a little before soldr-mini landed).
    caches = _minis() + [
        _bc(1, "a" * 16, "2026-10-02T20:50:00Z"),
        _bc(2, "b" * 16, "2026-10-02T21:30:00Z"),
        _registry(3, "a" * 16, "2026-10-02T20:50:00Z"),
    ]
    # Only the superseded v0.9.25 soldr-mini goes.
    assert _pre_prune(caches)["retireFirstIds"] == [90]


def test_previous_generation_retires_once_its_family_is_reseeded() -> None:
    caches = _minis() + [
        _bc(1, "a" * 16, "2026-09-30T14:00:00Z"),
        _bc(2, "b" * 16, "2026-10-02T20:57:00Z"),
        # Previous generation whose producer has not re-seeded yet: kept.
        _bc(3, "c" * 16, "2026-09-30T14:00:00Z", "x"),
        _registry(4, "a" * 16, "2026-09-30T14:00:00Z"),
        _registry(5, "c" * 16, "2026-09-30T14:00:00Z"),
        _registry(6, "b" * 16, "2026-09-30T14:00:00Z"),
    ]
    assert _pre_prune(caches)["retireFirstIds"] == [1, 4, 90]


def test_no_soldr_mini_means_no_generation_rule() -> None:
    caches = [_bc(1, "a" * 16, "2026-09-30T14:00:00Z"), _bc(2, "b" * 16, "2026-10-02T20:57:00Z")]
    assert _pre_prune(caches)["retireFirstIds"] == []
