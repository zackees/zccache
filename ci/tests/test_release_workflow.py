from __future__ import annotations

import csv
import io
import os
import re
import zipfile
from pathlib import Path

import pytest

from ci import release_workflow
from ci.release_workflow import (
    assert_installed_wheel_scripts_executable,
    assert_wheel_script_metadata,
)


def _write_wheel(
    path: Path,
    *,
    create_system: int,
    mode: int,
    include_dist_info: bool = False,
) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as whl:
        info = zipfile.ZipInfo("zccache-1.2.3.data/scripts/zccache")
        info.create_system = create_system
        info.external_attr = mode << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        whl.writestr(info, b"#!/bin/sh\n")
        if include_dist_info:
            metadata = b"Metadata-Version: 2.1\nName: zccache\nVersion: 1.2.3\n"
            wheel = (
                b"Wheel-Version: 1.0\n"
                b"Generator: zccache-test\n"
                b"Root-Is-Purelib: false\n"
                b"Tag: py3-none-any\n"
            )
            whl.writestr("zccache-1.2.3.dist-info/METADATA", metadata)
            whl.writestr("zccache-1.2.3.dist-info/WHEEL", wheel)
            record = io.StringIO()
            writer = csv.writer(record, lineterminator="\n")
            writer.writerow(("zccache-1.2.3.data/scripts/zccache", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/METADATA", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/WHEEL", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/RECORD", "", ""))
            whl.writestr("zccache-1.2.3.dist-info/RECORD", record.getvalue())


def test_assert_wheel_script_metadata_accepts_executable_unix_entries(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-manylinux_2_17_x86_64.whl"
    _write_wheel(wheel_path, create_system=3, mode=0o100755)

    assert_wheel_script_metadata(wheel_path)


def test_assert_wheel_script_metadata_rejects_bad_script_metadata(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-manylinux_2_17_x86_64.whl"
    _write_wheel(wheel_path, create_system=0, mode=0o100644)

    with pytest.raises(
        SystemExit,
        match=(
            r"invalid wheel script metadata for "
            r"zccache-1\.2\.3-py3-none-manylinux_2_17_x86_64\.whl:"
            r"zccache-1\.2\.3\.data/scripts/zccache "
            r"\(create_system=0, mode=0o100644, "
            r"is_regular_file=True, has_exec_bit=False\)"
        ),
    ):
        assert_wheel_script_metadata(wheel_path)


@pytest.mark.skipif(
    os.name == "nt",
    reason="Windows install targets do not expose POSIX execute bits",
)
def test_assert_installed_wheel_scripts_executable_accepts_pip_target_install(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-any.whl"
    _write_wheel(
        wheel_path,
        create_system=3,
        mode=0o100755,
        include_dist_info=True,
    )

    assert_installed_wheel_scripts_executable(wheel_path)


def test_can_smoke_install_wheel_on_host_rejects_cross_arch_linux_wheel(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(release_workflow.os, "name", "posix")
    monkeypatch.setattr(release_workflow.sys, "platform", "linux")
    monkeypatch.setattr(release_workflow.platform_module, "machine", lambda: "x86_64")

    assert release_workflow._can_smoke_install_wheel_on_host(
        Path("zccache-1.2.3-py3-none-manylinux_2_17_x86_64.whl")
    )
    assert not release_workflow._can_smoke_install_wheel_on_host(
        Path("zccache-1.2.3-py3-none-manylinux_2_17_aarch64.whl")
    )


def test_can_smoke_install_wheel_on_host_accepts_pure_python_wheel(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(release_workflow.os, "name", "posix")

    assert release_workflow._can_smoke_install_wheel_on_host(
        Path("zccache-1.2.3-py3-none-any.whl")
    )


def test_check_crates_versions_reports_all_existing_crates(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(release_workflow, "RUST_PUBLISH_ORDER", ["zccache-a", "zccache-b"])
    monkeypatch.setattr(release_workflow, "crate_version_exists", lambda _name, _version: True)

    assert release_workflow.check_crates_versions("1.2.3") == {
        "zccache-a",
        "zccache-b",
    }


def test_command_check_registries_writes_registry_completion_outputs(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setattr(release_workflow, "RUST_PUBLISH_ORDER", ["zccache-a", "zccache-b"])
    monkeypatch.setattr(release_workflow, "crate_version_exists", lambda _name, _version: True)
    monkeypatch.setattr(
        release_workflow,
        "read_project_meta",
        lambda: ("zccache", "1.2.3", "", ">=3.9", ""),
    )
    expected_wheels = release_workflow.expected_pypi_wheel_filenames("zccache", "1.2.3")
    monkeypatch.setattr(
        release_workflow,
        "check_pypi_version",
        lambda _name, _version: expected_wheels,
    )
    output_path = tmp_path / "github-output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output_path))

    release_workflow.command_check_registries(None)  # type: ignore[arg-type]

    assert output_path.read_text(encoding="utf-8") == (
        "pypi_complete=true\n"
        "crates_complete=true\n"
    )


def _release_workflow_job(job: str) -> str:
    """Return the YAML text of one top-level job in `release-auto.yml`.

    Text slicing rather than PyYAML: the repo does not carry a YAML runtime
    dependency for CI tests, and the surrounding tests parse workflows the
    same way.
    """
    path = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release-auto.yml"
    text = path.read_text(encoding="utf-8")
    marker = f"\n  {job}:\n"
    start = text.index(marker) + 1
    match = re.search(r"\n  [a-z][a-z0-9-]*:\n", text[start + 1 :])
    end = len(text) if match is None else start + 1 + match.start()
    return text[start:end]


def test_wheel_smoke_matrix_covers_exactly_the_built_wheel_families() -> None:
    """Every smoke leg must name a wheel family `build-wheels` produces.

    A leg for an unbuilt family (once: musllinux) can never pass, because
    `pip --no-index --find-links dist/wheels` finds no candidate. That failure
    is not cosmetic: `publish-pypi` needs `test-wheels`, so a stray leg blocks
    every release. A missing leg is the opposite hole -- a family ships to
    PyPI without ever being installed.
    """
    smoke_tags = set(
        re.findall(r"^\s+wheel_plat:\s*(\S+)$", _release_workflow_job("test-wheels"), re.M)
    )
    built_tags = {
        ".".join(plat_tags) for plat_tags in release_workflow.PLATFORMS.values()
    }

    assert smoke_tags == built_tags, (
        "release-auto.yml test-wheels matrix drifted from "
        f"ci/release_workflow.py PLATFORMS: unbuilt legs {sorted(smoke_tags - built_tags)}, "
        f"untested wheels {sorted(built_tags - smoke_tags)}"
    )


def test_wheel_smoke_matrix_has_one_leg_per_wheel() -> None:
    """No duplicate legs: a repeated family hides a missing one behind a
    passing job name, which is how the musllinux legs stayed unnoticed."""
    smoke_tags = re.findall(
        r"^\s+wheel_plat:\s*(\S+)$", _release_workflow_job("test-wheels"), re.M
    )

    assert len(smoke_tags) == len(set(smoke_tags)), (
        f"duplicate wheel_plat legs in release-auto.yml test-wheels: {sorted(smoke_tags)}"
    )
