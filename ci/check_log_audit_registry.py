"""Verify the checked-in log-audit registry matches the Rust catalog.

Mirrors the wire snapshot guard: the checked-in JSON makes review diffs clear,
while Rust remains the sole owner of rule construction and semantics.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "ci" / "log_audit_registry.json"


def main() -> int:
    result = subprocess.run(
        ["soldr", "cargo", "run", "-p", "zccache", "--features", "ci-bin", "--bin", "zccache-ci", "--", "dump-registry"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        sys.stderr.write(result.stderr)
        return result.returncode
    expected = SNAPSHOT.read_text(encoding="utf-8")
    if result.stdout != expected:
        sys.stderr.write("log-audit registry drift; update ci/log_audit_registry.json from zccache-ci dump-registry\n")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
