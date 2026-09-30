import shutil
from pathlib import Path

import pytest

from ci import check_cache_footprint as guard

CURRENT = next(iter(guard.SAVE_CACHE_REFS))
OLD = "5b2b45cecfc63c646413da68bb38677b87d043f3"  # v0.9.76, pre-#527


def _workflow(
    root: Path, name: str, body: str, on: str = "[push, pull_request]"
) -> None:
    path = root / ".github" / "workflows" / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"on: {on}\njobs:\n{body}", encoding="utf-8")


def _job(
    name: str, ref: str = CURRENT, os: str = "ubuntu-latest", **inputs: str
) -> str:
    inputs.setdefault("cook-delta", "false")
    inputs.setdefault("solo-toolchain-cache", "false")
    lines = "".join(f"          {k}: {v}\n" for k, v in inputs.items())
    return (
        f"  {name}:\n    runs-on: {os}\n    steps:\n"
        f"      - uses: zackees/setup-soldr@{ref}\n        with:\n"
        f"          toolchain: 1.95.0\n{lines}"
    )


def test_repository_workflows_pass() -> None:
    assert guard.check() == []


def test_measured_cache_cuts_cannot_be_reintroduced(tmp_path: Path) -> None:
    workflows = tmp_path / ".github" / "workflows"
    shutil.copytree(guard.ROOT / ".github/workflows", workflows)

    cases = (
        (
            "wrapper-e2e.yml",
            "prebuild-deps: none",
            "prebuild-deps: soldr-cook",
            "measured cook policy",
        ),
        (
            "ci.yml",
            "cache-key-suffix: dylint\n          linker: fast\n          prebuild-deps: none",
            "cache-key-suffix: dylint\n          linker: fast\n          prebuild-deps: soldr-cook",
            "measured cook policy",
        ),
        (
            "wrapper-e2e.yml",
            "build-cache: false",
            "build-cache: true",
            "disable Linux build-cache",
        ),
        (
            "ci-check.yml",
            "free more than it costs.\n          build-cache: ${{ inputs.os != 'macos-15' }}",
            "free more than it costs.\n          build-cache: true",
            "disable only macOS",
        ),
    )
    for index, (filename, old, new, diagnostic) in enumerate(cases):
        case_root = tmp_path / str(index)
        case_workflows = case_root / ".github" / "workflows"
        shutil.copytree(workflows, case_workflows)
        path = case_workflows / filename
        content = path.read_text(encoding="utf-8")
        assert old in content
        path.write_text(content.replace(old, new, 1), encoding="utf-8")
        assert any(diagnostic in error for error in guard.check(case_root)), (
            index,
            guard.check(case_root),
        )

    registry_root = tmp_path / "registry"
    registry_workflows = registry_root / ".github" / "workflows"
    shutil.copytree(workflows, registry_workflows)
    wrapper = registry_workflows / "wrapper-e2e.yml"
    wrapper.write_text(
        wrapper.read_text(encoding="utf-8").replace(
            "cargo-registry-cache: true", "cargo-registry-cache: false"
        ),
        encoding="utf-8",
    )
    registry_errors = guard.check(registry_root)
    assert sum("cargo-registry-cache: true" in error for error in registry_errors) == 2


