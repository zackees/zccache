from __future__ import annotations

import importlib.util
import re
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
        assert not any(member.name.endswith("/zccache-daemon") for member in tf.getmembers())


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
        assert not any(name.endswith("/zccache-daemon.exe") for name in zf.namelist())


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


def test_build_target_selects_native_or_cross_release_cache_policy() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")
    compact_action = " ".join(action.replace("\\\n", "").split())

    assert "cargo_build=(soldr --no-cache cargo build)" in action
    assert 'cargo_build=(rustup run "$RELEASE_RUST_TOOLCHAIN" cargo build)' in action
    assert "Native distribution builds keep the historical no-cache path" in action
    assert "cross lanes deliberately use the current-worktree bootstrap zccache" in action
    assert (
        '"${cargo_build[@]}" --release --target ${{ inputs.target }} -p zccache '
        "--features zccache-bin,fingerprint-bin "
        "--bin zccache --bin zccache-fp"
        in compact_action
    )
    assert "--bin zccache-daemon" not in compact_action
    assert '"${cargo_build[@]}" --release --target ${{ inputs.target }} -p zccache-cli --features python --lib' in action


def test_release_workflow_uses_bootstrap_zccache_for_cross_builds() -> None:
    release_workflow = _repo_text(".github/workflows/release-auto.yml")
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "bootstrap-zccache:" in release_workflow
    assert "RUSTC_WRAPPER: \"\"" in release_workflow
    assert "unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER" in release_workflow
    assert "name: bootstrap-zccache" in release_workflow
    assert "needs: [preflight, bootstrap-zccache]" in release_workflow
    assert "SOLDR_RUSTC_WRAPPER=" in release_workflow
    assert 'use_soldr: "true"' in release_workflow
    assert "runs-on: ubuntu-24.04" in release_workflow
    assert release_workflow.count("runs-on: ${{ matrix.os }}") == 1
    assert "if: inputs.use_soldr == 'true'" in action
    assert "Use setup-soldr for setup and caching" in action


def test_release_tests_exec_cached_in_every_native_wheel_family() -> None:
    """Every shipped wheel family runs the exec_cached smoke on real hardware.

    Which families exist is asserted against `ci/release_workflow.PLATFORMS`
    by `test_release_workflow.py`; this test owns the runner-OS coverage and
    the publish gating. musllinux is deliberately absent: `build-wheels` does
    not produce one (the musl build legs set `include_python: "false"`), so an
    alpine smoke leg could only ever fail and block the release.
    """
    workflow = _repo_text(".github/workflows/release-auto.yml")

    assert "test-wheels:" in workflow
    for platform in (
        "ubuntu-latest",
        "ubuntu-24.04-arm",
        "macos-15-intel",
        "macos-14",
        "windows-latest",
        "windows-11-arm",
    ):
        assert f"- os: {platform}" in workflow
    assert "python ci/test_exec_cached_wheel.py" in workflow
    assert "musllinux" not in workflow
    assert "alpine" not in workflow
    assert "needs: [preflight, publish-release, build-wheels, test-wheels]" in workflow
    assert "needs.test-wheels.result == 'success'" in workflow


def test_cross_builds_go_through_the_blessed_soldr_surface() -> None:
    """zccache#1497: one toolchain owner, one build surface.

    Two providers is what broke 1.13.6. `dtolnay/rust-toolchain` installed
    into a repo-local RUSTUP_HOME before setup-soldr ran, so setup-soldr had
    no toolchain to cache but still wrote a 6-file, 2.5 MB entry under the
    shared `solo-toolchain-v2-<host>-…` key that other jobs fill with
    146-219 MB. Newest-wins restore let the small entry shadow the good one
    and every cross target died on `can't find crate for core`.

    `soldr cargo …` is the documented legacy passthrough; `soldr build` is
    the blessed surface that prepares the sysroot and compiler/linker. Cross
    lanes must use the latter.
    """
    action = _repo_text(".github/actions/build-target/action.yml")
    release_workflow = _repo_text(".github/workflows/release-auto.yml")
    build_workflow = _repo_text(".github/workflows/build.yml")

    # The purge: no second toolchain provider, no legacy cross drivers.
    assert "dtolnay/rust-toolchain" not in action
    assert "mlugg/setup-zig" not in action
    assert "cargo-zigbuild" not in action
    assert "cargo-xwin" not in action
    assert "cross_driver" not in action
    assert "zigbuild" not in action
    assert "cargo xwin" not in action

    # setup-soldr owns the whole target lifecycle for cross lanes.
    assert "zackees/setup-soldr@" in action
    assert "cross-targets: ${{ inputs.cross_compile == 'true'" in action

    # Builds go through the blessed surface, still one rustc child at a time.
    assert action.count("cargo_build=(soldr build --jobs 1)") == 2
    assert "cargo_build=(soldr build)" not in action
    assert "cargo_build=(soldr build --jobs 2)" not in action

    assert "verify-compile-cache:" in action
    assert "Bootstrap zccache is not first on PATH" in action
    for workflow in (release_workflow, build_workflow):
        assert "cross_driver" not in workflow
        assert "name: binaries-${{ matrix.target }}" in workflow
        assert "name: release-${{ matrix.target }}" in workflow


