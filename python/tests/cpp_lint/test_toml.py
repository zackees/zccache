"""Tests for `zccache.cpp_lint._toml` round-trip."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from zccache.cpp_lint import (
    AstQuery,
    IwyuItem,
    LintInput,
    MissingClangPolicy,
    dump_lint_input_toml,
    load_lint_input_toml,
)


def test_minimal_round_trip(tmp_path: Path) -> None:
    cc = tmp_path / "cc.json"
    cc.write_text("[]", encoding="utf-8")
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="noexcept", matcher_body="match decl()"),),
        default_scope="src/**",
    )
    text = dump_lint_input_toml(li)
    parsed = load_lint_input_toml(text)
    assert parsed.compile_commands == cc
    assert parsed.ast_queries[0].name == "noexcept"
    assert parsed.default_scope == "src/**"


def test_iwyu_round_trip(tmp_path: Path) -> None:
    cc = tmp_path / "cc.json"
    cc.write_text("[]", encoding="utf-8")
    mf = tmp_path / "stl.imp"
    mf.write_text("[]", encoding="utf-8")
    li = LintInput(
        compile_commands=cc,
        iwyu_items=(
            IwyuItem(
                name="imports",
                mapping_files=(mf,),
                pch_in_code=True,
                extra_args=("--no_default_mappings",),
                auto_fix=True,
                cache_key_namespace=b"v1",
            ),
        ),
        default_scope=("src/**", "include/**"),
    )
    text = dump_lint_input_toml(li)
    parsed = load_lint_input_toml(text)
    r = parsed.iwyu_items[0]
    assert r.name == "imports"
    assert r.mapping_files == (mf,)
    assert r.pch_in_code is True
    assert r.extra_args == ("--no_default_mappings",)
    assert r.auto_fix is True
    assert r.cache_key_namespace == b"v1"
    assert parsed.default_scope == ("src/**", "include/**")


def test_tool_paths_and_policy_round_trip(tmp_path: Path) -> None:
    cc = tmp_path / "cc.json"
    cc.write_text("[]", encoding="utf-8")
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        clang_query_path=Path("/opt/llvm/clang-query"),
        iwyu_path=Path("/opt/iwyu/iwyu"),
        allow_missing_clang=MissingClangPolicy.FETCH,
        max_errors=5,
        max_jobs=8,
        order=True,
    )
    text = dump_lint_input_toml(li)
    parsed = load_lint_input_toml(text)
    assert parsed.clang_query_path == Path("/opt/llvm/clang-query")
    assert parsed.iwyu_path == Path("/opt/iwyu/iwyu")
    assert parsed.allow_missing_clang is MissingClangPolicy.FETCH
    assert parsed.max_errors == 5
    assert parsed.max_jobs == 8


@pytest.mark.skipif(sys.version_info < (3, 11), reason="tomllib only on 3.11+")
def test_dump_emits_valid_toml(tmp_path: Path) -> None:
    import tomllib

    cc = tmp_path / "cc.json"
    cc.write_text("[]", encoding="utf-8")
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="match decl()"),),
        default_scope="src/**",
    )
    text = dump_lint_input_toml(li)
    parsed = tomllib.loads(text)
    assert parsed["compile_commands"] == str(cc)
    assert isinstance(parsed["ast_queries"], list)


def test_abort_signal_omitted_from_toml(tmp_path: Path) -> None:
    import threading

    cc = tmp_path / "cc.json"
    cc.write_text("[]", encoding="utf-8")
    li = LintInput(
        compile_commands=cc,
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        abort_signal=threading.Event(),
    )
    text = dump_lint_input_toml(li)
    # abort_signal must not appear in the TOML body as a key. The path
    # may include the test directory name (e.g. test_abort_signal_...)
    # so substring search is too loose; check for the actual TOML key.
    assert "\nabort_signal " not in text
    assert "\nabort_signal=" not in text
    parsed = load_lint_input_toml(text)
    assert parsed.abort_signal is None
