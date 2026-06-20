"""Tests for `zccache.cpp_lint._validate.validate`."""

from __future__ import annotations

from pathlib import Path

import pytest

from zccache.cpp_lint import (
    AstQuery,
    IwyuItem,
    LintInput,
    LintInputError,
    validate,
)


def _cc(tmp_path: Path) -> Path:
    path = tmp_path / "cc.json"
    path.write_text("[]", encoding="utf-8")
    return path


def test_empty_lint_input_raises(tmp_path: Path) -> None:
    li = LintInput(compile_commands=_cc(tmp_path))
    with pytest.raises(LintInputError, match="no work"):
        validate(li)


def test_missing_compile_commands_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=tmp_path / "missing.json",
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
    )
    with pytest.raises(LintInputError, match="compile_commands"):
        validate(li)


def test_duplicate_name_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(
            AstQuery(name="x", matcher_body="m"),
            AstQuery(name="x", matcher_body="m"),
        ),
        default_scope="src/**",
    )
    with pytest.raises(LintInputError, match="Duplicate"):
        validate(li)


def test_missing_scope_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        # no default_scope, no item.scope
    )
    with pytest.raises(LintInputError, match="no scope"):
        validate(li)


def test_missing_matcher_body_path_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body=tmp_path / "nope.cqs"),),
        default_scope="src/**",
    )
    with pytest.raises(LintInputError, match="matcher_body"):
        validate(li)


def test_missing_mapping_file_raises(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        iwyu_items=(
            IwyuItem(name="i", mapping_files=(tmp_path / "missing.imp",)),
        ),
        default_scope="src/**",
    )
    with pytest.raises(LintInputError, match="mapping_files"):
        validate(li)


def test_max_errors_must_be_positive(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        max_errors=0,
    )
    with pytest.raises(LintInputError, match="max_errors"):
        validate(li)


def test_max_jobs_must_be_positive(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body="m"),),
        default_scope="src/**",
        max_jobs=-1,
    )
    with pytest.raises(LintInputError, match="max_jobs"):
        validate(li)


def test_valid_minimal_input_passes(tmp_path: Path) -> None:
    matcher = tmp_path / "m.cqs"
    matcher.write_text("match decl()", encoding="utf-8")
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body=matcher),),
        default_scope="src/**",
    )
    validate(li)


def test_per_item_scope_satisfies_requirement(tmp_path: Path) -> None:
    li = LintInput(
        compile_commands=_cc(tmp_path),
        ast_queries=(AstQuery(name="x", matcher_body="m", scope="src/**"),),
    )
    validate(li)
