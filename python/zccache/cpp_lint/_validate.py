"""Upfront validation of LintInput before any tool invocation.

Raises `LintInputError` on the first detected problem with a message
that points at the specific field. The runner calls `validate()` at
the top of `cpp_lint()` so callers see configuration errors as a
single exception rather than via a partial stream.
"""

from __future__ import annotations

from pathlib import Path

from zccache.cpp_lint._types import (
    LintInput,
    TextOrPath,
)


class LintInputError(ValueError):
    """Raised when a LintInput fails upfront validation."""


def validate(lint_input: LintInput) -> None:
    """Validate a LintInput. Raises LintInputError on the first problem."""

    if not lint_input.ast_queries and not lint_input.iwyu_items:
        raise LintInputError(
            "LintInput has no work: both ast_queries and iwyu_items are empty"
        )

    # compile_commands must be a real file.
    cc = lint_input.compile_commands
    if not isinstance(cc, Path):
        raise LintInputError(
            f"LintInput.compile_commands must be a Path (got {type(cc).__name__})"
        )
    if not cc.is_file():
        raise LintInputError(
            f"LintInput.compile_commands not found: {cc}"
        )

    # Validate every item's name is unique across families and that
    # each item has either its own scope or a default_scope fallback.
    seen_names: set[str] = set()

    for q in lint_input.ast_queries:
        _check_item_name(q.name, "AstQuery", seen_names)
        _check_matcher_body(q.name, q.matcher_body)
        _check_effective_scope(q.name, q.scope, lint_input.default_scope)

    for r in lint_input.iwyu_items:
        _check_item_name(r.name, "IwyuItem", seen_names)
        _check_mapping_files(r.name, r.mapping_files)
        _check_effective_scope(r.name, r.scope, lint_input.default_scope)

    # Optional max_errors must be positive when set.
    if lint_input.max_errors is not None and lint_input.max_errors <= 0:
        raise LintInputError(
            f"LintInput.max_errors must be positive (got {lint_input.max_errors})"
        )

    # Optional max_jobs must be positive when set.
    if lint_input.max_jobs is not None and lint_input.max_jobs <= 0:
        raise LintInputError(
            f"LintInput.max_jobs must be positive (got {lint_input.max_jobs})"
        )


def _check_item_name(name: str, family: str, seen: set[str]) -> None:
    if not name or not name.strip():
        raise LintInputError(f"{family}.name is empty")
    if name in seen:
        raise LintInputError(
            f"Duplicate item name across LintInput: {name!r} "
            f"(names are global within a LintInput)"
        )
    seen.add(name)


def _check_matcher_body(name: str, body: TextOrPath) -> None:
    if isinstance(body, Path) and not body.is_file():
        raise LintInputError(
            f"AstQuery({name!r}).matcher_body path does not exist: {body}"
        )


def _check_mapping_files(name: str, mapping_files: tuple[Path, ...]) -> None:
    for mf in mapping_files:
        if not isinstance(mf, Path):
            raise LintInputError(
                f"IwyuItem({name!r}).mapping_files must contain Path objects "
                f"(got {type(mf).__name__})"
            )
        if not mf.is_file():
            raise LintInputError(
                f"IwyuItem({name!r}).mapping_files entry does not exist: {mf}"
            )


def _check_effective_scope(name: str, item_scope: object, default_scope: object) -> None:
    if item_scope is None and default_scope is None:
        raise LintInputError(
            f"Item {name!r} has no scope: both item.scope and "
            f"LintInput.default_scope are None"
        )


__all__ = ["LintInputError", "validate"]
