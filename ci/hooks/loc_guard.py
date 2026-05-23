#!/usr/bin/env python3
"""PostToolUse hook: enforces line-count budget on source files.

After any Edit/Write to a source file, counts the resulting line count and:
  - emits a warning (exit 0 + stderr) when LOC > WARN_THRESHOLD
  - hard-errors (exit 2 + stderr) when LOC > ERROR_THRESHOLD

Tracks both /loop-style refactor pressure and stops files from growing
back to monolithic sizes after a split.

Exit codes:
  0 - file is fine, missing, not a tracked source extension, or only warned
  2 - file exceeds ERROR_THRESHOLD (stderr fed back to Claude as a block)
"""

import json
import os
import sys
from pathlib import Path

WARN_THRESHOLD = 1000
ERROR_THRESHOLD = 1500

SOURCE_EXTS = {".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".kt", ".swift", ".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp"}

EXCLUDED_DIRS = {".git", "target", ".cargo", ".rustup", ".venv", "node_modules", "__pycache__", "dist", ".claude"}


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0

    file_path = data.get("tool_input", {}).get("file_path", "")
    if not file_path:
        return 0

    norm = file_path.replace("\\", "/")
    ext = os.path.splitext(norm)[1].lower()
    if ext not in SOURCE_EXTS:
        return 0

    parts = Path(norm).parts
    if any(part in EXCLUDED_DIRS for part in parts):
        return 0

    if not os.path.isfile(file_path):
        return 0

    try:
        with open(file_path, "rb") as f:
            loc = sum(1 for _ in f)
    except OSError:
        return 0

    if loc > ERROR_THRESHOLD:
        print(
            f"LOC guard: {file_path} is {loc} lines (>{ERROR_THRESHOLD}). "
            f"Split it into focused submodules before continuing.",
            file=sys.stderr,
        )
        return 2

    if loc > WARN_THRESHOLD:
        print(
            f"LOC guard warning: {file_path} is {loc} lines (>{WARN_THRESHOLD}). "
            f"Consider splitting it before it crosses {ERROR_THRESHOLD}.",
            file=sys.stderr,
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
