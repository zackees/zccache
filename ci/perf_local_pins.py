"""Align soldr's exact pins with the zccache checkout under test (#1601).

The local perf harness builds soldr with this checkout patched in. Both
repositories pin shared crates exactly (kernal-api is below 1.0, so every
consumer pins one published release). When zccache moves such a pin ahead of
soldr (zccache `=0.1.22`, soldr `=0.1.20`), cargo cannot resolve the pair and
the soldr builder fails before any cell runs, which blocks the whole gate.

The checkout under test is the side being measured, so soldr's exact pins on
crates zccache also pins exactly are rewritten to zccache's version, the same
way `align_soldr_zccache_requirement` handles soldr's pin on zccache itself.
Only exact (`=`) pins on both sides are touched.
"""

from __future__ import annotations

import re
from pathlib import Path

import tomllib

# `name = "=1.2.3"` or `name = { version = "=1.2.3", ... }`, exact pins only.
_EXACT_REQUIREMENT = re.compile(
    r'^(?P<prefix>\s*(?P<name>[A-Za-z0-9_-]+)\s*=\s*(?:\{[^}\n]*?\bversion\s*=\s*)?")'
    r'=(?P<version>[^"]*)(?P<suffix>")',
    re.MULTILINE,
)


def exact_workspace_pins(repo_root: Path) -> dict[str, str]:
    """`{crate: version}` for every exact `=` pin in the workspace dependencies."""
    manifest = tomllib.loads((repo_root / "Cargo.toml").read_text(encoding="utf-8"))
    pins: dict[str, str] = {}
    for name, spec in manifest.get("workspace", {}).get("dependencies", {}).items():
        version = spec if isinstance(spec, str) else spec.get("version")
        if isinstance(version, str) and version.startswith("="):
            pins[name] = version[1:]
    return pins


def align_soldr_exact_pins(soldr_src: Path, pins: dict[str, str]) -> list[str]:
    """Rewrite soldr's exact pins on crates in ``pins`` to the pinned version.

    Returns one `manifest: crate old -> new` line per rewrite.
    """
    changes: list[str] = []
    for manifest in sorted(soldr_src.rglob("Cargo.toml")):
        if set(manifest.relative_to(soldr_src).parts) & {"target", ".git", "_vender"}:
            continue
        text = manifest.read_text(encoding="utf-8")

        def rewrite(match: re.Match[str], manifest: Path = manifest) -> str:
            name, version = match["name"], match["version"]
            wanted = pins.get(name)
            if wanted is None or wanted == version:
                return match.group(0)
            changes.append(
                f"{manifest.relative_to(soldr_src)}: {name} ={version} -> ={wanted}"
            )
            return f"{match['prefix']}={wanted}{match['suffix']}"

        updated = _EXACT_REQUIREMENT.sub(rewrite, text)
        if updated != text:
            manifest.write_text(updated, encoding="utf-8")
    return changes
