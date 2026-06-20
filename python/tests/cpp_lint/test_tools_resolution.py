"""Tests for `zccache.cpp_lint._tools.resolve_tools`."""

from __future__ import annotations

import os
import sys
from pathlib import Path
from unittest import mock

import pytest

from zccache.cpp_lint import (
    AstQuery,
    IwyuItem,
    LintInput,
    MissingClangPolicy,
)
from zccache.cpp_lint._tools import (
    TOOL_CLANG_QUERY,
    TOOL_FIX_INCLUDES,
    TOOL_IWYU,
    resolve_tools,
)


def _make_executable(path: Path) -> None:
    if sys.platform != "win32":
        path.chmod(0o755)


def test_explicit_path_wins(tmp_path: Path) -> None:
    cq = tmp_path / ("clang-query.bat" if sys.platform == "win32" else "clang-query")
    cq.write_text("")
    _make_executable(cq)
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        clang_query_path=cq,
    )
    result = resolve_tools(li)
    assert result.paths[TOOL_CLANG_QUERY] == str(cq)
    assert result.fetched == ()


def test_explicit_path_missing_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        clang_query_path=tmp_path / "nope",
    )
    with pytest.raises(FileNotFoundError):
        resolve_tools(li)


def test_env_path_lookup(tmp_path: Path) -> None:
    cq = tmp_path / ("clang-query.bat" if sys.platform == "win32" else "clang-query")
    if sys.platform == "win32":
        cq.write_text("@echo off\r\n", encoding="utf-8")
    else:
        cq.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    _make_executable(cq)
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
    )
    new_path = str(tmp_path) + os.pathsep + os.environ.get("PATH", "")
    with mock.patch.dict(os.environ, {"PATH": new_path}):
        result = resolve_tools(li)
    assert TOOL_CLANG_QUERY in result.paths
    assert result.fetched == ()


def test_missing_with_error_policy_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        allow_missing_clang=MissingClangPolicy.ERROR,
    )
    # Empty PATH so shutil.which can't find clang-query.
    with mock.patch.dict(os.environ, {"PATH": ""}):
        with pytest.raises(RuntimeError, match="missing required tools"):
            resolve_tools(li)


def test_missing_with_fetch_policy_calls_ensure(tmp_path: Path) -> None:
    """`FETCH` invokes clang_tool_chain_bins.ensure and reports fetched names."""
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        allow_missing_clang=MissingClangPolicy.FETCH,
    )
    cq_dir = tmp_path / "fake_install"
    cq_dir.mkdir()
    fake_cq = cq_dir / ("clang-query.exe" if sys.platform == "win32" else "clang-query")
    fake_cq.write_text("")
    _make_executable(fake_cq)

    class _FakeResult:
        install_path = str(fake_cq)

    fake_ctcb = mock.MagicMock()
    fake_ctcb.ensure = mock.MagicMock(return_value=[_FakeResult()])

    with mock.patch.dict(os.environ, {"PATH": ""}):
        with mock.patch.dict(sys.modules, {"clang_tool_chain_bins": fake_ctcb}):
            result = resolve_tools(li)
    assert TOOL_CLANG_QUERY in result.paths
    assert result.fetched == (TOOL_CLANG_QUERY,)
    fake_ctcb.ensure.assert_called_with(TOOL_CLANG_QUERY)


def test_iwyu_needed_only_when_iwyu_items_present(tmp_path: Path) -> None:
    cq = tmp_path / ("clang-query.bat" if sys.platform == "win32" else "clang-query")
    cq.write_text("")
    _make_executable(cq)
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        clang_query_path=cq,
    )
    result = resolve_tools(li)
    assert TOOL_IWYU not in result.paths


def test_fix_includes_needed_only_when_auto_fix_true(tmp_path: Path) -> None:
    iwyu = tmp_path / ("include-what-you-use.bat" if sys.platform == "win32" else "include-what-you-use")
    iwyu.write_text("")
    _make_executable(iwyu)
    li = LintInput(
        compile_commands=tmp_path / "cc.json",
        iwyu_items=(IwyuItem(name="i", auto_fix=False),),
        default_scope="src/**",
        iwyu_path=iwyu,
    )
    result = resolve_tools(li)
    assert TOOL_IWYU in result.paths
    assert TOOL_FIX_INCLUDES not in result.paths
