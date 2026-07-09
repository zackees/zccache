from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path


def _load_build_release_manifest():
    module_path = Path(__file__).resolve().parents[1] / "build_release_manifest.py"
    spec = importlib.util.spec_from_file_location("build_release_manifest", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


build_release_manifest = _load_build_release_manifest()


def _repo_text(*parts: str) -> str:
    return (Path(__file__).resolve().parents[2] / Path(*parts)).read_text(encoding="utf-8")


# The six targets that produce release assets (upload_release: "true").
RELEASE_TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)


def _seed_release_assets(assets_dir: Path, version: str = "1.12.15") -> None:
    assets_dir.mkdir(parents=True, exist_ok=True)
    for target in RELEASE_TARGETS:
        ext = ".zip" if "windows" in target else ".tar.gz"
        (assets_dir / f"zccache-v{version}-{target}{ext}").write_bytes(
            f"binary-{target}".encode()
        )
        # A debug sidecar archive that must NOT appear in the manifest.
        (assets_dir / f"zccache-v{version}-{target}-debug{ext}").write_bytes(b"debug")
    # Non-archive assets that must be ignored.
    (assets_dir / "install.sh").write_text("#!/bin/sh\n")
    (assets_dir / "install.ps1").write_text("# ps\n")
    (assets_dir / "SHA256SUMS").write_text("deadbeef  x\n")


def test_manifest_lists_every_release_target(tmp_path: Path) -> None:
    assets = tmp_path / "release-assets"
    _seed_release_assets(assets)

    manifest = build_release_manifest.build_manifest(
        assets, version="1.12.15", tag="v1.12.15", repo="zackees/zccache"
    )

    assert manifest["version"] == "1.12.15"
    assert manifest["tag"] == "v1.12.15"
    targets = [b["target"] for b in manifest["binaries"]]
    assert sorted(targets) == sorted(RELEASE_TARGETS)


def test_manifest_excludes_debug_installers_and_checksums(tmp_path: Path) -> None:
    assets = tmp_path / "release-assets"
    _seed_release_assets(assets)

    manifest = build_release_manifest.build_manifest(
        assets, version="1.12.15", tag="v1.12.15", repo="zackees/zccache"
    )

    assets_named = [b["asset"] for b in manifest["binaries"]]
    assert not any("-debug" in name for name in assets_named)
    assert not any(name in ("install.sh", "install.ps1", "SHA256SUMS") for name in assets_named)
    assert len(manifest["binaries"]) == len(RELEASE_TARGETS)


def test_manifest_url_uses_actual_tag_and_correct_sha256(tmp_path: Path) -> None:
    assets = tmp_path / "release-assets"
    _seed_release_assets(assets)

    # A bare (non-v) release tag must be preserved verbatim in the URL path,
    # while the asset filename keeps its v-prefix.
    manifest = build_release_manifest.build_manifest(
        assets, version="1.12.15", tag="1.12.15", repo="zackees/zccache"
    )

    entry = next(b for b in manifest["binaries"] if b["target"] == "x86_64-apple-darwin")
    asset = "zccache-v1.12.15-x86_64-apple-darwin.tar.gz"
    assert entry["asset"] == asset
    assert entry["url"] == (
        f"https://github.com/zackees/zccache/releases/download/1.12.15/{asset}"
    )
    expected_sha = hashlib.sha256((assets / asset).read_bytes()).hexdigest()
    assert entry["sha256"] == expected_sha


def test_manifest_is_deterministically_ordered(tmp_path: Path) -> None:
    assets = tmp_path / "release-assets"
    _seed_release_assets(assets)

    manifest = build_release_manifest.build_manifest(
        assets, version="1.12.15", tag="v1.12.15", repo="zackees/zccache"
    )
    targets = [b["target"] for b in manifest["binaries"]]
    assert targets == sorted(targets)


def test_manifest_targets_match_workflow_upload_release_true() -> None:
    # Guard: the manifest test's target set stays in sync with the workflow's
    # `upload_release: "true"` matrix rows.
    workflow = _repo_text(".github", "workflows", "release-auto.yml")
    blocks = workflow.split("- target: ")
    releasing = set()
    for block in blocks[1:]:
        target = block.splitlines()[0].strip()
        head = block[: block.find("- target: ") if "- target: " in block else len(block)]
        if 'upload_release: "true"' in head:
            releasing.add(target)
    assert releasing == set(RELEASE_TARGETS)
