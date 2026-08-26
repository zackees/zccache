from pathlib import Path

from ci import check_kernal_api_baseline


def test_current_inventory_is_complete() -> None:
    assert check_kernal_api_baseline.check() == []


def test_check_rejects_a_missing_platform_mapping(tmp_path: Path) -> None:
    root = tmp_path
    source = root / "crates/zccache-platform/src/platform/host.rs"
    source.parent.mkdir(parents=True)
    source.write_text("pub fn host_fact() {}\n", encoding="utf-8")
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text("", encoding="utf-8")
    (root / "crates").mkdir(exist_ok=True)

    assert any("unmapped public platform items" in error for error in check_kernal_api_baseline.check(root))


def test_check_rejects_an_unmapped_public_const_function(tmp_path: Path) -> None:
    root = tmp_path
    source = root / "crates/zccache-platform/src/platform/host.rs"
    source.parent.mkdir(parents=True)
    source.write_text("pub const fn is_host() -> bool { true }\n", encoding="utf-8")
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text("", encoding="utf-8")
    (root / "crates").mkdir(exist_ok=True)

    assert any("unmapped public platform items" in error for error in check_kernal_api_baseline.check(root))


def test_check_rejects_a_stale_platform_mapping(tmp_path: Path) -> None:
    root = tmp_path
    source = root / "crates/zccache-platform/src/platform/host.rs"
    source.parent.mkdir(parents=True)
    source.write_text("pub fn host_fact() {}\n", encoding="utf-8")
    inventory = root / "docs/architecture/kernal-api-migration.toml"
    inventory.parent.mkdir(parents=True)
    inventory.write_text(
        """[[platform_group]]
source = "crates/zccache-platform/src/platform/host.rs"
items = ["old_host_fact"]
disposition = "extend"
kernel_capability = "host facts"
""",
        encoding="utf-8",
    )
    (root / "crates").mkdir(exist_ok=True)

    assert any("stale platform mapping" in error for error in check_kernal_api_baseline.check(root))
