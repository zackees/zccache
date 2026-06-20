"""Tests for `zccache.cpp_lint._types` shapes and Summary rendering."""

from __future__ import annotations

import dataclasses
import json
from pathlib import Path

import pytest

from zccache.cpp_lint import (
    AstQuery,
    CacheStatus,
    IwyuItem,
    LintInput,
    MissingClangPolicy,
    ResultFilter,
    ResultItem,
    ResultKind,
    Summary,
)


def test_ast_query_defaults() -> None:
    q = AstQuery(name="noexcept", matcher_body="match decl()")
    assert q.name == "noexcept"
    assert q.scope is None
    assert q.ignore is None
    assert q.cache_key_namespace == b""


def test_iwyu_item_defaults() -> None:
    r = IwyuItem(name="imports")
    assert r.name == "imports"
    assert r.mapping_files == ()
    assert r.pch_in_code is False
    assert r.extra_args == ()
    assert r.auto_fix is False
    assert r.cache_key_namespace == b""


def test_lint_input_defaults() -> None:
    li = LintInput(
        compile_commands=Path("/dev/null"),
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
    )
    assert li.allow_missing_clang is MissingClangPolicy.ERROR
    assert li.order is False
    assert li.max_jobs is None
    assert li.max_errors is None
    assert li.cache_root is None
    assert li.abort_signal is None
    assert li.iwyu_items == ()


def test_lint_input_frozen() -> None:
    li = LintInput(compile_commands=Path("/x"))
    with pytest.raises(dataclasses.FrozenInstanceError):
        li.compile_commands = Path("/y")  # type: ignore[misc]


def test_result_item_invariant() -> None:
    # error + warning is forbidden.
    with pytest.raises(ValueError):
        ResultItem(
            path="x.cpp",
            kind=ResultKind.AST,
            cache=CacheStatus.MISS,
            message="m",
            item_name="i",
            error=True,
            warning=True,
        )


def test_result_item_ok_state() -> None:
    item = ResultItem(
        path="x.cpp",
        kind=ResultKind.IWYU,
        cache=CacheStatus.HIT,
        message="for std::string",
        item_name="imports",
        warning=True,
        extra={"action": "add", "include": "<string>"},
    )
    assert item.error is False
    assert item.warning is True
    assert item.line == 0
    assert item.cache is CacheStatus.HIT


def test_result_filter_values() -> None:
    assert ResultFilter.NONE.value == "none"
    assert ResultFilter.SUCCESSES.value == "successes"
    assert ResultFilter.ALL_BUT_ERRORS.value == "all_but_errors"


def test_summary_to_str_minimal() -> None:
    s = Summary(
        hits=0,
        misses=0,
        hit_rate=0.0,
        successes=0,
        warnings=0,
        errors=0,
        tus_invoked=0,
        elapsed_seconds=0.0,
    )
    text = s.to_str()
    assert "cpp_lint summary" in text
    # status row exists with the expected value
    assert "complete" in text
    assert "0.0s" in text


def test_summary_to_str_aborted_reports_status() -> None:
    s = Summary(
        hits=1,
        misses=1,
        hit_rate=0.5,
        successes=0,
        warnings=2,
        errors=1,
        tus_invoked=2,
        elapsed_seconds=1.2,
        aborted=True,
    )
    text = s.to_str()
    assert "ABORTED" in text
    assert "50.0%" in text


def test_summary_to_str_reports_fetched_tools_and_paths() -> None:
    s = Summary(
        hits=0,
        misses=1,
        hit_rate=0.0,
        successes=0,
        warnings=0,
        errors=0,
        tus_invoked=1,
        elapsed_seconds=0.1,
        tools_fetched=("clang-query",),
        resolved_tool_paths={"clang-query": "/tmp/cq"},
    )
    text = s.to_str()
    assert "tools fetched" in text
    assert "clang-query" in text
    assert "resolved tool paths:" in text
    assert "/tmp/cq" in text


def test_summary_to_json_round_trip() -> None:
    s = Summary(
        hits=2,
        misses=1,
        hit_rate=2.0 / 3.0,
        successes=3,
        warnings=1,
        errors=0,
        tus_invoked=3,
        elapsed_seconds=0.42,
        tools_fetched=("include-what-you-use",),
        resolved_tool_paths={"include-what-you-use": "/tmp/iwyu"},
    )
    payload = json.loads(s.to_json())
    assert payload["hits"] == 2
    assert payload["tools_fetched"] == ["include-what-you-use"]
    assert payload["resolved_tool_paths"]["include-what-you-use"] == "/tmp/iwyu"
