from __future__ import annotations

import shutil
from pathlib import Path

import pytest
import tomllib

from ci import release_checks
from ci.publish_amalgamate import (
    INTERNAL_MODULES,
    AmalgamatedModule,
    drop_python_extension_bindings,
    prepare_zccache_crate_for_publish,
    rewrite_rust_source_for_amalgamation,
    rewrite_zccache_manifest,
)


def test_compiler_amalgamation_retains_native_surface() -> None:
    source = '#[cfg(feature = "native")]\npub mod parse;\n#[cfg(all(test, feature = "native"))]\nmod tests;\n'
    assert (
        rewrite_rust_source_for_amalgamation(source, module="compiler", module_map={})
        == "pub mod parse;\n#[cfg(test)]\nmod tests;\n"
    )


def test_zccache_publish_manifest_keeps_gha_feature_dependencies(
    tmp_path: Path,
) -> None:
    source = Path(__file__).parents[2] / "crates" / "zccache" / "Cargo.toml"
    manifest_path = tmp_path / "Cargo.toml"
    shutil.copyfile(source, manifest_path)
    rewrite_zccache_manifest(
        manifest_path,
        {module.crate: module.module for module in INTERNAL_MODULES},
    )
    manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))

    assert manifest["features"]["gha"] == ["dep:reqwest", "dep:sha2"]
    assert manifest["features"]["formatter"] == []
    assert manifest["dependencies"]["sha2"] == {
        "workspace": True,
        "optional": True,
    }
    assert "zccache-cli-core" not in manifest["dependencies"]
    assert "zccache-daemon-core" not in manifest["dependencies"]


def test_zccache_publish_manifest_keeps_all_amalgamated_kernal_api_features(
    tmp_path: Path,
) -> None:
    source = Path(__file__).parents[2] / "crates" / "zccache" / "Cargo.toml"
    manifest_path = tmp_path / "Cargo.toml"
    shutil.copyfile(source, manifest_path)
    rewrite_zccache_manifest(
        manifest_path,
        {module.crate: module.module for module in INTERNAL_MODULES},
    )
    manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))

    assert set(manifest["dependencies"]["kernal-api"]["features"]) == {
        "broker-client",
        "crash",
        "daemon-frame-v1",
        "daemon-identity",
        "daemon-registration",
        "daemon-registration-v2",
        "fs",
        "fs-watch",
        "ipc",
        "ipc-async",
    }


def test_rewrite_rust_source_rebases_crate_root_and_internal_crate_paths() -> None:
    module_map = {
        "zccache-core": "core",
        "zccache-hash": "hash",
        "zccache-protocol": "protocol",
    }
    source = """
use crate::{ProtocolError, Request};
use zccache_core::NormalizedPath;

fn hash(path: &zccache_core::NormalizedPath) -> zccache_hash::ContentHash {
    zccache_hash::hash_file(path).unwrap()
}
"""

    rewritten = rewrite_rust_source_for_amalgamation(
        source,
        module="protocol",
        module_map=module_map,
    )

    assert "use crate::protocol::{ProtocolError, Request};" in rewritten
    assert "use crate::core::NormalizedPath;" in rewritten
    assert "path: &crate::core::NormalizedPath" in rewritten
    assert "-> crate::hash::ContentHash" in rewritten
    assert "crate::hash::hash_file(path)" in rewritten
    assert "zccache_core" not in rewritten
    assert "zccache_hash" not in rewritten


def test_drop_python_extension_bindings_removes_extension_only_exports() -> None:
    source = """
pub mod scan;
#[cfg(feature = "python")]
mod python;
pub use scan::walk_files;
#[cfg(feature = "python")]
pub use python::{NativeWatcher, WatchBatch};
"""

    rewritten = drop_python_extension_bindings(source)

    assert "pub mod scan;" in rewritten
    assert "pub use scan::walk_files;" in rewritten
    assert "python" not in rewritten


def test_rewrite_zccache_manifest_removes_facade_deps_and_retargets_features(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        """
[package]
name = "zccache"

[features]
cli = ["download-client", "gha", "zccache-artifact/cli"]
formatter = ["dep:zccache-cli-core", "zccache-cli-core/formatter"]
download = ["dep:zccache-download", "dep:futures", "dep:reqwest"]
download-protocol = ["download", "dep:zccache-download-protocol"]
gha = ["dep:zccache-gha", "zccache-artifact/gha"]
symbols = ["dep:zccache-symbols"]

[dependencies]
# internal facade crates
zccache-artifact = { workspace = true }
zccache-core = { workspace = true }
zccache-download = { workspace = true, optional = true }
zccache-gha = { workspace = true, optional = true }
futures = { workspace = true, optional = true }
reqwest = { workspace = true, optional = true }
sha2 = { workspace = true, optional = true }

[dev-dependencies]
zccache = { path = ".", features = ["test-support"] }
tokio = { workspace = true }
""".lstrip(),
        encoding="utf-8",
    )

    rewrite_zccache_manifest(
        manifest,
        {
            "zccache-artifact": "artifact",
            "zccache-core": "core",
            "zccache-download": "download",
            "zccache-download-protocol": "download_protocol",
            "zccache-gha": "gha",
            "zccache-symbols": "symbols",
        },
    )

    text = manifest.read_text(encoding="utf-8")
    assert "zccache-artifact =" not in text
    assert "zccache-core =" not in text
    assert "zccache-download =" not in text
    assert 'cli = ["download-client", "gha"]' in text
    assert "formatter = []" in text
    assert 'download = ["dep:futures", "dep:reqwest"]' in text
    assert 'download-protocol = ["download"]' in text
    assert 'gha = ["dep:reqwest", "dep:sha2"]' in text
    assert "symbols = []" in text
    assert 'zccache = { path = "."' not in text
    assert "prost-build = { workspace = true }" in text
    assert "protoc-bin-vendored = { workspace = true }" in text


