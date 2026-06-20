"""Resolution of the polymorphic `ListOrPath` type.

Accepts:
  - a single glob string
  - a tuple of glob strings / explicit Paths
  - a Path to a text file with one entry per line (# comments OK)

All three forms normalize to `tuple[str, ...]` of pattern lines.
"""

from __future__ import annotations

from pathlib import Path
from typing import Union

ListOrPath = Union[str, Path, tuple[Union[str, Path], ...]]


def resolve_to_lines(value: ListOrPath | None) -> tuple[str, ...]:
    """Normalize a `ListOrPath` to a flat tuple of pattern strings.

    None → ().
    str  → (str,).
    Path → if the path exists, read it as a one-glob-per-line file
           with `#` comments. If the path doesn't exist, treat the
           literal string form as a glob (this is the explicit-path
           shorthand: `Path("src/legacy.h")` is treated as the glob
           `src/legacy.h`).
    tuple → recurse over each element and flatten.
    """
    if value is None:
        return ()
    if isinstance(value, str):
        return (value,)
    if isinstance(value, Path):
        if value.is_file():
            return _read_pattern_file(value)
        return (str(value),)
    if isinstance(value, tuple):
        out: list[str] = []
        for part in value:
            out.extend(resolve_to_lines(part))
        return tuple(out)
    raise TypeError(  # pragma: no cover - exhaustive guard
        f"ListOrPath: expected str | Path | tuple, got {type(value).__name__}"
    )


def _read_pattern_file(path: Path) -> tuple[str, ...]:
    out: list[str] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        out.append(line)
    return tuple(out)
