from __future__ import annotations

from pathlib import Path

import pytest

from ci import release_checks
from ci.publish_amalgamate import (
    AmalgamatedModule,
    drop_python_extension_bindings,
    prepare_zccache_crate_for_publish,
    rewrite_rust_source_for_amalgamation,
    rewrite_zccache_manifest,
)


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
    assert "zccache-core =" not in (zccache / "Cargo.toml").read_text(
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
