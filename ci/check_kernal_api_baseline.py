"""Verify the checked-in kernal-api migration inventory (zccache#1519)."""

from __future__ import annotations

from collections import defaultdict
from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parent.parent
INVENTORY = ROOT / "docs" / "architecture" / "kernal-api-migration.toml"
PLATFORM_ROOT = ROOT / "crates" / "zccache-platform" / "src" / "platform"
BACKENDS = {"tokio", "tokio-util", "running-process", "interprocess", "crash-handler", "memmap2", "fs2", "blake3", "libc", "windows-sys"}
# The facade exposes both free functions and associated methods.  Keep this
# deliberately lexical: the inventory is a drift detector, not a Rust parser,
# and every externally visible declaration starts with `pub` in these modules.
PUBLIC_ITEM = re.compile(
    r"^\s*pub\s+(?:async\s+)?(?:const\s+)?(?:fn|struct|enum|trait|type|const)\s+([A-Za-z_][A-Za-z0-9_]*)"
)
VALID_DISPOSITIONS = {"reuse", "extend", "move", "retain"}


def public_items(path: Path) -> set[str]:
    return {match.group(1) for line in path.read_text(encoding="utf-8").splitlines() if (match := PUBLIC_ITEM.match(line))}


def production_dependencies(manifest: Path) -> set[str]:
    """Return backend keys in normal and target production dependency tables."""
    parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
    found = set(parsed.get("dependencies", {})) & BACKENDS
    for target in parsed.get("target", {}).values():
        found |= set(target.get("dependencies", {})) & BACKENDS
    return found


def check(root: Path = ROOT) -> list[str]:
    inventory_path = root / INVENTORY.relative_to(ROOT)
    platform_root = root / PLATFORM_ROOT.relative_to(ROOT)
    data = tomllib.loads(inventory_path.read_text(encoding="utf-8"))
    errors: list[str] = []
    mapped_by_source: dict[str, set[str]] = defaultdict(set)
    for group in data.get("platform_group", []):
        source = group.get("source", "")
        disposition = group.get("disposition")
        if disposition not in VALID_DISPOSITIONS:
            errors.append(f"invalid disposition for {source}: {disposition!r}")
        if disposition == "retain" and (not group.get("owner") or not group.get("reason")):
            errors.append(f"retained mapping lacks owner or reason: {source}")
        if disposition != "retain" and not group.get("kernel_capability"):
            errors.append(f"kernel mapping lacks capability: {source}")
        mapped_by_source[source].update(group.get("items", []))

    for source in sorted(platform_root.rglob("*.rs")):
        if source.name == "tests.rs":
            continue
        relative = source.relative_to(root).as_posix()
        actual = public_items(source)
        if not actual:
            continue
        mapped = mapped_by_source.pop(relative, set())
        missing, stale = sorted(actual - mapped), sorted(mapped - actual)
        if missing:
            errors.append(f"unmapped public platform items in {relative}: {', '.join(missing)}")
        if stale:
            errors.append(f"stale platform mapping in {relative}: {', '.join(stale)}")
    for source, items in sorted(mapped_by_source.items()):
        errors.append(f"inventory source is absent or non-public: {source}: {', '.join(sorted(items))}")

    expected: set[tuple[str, str]] = set()
    for dependency in data.get("backend_dependency", []):
        name, disposition = dependency.get("name"), dependency.get("disposition")
        if name not in BACKENDS:
            errors.append(f"unknown backend dependency: {name!r}")
        if disposition not in VALID_DISPOSITIONS:
            errors.append(f"invalid backend disposition for {name}: {disposition!r}")
        if not dependency.get("kernel_capability"):
            errors.append(f"backend mapping lacks capability: {name}")
        expected |= {(manifest, name) for manifest in dependency.get("manifests", [])}
    actual: set[tuple[str, str]] = set()
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        relative = manifest.relative_to(root).as_posix()
        actual |= {(relative, name) for name in production_dependencies(manifest)}
    for manifest, name in sorted(actual - expected):
        errors.append(f"unmapped production backend dependency: {manifest}: {name}")
    for manifest, name in sorted(expected - actual):
        errors.append(f"stale backend dependency mapping: {manifest}: {name}")

    for entry in data.get("characterization", []):
        contract, tests = entry.get("contract", "<unnamed>"), entry.get("tests", [])
        if not tests:
            errors.append(f"characterization lacks evidence paths: {contract}")
        for path in tests:
            if not (root / path).is_file():
                errors.append(f"characterization path missing: {contract}: {path}")
    return errors


def main() -> int:
    errors = check()
    if errors:
        print("kernal-api migration inventory errors:", file=sys.stderr)
        print("\n".join(f"- {error}" for error in errors), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
