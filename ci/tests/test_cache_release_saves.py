"""Release builds save no Actions cache (2026-10-02, 9.95 GB live).

Auto-Release's eight cross-target builds saved 3.41 GB of ``soldr prepare``
compiler/SDK archives (``-xbuild-release-<target>``) and eight empty build
caches on the 1.15.1 release push. Only the next release can restore them,
days later, so they hold a third of the 10 GB budget for one hit per release
(ci.yml cache policy: cache only reusable cross-run inputs; CACHE-005 for the
empty entries). Release builds now save nothing, and the janitor retires the
existing entries.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "ci/tests/fixtures/cache_inventory_live_release_saves.json"
LOCK = "619d1c68483863f6"
LOCK_CRLF = "521c07ddcb0f5560"
HASHES = f"{{linux:'{LOCK}',macos:'{LOCK}',windows:['{LOCK_CRLF}']}}"
TARGET = 9_200_000_000
TEST_BINARY_HEADROOM = 900_000_000


def _live() -> list[dict]:
    return json.loads(FIXTURE.read_text(encoding="utf-8"))["actions_caches"]


def _node(expression: str, caches: list[dict]) -> object:
    script = (
        "const fs=require('node:fs');const p=require(process.argv[1]);"
        "const caches=JSON.parse(fs.readFileSync(0,'utf8'));"
        "const total=caches.reduce((s,c)=>s+c.size_in_bytes,0);"
        f"process.stdout.write(JSON.stringify({expression}));"
    )
    out = subprocess.run(
        ["node", "-e", script, str(ROOT / "ci/cache_cleanup_plan.js")],
        input=json.dumps(caches), check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def _is_release(key: str) -> bool:
    return "-xbuild-release-" in key or "-build-release-" in key


def test_release_build_caches_are_retired_main_keys() -> None:
    caches = _live()
    retired = set(_node(f"p.planLockTransitionPrePrune(caches,{HASHES},total,total).retireFirstIds", caches))
    release = {c["id"] for c in caches if _is_release(c["key"])}
    assert len(release) == 16
    assert release <= retired
    stale = set(_node(f"p.planCountPrune(caches,1,{HASHES}).stale.map((c)=>c.id)", caches))
    assert release <= stale
    # Check-lane prepare archives (`-xcheck-...`) are the 100%-hit musl caches.
    assert not any("-xcheck-" in c["key"] for c in caches if c["id"] in retired)


def test_live_inventory_fits_with_test_binary_headroom_after_retire_first() -> None:
    caches = _live()
    retired = set(_node(f"p.planLockTransitionPrePrune(caches,{HASHES},total,total).retireFirstIds", caches))
    rest = [c for c in caches if c["id"] not in retired]
    plan = _node(f"p.planLockTransitionPrePrune(caches,{HASHES},total,total)", rest)
    assert plan["ok"] is True, plan.get("reason")
    assert plan["projectedPeakBytes"] + TEST_BINARY_HEADROOM <= TARGET, plan["projectedPeakBytes"]


def test_release_builds_never_save_the_actions_cache() -> None:
    release = yaml.safe_load((ROOT / ".github/workflows/release-auto.yml").read_text(encoding="utf-8"))
    steps = [step for job in release["jobs"].values() for step in job.get("steps", [])]
    target = next(step for step in steps if step.get("uses") == "./.github/actions/build-target")
    assert target["with"]["save_cache"] == "false"
