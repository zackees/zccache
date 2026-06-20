"""Tool path resolution for cpp_lint.

Priority per tool:
  1. Explicit path on LintInput (e.g. `clang_query_path`).
  2. `shutil.which("<tool>")` against the inherited env PATH.
  3. If `allow_missing_clang=FETCH`, ask `clang_tool_chain_bins.ensure()`
     to install the missing tools. Names of fetched tools are reported
     in `Summary.tools_fetched`.
  4. Otherwise raise RuntimeError listing what's missing.

The resolver returns a `ToolResolution` snapshot the runner attaches to
the final Summary so callers can audit what actually ran.
"""

from __future__ import annotations

import shutil
from dataclasses import dataclass
from pathlib import Path

from zccache.cpp_lint._types import LintInput, MissingClangPolicy

# Canonical names match what `clang_tool_chain_bins` knows about.
TOOL_CLANG_QUERY = "clang-query"
TOOL_IWYU = "include-what-you-use"
TOOL_FIX_INCLUDES = "fix_includes.py"


@dataclass(frozen=True)
class ToolResolution:
    """Resolved paths + provenance for the tools a run will use."""

    paths: dict[str, str]
    fetched: tuple[str, ...]


def resolve_tools(lint_input: LintInput) -> ToolResolution:
    """Resolve the tool paths needed for this LintInput.

    Only resolves tools the input actually uses — clang-query iff
    ast_queries non-empty; IWYU iff iwyu_items non-empty; fix_includes
    iff at least one iwyu_item has auto_fix=True.
    """
    needed: list[str] = []
    if lint_input.ast_queries:
        needed.append(TOOL_CLANG_QUERY)
    if lint_input.iwyu_items:
        needed.append(TOOL_IWYU)
        if any(r.auto_fix for r in lint_input.iwyu_items):
            needed.append(TOOL_FIX_INCLUDES)

    explicit = {
        TOOL_CLANG_QUERY: lint_input.clang_query_path,
        TOOL_IWYU: lint_input.iwyu_path,
        TOOL_FIX_INCLUDES: lint_input.fix_includes_path,
    }

    resolved: dict[str, str] = {}
    missing: list[str] = []

    for tool in needed:
        # 1. Explicit override.
        override = explicit.get(tool)
        if override is not None:
            if not Path(override).exists():
                raise FileNotFoundError(
                    f"LintInput.{_field_for(tool)} points at non-existent file: {override}"
                )
            resolved[tool] = str(override)
            continue
        # 2. env PATH.
        found = shutil.which(tool)
        if found is not None:
            resolved[tool] = str(Path(found).resolve())
            continue
        # 3. Defer to FETCH path.
        missing.append(tool)

    if not missing:
        return ToolResolution(paths=resolved, fetched=())

    if lint_input.allow_missing_clang is MissingClangPolicy.ERROR:
        raise RuntimeError(
            "cpp_lint: missing required tools on PATH: "
            + ", ".join(missing)
            + ". Set allow_missing_clang=MissingClangPolicy.FETCH to auto-install, "
            + "or pass the explicit path on LintInput."
        )

    fetched = _fetch_missing(missing)
    for tool in missing:
        path = fetched.get(tool)
        if path is None:
            raise RuntimeError(
                f"cpp_lint: clang_tool_chain_bins.ensure failed to provide {tool!r}"
            )
        resolved[tool] = str(Path(path).resolve())

    return ToolResolution(paths=resolved, fetched=tuple(missing))


def _field_for(tool: str) -> str:
    return {
        TOOL_CLANG_QUERY: "clang_query_path",
        TOOL_IWYU: "iwyu_path",
        TOOL_FIX_INCLUDES: "fix_includes_path",
    }[tool]


def _fetch_missing(tool_names: list[str]) -> dict[str, str]:
    """Use clang_tool_chain_bins to install each missing tool.

    Returns tool_name -> absolute install path. Tools that fail to
    install raise out of clang_tool_chain_bins; the caller turns those
    into RuntimeErrors with context.
    """
    try:
        import clang_tool_chain_bins as ctcb  # type: ignore[import-not-found]
    except ImportError as exc:  # pragma: no cover - guarded at runtime
        raise RuntimeError(
            "cpp_lint: allow_missing_clang=FETCH requires the "
            "clang-tool-chain-bins package. Install with `uv pip install "
            "clang-tool-chain-bins` (or pip install clang-tool-chain-bins)."
        ) from exc

    out: dict[str, str] = {}
    for tool in tool_names:
        results = ctcb.ensure(tool)
        if not results:
            continue
        install_path = getattr(results[0], "install_path", None)
        if install_path is None:
            continue
        out[tool] = str(install_path)
    return out


__all__ = ["TOOL_CLANG_QUERY", "TOOL_IWYU", "TOOL_FIX_INCLUDES", "ToolResolution", "resolve_tools"]
