from __future__ import annotations

import importlib.util
import tarfile
import zipfile
from pathlib import Path

import pytest


def _load_package_release():
    module_path = Path(__file__).resolve().parents[1] / "package_release.py"
    spec = importlib.util.spec_from_file_location("package_release", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _load_stamp_release():
    module_path = Path(__file__).resolve().parents[1] / "stamp_release.py"
    spec = importlib.util.spec_from_file_location("stamp_release", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package_release = _load_package_release()
stamp_release = _load_stamp_release()


def _repo_text(*parts: str) -> str:
    return (Path(__file__).resolve().parents[2] / Path(*parts)).read_text(
        encoding="utf-8"
    )


def _matrix_entry(workflow_text: str, target: str) -> str:
    marker = f"          - target: {target}\n"
    start = workflow_text.index(marker)
    next_start = workflow_text.find("\n          - target:", start + len(marker))
    steps_start = workflow_text.find("\n    steps:", start)
    end_candidates = [pos for pos in (next_start, steps_start) if pos != -1]
    assert end_candidates
    return workflow_text[start : min(end_candidates)]


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
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'cp -RL "$src" staging-debug/' in action
    assert 'cp -L "$src" staging-debug/' in action


def test_build_target_stamps_release_binaries_with_python_footer() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "python ci/stamp_release.py" in action
    assert "zccache-stamp" not in action
    assert 'soldr cargo build --release --target "$HOST_TARGET"' not in action


def test_build_target_compiles_release_artifacts_without_compile_cache() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "cargo_build=(soldr --no-cache cargo build)" in action
    assert 'cargo_build=(rustup run "$RELEASE_RUST_TOOLCHAIN" cargo build)' in action
    assert "Release artifacts are distribution outputs" in action
    assert '"${cargo_build[@]}" --release --target ${{ inputs.target }} -p zccache --bin zccache' in action
    assert '"${cargo_build[@]}" --release --target ${{ inputs.target }} -p zccache-cli --features python --lib' in action


def test_release_workflow_disables_soldr_for_artifact_builds() -> None:
    release_workflow = _repo_text(".github/workflows/release-auto.yml")
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'use_soldr: "false"' in release_workflow
    assert "if: inputs.use_soldr == 'true'" in action
    assert "Use setup-soldr for setup and caching" in action


def test_build_target_forces_msvc_host_toolchain_for_windows() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "1.94.1-x86_64-pc-windows-msvc" in action
    assert 'rustup run "$RELEASE_RUST_TOOLCHAIN" rustc -vV' in action
    assert "Windows release builds must use the MSVC host toolchain" in action
    assert 'rustup which --toolchain "$RELEASE_RUST_TOOLCHAIN" rustc' in action


def test_build_target_smoke_requires_valid_version_output() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'version="$("$BIN" --version)"' in action
    assert "Built zccache binary did not report a valid version" in action
    assert "grep -Eq '^zccache [0-9]'" in action


def test_build_target_exposes_cross_cache_controls() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "prebuild_deps:" in action
    assert "use_soldr:" in action
    assert "clear_target_after_setup:" in action
    assert "require_debug_sidecars:" in action
    assert "prebuild-deps: ${{ inputs.prebuild_deps }}" in action
    assert "if: inputs.clear_target_after_setup == 'true'" in action
    assert 'TARGET_DIR="target/${{ inputs.target }}"' in action
    assert 'rm -rf "$TARGET_DIR"' in action


def test_build_target_configures_target_c_compiler_for_cross_c_sources() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'TARGET_CC=$(echo "${{ inputs.target }}" | tr \'-\' \'_\')' in action
    assert 'echo "CC_${TARGET_CC}=${{ inputs.linker }}" >> "$GITHUB_ENV"' in action


def test_build_target_can_synthesize_macos_dsym_sidecars() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "copy_or_create_dsym()" in action
    assert 'dsymutil "$TARGET_DIR/$bin" -o "staging-debug/$dsym"' in action
    assert 'copy_or_create_dsym "zccache" "zccache.dSYM"' in action
    assert 'copy_or_create_dsym "zccache-daemon" "zccache-daemon.dSYM"' in action
    assert 'copy_or_create_dsym "zccache-fp" "zccache-fp.dSYM"' in action


def test_build_target_can_treat_debug_sidecars_as_optional() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'REQUIRE_DEBUG_SIDECARS="${{ inputs.require_debug_sidecars }}"' in action
    assert 'if [ "$REQUIRE_DEBUG_SIDECARS" = "true" ]; then' in action
    assert "::warning::No debug sidecars staged for target $TARGET" in action
    assert "::warning::Missing debug sidecars for $TARGET: ${missing[*]}" in action


def test_build_target_uses_target_specific_binary_size_floor() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "*pc-windows-msvc)" in action
    assert "min_size=1048576" in action
    assert "min_size=262144" in action
    assert "minimum $min_size" in action


def test_release_and_build_workflows_disable_cook_cache_for_cross_targets() -> None:
    cross_targets = {
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "aarch64-pc-windows-msvc",
    }
    native_targets = {
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    }

    build_workflow = _repo_text(".github/workflows/build.yml")
    release_workflow = _repo_text(".github/workflows/release-auto.yml")

    for workflow in (build_workflow, release_workflow):
        assert "prebuild_deps: ${{ matrix.prebuild_deps || 'soldr-cook' }}" in workflow
        assert (
            "clear_target_after_setup: "
            "${{ matrix.clear_target_after_setup || 'false' }}"
        ) in workflow
        assert (
            "require_debug_sidecars: "
            "${{ matrix.require_debug_sidecars || 'true' }}"
        ) in workflow

        for target in cross_targets:
            block = _matrix_entry(workflow, target)
            assert "prebuild_deps: none" in block
            assert 'clear_target_after_setup: "true"' in block

        windows_arm_block = _matrix_entry(workflow, "aarch64-pc-windows-msvc")
        assert 'require_debug_sidecars: "false"' in windows_arm_block

    for target in native_targets:
        block = _matrix_entry(build_workflow, target)
        assert "prebuild_deps: none" not in block
        assert "clear_target_after_setup:" not in block

        release_block = _matrix_entry(release_workflow, target)
        assert "prebuild_deps: none" not in release_block
        assert 'clear_target_after_setup: "true"' in release_block


def test_release_workflow_restart_attempts_resume_existing_github_release() -> None:
    workflow = _repo_text(".github/workflows/release-auto.yml")

    assert "RUN_ATTEMPT: ${{ github.run_attempt }}" in workflow
    assert 'if [ "${RUN_ATTEMPT:-1}" != "1" ]; then' in workflow
    assert "GitHub Release checkpoint" in workflow
    assert "overwrite_files: true" in workflow


def test_stamp_release_marker_layout_and_append(tmp_path: Path) -> None:
    marker = stamp_release.encode_marker(
        git_sha="0123456789abcdef0123456789abcdef01234567",
        version="1.11.4",
        triple="x86_64-unknown-linux-gnu",
        build_timestamp=1_700_000_123,
    )

    assert len(marker) == 128
    assert marker[0:40] == b"0123456789abcdef0123456789abcdef01234567"
    assert marker[40:46] == b"1.11.4"
    assert marker[56:80] == b"x86_64-unknown-linux-gnu"
    assert marker[88:96] == (1_700_000_123).to_bytes(8, "little")
    assert marker[120:128] == b"ZCCSYMv1"

    binary = tmp_path / "zccache"
    binary.write_bytes(b"binary")
    stamp_release.append_marker(binary, marker)
    assert binary.read_bytes() == b"binary" + marker


def test_stamp_release_rejects_oversized_fields() -> None:
    with pytest.raises(ValueError):
        stamp_release.encode_marker(
            git_sha="0" * 40,
            version="1.11.4",
            triple="x86_64-some-extremely-long-triple-that-cannot-fit",
            build_timestamp=1,
        )


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
