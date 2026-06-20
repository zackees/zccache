"""C/C++ lint cache API (issue #841).

Public entry point:

    from zccache.cpp_lint import cpp_lint, LintInput, AstQuery, IwyuItem

    for item in cpp_lint(LintInput(...)):
        ...

This package implements the streaming cpp_lint API from issue #841:
typed dataclasses (`LintInput`, `AstQuery`, `IwyuItem`), the streaming
iterator entry point (`cpp_lint`), the unified result schema
(`ResultItem`, `Summary`, `ResultKind`, `CacheStatus`, `ResultFilter`),
and the supporting validation, tool-resolution, and per-(TU, item)
cache layers.

The current implementation is pure Python — subprocess-based clang-query
and IWYU runners, simple on-disk JSON cache, threaded worker pool.
Daemon-side depgraph integration and the GIL-isolated pyo3 feeder
thread described in #841 land in follow-up work; the API surface here
is the long-term contract.
"""

from __future__ import annotations

from zccache.cpp_lint._cache import LintCache
from zccache.cpp_lint._runner import cpp_lint
from zccache.cpp_lint._tools import (
    MissingClangPolicy,
    ToolResolution,
    resolve_tools,
)
from zccache.cpp_lint._toml import dump_lint_input_toml, load_lint_input_toml
from zccache.cpp_lint._types import (
    AstQuery,
    CacheStatus,
    IwyuItem,
    LintInput,
    ListOrPath,
    ResultFilter,
    ResultItem,
    ResultKind,
    Summary,
    TextOrPath,
)
from zccache.cpp_lint._validate import LintInputError, validate

__all__ = [
    "AstQuery",
    "CacheStatus",
    "IwyuItem",
    "LintCache",
    "LintInput",
    "LintInputError",
    "ListOrPath",
    "MissingClangPolicy",
    "ResultFilter",
    "ResultItem",
    "ResultKind",
    "Summary",
    "TextOrPath",
    "ToolResolution",
    "cpp_lint",
    "dump_lint_input_toml",
    "load_lint_input_toml",
    "resolve_tools",
    "validate",
]
