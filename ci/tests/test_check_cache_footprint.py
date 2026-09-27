import shutil
from pathlib import Path

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
            "build-cache: ${{ inputs.os != 'macos-15' && inputs.os != 'windows-latest' && inputs.os != 'windows-11-arm' }}",
            "build-cache: true",
            "disable Windows x64/ARM64 build-cache",
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

    # So must emitting the measured 1 GiB release-profile cook instead of the
    # already-warm fnone base.
    path.write_text(content.replace('prebuild-deps-flags: ""', 'prebuild-deps-flags: "--release"', 1))
    errors = guard.check(tmp_path)
    assert any("Windows setup must keep prebuild-deps-flags=''" in error for error in errors)


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
        + _job("mac", os="macos-15", **{"cache-key-suffix": "mac"}),
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
