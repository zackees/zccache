"""End-to-end tests for `cpp_lint()` using fake clang-query / IWYU stubs.

No real toolchain required. The stub fixtures in `conftest.py` emit
canned output mimicking the real tool's wire format; the runner
parses it the same way it would for real binaries.
"""

from __future__ import annotations

import threading
from pathlib import Path

import pytest

from zccache.cpp_lint import (
    AstQuery,
    CacheStatus,
    IwyuItem,
    LintInput,
    ResultFilter,
    ResultKind,
    Summary,
    cpp_lint,
)


def _consume(iterator) -> tuple[list[tuple], Summary]:
    """Drain the iterator into (batches, final summary)."""
    batches: list[tuple] = []
    summary: Summary | None = None
    for item in iterator:
        if isinstance(item, Summary):
            summary = item
        else:
            batches.append(item)
    assert summary is not None
    return batches, summary


def test_basic_ast_run(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="noexcept", matcher_body="match decl()"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
    )
    batches, summary = _consume(cpp_lint(li))
    # One batch — one path (the TU) — one hit.
    assert len(batches) == 1
    assert len(batches[0]) == 1
    item = batches[0][0]
    assert item.kind is ResultKind.AST
    assert item.item_name == "noexcept"
    assert item.warning is True
    assert item.error is False
    assert item.cache is CacheStatus.MISS
    assert summary.warnings == 1
    assert summary.errors == 0
    assert summary.misses == 1
    assert summary.hits == 0
    assert summary.tus_invoked == 1


def test_cache_hit_on_second_run(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    cache_root = tmp_path / "cache"
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="noexcept", matcher_body="match decl()"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=cache_root,
    )
    # Warm.
    _consume(cpp_lint(li))
    # Replay.
    batches, summary = _consume(cpp_lint(li))
    assert summary.hits == 1
    assert summary.misses == 0
    assert summary.hit_rate == 1.0
    assert batches[0][0].cache is CacheStatus.HIT


def test_cached_false_forces_fresh_run(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    cache_root = tmp_path / "cache"
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="noexcept", matcher_body="match decl()"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=cache_root,
    )
    _consume(cpp_lint(li))
    batches, summary = _consume(cpp_lint(li, cached=False))
    # Cache write happened on first run; second run skips READ → MISS.
    assert summary.hits == 0
    assert summary.misses == 1
    assert batches[0][0].cache is CacheStatus.MISS


def test_iwyu_run_emits_per_action_items(
    tmp_path: Path,
    stub_iwyu: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    li = LintInput(
        compile_commands=cc,
        iwyu_items=(IwyuItem(name="imports"),),
        default_scope=str(tmp_path / "**"),
        iwyu_path=stub_iwyu,
        cache_root=tmp_path / "cache",
    )
    batches, summary = _consume(cpp_lint(li))
    # Stub emits one add + one keep for the TU.
    all_items = [it for batch in batches for it in batch]
    kinds = {it.kind for it in all_items}
    assert kinds == {ResultKind.IWYU}
    actions = sorted({it.extra.get("action") for it in all_items})
    assert "add" in actions
    assert "keep" in actions
    assert summary.warnings == 1   # the "add"
    assert summary.successes == 1  # the "keep"


def test_filter_ALL_BUT_ERRORS_suppresses_non_errors(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
    )
    batches, summary = _consume(
        cpp_lint(li, filter_out=ResultFilter.ALL_BUT_ERRORS)
    )
    assert batches == []  # warnings suppressed
    assert summary.warnings == 1  # still counted in summary


def test_per_path_grouping(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    """Two AstQuery items applied to same TU → single per-path tuple."""
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    li = LintInput(
        compile_commands=cc,
        ast_queries=(
            AstQuery(name="q1", matcher_body="m"),
            AstQuery(name="q2", matcher_body="m"),
        ),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
    )
    batches, _ = _consume(cpp_lint(li))
    # One TU → one path (the TU itself) → one tuple containing both items' hits.
    assert len(batches) == 1
    tuple_for_path = batches[0]
    names = sorted(it.item_name for it in tuple_for_path)
    assert names == ["q1", "q2"]


def test_abort_signal_terminates_cleanly(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tus = [write_tu(f"a{i}.cpp") for i in range(10)]
    cc = compile_commands_for(tus)
    abort = threading.Event()
    abort.set()  # pre-set so the runner aborts immediately
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
        abort_signal=abort,
    )
    _, summary = _consume(cpp_lint(li))
    assert summary.aborted is True


def test_max_errors_triggers_abort(
    tmp_path: Path,
    compile_commands_for,
    write_tu,
) -> None:
    """A failing tool path produces errors per TU; max_errors=1 aborts after 1."""
    tus = [write_tu(f"a{i}.cpp") for i in range(5)]
    cc = compile_commands_for(tus)
    # Use a tool path that exists but exits non-zero with no output —
    # mimics a parse error per TU.
    import sys

    nonexistent_helper = tmp_path / ("fail_tool.bat" if sys.platform == "win32" else "fail_tool")
    if sys.platform == "win32":
        nonexistent_helper.write_text("@echo off\r\nexit /b 1\r\n", encoding="utf-8")
    else:
        nonexistent_helper.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        nonexistent_helper.chmod(0o755)

    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=nonexistent_helper,
        cache_root=tmp_path / "cache",
        max_errors=1,
    )
    _, summary = _consume(cpp_lint(li))
    assert summary.aborted is True
    assert summary.errors >= 1


def test_order_emits_stable_sequence(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tus = [write_tu(f"z{i}.cpp") for i in range(3)]
    cc = compile_commands_for(tus)
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
        order=True,
    )
    runs = []
    for _ in range(3):
        batches, _ = _consume(cpp_lint(li))
        paths = [batch[0].path for batch in batches]
        runs.append(paths)
    # All three runs produce identical path order.
    assert runs[0] == runs[1] == runs[2]


def test_summary_includes_resolved_tool_paths(
    tmp_path: Path,
    stub_clang_query: Path,
    compile_commands_for,
    write_tu,
) -> None:
    tu = write_tu("a.cpp")
    cc = compile_commands_for([tu])
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope=str(tmp_path / "**"),
        clang_query_path=stub_clang_query,
        cache_root=tmp_path / "cache",
    )
    _, summary = _consume(cpp_lint(li))
    assert "clang-query" in summary.resolved_tool_paths
    assert summary.resolved_tool_paths["clang-query"] == str(stub_clang_query.resolve())
    assert summary.tools_fetched == ()
