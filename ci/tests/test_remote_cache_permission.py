"""Remote cache permissions must never disable local automatic cache saves."""

from dataclasses import dataclass
from pathlib import Path

import pytest
import yaml

from ci import check_cache_footprint as guard


@dataclass(frozen=True)
class UnsafePermission:
    global_mode: str
    remote_mode: str
    ref: str = "v0"


@dataclass(frozen=True)
class IndependentWriterCase:
    permission: UnsafePermission
    rejected: bool


def write_workflow(root: Path, case: UnsafePermission) -> None:
    path = root / ".github" / "workflows" / "sample.yml"
    path.parent.mkdir(parents=True)
    path.write_text(
        "on: [push, pull_request]\njobs:\n  check:\n"
        "    runs-on: ubuntu-latest\n    steps:\n"
        f"      - uses: zackees/setup-soldr@{case.ref}\n        with:\n"
        "          cook-delta: false\n          solo-toolchain-cache: false\n"
        f"          save-cache: {case.global_mode}\n"
        f"          save-cache-remote: {case.remote_mode}\n",
        encoding="utf-8",
    )


@pytest.mark.parametrize(
    "case",
    (
        UnsafePermission("auto", "'true'"),
        UnsafePermission("auto", "${{ github.event_name == 'pull_request' }}"),
        UnsafePermission(
            "auto",
            guard.MAIN_PUSH_ONLY_SAVE,
            "a07bab94f16124b5c6857b137a237a53a61e06d1",
        ),
        UnsafePermission(
            "${{ env.ACT && 'true' || (github.event_name == 'push' && github.ref == 'refs/heads/main' && env.ZCCACHE_CACHE_WRITES == 'true' && 'auto' || 'false') }}",
            guard.MAIN_PUSH_ONLY_SAVE,
        ),
    ),
)
def test_unsafe_or_unsupported_remote_permission_is_rejected(
    tmp_path: Path, case: UnsafePermission
) -> None:
    write_workflow(tmp_path, case)
    assert guard.check(tmp_path), (
        "a remote override must not bypass the PR writer guard"
    )


def test_global_false_remains_read_only_even_with_remote_true(tmp_path: Path) -> None:
    write_workflow(tmp_path, UnsafePermission("false", "'true'"))
    assert guard.check(tmp_path) == []


@pytest.mark.parametrize(
    "case",
    (
        IndependentWriterCase(UnsafePermission("auto", "'true'"), True),
        IndependentWriterCase(
            UnsafePermission("auto", guard.MAIN_PUSH_ONLY_SAVE), False
        ),
        IndependentWriterCase(UnsafePermission("false", "'true'"), False),
    ),
)
def test_independent_build_writer_obeys_remote_permission(
    tmp_path: Path, case: IndependentWriterCase
) -> None:
    write_workflow(tmp_path, case.permission)
    path = tmp_path / ".github/workflows/sample.yml"
    path.write_text(
        path.read_text(encoding="utf-8")
        + "          cache: false\n          build-cache: true\n",
        encoding="utf-8",
    )
    assert bool(guard.check(tmp_path)) is case.rejected


def test_global_auto_with_guarded_remote_permission_is_supported(
    tmp_path: Path,
) -> None:
    write_workflow(tmp_path, UnsafePermission("auto", guard.MAIN_PUSH_ONLY_SAVE))
    assert guard.check(tmp_path) == []


def test_repository_savers_use_published_remote_permission() -> None:
    steps = [step for step in guard.collect(guard.ROOT) if step.main_action]
    assert steps
    for step in steps:
        if step.where.startswith("actions/"):
            assert step.inputs.get("save-cache") == "${{ inputs.save_cache }}"
        elif str(step.inputs.get("save-cache", "")).lower() != "false":
            assert step.inputs.get("save-cache") == "auto", step.where
            assert step.inputs.get("save-cache-remote") == guard.MAIN_PUSH_ONLY_SAVE, (
                step.where
            )
        assert "env.ACT" not in str(step.inputs), step.where


def test_build_target_preserves_explicit_global_disable() -> None:
    path = guard.ROOT / ".github/actions/build-target/action.yml"
    document = yaml.safe_load(path.read_text(encoding="utf-8"))
    assert document["inputs"]["save_cache"]["default"] == "false"
    setup = next(
        step
        for step in document["runs"]["steps"]
        if str(step.get("uses", "")).startswith("zackees/setup-soldr@")
    )
    assert setup["with"]["save-cache"] == "${{ inputs.save_cache }}"


def test_cook_remote_permission_requires_a_published_supporting_ref(
    tmp_path: Path,
) -> None:
    path = tmp_path / ".github/workflows/ci-check.yml"
    path.parent.mkdir(parents=True)
    path.write_text(
        "on: [push, pull_request]\njobs:\n  test:\n"
        "    runs-on: ubuntu-latest\n    steps:\n"
        "      - uses: zackees/setup-soldr/cook@a07bab94f16124b5c6857b137a237a53a61e06d1\n"
        "        if: inputs.os == 'ubuntu-latest'\n        with:\n"
        "          cook-delta: false\n          save-cache: auto\n"
        f"          save-cache-remote: {guard.MAIN_PUSH_ONLY_SAVE}\n",
        encoding="utf-8",
    )
    assert any(
        "does not honor save-cache-remote" in error
        for error in guard._cook_subaction_errors(tmp_path)
    )
