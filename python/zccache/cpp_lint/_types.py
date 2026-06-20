"""Frozen dataclasses + enums for the cpp_lint API (issue #841).

Types here intentionally use only stdlib so they import on any Python
3.10+ interpreter without the `_native` extension module loaded.
"""

from __future__ import annotations

import json
import threading
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Union

# `TextOrPath` accepts either an inline string body (matcher source code
# or scope glob) or a path to a file containing it.
TextOrPath = Union[str, Path]

# `ListOrPath` accepts any of:
#   - a single glob string ("src/fl/**")
#   - a tuple of strings/paths (("src/fl/**", Path("src/legacy.h")))
#   - a path to a text file with one entry per line (Path("scope.txt"))
# All three forms resolve to the same set[Path] before hashing (see
# `_listorpath.resolve_to_lines`).
ListOrPath = Union[str, Path, tuple[Union[str, Path], ...]]


class ResultKind(Enum):
    """Which tool family produced this result. Orthogonal to severity."""

    AST = "ast"
    IWYU = "iwyu"
    # CPPCHECK = "cppcheck"  # future


class CacheStatus(Enum):
    """Whether the item came from the on-disk cache or was freshly produced."""

    HIT = "hit"
    MISS = "miss"


class ResultFilter(Enum):
    """Daemon-side filter naming what gets SUPPRESSED before items reach Python."""

    NONE = "none"
    SUCCESSES = "successes"
    ALL_BUT_ERRORS = "all_but_errors"


class MissingClangPolicy(Enum):
    """What to do when a required clang tool isn't on PATH and no explicit path was given."""

    ERROR = "error"
    FETCH = "fetch"


@dataclass(frozen=True)
class AstQuery:
    """One clang-query AST matcher to run against the TU set."""

    name: str
    matcher_body: TextOrPath
    scope: ListOrPath | None = None
    ignore: ListOrPath | None = None
    cache_key_namespace: bytes = b""


@dataclass(frozen=True)
class IwyuItem:
    """One IWYU run configuration."""

    name: str
    mapping_files: tuple[Path, ...] = ()
    pch_in_code: bool = False
    extra_args: tuple[str, ...] = ()
    auto_fix: bool = False
    scope: ListOrPath | None = None
    ignore: ListOrPath | None = None
    cache_key_namespace: bytes = b""


@dataclass(frozen=True)
class LintInput:
    """A complete lint specification.

    At least one of `ast_queries` / `iwyu_items` must be non-empty.
    `default_scope` is required if any item omits its own.
    """

    compile_commands: Path

    ast_queries: tuple[AstQuery, ...] = ()
    iwyu_items: tuple[IwyuItem, ...] = ()

    default_scope: ListOrPath | None = None
    default_ignore: ListOrPath | None = None

    let_bindings: ListOrPath = ()
    extra_clang_query_args: tuple[str, ...] = ()

    default_mapping_files: tuple[Path, ...] = ()
    extra_iwyu_args: tuple[str, ...] = ()

    # Tool path fields — None means "resolve via env PATH at runtime".
    # When all three are None and the relevant family is in use, the
    # resolver falls back to `shutil.which("clang-query")`,
    # `shutil.which("include-what-you-use")`, etc. (see _tools.resolve_tools).
    clang_query_path: Path | None = None
    iwyu_path: Path | None = None
    fix_includes_path: Path | None = None

    # When a required tool is missing from PATH and the corresponding
    # path field is None, this controls fallback behaviour:
    #   ERROR (default) raises RuntimeError listing the missing tools.
    #   FETCH attempts `clang_tool_chain_bins.ensure(...)` to install
    #   the missing tools on demand. Auto-fetched tools are reported in
    #   Summary.tools_fetched.
    allow_missing_clang: MissingClangPolicy = MissingClangPolicy.ERROR

    # Cooperative cancellation. None disables the watcher entirely.
    abort_signal: threading.Event | None = None

    # Worker concurrency. Defaults to None → use os.cpu_count().
    max_jobs: int | None = None

    # On-disk cache root. None → use a per-user XDG-style default.
    cache_root: Path | None = None

    # Early-abort threshold. None disables. When the run accumulates
    # this many ResultItems with `error=True`, the dispatcher stops
    # scheduling new jobs, drains in-flight ones, and emits
    # `Summary(aborted=True)`. Same cancellation path as `abort_signal`.
    # Useful for CI smoke runs that don't need every failure listed.
    max_errors: int | None = None

    # When True, emit items in deterministic per-(TU, item) order: the
    # dispatcher routes worker output through an atomic min-heap keyed
    # by the TU's stable index and the item's stable index, and the
    # feeder pops the heap whenever the head matches the next expected
    # counter. Output across runs becomes byte-identical (useful for
    # diff-based CI tooling). When False (default), items emit as
    # workers produce them — faster, but order varies across runs.
    order: bool = False


