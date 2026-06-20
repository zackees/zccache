"""IWYU subprocess driver + output parser.

include-what-you-use emits per-file sections of the form:

    <file>.cc should add these lines:
    #include <string>           // for std::string

    <file>.cc should remove these lines:
    - #include <vector>  // lines 3-3

    The full include-list for <file>.cc:
    #include <string>
    ---

We parse this into individual `IwyuItemResult` entries — one per
suggested change — so the runner can stream them as ResultItems with
`extra['action']` ∈ {"add", "remove", "keep"}.

Errors:
  - Tool spawn failures → INTERNAL (transient)
  - Mapping file failures → IWYU_CONFIG (deterministic)
  - Bad compile flags → COMPILE_FLAGS (deterministic)
  - Parse failures on the TU → PARSE_ERROR (deterministic)
  - SIGNAL / Timeout → transient
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

from zccache.cpp_lint._types import IwyuItem

_HEADER_ADD_RE = re.compile(r"^(.+?) should add these lines:")
_HEADER_REMOVE_RE = re.compile(r"^(.+?) should remove these lines:")
_HEADER_FULL_RE = re.compile(r"^The full include-list for (.+?):")
_INCLUDE_LINE_RE = re.compile(
    r"^(?P<lead>[- ]?)#include\s+(?P<spelling>[\"<][^\">]+[\">])(?P<rest>.*)$"
)
_REASON_RE = re.compile(r"//\s*(?:for\s+)?(?P<reason>.+?)\s*$")


@dataclass(frozen=True)
class IwyuItemResult:
    """One suggestion line from IWYU."""

    path: str          # the file the recommendation is for
    action: str        # "add" | "remove" | "keep"
    spelling: str      # the #include token, including angle brackets / quotes
    reason: str        # human-readable; empty when not provided


@dataclass(frozen=True)
class IwyuRun:
    """Result of running IWYU against one TU."""

    items: tuple[IwyuItemResult, ...]
    error_kind: str | None
    error_message: str
    exit_code: int


def run_iwyu(
    iwyu_path: Path,
    tu: Path,
    item: IwyuItem,
    compile_args: tuple[str, ...],
    default_mapping_files: tuple[Path, ...] = (),
    extra_iwyu_args: tuple[str, ...] = (),
    timeout_seconds: float = 60.0,
) -> IwyuRun:
    """Invoke IWYU against `tu` with `item`'s mapping/config.

    `compile_args` is the compile-commands.json arguments list for this TU.
    """
    mapping_files = (*default_mapping_files, *item.mapping_files)

    cmd: list[str] = [str(iwyu_path)]
    # IWYU's "Xiwyu" prefix delivers options to iwyu, not the compiler.
    for mf in mapping_files:
        cmd += ["-Xiwyu", f"--mapping_file={mf}"]
    if item.pch_in_code:
        cmd += ["-Xiwyu", "--pch_in_code"]
    for arg in item.extra_args:
        cmd += ["-Xiwyu", arg]
    for arg in extra_iwyu_args:
        cmd += ["-Xiwyu", arg]
    cmd.append(str(tu))
    # Pass the compile flags after `--` so iwyu hands them to the
    # internal clang frontend.
    cmd.append("--")
    cmd.extend(compile_args)

    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return IwyuRun((), "TIMEOUT", f"iwyu timed out after {timeout_seconds:.0f}s", -1)
    except OSError as exc:
        return IwyuRun((), "INTERNAL", f"iwyu spawn failed: {exc}", -1)

    # IWYU writes recommendations to stderr by default. Some builds use
    # stdout. Concatenate both for parsing.
    output = (proc.stdout or "") + "\n" + (proc.stderr or "")
    items = _parse_iwyu_output(output)

    # IWYU's exit code is unusual: 0 means "no recommendations" (clean),
    # >0 typically means "had recommendations" OR "error". We treat
    # non-zero exit code with no recommendations parsed as an error.
    if proc.returncode != 0 and not items:
        return IwyuRun(
            items=(),
            error_kind=_classify_iwyu_error(proc.stderr),
            error_message=(proc.stderr.strip().splitlines() or [""])[-1],
            exit_code=proc.returncode,
        )

    return IwyuRun(items=items, error_kind=None, error_message="", exit_code=proc.returncode)


def _classify_iwyu_error(stderr: str) -> str:
    text = stderr.lower()
    if "mapping_file" in text and ("error" in text or "cannot" in text):
        return "IWYU_CONFIG"
    if "unrecognized" in text and "flag" in text:
        return "COMPILE_FLAGS"
    if "fatal error" in text or "error: " in text:
        return "PARSE_ERROR"
    return "INTERNAL"


def _parse_iwyu_output(text: str) -> tuple[IwyuItemResult, ...]:
    items: list[IwyuItemResult] = []
    current_path: str | None = None
    section: str | None = None  # "add" | "remove" | "full" | None

    for raw in text.splitlines():
        line = raw.rstrip()
        if not line:
            continue

        m = _HEADER_ADD_RE.match(line)
        if m:
            current_path = m.group(1).strip()
            section = "add"
            continue
        m = _HEADER_REMOVE_RE.match(line)
        if m:
            current_path = m.group(1).strip()
            section = "remove"
            continue
        m = _HEADER_FULL_RE.match(line)
        if m:
            current_path = m.group(1).strip()
            section = "full"
            continue
        if line.startswith("---"):
            section = None
            continue

        if current_path is None or section is None:
            continue

        # Try to parse an include line.
        m = _INCLUDE_LINE_RE.match(line.strip())
        if not m:
            continue
        spelling = m.group("spelling")
        rest = m.group("rest")
        reason_match = _REASON_RE.search(rest)
        reason = reason_match.group("reason") if reason_match else ""

        if section == "add":
            items.append(
                IwyuItemResult(
                    path=current_path, action="add", spelling=spelling, reason=reason
                )
            )
        elif section == "remove":
            items.append(
                IwyuItemResult(
                    path=current_path, action="remove", spelling=spelling, reason=reason
                )
            )
        elif section == "full":
            items.append(
                IwyuItemResult(
                    path=current_path, action="keep", spelling=spelling, reason=reason
                )
            )

    return tuple(items)


def apply_iwyu_fixes(
    fix_includes_path: Path,
    iwyu_run: IwyuRun,
) -> tuple[str, ...]:
    """Apply IWYU's fix_includes.py to the per-file recommendations.

    Returns the tuple of file paths that were modified. Best-effort —
    on failure returns ().
    """
    # We invoke `fix_includes.py` by piping the original IWYU output back
    # in via stdin (its documented interface).
    text = "\n".join(
        f"{r.path} should add these lines:\n#include {r.spelling}"
        if r.action == "add"
        else f"{r.path} should remove these lines:\n- #include {r.spelling}"
        for r in iwyu_run.items
        if r.action in ("add", "remove")
    )
    if not text:
        return ()
    try:
        proc = subprocess.run(
            [str(fix_includes_path)],
            input=text,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return ()
    if proc.returncode != 0:
        return ()
    # fix_includes.py prints `IWYU edited <N> files` and the file list;
    # parse loosely.
    out: list[str] = []
    for line in proc.stdout.splitlines():
        s = line.strip()
        if s.endswith(".cc") or s.endswith(".cpp") or s.endswith(".h") or s.endswith(".hpp"):
            out.append(s)
    return tuple(out)


__all__ = ["IwyuItemResult", "IwyuRun", "apply_iwyu_fixes", "run_iwyu"]
