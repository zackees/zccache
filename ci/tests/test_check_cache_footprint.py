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
    assert any("macOS Check" in error and "disable build-cache" in error for error in errors)
    assert any("macOS Test" in error and "disable build-cache" in error for error in errors)
    assert any(
        "macOS wrapper-e2e" in error and "disable build-cache" in error
        for error in errors
    )
    assert any(
        "macOS filesystem matrix" in error and "disable build-cache" in error
        for error in errors
    )


def test_accepts_only_exact_linux_wrapper_build_cache_probe(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "wrapper-e2e.yml",
        _job(
            "wrapper-e2e",
            os="${{ matrix.os }}",
            **{
                "build-cache": guard.LINUX_WRAPPER_BUILD_CACHE_PROBE,
                "prebuild-deps": "soldr-cook",
            },
        ),
    )
    assert guard.check(tmp_path) == []


def test_rejects_unrecognized_linux_wrapper_build_cache_probe(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "wrapper-e2e.yml",
        _job(
            "wrapper-e2e",
            os="${{ matrix.os }}",
            **{
                "build-cache": "${{ github.head_ref == 'probe/cook-off-matrix' && 'false' || matrix.os != 'macos-15' }}",
                "prebuild-deps": "soldr-cook",
            },
        ),
    )
    assert any(
        "must disable build-cache on macos-15" in error
        for error in guard.check(tmp_path)
    )


def test_accepts_only_exact_windows_test_build_cache_probe(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "ci-check.yml",
        _job(
            "test",
            os="${{ inputs.os }}",
            **{
                "save-cache": guard.WINDOWS_COOK_OFF_PROBE_SAVE,
                "build-cache": guard.WINDOWS_TEST_BUILD_CACHE_PROBE,
                "prebuild-deps": "soldr-cook",
                "cargo-registry-cache": "true",
            },
        ),
    )
    assert guard.check(tmp_path) == []


def test_rejects_unrecognized_windows_test_build_cache_probe(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "ci-check.yml",
        _job(
            "test",
            os="${{ inputs.os }}",
            **{
                "save-cache": guard.WINDOWS_COOK_OFF_PROBE_SAVE,
                "build-cache": "${{ github.head_ref == 'probe/cook-off-matrix' && 'false' || inputs.os != 'macos-15' }}",
                "prebuild-deps": "soldr-cook",
                "cargo-registry-cache": "true",
            },
        ),
    )
    assert any(
        "macOS Test" in error and "disable build-cache" in error
        for error in guard.check(tmp_path)
    )


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


def test_exact_cook_off_probe_gate_is_pr_safe_and_keeps_base_shape(
    tmp_path: Path,
) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job(
            "dylint",
            **{
                "save-cache": guard.COOK_OFF_PROBE_SAVE,
                "prebuild-deps": next(
                    item
                    for item in guard.COOK_OFF_PROBE_INPUTS
                    if "inputs.os" not in item
                ),
            },
        )
        + _job(
            "windows-test",
            os="${{ inputs.os }}",
            **{
                "save-cache": guard.WINDOWS_COOK_OFF_PROBE_SAVE,
                "prebuild-deps": next(
                    item
                    for item in guard.COOK_OFF_PROBE_INPUTS
                    if "inputs.os" in item
                ),
            },
        ),
    )
    assert guard.check(tmp_path) == []
    assert all(guard._shape(step)[1] == "soldr-cook" for step in guard.collect(tmp_path))


def test_unrecognized_probe_save_expression_is_red(tmp_path: Path) -> None:
    _workflow(
        tmp_path,
        "a.yml",
        _job(
            "probe",
            **{
                "save-cache": "${{ github.head_ref == 'probe/cook-off-matrix' && 'false' || 'auto' }}",
            },
        ),
    )
    assert any("can save caches on pull_request" in e for e in guard.check(tmp_path))


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
