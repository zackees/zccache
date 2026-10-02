"""Fail when the repository's total Actions-cache usage exceeds a budget.

GitHub evicts least-recently-used entries once a repository passes 10 GB, so
the warm cook bases and toolchain caches start disappearing well before the
limit is visible anywhere.  This check fails above ``BUDGET_BYTES`` (9.5 GB)
and lists the largest entries by ref so the producer can be found
(zccache#1677, zackees/setup-soldr#528).

It needs the network and a token, so it runs from a scheduled / main-only
workflow step, not from the offline lint.

It also holds each main-ref cache family to a per-family budget sized for
one live generation (``FAMILY_BUDGETS``), so a second generation of the same
family (zccache#1875: soldr 0.9.25 and 0.9.27 side by side, 12.41 GB) is
named by family before the total ever reaches GitHub's LRU eviction.

Usage: ``GH_TOKEN=... uv run --no-project python ci/check_cache_budget.py [owner/repo]``
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

BUDGET_BYTES = int(9.5 * 1000**3)
TOP = 10

# Main-ref key prefix of each budgeted family (ci.yml's `[cache.family]`
# shapes, docs/ci-toml.md "Family resolution table").
FAMILY_PREFIXES: dict[str, tuple[str, ...]] = {
    "cook-base": ("cook-base-v2-",),
    "build-cache": ("setup-soldr-buildcache-v2-",),
    "cargo-registry": ("setup-soldr-cargoregistry-",),
    "dylint-output": ("setup-soldr-dylint-output-v2-",),
    "dylint": ("setup-soldr-dylint-v2-",),
    "cross-targets": ("setup-soldr-prepare-v3-",),
    "soldr-mini": ("soldr-mini-v2-",),
}

# One live generation per family, measured on 2026-10-02 (soldr 0.9.27, after
# #1875 retired 0.9.25) with ~10% headroom:
#   cook-base       1.52 GB (f9e7e4902 0.83, fnone 0.41, f6cafa616 0.28)
#   build-cache     1.71 GB, plus ~0.9 GB of #1550 debug test binaries
#   cargo-registry  0.90 GB (one entry per toolchain digest: 4 linux, 2 windows, 1 macos)
#   dylint-output   0.86 GB (ci/check_cache_footprint.py caps it at 0.9 GB)
#   dylint          0.55 GB
#   cross-targets   0.19 GB (two musl prepare archives)
#   soldr-mini      0.04 GB (four platforms)
FAMILY_BUDGETS: dict[str, int] = {
    "cook-base": 1_700_000_000,
    "build-cache": 2_900_000_000,
    "cargo-registry": 1_000_000_000,
    "dylint-output": 900_000_000,
    "dylint": 600_000_000,
    "cross-targets": 250_000_000,
    "soldr-mini": 60_000_000,
}
# Everything else (setup-uv, bench, per-commit action entries, PR refs) shares
# what is left of the total; check_cache_footprint keeps PR saves off.
OTHER_BUDGET_BYTES = BUDGET_BYTES - sum(FAMILY_BUDGETS.values())


def _gb(size: int) -> str:
    return f"{size / 1000**3:.2f} GB"


def evaluate(usage: dict, caches: list[dict], budget: int = BUDGET_BYTES) -> list[str]:
    """Return error lines; empty when both cache inventory views are in budget."""
    listed_total = sum(int(cache.get("size_in_bytes", 0)) for cache in caches)
    total = max(int(usage.get("active_caches_size_in_bytes", 0)), listed_total)
    if total <= budget:
        return []
    errors = [
        f"repository cache usage {_gb(total)} across at least "
        f"{max(int(usage.get('active_caches_count', 0)), len(caches))} entries exceeds the "
        f"{_gb(budget)} budget"
    ]
    largest = sorted(caches, key=lambda c: int(c.get("size_in_bytes", 0)), reverse=True)
    for entry in largest[:TOP]:
        errors.append(
            f"  {_gb(int(entry.get('size_in_bytes', 0)))}  {entry.get('ref', '?')}  "
            f"{entry.get('key', '?')}"
        )
    return errors


def _family(key: str) -> str | None:
    for family, prefixes in FAMILY_PREFIXES.items():
        if key.startswith(prefixes):
            return family
    return None


def evaluate_families(caches: list[dict]) -> list[str]:
    """Return one error per main-ref family over its ``FAMILY_BUDGETS`` entry."""
    totals: dict[str, int] = {}
    members: dict[str, list[dict]] = {}
    for cache in caches:
        if cache.get("ref") != "refs/heads/main":
            continue
        family = _family(str(cache.get("key", "")))
        if family is None:
            continue
        totals[family] = totals.get(family, 0) + int(cache.get("size_in_bytes", 0))
        members.setdefault(family, []).append(cache)
    errors = []
    for family, total in sorted(totals.items()):
        limit = FAMILY_BUDGETS[family]
        if total <= limit:
            continue
        errors.append(
            f"family {family} holds {_gb(total)} in {len(members[family])} main entries, over its "
            f"{_gb(limit)} one-generation budget (a superseded generation still live?)"
        )
        oldest = sorted(members[family], key=lambda c: str(c.get("created_at", "")))
        for entry in oldest[:TOP]:
            errors.append(
                f"  {_gb(int(entry.get('size_in_bytes', 0)))}  created {entry.get('created_at', '?')}  "
                f"{entry.get('key', '?')}"
            )
    return errors


def _gh(path: str) -> object:
    out = subprocess.run(
        ["gh", "api", "--paginate", "--slurp", path],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)


def main(argv: list[str]) -> int:
    repo = argv[1] if len(argv) > 1 else os.environ.get("GITHUB_REPOSITORY", "")
    if not repo:
        print("error: pass owner/repo or set GITHUB_REPOSITORY", file=sys.stderr)
        return 2
    usage = _gh(f"repos/{repo}/actions/cache/usage")[0]
    pages = _gh(f"repos/{repo}/actions/caches?per_page=100")
    caches = [c for page in pages for c in page.get("actions_caches", [])]
    errors = evaluate(usage, caches) + evaluate_families(caches)
    for line in errors:
        print(f"error: {line}", file=sys.stderr)
    if not errors:
        total = max(
            int(usage.get("active_caches_size_in_bytes", 0)),
            sum(int(cache.get("size_in_bytes", 0)) for cache in caches),
        )
        print(f"cache budget: ok ({_gb(total)} of {_gb(BUDGET_BYTES)})")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
