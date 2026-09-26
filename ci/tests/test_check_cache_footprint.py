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
    lines = "".join(f"          {k}: {v}\n" for k, v in inputs.items())
    return (
        f"  {name}:\n    runs-on: {os}\n    steps:\n"
        f"      - uses: zackees/setup-soldr@{ref}\n        with:\n"
        f"          toolchain: 1.95.0\n{lines}"
    )


def test_repository_workflows_pass() -> None:
    assert guard.check() == []


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


def test_save_cache_true_needs_justification(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("seed", **{"save-cache": '"true"'}))
    assert any("JUSTIFIED_PR_SAVES" in e for e in guard.check(tmp_path))


def test_save_cache_true_with_justification_passes(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setitem(guard.JUSTIFIED_PR_SAVES, "a.yml:seed", "seeds job b")
    _workflow(tmp_path, "a.yml", _job("seed", **{"save-cache": '"true"'}))
    assert guard.check(tmp_path) == []


def test_old_pin_false_expression_or_cache_off_passes(tmp_path: Path) -> None:
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


def test_push_only_workflow_may_save(tmp_path: Path) -> None:
    _workflow(tmp_path, "a.yml", _job("a", ref=OLD), on="[push]")
    assert guard.check(tmp_path) == []
