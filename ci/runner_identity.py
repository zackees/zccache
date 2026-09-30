"""Parse the runner-identity block that `ci/runner_identity.sh` logs (#1861, #1807).

Perf Guard prints, once per job, a delimited block of `runner.<key>=<value>`
lines plus a closing `runner-identity-delta` line. This module turns a raw
GitHub Actions log (timestamp prefixes and all) back into dictionaries so an
analysis script can group attempts by runner CPU model. Observability only:
nothing here feeds a threshold or pass/fail decision.
"""

from __future__ import annotations

import re
from typing import Any

BEGIN_MARKER = "runner-identity-begin"
END_MARKER = "runner-identity-end"
DELTA_MARKER = "runner-identity-delta"
KEY_PREFIX = "runner."

_INT_KEYS = frozenset(
    {"logical_cores", "mem_total_kb", "steal_ticks_begin", "total_ticks_begin"}
)
_FLOAT_KEYS = frozenset({"load1_begin", "load5_begin"})
_DELTA_INT_KEYS = frozenset({"steal_ticks", "total_ticks"})
_DELTA_FLOAT_KEYS = frozenset({"steal_pct", "load1_end", "load5_end"})
# `2026-09-30T12:00:00.1234567Z ` prefix on raw Actions log lines.
_TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T[\d:.]+Z\s+")


def _coerce(value: str, *, ints: frozenset[str], floats: frozenset[str], key: str) -> Any:
    try:
        if key in ints:
            return int(value)
        if key in floats:
            return float(value)
    except ValueError:
        pass  # "unknown" stays a string
    return value


def _clean(line: str) -> str:
    return _TIMESTAMP.sub("", line.rstrip("\r\n").strip())


def parse_runner_identity(text: str) -> list[dict[str, Any]]:
    """Return one dict per runner-identity block found in `text`.

    Each dict holds the identity keys (without the `runner.` prefix, numeric
    fields converted) and, when a `runner-identity-delta` line followed the
    block, a nested `delta` dict. Blocks that never reach the end marker are
    skipped. Returns `[]` when the log has no block.
    """
    blocks: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for raw in text.splitlines():
        line = _clean(raw)
        if line.startswith(BEGIN_MARKER):
            current = {}
        elif line.startswith(END_MARKER):
            if current is not None:
                blocks.append(current)
            current = None
        elif current is not None:
            if line.startswith(KEY_PREFIX) and "=" in line:
                key, _, value = line[len(KEY_PREFIX):].partition("=")
                current[key] = _coerce(value, ints=_INT_KEYS, floats=_FLOAT_KEYS, key=key)
        elif line.startswith(DELTA_MARKER) and blocks:
            delta: dict[str, Any] = {}
            for token in line.split()[1:]:
                key, sep, value = token.partition("=")
                if sep and key != "v":
                    delta[key] = _coerce(
                        value, ints=_DELTA_INT_KEYS, floats=_DELTA_FLOAT_KEYS, key=key
                    )
            blocks[-1]["delta"] = delta
    return blocks


def group_by_cpu_model(blocks: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    """Group parsed blocks by `cpu_model` (missing model groups as "unknown")."""
    groups: dict[str, list[dict[str, Any]]] = {}
    for block in blocks:
        groups.setdefault(str(block.get("cpu_model", "unknown")), []).append(block)
    return groups
