#!/usr/bin/env python3
"""Remove only cache telemetry that ``zccache-ci audit-logs`` consumes.

The integration workflow seeds a warm cache, so its runtime telemetry must be
cleared without deleting the cached artifacts that make the test phase warm.
Keep these filename rules aligned with ``zccache-audit::classify_source``.
"""

from __future__ import annotations

import argparse
from pathlib import Path


def _is_audit_source(path: Path) -> bool:
    """Whether the Rust log audit classifies ``path`` as telemetry."""
    name = path.name
    return (
        name == "audit.jsonl"
        or name.startswith(("audit.jsonl.", "daemon.log."))
        or name.endswith(".jsonl")
        or ".jsonl." in name
        or (name.startswith("daemon-lifecycle") and ".log" in name)
        or name == "daemon.log"
    )


def clear_runtime_telemetry(cache_root: Path) -> list[Path]:
    """Delete audit input files below ``cache_root``, preserving cache data."""
    if not cache_root.is_dir():
        return []

    removed = []
    for path in cache_root.rglob("*"):
        if path.is_symlink() or not path.is_file() or not _is_audit_source(path):
            continue
        path.unlink()
        removed.append(path)
    return removed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache-root", type=Path, required=True)
    args = parser.parse_args()

    removed = clear_runtime_telemetry(args.cache_root)
    print(f"clear_runtime_telemetry: removed {len(removed)} audit telemetry file(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
