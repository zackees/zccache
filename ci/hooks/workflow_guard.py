#!/usr/bin/env python3
"""PostToolUse hook: run the setup-soldr cache-footprint guard on workflow edits.

`ci/check_cache_footprint.py` blocks cache regressions (retired cache
families, PR-reachable saves, non-Linux cook producers), but it only ran in
CI's Static checks job. An agent could therefore edit a workflow, report the
change as done, and never see the guard fail (#1760).

This hook runs after every Edit/Write/Bash tool call. It is a no-op unless a
workflow, a composite action, `action.yml`, or the cache planner/guard itself
is edited or dirty in the working tree, so ordinary calls pay one `git status`.
Bash edits (sed, heredocs) are covered through `git status`, not the tool's
file path.

Exit codes:
  0 - nothing relevant changed, or the guard passed
  2 - the guard failed (stderr is fed back to the agent)
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

WATCHED_PREFIXES = (".github/workflows/", ".github/actions/")
WATCHED_FILES = {
    "action.yml",
    "action/cleanup/action.yml",
    "ci/cache_cleanup_plan.js",
    "ci/check_cache_footprint.py",
}


def is_watched(relative: str) -> bool:
    relative = relative.replace("\\", "/").removeprefix("./")
    return relative in WATCHED_FILES or relative.startswith(WATCHED_PREFIXES)


def _relative(path: str, root: Path) -> str | None:
    try:
        return Path(path).resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None


def dirty_watched_paths(root: Path) -> list[str]:
    """Watched paths modified or untracked relative to HEAD."""
    result = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain", "--", *WATCHED_PREFIXES, *WATCHED_FILES],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return []
    return [line[3:].strip() for line in result.stdout.splitlines() if line.strip()]


def relevant(payload: dict, root: Path) -> bool:
    file_path = (payload.get("tool_input") or {}).get("file_path")
    if file_path:
        relative = _relative(file_path, root)
        if relative and is_watched(relative):
            return True
    return bool(dirty_watched_paths(root))


def run_guard(root: Path) -> list[str]:
    # Import the guard from this checkout, then check the given root (tests
    # point it at a scratch repository).
    if str(ROOT) not in sys.path:
        sys.path.insert(0, str(ROOT))
    from ci import check_cache_footprint

    return check_cache_footprint.check(root)


def main(stdin: str, root: Path = ROOT) -> int:
    try:
        payload = json.loads(stdin) if stdin.strip() else {}
    except json.JSONDecodeError:
        payload = {}
    if not relevant(payload, root):
        return 0
    errors = run_guard(root)
    if not errors:
        return 0
    print("setup-soldr cache-footprint guard failed (ci/check_cache_footprint.py):", file=sys.stderr)
    for error in errors:
        print(f"  error: {error}", file=sys.stderr)
    print("Fix the workflow before reporting the change as done (#1760).", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.stdin.read()))
