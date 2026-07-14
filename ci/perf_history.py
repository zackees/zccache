"""Audit performance threshold history and enforce ratchet evidence."""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path
from typing import Any

TRACKED_PATHS = (
    ".github/workflows/perf-rust-cluster.yml",
    "PERF.md",
    "ci/perf_local.py",
    "ci/perf_thresholds.json",
)
RELAXATION_KEYS = ("minimum_speedup", "maximum_warm_ms", "maximum_staged_overhead_ms")


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def history_inventory(repo: Path) -> list[dict[str, Any]]:
    """Return every commit touching the threshold/provenance surfaces."""
    raw = _git(repo, "log", "--format=%H%x00%s", "--name-only", "--", *TRACKED_PATHS)
    rows: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for line in raw.splitlines():
        if "\x00" in line:
            commit, subject = line.split("\x00", 1)
            current = {"commit": commit, "subject": subject, "paths": []}
            rows.append(current)
        elif current is not None and line in TRACKED_PATHS and line not in current["paths"]:
            current["paths"].append(line)
    for row in rows:
        patch = _git(repo, "show", "--format=", "--unified=0", row["commit"], "--", *row["paths"])
        row["threshold_lines"] = [
            line
            for line in patch.splitlines()
            if line[:1] in "+-" and not line.startswith(("+++", "---"))
            and any(key in line for key in RELAXATION_KEYS)
        ]
    return rows


def _flatten(payload: Any, prefix: str = "") -> dict[str, int | float | None]:
    values: dict[str, int | float | None] = {}
    if isinstance(payload, dict):
        for key, value in payload.items():
            values.update(_flatten(value, f"{prefix}.{key}" if prefix else key))
    elif payload is None or isinstance(payload, (int, float)) and not isinstance(payload, bool):
        values[prefix] = payload
    return values


def manifest_relaxations(old: dict[str, Any], new: dict[str, Any]) -> list[dict[str, Any]]:
    """Identify changes that make timing acceptance less strict."""
    old_values = _flatten(old)
    new_values = _flatten(new)
    relaxations: list[dict[str, Any]] = []
    for key in sorted(set(old_values) & set(new_values)):
        before, after = old_values[key], new_values[key]
        if before is None or after is None or not isinstance(before, (int, float)) or not isinstance(after, (int, float)):
            continue
        is_floor = key.endswith("minimum_speedup")
        is_ceiling = "maximum_warm_ms" in key or "maximum_staged_overhead_ms" in key
        if (is_floor and after < before) or (is_ceiling and after > before):
            relaxations.append({"path": key, "old": before, "new": after})
    return relaxations


def validate_ratchet(
    old: dict[str, Any], new: dict[str, Any], evidence: dict[str, Any] | None
) -> list[str]:
    """Return violations for an unexplained threshold relaxation."""
    relaxations = manifest_relaxations(old, new)
    if not relaxations:
        return []
    if not isinstance(evidence, dict):
        return ["threshold relaxation requires an evidence object"]
    required = ("issue", "samples", "rationale")
    missing = [key for key in required if not evidence.get(key)]
    if missing:
        return [f"threshold relaxation evidence missing: {', '.join(missing)}"]
    return []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path)
    parser.add_argument("--old", type=Path, help="old threshold manifest for ratchet checking")
    parser.add_argument("--new", type=Path, help="new threshold manifest for ratchet checking")
    parser.add_argument("--evidence", type=Path, help="JSON evidence for an intentional relaxation")
    args = parser.parse_args()
    if args.old or args.new:
        if not args.old or not args.new:
            parser.error("--old and --new must be supplied together")
        old = json.loads(args.old.read_text(encoding="utf-8"))
        new = json.loads(args.new.read_text(encoding="utf-8"))
        evidence = json.loads(args.evidence.read_text(encoding="utf-8")) if args.evidence else None
        violations = validate_ratchet(old, new, evidence)
        for violation in violations:
            print(f"RATCHET FAIL: {violation}")
        return 1 if violations else 0
    payload = history_inventory(args.repo)
    rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