def test_rejects_solo_toolchain_cache_producer(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a", **{"solo-toolchain-cache": "true"}))

    assert any("solo-toolchain-cache" in error for error in guard.check(tmp_path))


def test_rejects_macos_build_cache_reintroduction(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "ci-check.yml",
        _job(
            "check",
            os="${{ inputs.os }}",
            **{"build-cache": "true", "prebuild-deps": "soldr-cook"},
        )
        + _job(
            "test",
            os="${{ inputs.os }}",
            **{"build-cache": "true", "prebuild-deps": "soldr-cook"},
        ),
        on="workflow_call",
    )
    _workflow(
        tmp_path,
        "wrapper-e2e.yml",
        _job(
            "wrapper-e2e",
            os="${{ matrix.os }}",
            **{"build-cache": "true", "prebuild-deps": "soldr-cook"},
        ),
    )
    _workflow(
        tmp_path,
        "fs-matrix.yml",
        _job(
            "matrix",
            os="${{ matrix.os }}",
            **{"build-cache": "true"},
        ),
    )

    errors = guard.check(tmp_path)
    assert any(
        "macOS Check" in error and "disable build-cache" in error for error in errors
    )
    assert any(
        "macOS filesystem matrix" in error and "disable build-cache" in error
        for error in errors
    )


def test_windows_fs_matrix_cache_profile_is_guarded_red_green(tmp_path: Path) -> None:
    source = guard.ROOT / ".github/workflows/fs-matrix.yml"
    path = tmp_path / ".github/workflows/fs-matrix.yml"
    path.parent.mkdir(parents=True)
    content = source.read_text(encoding="utf-8")
    path.write_text(content, encoding="utf-8")

    assert guard.check(tmp_path) == []

    # Re-enabling the no-hit Windows build cache must fail the producer guard.
    path.write_text(content.replace("build-cache: false", "build-cache: true", 1))
    errors = guard.check(tmp_path)
    assert any("Windows setup must keep build-cache=False" in error for error in errors)

    # So must cooking again on Windows: cook bases are Linux-only (#1758).
    path.write_text(
        content.replace("prebuild-deps: none", "prebuild-deps: soldr-cook", 1)
    )
    errors = guard.check(tmp_path)
    assert any("Windows setup must keep prebuild-deps='none'" in error for error in errors)


def test_rejects_distinct_suffixes_for_same_shape(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job("test", **{"cache-key-suffix": "test"})
        + _job("check", **{"cache-key-suffix": "check"}),
    )
    assert any("same OS x feature shape" in e for e in guard.check(tmp_path))


def test_allows_justified_suffix_and_different_shapes(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job("test")
        + _job("dylint", **{"cache-key-suffix": "dylint"})
        + _job("e2e", **{"cache-key-suffix": "e2e", "prebuild-deps-flags": "--release"})
        + _job("mac", os="macos-15", **{"cache-key-suffix": "mac", "prebuild-deps": "none"}),
    )
    assert guard.check(tmp_path) == []


def test_matrix_os_overlap_is_detected(tmp_path: Path) -> None:
    matrix = (
        "${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-latest]"
    )
    _workflow(
        tmp_path,
        "a.yml",
        _job("m", os=matrix, **{"cache-key-suffix": "m-${{ matrix.os }}"}) + _job("u"),
    )
    assert len(guard.check(tmp_path)) == 1


def test_rejects_mixed_versions_and_pins(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job("a", version="0.9.21") + _job("b", ref=OLD, **{"save-cache": "auto"}),
    )
    errors = guard.check(tmp_path)
    assert any("soldr versions" in e for e in errors)
    assert any("pinned at 2 refs" in e for e in errors)


def test_old_pin_without_exemption_is_red(tmp_path: Path) -> None:
    # The pre-#527 pin cannot stop PR saves; with the exemption gone it fails
    # even when save-cache: auto is written (the old action ignores it).
    _workflow(tmp_path, "a.yml", _job("a", ref=OLD, **{"save-cache": "auto"}))
    assert any("can save caches on pull_request" in e for e in guard.check(tmp_path))


def test_new_pin_auto_or_unset_is_green(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a") + _job("b", **{"save-cache": "auto"}))
    assert guard.check(tmp_path) == []


def test_local_action_must_disable_pr_saves(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        "  action:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./\n",
    )

    assert any(
        "local action can save caches on pull_request" in error
        for error in guard.check(tmp_path)
    )


def test_local_action_main_only_save_gate_passes(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        "  action:\n    runs-on: ubuntu-latest\n    steps:\n"
        "      - uses: ./\n        with:\n"
        "          save-cache: ${{ github.ref == 'refs/heads/main' }}\n",
    )

    assert guard.check(tmp_path) == []


def test_save_cache_true_needs_justification(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("seed", **{"save-cache": '"true"'}))
    assert any("JUSTIFIED_PR_SAVES" in e for e in guard.check(tmp_path))


def test_save_cache_true_with_justification_passes(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setitem(guard.JUSTIFIED_PR_SAVES, "a.yml:seed", "seeds job b")
    _workflow(tmp_path, "a.yml", _job("seed", **{"save-cache": '"true"'}))
    assert guard.check(tmp_path) == []


def test_old_pin_false_expression_or_cache_off_passes(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setitem(guard.COOK_DELTA_REFS, OLD, "test")
    _workflow(
        tmp_path,
        "a.yml",
        _job("a", ref=OLD, **{"save-cache": "false"})
        + _job(
            "b",
            ref=OLD,
            **{"save-cache": "${{ github.event_name != 'pull_request' }}"},
        )
        + _job("c", ref=OLD, cache="false"),
    )
    assert guard.check(tmp_path) == []


def test_reusable_workflow_counts_as_pr_reachable(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a", ref=OLD), on="workflow_call")
    assert guard.check(tmp_path) != []


def test_push_only_workflow_may_save(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setitem(guard.COOK_DELTA_REFS, OLD, "test")
    _workflow(tmp_path, "a.yml", _job("a", ref=OLD), on="[push]")
    assert guard.check(tmp_path) == []


def test_cook_delta_unset_or_true_is_red(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job("a", **{"cook-delta": '""'}) + _job("b", **{"cook-delta": "true"}),
    )
    errors = [e for e in guard.check(tmp_path) if "cook-delta" in e]
    assert len(errors) == 2


def test_cook_delta_justified_passes(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setitem(guard.JUSTIFIED_COOK_DELTA, "a.yml:a", "measured win")
    _workflow(tmp_path, "a.yml", _job("a", **{"cook-delta": "true"}))
    assert guard.check(tmp_path) == []


def test_cook_delta_false_on_ref_without_input_is_red(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a", ref=OLD, **{"save-cache": "false"}))
    assert any("does not honor it" in e for e in guard.check(tmp_path))


def test_cook_delta_ignored_when_cache_off(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a", cache="false", **{"cook-delta": "true"}))
    assert guard.check(tmp_path) == []


def test_non_linux_cook_producer_is_red_green(tmp_path: Path) -> None:
    """#1758: cook bases are Linux-only (zackees/ci.yml#5, RUST-010)."""
    _workflow(tmp_path, "mac.yml", _job("mac", os="macos-15"))
    errors = guard.check(tmp_path)
    assert any("cook bases are Linux-only" in error for error in errors)

    _workflow(tmp_path, "mac.yml", _job("mac", os="macos-15", **{"prebuild-deps": "none"}))
    assert not any("cook bases are Linux-only" in e for e in guard.check(tmp_path))


def test_linux_only_cook_expression_passes_on_a_mixed_matrix(tmp_path: Path) -> None:
    expression = "${{ startsWith(matrix.os, 'ubuntu') && 'soldr-cook' || 'none' }}"
    _workflow(
        tmp_path,
        "mixed.yml",
        _job("mixed", os="${{ matrix.os }}", **{"prebuild-deps": expression}),
    )
    assert not any("cook bases are Linux-only" in e for e in guard.check(tmp_path))
    _workflow(tmp_path, "linux.yml", _job("linux", os="ubuntu-latest"))
    assert not any("cook bases are Linux-only" in e for e in guard.check(tmp_path))


def test_save_cache_policy_is_main_push_only_on_github_and_full_under_act() -> None:
    """GitHub pull_request runs never save; local act (bosn#309) saves fully."""
    policy = guard.SAVE_CACHE_POLICY

    def evaluate(event_name: str, ref: str, act: str = "") -> str:
        return guard.evaluate_save_policy(policy, event_name=event_name, ref=ref, act=act)

    assert evaluate("pull_request", "refs/pull/7/merge") == "false"
    assert evaluate("pull_request_target", "refs/heads/main") == "false"
    assert evaluate("push", "refs/heads/feature") == "false"
    assert evaluate("schedule", "refs/heads/main") == "false"
    assert evaluate("push", "refs/heads/main") == "auto"
    assert evaluate("pull_request", "refs/pull/7/merge", act="true") == "true"
    # On GitHub the act prefix is inert: identical to the bare #1677 rule.
    for event_name, ref in (
        ("pull_request", "refs/pull/7/merge"),
        ("push", "refs/heads/main"),
        ("push", "refs/heads/feature"),
    ):
        assert evaluate(event_name, ref) == guard.evaluate_save_policy(
            guard.MAIN_PUSH_ONLY_SAVE, event_name=event_name, ref=ref
        )
    assert guard._save_policy_errors() == []


def test_evaluate_save_policy_fails_closed_on_unknown_syntax() -> None:
    with pytest.raises(ValueError):
        guard.evaluate_save_policy(
            "${{ github.head_ref && 'true' || 'false' }}",
            event_name="pull_request",
            ref="refs/pull/7/merge",
        )


def test_save_cache_policy_is_accepted(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job("a", **{"save-cache": guard.SAVE_CACHE_POLICY}),
    )
    assert guard.check(tmp_path) == []


def test_bare_main_push_only_expression_is_rejected(tmp_path: Path) -> None:
    """The pre-act rule is safe on GitHub but leaves local act runs cold."""
    _workflow(
        tmp_path,
        "a.yml",
        _job("a", **{"save-cache": guard.MAIN_PUSH_ONLY_SAVE}),
    )
    errors = guard.check(tmp_path)
    assert any(
        "bare main-push-only save-cache rule" in error and "SAVE_CACHE_POLICY" in error
        for error in errors
    ), errors


def test_repository_setup_soldr_steps_use_the_act_policy() -> None:
    steps = [s for s in guard.collect(guard.ROOT) if s.main_action]
    savers = [
        s for s in steps if str(s.inputs.get("save-cache", "")).strip() != "false"
    ]
    assert savers
    for step in savers:
        expected = (
            guard.COMPOSITE_SAVE_CACHE_POLICY
            if step.where.startswith("actions/")
            else guard.SAVE_CACHE_POLICY
        )
        assert step.inputs.get("save-cache") == expected, step.where


def test_composite_without_act_prefix_is_rejected(tmp_path: Path) -> None:
    action = tmp_path / ".github" / "actions" / "build-target" / "action.yml"
    action.parent.mkdir(parents=True)
    action.write_text(
        "runs:\n  using: composite\n  steps:\n"
        f"    - uses: zackees/setup-soldr@{CURRENT}\n      with:\n"
        "        cook-delta: false\n        solo-toolchain-cache: false\n"
        "        save-cache: ${{ inputs.save_cache }}\n",
        encoding="utf-8",
    )
    assert any("COMPOSITE_SAVE_CACHE_POLICY" in e for e in guard.check(tmp_path))


def _cook_workflow(root: Path, body: str) -> None:
    _workflow(
        root,
        "ci-check.yml",
        "  test:\n    runs-on: ubuntu-latest\n    steps:\n" + body,
    )


def _cook_step(**overrides: str) -> str:
    fields = {
        "if": guard.COOK_SUBACTION_LINUX_ONLY_IF,
        "cook-delta": "false",
        "save-cache": guard.SAVE_CACHE_POLICY,
    }
    fields.update(overrides)
    when = fields.pop("if")
    lines = "".join(f'          {k}: "{v}"\n' for k, v in fields.items() if v)
    return (
        f"      - uses: zackees/setup-soldr/cook@{CURRENT}\n"
        f"        if: {when}\n        with:\n          flags: --workspace\n{lines}"
    )


def test_budgeted_cook_subaction_is_accepted(tmp_path: Path) -> None:
    _cook_workflow(tmp_path, _cook_step())
    assert guard._cook_subaction_errors(tmp_path) == []


def test_unbudgeted_cook_subaction_family_is_rejected(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "other.yml",
        "  x:\n    runs-on: ubuntu-latest\n    steps:\n" + _cook_step(),
    )
    errors = guard._cook_subaction_errors(tmp_path)
    assert any("no budget" in e for e in errors), errors


def test_cook_subaction_must_be_linux_only_delta_free_and_main_saved(tmp_path: Path) -> None:
    _cook_workflow(tmp_path, _cook_step(**{"if": "always()", "cook-delta": "true"}))
    errors = guard._cook_subaction_errors(tmp_path)
    assert any("Linux-only" in e for e in errors), errors
    assert any("cook-delta: false" in e for e in errors), errors
    _cook_workflow(tmp_path, _cook_step(**{"save-cache": "true"}))
    assert any("SAVE_CACHE_POLICY" in e for e in guard._cook_subaction_errors(tmp_path))


def test_cook_subaction_budgets_cannot_exceed_the_family_total(monkeypatch) -> None:
    monkeypatch.setattr(
        guard, "COOK_SUBACTION_BUDGETS", {"a.yml:x": 400_000_000, "b.yml:y": 400_000_000}
    )
    errors = guard._cook_subaction_errors(guard.ROOT)
    assert any("COOK_SUBACTION_TOTAL_BUDGET_BYTES" in e for e in errors), errors