def test_prepare_zccache_crate_for_publish_copies_and_rewrites_sources(
    tmp_path: Path,
) -> None:
    root = tmp_path
    zccache = root / "crates" / "zccache"
    (zccache / "src").mkdir(parents=True)
    (zccache / "src" / "lib.rs").write_text(
        "pub use zccache_core as core;\n",
        encoding="utf-8",
    )
    (zccache / "Cargo.toml").write_text(
        """
[package]
name = "zccache"

[features]
gha = ["dep:zccache-gha", "zccache-artifact/gha"]

[dependencies]
zccache-core = { workspace = true }
zccache-hash = { workspace = true }
reqwest = { workspace = true, optional = true }
sha2 = { workspace = true, optional = true }
""".lstrip(),
        encoding="utf-8",
    )
    (zccache / "build.rs").write_text("fn main() {}\n", encoding="utf-8")

    core_src = root / "crates" / "zccache-core" / "src"
    core_src.mkdir(parents=True)
    (core_src / "lib.rs").write_text(
        "pub mod config;\nuse zccache_hash::ContentHash;\n",
        encoding="utf-8",
    )
    (core_src / "config.rs").write_text(
        "pub fn version() -> &'static str { crate::VERSION }\n",
        encoding="utf-8",
    )
    hash_src = root / "crates" / "zccache-hash" / "src"
    hash_src.mkdir(parents=True)
    (hash_src / "lib.rs").write_text(
        '#[cfg(feature = "native")]\nmod native;\n'
        '#[cfg(feature = "native")]\npub use native::*;\n',
        encoding="utf-8",
    )
    (hash_src / "native.rs").write_text(
        "pub struct ContentHash;\n",
        encoding="utf-8",
    )
    proto_dir = root / "crates" / "zccache-protocol" / "proto"
    proto_dir.mkdir(parents=True)
    (proto_dir / "zccache_v1.proto").write_text(
        'syntax = "proto3";\n',
        encoding="utf-8",
    )

    prepare_zccache_crate_for_publish(
        root,
        modules=(
            AmalgamatedModule("zccache-core", "core", "pub mod core;"),
            AmalgamatedModule("zccache-hash", "hash", "pub mod hash;"),
        ),
    )

    assert (zccache / "src" / "core" / "mod.rs").is_file()
    assert (zccache / "src" / "hash" / "mod.rs").is_file()
    assert (zccache / "src" / "hash" / "native.rs").is_file()
    assert (zccache / "src" / "hash" / "mod.rs").read_text(
        encoding="utf-8"
    ) == "mod native;\npub use native::*;\n"
    assert (zccache / "proto" / "zccache_v1.proto").is_file()
    assert "crate::hash::ContentHash" in (
        zccache / "src" / "core" / "mod.rs"
    ).read_text(encoding="utf-8")
    assert "crate::core::VERSION" in (zccache / "src" / "core" / "config.rs").read_text(
        encoding="utf-8"
    )
    assert "pub mod core;" in (zccache / "src" / "lib.rs").read_text(encoding="utf-8")
    assert '#[cfg(feature = "download-daemon-entry")]' in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert "pub mod download_daemon_entry;" in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert "pub mod dev_daemon_identity;" in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert '#[cfg(feature = "formatter")]' in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert "pub use cli_core::formatter;" in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert "zccache-core =" not in (zccache / "Cargo.toml").read_text(encoding="utf-8")


def test_release_metadata_allows_only_public_zccache_crate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    assert release_checks.RUST_PUBLISH_ORDER == ["zccache"]

    monkeypatch.setattr(
        release_checks,
        "read_workspace_metadata",
        lambda: {
            "packages": [
                {"name": "zccache", "dependencies": []},
                {"name": "zccache-core", "dependencies": []},
            ]
        },
    )

    with pytest.raises(release_checks.ReleaseCheckError, match="zccache-core"):
        release_checks.validate_rust_publish_order()


def test_facade_kernal_api_features_cover_internal_crates() -> None:
    """The published facade must enable every kernal-api feature any internal
    crate enables.

    `publish_amalgamate` copies each internal crate's source into
    `crates/zccache`, but it does not merge their dependency features: the
    facade manifest declares those by hand. A feature only one internal crate
    turned on therefore still compiles in the workspace (that crate enables it
    for itself) and fails only inside `cargo package --allow-dirty -p zccache`,
    which runs at release time. zccache 1.14.4 failed to publish exactly this
    way after #1600 (`cannot find crash in kernal_api`, `cannot find
    broker_client in kernal_api`, then `fs_watch::Watcher`).
    """
    root = Path(__file__).parents[2]

    def kernal_features(manifest: Path) -> set[str]:
        table = tomllib.loads(manifest.read_text(encoding="utf-8"))
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            entry = table.get(section, {}).get("kernal-api")
            if isinstance(entry, dict):
                return set(entry.get("features", []))
        return set()

    facade = kernal_features(root / "crates" / "zccache" / "Cargo.toml")
    required: dict[str, set[str]] = {}
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        if manifest.parent.name == "zccache":
            continue
        features = kernal_features(manifest)
        if features:
            required[manifest.parent.name] = features

    assert required, "no internal crate declares kernal-api; this guard is stale"
    missing = {
        crate: sorted(features - facade)
        for crate, features in required.items()
        if features - facade
    }
    assert not missing, (
        "crates/zccache/Cargo.toml must enable every kernal-api feature its "
        f"internal crates use; missing: {missing}. Add them to the facade's "
        "kernal-api features list, or `cargo package -p zccache` fails during "
        "the release."
    )
