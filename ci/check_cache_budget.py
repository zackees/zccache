"""Fail when the repository's total Actions-cache usage exceeds a budget.

GitHub evicts least-recently-used entries once a repository passes 10 GB, so
the warm cook bases and toolchain caches start disappearing well before the
limit is visible anywhere.  This check fails above ``BUDGET_BYTES`` (9.5 GB)
and lists the largest entries by ref so the producer can be found
(zccache#1677, zackees/setup-soldr#528).

It needs the network and a token, so it runs from a scheduled / main-only
workflow step, not from the offline lint.

Usage: ``GH_TOKEN=... uv run --no-project python ci/check_cache_budget.py [owner/repo]``
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

BUDGET_BYTES = int(9.5 * 1000**3)
TOP = 10


def _gb(size: int) -> str:
    return f"{size / 1000**3:.2f} GB"


def evaluate(usage: dict, caches: list[dict], budget: int = BUDGET_BYTES) -> list[str]:
    """Return error lines; empty when ``usage`` is within ``budget``."""
    total = int(usage.get("active_caches_size_in_bytes", 0))
    if total <= budget:
        return []
    errors = [
        f"repository cache usage {_gb(total)} across "
        f"{usage.get('active_caches_count', '?')} entries exceeds the "
        f"{_gb(budget)} budget"
    ]
    largest = sorted(caches, key=lambda c: int(c.get("size_in_bytes", 0)), reverse=True)
    for entry in largest[:TOP]:
        errors.append(
            f"  {_gb(int(entry.get('size_in_bytes', 0)))}  {entry.get('ref', '?')}  "
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
    errors = evaluate(usage, caches)
    for line in errors:
        print(f"error: {line}", file=sys.stderr)
    if not errors:
        total = int(usage.get("active_caches_size_in_bytes", 0))
        print(f"cache budget: ok ({_gb(total)} of {_gb(BUDGET_BYTES)})")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
