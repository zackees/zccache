from __future__ import annotations

import shutil
import tomllib
from pathlib import Path

import pytest

from ci import release_checks
from ci.publish_amalgamate import (
    AmalgamatedModule,
    INTERNAL_MODULES,
    drop_python_extension_bindings,
    prepare_zccache_crate_for_publish,
    rewrite_rust_source_for_amalgamation,
    rewrite_zccache_manifest,
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
zccache-platform = { workspace = true }
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
        "pub struct ContentHash;\n",
        encoding="utf-8",
    )
    platform_src = root / "crates" / "zccache-platform" / "src"
    platform_src.mkdir(parents=True)
    (platform_src / "lib.rs").write_text(
        "pub mod platform;\n",
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
            AmalgamatedModule("zccache-platform", "platform", "mod platform;"),
        ),
    )

    assert (zccache / "src" / "core" / "mod.rs").is_file()
    assert (zccache / "src" / "hash" / "mod.rs").is_file()
    assert (zccache / "proto" / "zccache_v1.proto").is_file()
    assert "crate::hash::ContentHash" in (
        zccache / "src" / "core" / "mod.rs"
    ).read_text(encoding="utf-8")
    assert "crate::core::VERSION" in (
        zccache / "src" / "core" / "config.rs"
    ).read_text(encoding="utf-8")
    assert "pub mod core;" in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )
    assert '#[cfg(feature = "download-daemon-entry")]' in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert "pub mod download_daemon_entry;" in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert "pub mod dev_daemon_identity;" in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert '#[cfg(feature = "formatter")]' in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert "pub use cli_core::formatter;" in (
        zccache / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    assert "zccache-core =" not in (zccache / "Cargo.toml").read_text(
        encoding="utf-8"
    )
    assert "zccache-platform =" not in (zccache / "Cargo.toml").read_text(
        encoding="utf-8"
    )


def test_public_crate_matches_platform_native_dependencies() -> None:
    root = Path(__file__).parents[2]
    public_manifest = tomllib.loads(
        (root / "crates" / "zccache" / "Cargo.toml").read_text(encoding="utf-8")
    )
    platform_manifest = tomllib.loads(
        (root / "crates" / "zccache-platform" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
    )

    for target in ("cfg(unix)", "cfg(windows)"):
        assert public_manifest["target"][target]["dependencies"] == (
            platform_manifest["target"][target]["dependencies"]
        )


def test_platform_host_modules_can_be_reexported_after_amalgamation() -> None:
    root = Path(__file__).parents[2]
    platform_lib = (
        root / "crates" / "zccache-platform" / "src" / "lib.rs"
    ).read_text(encoding="utf-8")

    assert "pub(crate) mod platform_win;" in platform_lib
    assert "pub(crate) mod platform_linux;" in platform_lib
    assert "pub(crate) mod platform_macos;" in platform_lib


def test_platform_module_is_declared_as_a_private_root_module() -> None:
    platform = next(
        module for module in INTERNAL_MODULES if module.crate == "zccache-platform"
    )
    assert platform.module == "platform"
    # The platform leaf is internal machinery, not a public crates.io API.
    # The amalgamated module is private, so platform primitives unused by the
    # public crate would otherwise fail release packaging under `-D warnings`.
    assert platform.declaration == (
        "#[allow(dead_code, unused_imports)]\nmod platform;"
    )
    assert platform.drop_python_bindings is False


def test_prepare_copies_platform_sources_and_rewrites_platform_paths(
    tmp_path: Path,
) -> None:
    root = tmp_path
    zccache = root / "crates" / "zccache"
    (zccache / "src").mkdir(parents=True)
    (zccache / "src" / "lib.rs").write_text("", encoding="utf-8")
    (zccache / "Cargo.toml").write_text(
        "[package]\nname = \"zccache\"\n\n[dependencies]\n"
        "zccache-platform = { workspace = true }\n",
        encoding="utf-8",
    )
    (zccache / "build.rs").write_text("fn main() {}\n", encoding="utf-8")

    proto_dir = root / "crates" / "zccache-protocol" / "proto"
    proto_dir.mkdir(parents=True)
    (proto_dir / "zccache_v1.proto").write_text(
        'syntax = "proto3";\n',
        encoding="utf-8",
    )

    platform_src = root / "crates" / "zccache-platform" / "src"
    platform_src.mkdir(parents=True)
    (platform_src / "lib.rs").write_text(
        "mod platform;\npub use platform::fs;\n",
        encoding="utf-8",
    )
    (platform_src / "platform.rs").write_text(
        "pub mod fs;\npub fn leaf() -> u32 { crate::platform::fs::answer() }\n",
        encoding="utf-8",
    )
    (platform_src / "platform").mkdir()
    (platform_src / "platform" / "fs.rs").write_text(
        "pub fn answer() -> u32 { 42 }\n",
        encoding="utf-8",
    )

    core_src = root / "crates" / "zccache-core" / "src"
    core_src.mkdir(parents=True)
    (core_src / "lib.rs").write_text(
        "use zccache_platform::fs;\npub fn probe() -> u32 { fs::answer() }\n",
        encoding="utf-8",
    )

    prepare_zccache_crate_for_publish(
        root,
        modules=(
            AmalgamatedModule("zccache-platform", "platform", "mod platform;"),
            AmalgamatedModule("zccache-core", "core", "pub mod core;"),
        ),
    )

    # Platform sources are copied with lib.rs renamed to mod.rs.
    assert (zccache / "src" / "platform" / "mod.rs").is_file()
    assert (zccache / "src" / "platform" / "platform.rs").is_file()
    assert (zccache / "src" / "platform" / "platform" / "fs.rs").is_file()
    # Module-relative paths in the old lib.rs need no rewrite; `crate::`
    # inside platform sources rebases to `crate::platform::`, so facade
    # self-references double the segment and still resolve
    # (crate::platform::platform::fs).
    platform_mod = (zccache / "src" / "platform" / "mod.rs").read_text(
        encoding="utf-8"
    )
    assert "mod platform;" in platform_mod
    assert "pub use platform::fs;" in platform_mod
    platform_facade = (zccache / "src" / "platform" / "platform.rs").read_text(
        encoding="utf-8"
    )
    assert "crate::platform::platform::fs::answer()" in platform_facade
    # `zccache_platform::` in consumers rewrites to `crate::platform::`.
    core_mod = (zccache / "src" / "core" / "mod.rs").read_text(
        encoding="utf-8"
    )
    assert "use crate::platform::fs;" in core_mod
    assert "zccache_platform" not in core_mod
    # The prepared manifest drops the internal path dependency.
    manifest = (zccache / "Cargo.toml").read_text(encoding="utf-8")
    assert "zccache-platform =" not in manifest
    # The private root module is declared in the regenerated lib.rs.
    assert "mod platform;" in (zccache / "src" / "lib.rs").read_text(
        encoding="utf-8"
    )


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
