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