def test_cross_prerequisites_cover_vendored_c_and_macos_debug_info() -> None:
    """The two cross prerequisites that outlived the zig/xwin purge (#1497).

    Both were previously keyed on `cross_driver`; they are keyed on the target
    or on `cross_compile` now, because the driver input is gone -- not because
    the underlying need went away.
    """
    action = _repo_text(".github/actions/build-target/action.yml")

    # soldr still materializes zig for the Linux/Darwin cross lanes, and the
    # mimalloc-pprof amalgamation reports __DATE__/__TIME__.
    assert "-Wno-error=date-time" in action
    assert "if: inputs.cross_compile == 'true'" in action

    # dsymutil is consumed by debug-sidecar staging, so it is keyed on the
    # target rather than on the deleted driver.
    assert "Install LLVM dSYM tools" in action
    assert "if: contains(inputs.target, 'apple-darwin')" in action
    assert "llvm-dsymutil" in action
    assert "find -L /usr/bin" in action
    assert 'test -x "$llvm_dsymutil"' in action
    assert 'ln -s "$llvm_dsymutil" /usr/local/bin/dsymutil' in action
    assert "test -x /usr/local/bin/dsymutil" in action


def test_linux_cross_build_repairs_debug_sidecars_missing_from_cache_hits() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "name: Repair missing Linux debug sidecars" in action
    assert (
        "if: inputs.cross_compile == 'true' && "
        "contains(inputs.target, 'unknown-linux')"
    ) in action
    assert 'soldr cargo clean -p zccache --release --target "$TARGET"' in action
    assert "soldr --no-cache build --jobs 1" in action
    assert 'test -e "$TARGET_DIR/zccache.dwp"' in action
    assert 'test -e "$TARGET_DIR/zccache-fp.dwp"' in action


def test_xwin_arm64_lane_injects_no_global_cxx_language_mode() -> None:
    """zccache#1439: `CFLAGS` reaches every `cc-rs` build script.

    A global `-TP`/`/TP` compiles `ring`'s C sources as C++, which fails with
    `void *` initialisation and `-Wc++11-narrowing` errors and stops the
    release before publication. Windows ARM64 uses the system allocator.
    """
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "-TP" not in action
    assert "/TP" not in action
    assert "Configure xwin ARM64 C compatibility" not in action


def test_locked_mimalloc_pprof_selects_arm64_c11_atomics() -> None:
    """zccache#1439: mimalloc's MSVC C-mode atomics wrapper calls Interlocked
    intrinsics that clang-cl does not declare for ARM64.

    mimalloc-pprof 0.9.3 selects clang's C11 `stdatomic` for that target in
    its own build script, which is the only package-scoped place the choice
    can be made. Anything older forces the consumer back to a global
    `CFLAGS=-TP`, which breaks every other `cc-rs` dependency.
    """
    lockfile = _repo_text("Cargo.lock")
    locked = re.search(
        r'\[\[package\]\]\nname = "mimalloc-pprof"\nversion = "([^"]+)"', lockfile
    )

    assert locked is not None, "mimalloc-pprof missing from Cargo.lock"
    major, minor, patch = (int(part) for part in locked.group(1).split(".")[:3])
    assert (major, minor, patch) >= (0, 9, 3), (
        f"mimalloc-pprof {locked.group(1)} predates the ARM64 C11-atomics fix; "
        "aarch64-pc-windows-msvc cannot cross-compile against it"
    )


def test_release_workflow_dry_run_builds_without_publishing() -> None:
    workflow = _repo_text(".github/workflows/release-auto.yml")

    assert "dry-run:" in workflow
    assert "type: boolean" in workflow
    assert "if: inputs['dry-run'] != true" in workflow
    assert workflow.count("inputs['dry-run'] != true") >= 3


def test_build_target_forces_msvc_host_toolchain_for_windows() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "1.95.0-x86_64-pc-windows-msvc" in action
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
    assert 'copy_or_create_dsym "zccache-fp" "zccache-fp.dSYM"' in action
    assert "zccache-daemon.dSYM" not in action


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


def test_release_and_build_workflows_disable_cook_cache_for_linux_cross_matrix() -> None:
    cross_targets = {
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
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
