from pathlib import Path

from ci import check_dylint_wiring


def _write(root: Path, relative: str, text: str = "") -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _scaffold(root: Path, *, excluded: bool = True, source: str = "") -> None:
    exclusion = '["dylints/example"]' if excluded else "[]"
    _write(root, "Cargo" + ".toml", f"[workspace]\nexclude = {exclusion}\n")
    _write(root, "dylints/example/" + "Cargo.toml")
    _write(root, "dylints/example/README.md")
    _write(root, "dylints/example/rust-toolchain.toml")
    _write(root, "dylints/example/src/README.md")
    _write(root, "dylints/example/src/lib.rs", source)
    _write(root, "ci/lint.py", 'paths.glob("*/Cargo.toml")')
    _write(
        root,
        ".github/workflows/ci.yml",
        "ci/check_dylint_wiring.py\ndylints/example/Cargo.toml",
    )


def test_check_accepts_dynamic_manifest_wiring(tmp_path: Path) -> None:
    _scaffold(tmp_path)

    assert check_dylint_wiring.check(tmp_path) == []


def test_check_rejects_stale_allowlist_scope(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _write(
        tmp_path,
        "dylints/example/src/allowlist.txt",
        "crates/gone/src/lib.rs\n",
    )

    assert any(
        "stale allowlist path" in error
        for error in check_dylint_wiring.check(tmp_path)
    )


def test_check_rejects_missing_exclude_and_stale_source_prefix(
    tmp_path: Path,
) -> None:
    _scaffold(
        tmp_path,
        excluded=False,
        source='const DAEMON_SOURCE_PREFIX: &str = "crates/removed/src/";\n',
    )

    errors = check_dylint_wiring.check(tmp_path)
    assert any("workspace.exclude" in error for error in errors)
    assert any("stale Dylint source prefix" in error for error in errors)


def test_check_rejects_stale_source_prefix_in_array(tmp_path: Path) -> None:
    _scaffold(
        tmp_path,
        source=(
            'const SOURCE_PREFIXES: &[&str] = &[\n'
            '    "crates/removed/src/",\n'
            "];\n"
        ),
    )

    assert any(
        "stale Dylint source prefix" in error
        for error in check_dylint_wiring.check(tmp_path)
    )


def _scaffold_baseline(root: Path, content: str) -> Path:
    _write(root, "crates/zccache-ipc/src/lib.rs")
    baseline = root / "dylints" / "example" / "src" / "baseline.txt"
    baseline.parent.mkdir(parents=True, exist_ok=True)
    baseline.write_text(content, encoding="utf-8")
    return baseline


def test_platform_baseline_accepts_exact_occurrences(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _scaffold_baseline(
        tmp_path,
        "# total = 2\n"
        "crates/zccache-ipc/src/lib.rs\tattr_cfg\twindows\t0\n"
        "crates/zccache-ipc/src/lib.rs\tnative_import\tlibc\t0\n",
    )

    assert check_dylint_wiring.check(tmp_path) == []


def test_platform_baseline_rejects_stale_paths_and_zones(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _scaffold_baseline(
        tmp_path,
        "# total = 3\n"
        "crates/gone/src/lib.rs\tattr_cfg\twindows\t0\n"
        "crates/zccache-ipc/src/platform.rs\tattr_cfg\twindows\t0\n"
        "crates/zccache-ipc/src/tests/x.rs\tattr_cfg\twindows\t0\n",
    )

    errors = check_dylint_wiring.check(tmp_path)
    assert any("stale platform-boundary baseline path" in error for error in errors)
    assert any("allowed zone" in error for error in errors)
    assert any("outside production scope" in error for error in errors)


def test_platform_baseline_rejects_duplicates_and_gaps(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _scaffold_baseline(
        tmp_path,
        "# total = 3\n"
        "crates/zccache-ipc/src/lib.rs\tattr_cfg\twindows\t0\n"
        "crates/zccache-ipc/src/lib.rs\tattr_cfg\twindows\t0\n"
        "crates/zccache-ipc/src/lib.rs\tattr_cfg\tunix\t2\n",
    )

    errors = check_dylint_wiring.check(tmp_path)
    assert any("duplicate row" in error for error in errors)
    assert any("not contiguous" in error for error in errors)


def test_platform_baseline_total_must_match_rows(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _scaffold_baseline(
        tmp_path,
        "# total = 9\n"
        "crates/zccache-ipc/src/lib.rs\tattr_cfg\twindows\t0\n",
    )

    errors = check_dylint_wiring.check(tmp_path)
    assert any("total 9 != 1 rows" in error for error in errors)


def test_cfg_coverage_accepts_os_level_gates(tmp_path: Path) -> None:
    _scaffold(tmp_path)
    _write(
        tmp_path,
        "crates/x/src/lib.rs",
        '#[cfg(all(unix, not(target_os = "macos")))]\n'
        "fn a() {}\n"
        '#[cfg_attr(target_family = "windows", allow(dead_code))]\n'
        "fn b() {}\n"
        'fn c() -> bool { cfg!(target_arch = "aarch64") }\n',
    )

    assert check_dylint_wiring.check(tmp_path) == []


def test_cfg_coverage_rejects_gates_no_dylint_leg_selects(tmp_path: Path) -> None:
    # #1740: Dylint lints one triple per OS, so an arch/env/unknown-OS gate
    # would hide source from every leg.
    _scaffold(tmp_path)
    _write(
        tmp_path,
        "crates/x/src/lib.rs",
        '#[cfg(target_arch = "aarch64")]\n'
        "fn a() {}\n"
        "#[cfg(all(\n"
        "    windows,\n"
        '    target_env = "gnu",\n'
        "))]\n"
        "fn b() {}\n"
        '#![cfg_attr(target_os = "freebsd", allow(dead_code))]\n',
    )

    errors = check_dylint_wiring.check(tmp_path)
    assert any('crates/x/src/lib.rs:1: target_arch = "aarch64"' in e for e in errors)
    assert any('crates/x/src/lib.rs:5: target_env = "gnu"' in e for e in errors)
    assert any('crates/x/src/lib.rs:8: target_os = "freebsd"' in e for e in errors)


def test_cfg_coverage_os_set_matches_the_lint_targets() -> None:
    from ci import lint

    assert check_dylint_wiring.DYLINT_OSES == frozenset(lint.DYLINT_OS_TARGETS)


def test_dylint_library_tests_share_one_target_dir() -> None:
    """Each lint library is its own workspace; without a shared target dir every
    step rebuilt dylint_linting/clippy_utils/dylint_testing (~30 s each)."""
    import re as _re

    root = Path(__file__).resolve().parents[2]
    workflow = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    steps = _re.findall(
        r"      - name: Test Dylint Library \([a-z_]+\)\n(.*?)(?=\n      - name: |\Z)",
        workflow,
        flags=_re.DOTALL,
    )
    assert steps
    for step in steps:
        assert "CARGO_TARGET_DIR: ${{ runner.temp }}/dylint-library-tests" in step
