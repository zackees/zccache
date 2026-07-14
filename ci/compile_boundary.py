#!/usr/bin/env python3
"""Inventory compile-handler coupling and reject new glob imports."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SERVER = Path("crates/zccache-daemon-core/src/daemon/server")
COMPILE_ROOT_FILES = {
    "handle_compile.rs",
    "handle_compile_ephemeral.rs",
    "handle_compile_multi.rs",
    "handle_compile_multi_args.rs",
    "handle_compile_multi_preflight.rs",
    "handle_compile_multi_staged.rs",
    "handle_compile_multi_types.rs",
}
ALLOWED_GLOB_IMPORTS = {
    "handle_compile.rs",
    "handle_compile/cached_hit.rs",
    "handle_compile/error_cache.rs",
    "handle_compile_ephemeral.rs",
    "handle_compile_multi.rs",
    "handle_compile_multi_args.rs",
    "handle_compile_multi_preflight.rs",
    "handle_compile_multi_staged.rs",
    "handle_compile_multi_types.rs",
}
GLOB_IMPORT = re.compile(r"^\s*use\s+(?:super|crate::daemon::server)::\*;", re.MULTILINE)
PRIVATE_ITEM = re.compile(
    r"\bpub\(super\)\s+(?:async\s+)?"
    r"(?:struct|enum|trait|fn|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def compile_files(root: Path) -> list[Path]:
    server = root / SERVER
    files = [server / name for name in sorted(COMPILE_ROOT_FILES)]
    files.extend(sorted((server / "handle_compile").rglob("*.rs")))
    return files


def module_private_items(text: str) -> set[str]:
    """Find module-level private items while excluding impl/test members."""
    items: set[str] = set()
    depth = 0
    for line in text.splitlines():
        code = line.split("//", 1)[0]
        if depth == 0:
            items.update(PRIVATE_ITEM.findall(code))
        depth += code.count("{") - code.count("}")
        depth = max(depth, 0)
    return items


def inventory(root: Path) -> dict[str, object]:
    server = root / SERVER
    files = compile_files(root)
    glob_files = [path.relative_to(server).as_posix() for path in files if GLOB_IMPORT.search(path.read_text(encoding="utf-8"))]
    private_symbols: set[str] = set()
    compile_set = {path.resolve() for path in files}
    for path in server.rglob("*.rs"):
        if path.resolve() not in compile_set:
            private_symbols.update(module_private_items(path.read_text(encoding="utf-8")))
    references: dict[str, list[str]] = {}
    for path in files:
        text = path.read_text(encoding="utf-8")
        relative = path.relative_to(server).as_posix()
        for symbol in private_symbols:
            if re.search(rf"\b{re.escape(symbol)}\b", text):
                references.setdefault(symbol, []).append(relative)
    unexpected = sorted(set(glob_files) - ALLOWED_GLOB_IMPORTS)
    return {
        "schema_version": 1,
        "compile_files": len(files),
        "glob_imports": sorted(glob_files),
        "allowed_glob_imports": sorted(ALLOWED_GLOB_IMPORTS),
        "unexpected_glob_imports": unexpected,
        "server_private_references": {key: sorted(value) for key, value in sorted(references.items())},
        "server_private_symbol_count": len(references),
        "shared_state_reference_files": sorted(path.relative_to(server).as_posix() for path in files if re.search(r"\bSharedState\b", path.read_text(encoding="utf-8"))),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--deny-all-globs", action="store_true")
    args = parser.parse_args()
    result = inventory(args.root)
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    if result["unexpected_glob_imports"]:
        return 1
    if args.deny_all_globs and result["glob_imports"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