@dataclass(frozen=True)
class ResultItem:
    """One streamed lint result.

    Same shape across all tool families. `kind` (tool family) is
    orthogonal to severity (`error` / `warning`).
    Invariant: NOT (error AND warning).
    """

    path: str
    kind: ResultKind
    cache: CacheStatus
    message: str
    item_name: str
    error: bool = False
    warning: bool = False
    line: int = 0
    column: int = 0
    tu: str = ""
    extra: dict[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.error and self.warning:
            raise ValueError(
                f"ResultItem({self.path!r}): error and warning are mutually exclusive"
            )


@dataclass(frozen=True)
class Summary:
    """Final item in every cpp_lint stream — cache + result counts + timing.

    Cache metrics (`hits`, `misses`, `hit_rate`) are per-(TU, item) pair.
    Result-item counts (`successes`, `warnings`, `errors`) are per
    ResultItem (a single pair produces N items).
    """

    hits: int
    misses: int
    hit_rate: float

    successes: int
    warnings: int
    errors: int

    tus_invoked: int
    elapsed_seconds: float
    aborted: bool = False

    # Names of tools that were auto-fetched because they were missing
    # and `allow_missing_clang=FETCH` was set. Empty when nothing was
    # fetched (either all tools were already on PATH or explicit paths
    # were provided).
    tools_fetched: tuple[str, ...] = ()

    # Final resolved paths for every tool the run used. Keys are
    # canonical tool names ("clang-query", "include-what-you-use",
    # "fix_includes.py"); values are absolute paths. Empty when no tool
    # was needed (e.g. all items hit cache and IWYU auto-fix off — even
    # then we still resolve so callers can inspect what would have run).
    resolved_tool_paths: dict[str, str] = field(default_factory=dict)

    def to_str(self) -> str:
        status = "ABORTED" if self.aborted else "complete"
        rows = [
            ("hits", str(self.hits)),
            ("misses", str(self.misses)),
            ("hit rate", f"{self.hit_rate:.1%}"),
            ("successes", str(self.successes)),
            ("warnings", str(self.warnings)),
            ("errors", str(self.errors)),
            ("tus invoked", str(self.tus_invoked)),
            ("elapsed", f"{self.elapsed_seconds:.1f}s"),
            ("status", status),
        ]
        if self.tools_fetched:
            rows.append(("tools fetched", ",".join(self.tools_fetched)))
        label_w = max(len(k) for k, _ in rows)
        value_w = max(len(v) for _, v in rows)
        sep = "-" * (label_w + value_w + 3)
        out = ["cpp_lint summary", sep]
        out.extend(f"{k:<{label_w}}   {v:>{value_w}}" for k, v in rows)
        if self.resolved_tool_paths:
            out.append("")
            out.append("resolved tool paths:")
            for name, path in sorted(self.resolved_tool_paths.items()):
                out.append(f"  {name}: {path}")
        return "\n".join(out)

    def to_json(self) -> str:
        return json.dumps(
            {
                "hits": self.hits,
                "misses": self.misses,
                "hit_rate": self.hit_rate,
                "successes": self.successes,
                "warnings": self.warnings,
                "errors": self.errors,
                "tus_invoked": self.tus_invoked,
                "elapsed_seconds": self.elapsed_seconds,
                "aborted": self.aborted,
                "tools_fetched": list(self.tools_fetched),
                "resolved_tool_paths": dict(self.resolved_tool_paths),
            }
        )

    def __str__(self) -> str:  # noqa: D401 — alias to to_str
        return self.to_str()
