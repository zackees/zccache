"""Tests for `zccache.cpp_lint._listorpath.resolve_to_lines`."""

from __future__ import annotations

from pathlib import Path

import pytest

from zccache.cpp_lint._listorpath import resolve_to_lines


def test_none() -> None:
    assert resolve_to_lines(None) == ()


def test_single_string() -> None:
    assert resolve_to_lines("src/**") == ("src/**",)


def test_tuple_mixed_strings_and_paths() -> None:
    out = resolve_to_lines(("src/**", Path("src/legacy.h")))
    assert "src/**" in out
    # Path treated as glob since it doesn't exist on disk.
    assert any("legacy.h" in s for s in out)


def test_path_pointing_at_text_file(tmp_path: Path) -> None:
    scope = tmp_path / "scope.txt"
    scope.write_text(
        "# comments are stripped\n"
        "src/fl/**\n"
        "\n"
        "# blank lines too\n"
        "include/fl/**\n",
        encoding="utf-8",
    )
    out = resolve_to_lines(scope)
    assert out == ("src/fl/**", "include/fl/**")


def test_path_missing_file_treated_as_literal_glob(tmp_path: Path) -> None:
    missing = tmp_path / "no_such_scope.txt"
    out = resolve_to_lines(missing)
    assert len(out) == 1
    assert str(missing) in out[0] or "no_such_scope" in out[0]


def test_nested_tuples_flatten() -> None:
    out = resolve_to_lines(("a/**", ("b/**", "c/**")))
    assert "a/**" in out and "b/**" in out and "c/**" in out


def test_invalid_type_raises() -> None:
    with pytest.raises(TypeError):
        resolve_to_lines(42)  # type: ignore[arg-type]
