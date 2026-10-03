"""Live-inventory coverage for the 2026-09-30 second pre-prune overrun (#1852).

After #1851 the projection was 9.49 GB against a 9.2 GB target. Sources of the
gap: superseded ``soldr-mini`` / ``prepare-v3`` generations were neither
deleted nor excluded, and the #1838 arm64 cook leg added a budgeted 277 MB.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
NEW_LOCK = "01fdce755221f8f4"
TARGET = 9_200_000_000
# `S:` abbreviates `setup-soldr-`. Main-ref entries of the live inventory
# (263 entries, 8.77 GB) at run 36717811611, after #1851's retire-first.
LIVE_INVENTORY: list[tuple[str, int, str]] = [
    ("cargo-registry-Linux-X64-bench-01fdce755221f8f4", 59_914_043, "2026-09-30T12:35:05.057169Z"),
    ("cargo-target-Linux-X64-bench-01fdce755221f8f4-f7ed4b0d4b5e42400108d2c7673c41ada02d8ba2", 239_375_644, "2026-09-30T12:35:10.040187Z"),
    ("cook-base-v2-linux-arm64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25", 276_840_157, "2026-09-30T07:32:10.712524Z"),
    ("cook-base-v2-linux-arm64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.26", 422_986_355, "2026-09-29T08:08:45.346849Z"),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-f6cafa616-lec2a428e3983361d-soldr0.9.25", 279_507_264, "2026-09-30T07:31:52.76077Z"),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-lec2a428e3983361d-soldrv0.9.26", 831_800_389, "2026-09-29T08:14:13.822416Z"),
    ("cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-lec2a428e3983361d-soldrv0.9.26", 679_752_798, "2026-09-29T08:11:19.80964Z"),
    ("S:buildcache-v2-linux-arm64-85229e05b0612895-ec2a428e3983361d", 19_980_156, "2026-09-30T06:41:39.848247Z"),
    ("S:buildcache-v2-linux-x64-223016ae85f4db41-dylint-ec2a428e3983361d", 546_146_899, "2026-09-30T06:49:03.211687Z"),
    ("S:buildcache-v2-linux-x64-27deaf711481c4fc-check-linux-arm-musl-ec2a428e3983361d", 214_747_857, "2026-09-30T06:41:10.906337Z"),
    ("S:buildcache-v2-linux-x64-6fef49df0527cdf1-ec2a428e3983361d", 226_727_779, "2026-09-30T06:43:27.098235Z"),
    ("S:buildcache-v2-linux-x64-85229e05b0612895-ec2a428e3983361d", 6_075_299, "2026-09-30T06:42:05.975546Z"),
    ("S:buildcache-v2-linux-x64-f87c9084b4c91b6a-check-linux-x86-musl-ec2a428e3983361d", 214_713_894, "2026-09-30T06:42:57.132392Z"),
    ("S:buildcache-v2-windows-arm64-4962042243664ed1-d2ef11473c3c1d13", 251, "2026-09-30T06:47:45.810483Z"),
    ("S:buildcache-v2-windows-x64-4962042243664ed1-d2ef11473c3c1d13", 243, "2026-09-30T06:48:08.622609Z"),
    ("S:buildcache-v2-windows-x64-9de1f1526ac8e278-d2ef11473c3c1d13", 418_706_703, "2026-09-30T06:50:22.750569Z"),
    ("S:cargoregistry-v1-linux-arm64-ec2a428e3983361d-5dd7376e6a94b983", 112_116_553, "2026-09-29T08:08:36.292738Z"),
    ("S:cargoregistry-v1-linux-arm64-ec2a428e3983361d-85229e05b0612895", 112_234_523, "2026-09-28T22:27:07.290223Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-27deaf711481c4fc", 112_275_245, "2026-09-28T22:25:56.741061Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-5dd7376e6a94b983", 117_336_295, "2026-09-29T08:10:59.938208Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-5f6e18fb2e53935b", 117_315_391, "2026-09-29T08:10:32.114311Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-6fef49df0527cdf1", 117_459_355, "2026-09-28T22:28:44.757457Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-76c7b3f7e670f4a2", 117_342_570, "2026-09-29T08:10:20.531394Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-85229e05b0612895", 110_679_362, "2026-09-28T22:28:11.735875Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-f87c9084b4c91b6a", 112_299_637, "2026-09-28T22:25:51.214911Z"),
    ("S:cargoregistry-v1-linux-x64-ec2a428e3983361d-ff7d446a118a3e0b", 117_302_342, "2026-09-29T08:10:42.962586Z"),
    ("S:cargoregistry-v1-macos-arm64-ec2a428e3983361d-6fef49df0527cdf1", 113_881_292, "2026-09-28T22:31:23.435472Z"),
    ("S:cargoregistry-v1-macos-arm64-ec2a428e3983361d-76c7b3f7e670f4a2", 113_900_263, "2026-09-29T08:11:14.63683Z"),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-031df19e2fdf1f1e", 159_220_635, "2026-09-29T08:15:18.094843Z"),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-4962042243664ed1", 159_218_745, "2026-09-28T22:55:07.86858Z"),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-697e5b4122d90f00", 159_224_202, "2026-09-29T08:13:21.717716Z"),
    ("S:cargoregistry-v1-windows-x64-d2ef11473c3c1d13-9de1f1526ac8e278", 159_302_901, "2026-09-28T22:35:41.51664Z"),
    ("S:dylint-output-v2-linux-x64-0b91abf489b62909-ec2a428e3983361d", 862_402_320, "2026-09-30T06:49:21.736766Z"),
    ("S:dylint-v2-linux-x64-x86_64-unknown-linux-gnu-d153183e2b438407-dylint", 549_203_070, "2026-09-30T06:49:10.620637Z"),
    ("S:prepare-v3-linux-x64-aarch64-unknown-linux-musl-re162f3a368305035-s129892c5510eebd1-xcheck-linux-arm-musl", 93_165_348, "2026-09-28T22:26:15.514432Z"),
    ("S:prepare-v3-linux-x64-aarch64-unknown-linux-musl-re162f3a368305035-s3361bb806daddd47-xcheck-linux-arm-musl", 93_161_476, "2026-09-29T08:10:44.885349Z"),
    ("S:prepare-v3-linux-x64-x86_64-unknown-linux-musl-re162f3a368305035-s129892c5510eebd1-xcheck-linux-x86-musl", 99_859_555, "2026-09-28T22:26:08.572442Z"),
    ("S:prepare-v3-linux-x64-x86_64-unknown-linux-musl-re162f3a368305035-s3361bb806daddd47-xcheck-linux-x86-musl", 99_895_669, "2026-09-29T08:10:51.112373Z"),
    ("soldr-mini-v2-linux-arm64-glibc-v0.9.25", 10_426_961, "2026-09-28T22:27:31.094589Z"),
    ("soldr-mini-v2-linux-arm64-glibc-v0.9.26", 10_509_860, "2026-09-29T08:09:00.37037Z"),
    ("soldr-mini-v2-linux-x64-glibc-v0.9.25", 10_778_826, "2026-09-28T22:26:06.28769Z"),
    ("soldr-mini-v2-linux-x64-glibc-v0.9.26", 10_861_506, "2026-09-29T08:08:55.782124Z"),
    ("soldr-mini-v2-macos-arm64-darwin-v0.9.25", 9_049_118, "2026-09-28T22:31:41.132036Z"),
    ("soldr-mini-v2-macos-arm64-darwin-v0.9.26", 9_123_634, "2026-09-29T08:09:25.194033Z"),
    ("soldr-mini-v2-windows-arm64-msvc-v0.9.25", 9_789_675, "2026-09-28T22:36:41.427809Z"),
    ("soldr-mini-v2-windows-arm64-msvc-v0.9.26", 9_851_731, "2026-09-29T08:15:01.932588Z"),
    ("soldr-mini-v2-windows-x64-msvc-v0.9.25", 10_597_358, "2026-09-28T22:36:02.312573Z"),
    ("soldr-mini-v2-windows-x64-msvc-v0.9.26", 10_675_932, "2026-09-29T08:13:41.779931Z"),
    ("zccache-Linux-X64-bench-f7ed4b0d4b5e42400108d2c7673c41ada02d8ba2", 244_412_525, "2026-09-30T12:35:02.746123Z"),
]
LIVE_EXTRA_BYTES = 180597786  # sccache, setup-uv, PR-ref leftovers


def _caches() -> list[dict[str, object]]:
    return [
        {
            "id": index + 1,
            "key": key.replace("S:", "setup-soldr-"),
            "ref": "refs/heads/main",
            "size_in_bytes": size,
            "created_at": created,
        }
        for index, (key, size, created) in enumerate(LIVE_INVENTORY)
    ]


def _plan(caches: list[dict[str, object]], extra: int = LIVE_EXTRA_BYTES) -> dict:
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


def test_live_9490_inventory_cannot_also_bootstrap_integration() -> None:
    caches = _caches()
    plan = _plan(caches)
    # The historical inventory fitted before Integration existed. Its new
    # 1.2 GB producer reservation must now refuse writes, not raise the target.
    assert plan["ok"] is False
    assert plan["projectedPeakBytes"] > TARGET
    assert plan["deleteIds"] == []
    keys = {c["id"]: c["key"] for c in caches}
    retired = sorted(keys[i] for i in plan["retireFirstIds"])
    # Older soldr-mini versions, older prepare-v3 signatures, and (#1875)
    # registries saved before the 0.9.26 generation whose family already holds
    # a 0.9.26-generation registry.
    previous_registry_digests = (
        "85229e05b0612895", "27deaf711481c4fc", "6fef49df0527cdf1", "f87c9084b4c91b6a",
        "4962042243664ed1", "9de1f1526ac8e278",
    )
    assert retired and all(
        k.startswith("soldr-mini-v2-") or "-prepare-v3-" in k or "f6cafa616" in k
        or (k.startswith("setup-soldr-cargoregistry-") and k.endswith(previous_registry_digests))
        for k in retired
    ), retired
    assert any("-v0.9.25" in k for k in retired)
    assert any("-s129892c5510eebd1-" in k for k in retired)
    # The retired arm64 f6cafa616 producer (#1838 leg) is dead weight too.
    assert any("linux-arm64" in k and "f6cafa616" in k for k in retired)
    assert not any("-v0.9.26" in k or "-s3361bb806daddd47-" in k for k in retired)


def test_superseded_generations_are_family_scoped_and_keep_the_newest() -> None:
    def mini(i: int, os_arch: str, version: str) -> dict[str, object]:
        return {"id": i, "key": f"soldr-mini-v2-{os_arch}-v{version}", "ref": "refs/heads/main",
                "size_in_bytes": 10, "created_at": "2026-09-30T00:00:00Z"}

    caches = [
        mini(1, "linux-x64-glibc", "0.9.25"), mini(2, "linux-x64-glibc", "0.9.26"),
        mini(3, "linux-arm64-glibc", "0.9.25"),  # only one in its family: kept
        mini(4, "linux-x64-glibc", "0.9.100"),  # numeric, not lexical, compare
    ]
    plan = _plan(caches, extra=0)
    assert plan["retireFirstIds"] == [1, 2]


def test_over_budget_live_footprint_still_fails_closed() -> None:
    caches = _caches() + [
        {"id": 9_999, "key": f"cook-base-v2-linux-x64-glibc-rustc1.95.0-fnone-l{NEW_LOCK}-soldrv0.9.26-bloat",
         "ref": "refs/heads/main", "size_in_bytes": 1_500_000_000,
         "created_at": "2026-09-30T00:00:00Z"}
    ]
    plan = _plan(caches)
    assert plan["ok"] is False and plan["deleteIds"] == []
    assert plan["retireFirstIds"]  # dead generations still reported for deletion
