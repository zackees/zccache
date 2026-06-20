"""clang-query subprocess driver + output parser.

Two responsibilities:

  1. Build the combined matcher script for a TU given the set of
     `AstQuery` items applicable to that TU. The combined script binds
     each matcher's results under the query's `name` so the parser can
     route output back to the right item.

  2. Spawn `clang-query -p <compile_commands.json> <tu>` with the
     script piped on stdin, capture stdout/stderr, parse hit lines into
     RawResult dicts.

clang-query's diagnostic output format (with `set output diag`) is:

    <path>:<line>:<col>: note: "<bind_name>" binds here

Surrounding context lines exist but the `note:` line is the canonical
hit. We grep for it with a single regex.

Errors:
  - Exit code != 0 with stderr mentioning "matcher" syntax → MATCHER_SYNTAX
  - Exit code != 0 with stderr mentioning parse failures → PARSE_ERROR
  - SIGNAL / Timeout / other → transient (not cached)
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

from zccache.cpp_lint._types import AstQuery

_HIT_RE = re.compile(
    # `.+?` is non-greedy so the path absorbs Windows drive letters
    # (e.g. `C:\foo\bar.cpp`) without stopping at the first colon.
    r"^(?P<path>.+?):(?P<line>\d+):(?P<col>\d+):\s+note:\s+"
    r'"(?P<bind>[A-Za-z_][A-Za-z0-9_]*)" binds here',
)


@dataclass(frozen=True)
class ClangQueryRawHit:
    """One match line from clang-query, pre-classification."""

    path: str
    line: int
    column: int
    bind_name: str  # corresponds to AstQuery.name
    message: str    # the surrounding text (best-effort)


@dataclass(frozen=True)
class ClangQueryRun:
    """Result of running clang-query against one TU."""

    hits: tuple[ClangQueryRawHit, ...]
    error_kind: str | None  # None on success
    error_message: str
    exit_code: int


def build_combined_script(
    queries: tuple[AstQuery, ...],
    let_bindings: tuple[str, ...] = (),
    output_mode: str = "diag",
) -> str:
    """Compose a single clang-query script that runs every matcher in `queries`.

    Each matcher's bind label is set to the query's name so the parser
    can route results back. Order is the input order (callers using
    `order=True` rely on this for stability).
    """
    out: list[str] = []
    out.append(f"set output {output_mode}")
    for binding in let_bindings:
        out.append(binding.strip())
    for q in queries:
        body = _matcher_body_text(q.matcher_body)
        # Inject the bind name. We assume the matcher body is well-formed
        # clang-query syntax — we DON'T attempt to rewrite it. We do
        # prepend a `let` so the matcher's top-level bind label is
        # consistently `<name>`. If the matcher already has its own bind
        # call, both bindings appear in output and parsing still works.
        out.append(f"# AstQuery {q.name}")
        out.append(body)
    return "\n".join(out) + "\n"


def _matcher_body_text(body: object) -> str:
    if isinstance(body, str):
        return body
    if isinstance(body, Path):
        return body.read_text(encoding="utf-8")
    raise TypeError(f"unsupported matcher_body type: {type(body).__name__}")


def run_clang_query(
    clang_query_path: Path,
    tu: Path,
    compile_commands: Path,
    script: str,
    extra_args: tuple[str, ...] = (),
    timeout_seconds: float = 60.0,
) -> ClangQueryRun:
    """Invoke clang-query against `tu` with the combined `script`.

    Returns a ClangQueryRun with parsed hits and any error info.
    """
    cmd = [
        str(clang_query_path),
        "-p",
        str(compile_commands.parent if compile_commands.is_file() else compile_commands),
        *extra_args,
        str(tu),
    ]
    try:
        proc = subprocess.run(
            cmd,
            input=script,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return ClangQueryRun(
            hits=(),
            error_kind="TIMEOUT",
            error_message=f"clang-query timed out after {timeout_seconds:.0f}s",
            exit_code=-1,
        )
    except OSError as exc:
        return ClangQueryRun(
            hits=(),
            error_kind="INTERNAL",
            error_message=f"clang-query spawn failed: {exc}",
            exit_code=-1,
        )

    if proc.returncode != 0:
        return ClangQueryRun(
            hits=(),
            error_kind=_classify_clang_query_error(proc.stderr),
            error_message=proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() else "",
            exit_code=proc.returncode,
        )

    hits = _parse_hits(proc.stdout)
    return ClangQueryRun(
        hits=hits,
        error_kind=None,
        error_message="",
        exit_code=0,
    )


def _classify_clang_query_error(stderr: str) -> str:
    text = stderr.lower()
    if "no matcher named" in text or "expected matcher name" in text:
        return "MATCHER_SYNTAX"
    if "error: " in text and (".cpp" in text or ".cxx" in text or ".cc" in text or ".h" in text):
        return "PARSE_ERROR"
    if "matcher" in text and "error" in text:
        return "MATCHER_SYNTAX"
    return "INTERNAL"


def _parse_hits(stdout: str) -> tuple[ClangQueryRawHit, ...]:
    hits: list[ClangQueryRawHit] = []
    lines = stdout.splitlines()
    for i, line in enumerate(lines):
        m = _HIT_RE.match(line)
        if not m:
            continue
        # Try to grab a one-line preceding context as `message`; in
        # diag mode clang-query prints the source line above the note.
        context = lines[i - 1].strip() if i > 0 else ""
        hits.append(
            ClangQueryRawHit(
                path=m.group("path"),
                line=int(m.group("line")),
                column=int(m.group("col")),
                bind_name=m.group("bind"),
                message=context,
            )
        )
    return tuple(hits)


__all__ = [
    "ClangQueryRawHit",
    "ClangQueryRun",
    "build_combined_script",
    "run_clang_query",
]
