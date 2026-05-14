from __future__ import annotations

import importlib.util
import tarfile
import zipfile
from pathlib import Path


def _load_package_release():
    module_path = Path(__file__).resolve().parents[1] / "package_release.py"
    spec = importlib.util.spec_from_file_location("package_release", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package_release = _load_package_release()


def _write_fake_binary(path: Path) -> None:
    path.write_bytes(b"binary\n")


def test_write_tarball_preserves_full_version_and_target(tmp_path: Path) -> None:
    input_dir = tmp_path / "input"
    output_dir = tmp_path / "output"
    input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        _write_fake_binary(input_dir / name)

    stage_dir, archive_base = package_release.stage_tree(
        version="1.3.10",
        target="x86_64-unknown-linux-musl",
        binary_ext="",
        input_dir=input_dir,
        output_dir=output_dir,
    )
    archive = package_release.write_tarball(stage_dir, archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-unknown-linux-musl.tar.gz"

    with tarfile.open(archive, "r:gz") as tf:
        assert tf.getmember("zccache-v1.3.10-x86_64-unknown-linux-musl/zccache")


def test_write_zip_preserves_full_version_and_target(tmp_path: Path) -> None:
    input_dir = tmp_path / "input"
    output_dir = tmp_path / "output"
    input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        _write_fake_binary(input_dir / f"{name}.exe")

    stage_dir, archive_base = package_release.stage_tree(
        version="1.3.10",
        target="x86_64-pc-windows-msvc",
        binary_ext=".exe",
        input_dir=input_dir,
        output_dir=output_dir,
    )
    archive = package_release.write_zip(stage_dir, archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-pc-windows-msvc.zip"

    with zipfile.ZipFile(archive) as zf:
        assert "zccache-v1.3.10-x86_64-pc-windows-msvc/zccache.exe" in zf.namelist()


def test_stage_debug_tree_packages_dwp_files(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        (debug_input_dir / f"{name}.dwp").write_bytes(b"dwp\n")

    result = package_release.stage_debug_tree(
        version="1.3.10",
        target="x86_64-unknown-linux-gnu",
        debug_input_dir=debug_input_dir,
        output_dir=output_dir,
    )
    assert result is not None
    debug_stage_dir, debug_archive_base = result
    archive = package_release.write_tarball(debug_stage_dir, debug_archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-unknown-linux-gnu-debug.tar.gz"
    with tarfile.open(archive, "r:gz") as tf:
        members = {member.name for member in tf.getmembers()}
        for name in package_release.INCLUDE:
            assert (
                f"zccache-v1.3.10-x86_64-unknown-linux-gnu-debug/{name}.dwp" in members
            )


def test_stage_debug_tree_handles_dsym_directories(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    dsym = debug_input_dir / "zccache.dSYM"
    (dsym / "Contents/Resources/DWARF").mkdir(parents=True)
    (dsym / "Contents/Resources/DWARF/zccache").write_bytes(b"dwarf\n")

    result = package_release.stage_debug_tree(
        version="1.3.10",
        target="x86_64-apple-darwin",
        debug_input_dir=debug_input_dir,
        output_dir=output_dir,
    )
    assert result is not None
    debug_stage_dir, debug_archive_base = result
    archive = package_release.write_tarball(debug_stage_dir, debug_archive_base)

    with tarfile.open(archive, "r:gz") as tf:
        members = {member.name for member in tf.getmembers()}
        assert (
            "zccache-v1.3.10-x86_64-apple-darwin-debug/zccache.dSYM/"
            "Contents/Resources/DWARF/zccache" in members
        )


def test_build_target_dereferences_debug_sidecar_symlinks() -> None:
    action = (
        Path(__file__).resolve().parents[2]
        / ".github/actions/build-target/action.yml"
    ).read_text(encoding="utf-8")

    assert 'cp -RL "$src" staging-debug/' in action
    assert 'cp -L "$src" staging-debug/' in action


def test_stage_debug_tree_skips_empty_input(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    assert (
        package_release.stage_debug_tree(
            version="1.3.10",
            target="x86_64-unknown-linux-gnu",
            debug_input_dir=debug_input_dir,
            output_dir=output_dir,
        )
        is None
    )


def test_stage_debug_tree_skips_missing_input(tmp_path: Path) -> None:
    missing = tmp_path / "nope"
    output_dir = tmp_path / "output"
    output_dir.mkdir()

    assert (
        package_release.stage_debug_tree(
            version="1.3.10",
            target="x86_64-unknown-linux-gnu",
            debug_input_dir=missing,
            output_dir=output_dir,
        )
        is None
    )
